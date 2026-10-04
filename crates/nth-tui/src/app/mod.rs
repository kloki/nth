//! The app: its state, the loop that drives it, and the layout of four
//! bands: the header, the content panel, the input panel and the status
//! bar. Row heights never depend on content, so nothing shifts while a turn
//! runs.

mod content;
mod input;
mod job;
mod keys;
mod resume;
mod turn;

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use content::Content;
use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind, MouseEventKind};
use futures::StreamExt;
use input::Input;
use job::Job;
use nth_context::{Context as ProjectContext, Paths};
use nth_protocol::{Ask, BoxError, Effort, Event, ModelInfo, Provider, Tool, Usage};
use nth_session::{Session, Store, Summary, store};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style},
    widgets::{Scrollbar, ScrollbarOrientation},
};
use tokio::{sync::mpsc, task::JoinError, time::MissedTickBehavior};
use turn::Ended;

use crate::{
    chat::Chat,
    command::{Command, Entry},
    git::{self, GitStatus},
    header,
    history::History,
    llm_picker, mention,
    popup::{self, Popup},
    prompt::{self, Mode, Prompt},
    question, session_picker, spinner, status,
};

const WHEEL_LINES: usize = 3;
/// How often a running turn redraws, so the spinner shows every frame and
/// the live reasoning timer advances.
const TICK: Duration = spinner::FRAME;

pub struct App {
    pub chat: Chat,
    pub prompt: Prompt,
    pub mode: Mode,
    /// Prompts sent before, recalled with Up and Down.
    history: History,
    /// Each save writes the whole history; one asked for mid-save runs
    /// after it, since an aborted task can't stop a write already on the
    /// blocking pool.
    history_saving: Job<std::io::Result<()>>,
    /// What fills the content panel.
    content: Content,
    /// What fills the input panel.
    input: Input,
    /// The model and effort the next turn runs with.
    pub model: String,
    pub effort: Effort,
    pub cwd: PathBuf,
    /// The working directory as shown in the status bar, `~` for home.
    pub place: String,
    /// When the running turn started; `None` while idle.
    pub busy_since: Option<Instant>,
    /// Open while the prompt starts a command; Esc closes it until the next edit.
    completion: Option<Completion>,
    /// What `@` mentions complete to, refreshed after every turn since the
    /// model may have added files.
    files: Vec<String>,
    /// Queued again when a turn ends mid-walk, so its files get picked up
    /// by one more walk rather than a second one racing the first.
    indexing: Job<Vec<String>>,
    /// The working tree's git state; `None` outside a repository or until
    /// the first load lands.
    pub git: Option<GitStatus>,
    /// What the last model reply used; `None` until the first one.
    pub usage: Option<Usage>,
    /// Queued again when the tree may have changed mid-load, like `indexing`.
    git_loading: Job<Result<Option<GitStatus>, String>>,
    /// The LLMs the endpoint serves, kept once listed; after a failure the
    /// next open asks again.
    llms: Option<Vec<ModelInfo>>,
    llm_listing: Job<Result<Vec<ModelInfo>, BoxError>>,
    /// Where sessions are saved after every turn; `None` keeps them in
    /// memory only.
    store: Option<Arc<Store>>,
    /// Where instruction files are looked for when a resumed session moves
    /// to another directory.
    paths: Paths,
    /// The session's instruction files and skills; kept here too, since
    /// the session is in the turn task while one runs and `/` still has to
    /// list the skills.
    context: Arc<ProjectContext>,
    /// The configured step limit, which every session the app moves on to
    /// keeps; a loaded one would otherwise fall back to the default.
    max_steps: usize,
    session_listing: Job<Result<Vec<Summary>, store::Error>>,
    /// The session chosen in the session picker, being read.
    session_loading: Job<Result<Session, store::Error>>,
    /// Held here between turns; moved into the turn task while one runs.
    session: Option<Session>,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    /// Where the running turn's tools send their questions.
    asks_tx: mpsc::Sender<Ask>,
    asks_rx: mpsc::Receiver<Ask>,
    /// Questions waiting behind the ones in the input panel.
    asks: VecDeque<Ask>,
    turn: Job<Ended>,
    quit: bool,
}

/// The open completion popup.
enum Completion {
    Command(Popup<Entry>),
    /// `start` is the byte offset of the mention's `@` in the prompt.
    File {
        popup: Popup<String>,
        start: usize,
    },
}

impl Completion {
    fn next(&mut self) {
        match self {
            Completion::Command(popup) => popup.next(),
            Completion::File { popup, .. } => popup.next(),
        }
    }

    fn prev(&mut self) {
        match self {
            Completion::Command(popup) => popup.prev(),
            Completion::File { popup, .. } => popup.prev(),
        }
    }

    /// Where in the prompt the completed token begins; a command is always
    /// the whole prompt.
    fn start(&self) -> usize {
        match self {
            Completion::Command(_) => 0,
            Completion::File { start, .. } => *start,
        }
    }

    fn draw(&self, frame: &mut Frame, area: Rect, anchor: Position) {
        let (rows, selected): (Vec<(String, &str)>, _) = match self {
            Completion::Command(popup) => (Entry::rows(popup.items()), popup.selected_index()),
            Completion::File { popup, .. } => (
                popup.items().iter().map(|f| (f.clone(), "")).collect(),
                popup.selected_index(),
            ),
        };
        popup::draw(frame, area, anchor, &rows, selected);
    }
}

enum Step {
    Terminal(Option<std::io::Result<TermEvent>>),
    Session(Event),
    Asked(Ask),
    TurnEnded(Result<Ended, JoinError>),
    Indexed(Result<Vec<String>, JoinError>),
    GitLoaded(Result<Result<Option<GitStatus>, String>, JoinError>),
    LlmsListed(Result<Result<Vec<ModelInfo>, BoxError>, JoinError>),
    SessionsListed(Result<Result<Vec<Summary>, store::Error>, JoinError>),
    SessionLoaded(Result<Result<Session, store::Error>, JoinError>),
    HistorySaved(Result<std::io::Result<()>, JoinError>),
    Tick,
}

impl App {
    pub fn new(
        session: Session,
        provider: Arc<dyn Provider>,
        tools: Arc<Vec<Box<dyn Tool>>>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(256);
        let (asks_tx, asks_rx) = mpsc::channel(4);
        let home = std::env::var("HOME").ok();
        let mut chat = Chat::replay(session.cwd.clone(), &session.messages);
        chat.warn(&session.context().warnings);
        Self {
            chat,
            prompt: Prompt::default(),
            mode: Mode::default(),
            history: History::default(),
            history_saving: Job::default(),
            content: Content::Chat,
            input: Input::Prompt,
            model: session.model.clone(),
            effort: session.effort,
            cwd: session.cwd.clone(),
            place: status::place(&session.cwd, home.as_deref()),
            busy_since: None,
            completion: None,
            files: Vec::new(),
            indexing: Job::default(),
            git: None,
            usage: None,
            git_loading: Job::default(),
            llms: None,
            llm_listing: Job::default(),
            store: None,
            paths: Paths::default(),
            context: session.context().clone(),
            max_steps: session.max_steps,
            session_listing: Job::default(),
            session_loading: Job::default(),
            session: Some(session),
            provider,
            tools,
            events_tx,
            events_rx,
            asks_tx,
            asks_rx,
            asks: VecDeque::new(),
            turn: Job::default(),
            quit: false,
        }
    }

    pub fn with_store(mut self, store: Store) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    pub fn with_paths(mut self, paths: Paths) -> Self {
        self.paths = paths;
        self
    }

    pub fn with_history(mut self, history: History) -> Self {
        self.history = history;
        self
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut input = EventStream::new();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.index_files();
        self.load_git();
        self.list_llms();

        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            let busy = self.is_busy();
            let step = tokio::select! {
                event = input.next() => Step::Terminal(event),
                Some(event) = self.events_rx.recv() => Step::Session(event),
                Some(ask) = self.asks_rx.recv() => Step::Asked(ask),
                ended = self.turn.join() => Step::TurnEnded(ended),
                files = self.indexing.join() => Step::Indexed(files),
                status = self.git_loading.join() => Step::GitLoaded(status),
                llms = self.llm_listing.join() => Step::LlmsListed(llms),
                sessions = self.session_listing.join() => Step::SessionsListed(sessions),
                session = self.session_loading.join() => Step::SessionLoaded(session),
                saved = self.history_saving.join() => Step::HistorySaved(saved),
                _ = tick.tick(), if busy => Step::Tick,
            };
            match step {
                Step::Terminal(None) => break,
                Step::Terminal(Some(event)) => {
                    self.on_terminal(event.context("reading terminal input")?)
                }
                Step::Session(event) => self.on_session(event),
                Step::Asked(ask) => self.on_ask(ask),
                Step::TurnEnded(ended) => self.end_turn(ended.context("turn task failed")?),
                Step::Indexed(files) => self.indexed(files.context("listing files failed")?),
                Step::GitLoaded(status) => {
                    self.git_loaded(status.context("reading git status failed")?)
                }
                Step::LlmsListed(llms) => self.llms_listed(llms.context("listing LLMs failed")?),
                Step::SessionsListed(sessions) => {
                    self.sessions_listed(sessions.context("listing sessions failed")?)
                }
                Step::SessionLoaded(session) => {
                    self.session_loaded(session.context("loading the session failed")?)
                }
                Step::HistorySaved(saved) => {
                    self.history_saved(saved.context("saving prompt history failed")?)
                }
                // Nothing changed but time: the redraw advances the reasoning timer.
                Step::Tick => {}
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(1, 0));
        let [header, content, input, status] = Layout::vertical([
            Constraint::Length(header::ROWS),
            Constraint::Min(0),
            Constraint::Length(self.input.rows(frame.area())),
            Constraint::Length(status::ROWS),
        ])
        .spacing(1)
        .areas(area);

        header::draw(frame, header);
        match self.content {
            Content::Chat => {
                let banner = format!("nth · {} · {}", self.model, self.place);
                self.chat.draw(frame, content, &banner);
                // In the right margin, so the chat keeps its width and
                // doesn't rewrap when the bar comes and goes.
                if let Some(mut state) = self.chat.scrollbar() {
                    let column = Rect {
                        x: content.right(),
                        width: 1,
                        ..content
                    };
                    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(None)
                        .end_symbol(None)
                        .track_symbol(None)
                        .thumb_symbol("┃")
                        .thumb_style(Style::new().fg(Color::Gray));
                    frame.render_stateful_widget(bar, column, &mut state);
                }
            }
        }
        status::draw(frame, status, self);
        match &self.input {
            Input::Prompt => {
                let spinner = self.busy_since.map(|since| spinner::frame(since.elapsed()));
                prompt::draw(frame, input, &self.prompt, self.mode, spinner);
                // Last, so it pops over the content panel; it sits right
                // above the row being typed, lined up with the token it
                // completes.
                if let Some(completion) = &self.completion {
                    let at = prompt::position(input, &self.prompt, completion.start());
                    completion.draw(frame, area, at);
                }
            }
            Input::LlmPicker(picker) => llm_picker::draw(frame, input, picker),
            Input::SessionPicker(picker) => session_picker::draw(frame, input, picker),
            Input::Question(panel) => question::draw(frame, input, panel),
        }
    }

    fn on_terminal(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            TermEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.chat.scroll_up(WHEEL_LINES),
                MouseEventKind::ScrollDown => self.chat.scroll_down(WHEEL_LINES),
                _ => {}
            },
            TermEvent::Paste(text) if matches!(self.input, Input::Prompt) => {
                self.prompt
                    .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_completion();
            }
            _ => {}
        }
    }

    fn on_session(&mut self, event: Event) {
        // A write or a command may have changed the tree.
        match &event {
            Event::ToolFinished { call, .. } if matches!(call.name.as_str(), "write" | "bash") => {
                self.load_git();
            }
            Event::Usage(usage) => self.usage = Some(*usage),
            _ => {}
        }
        self.chat.apply(&event);
    }

    /// Reads git status in the background; git is slow on a big tree.
    pub(super) fn load_git(&mut self) {
        let cwd = self.cwd.clone();
        self.git_loading
            .start_or_queue(|_| tokio::spawn(async move { git::load(&cwd).await }));
    }

    /// A failed load (no git installed, say) shows no git state rather
    /// than stopping the app.
    fn git_loaded(&mut self, status: Result<Option<GitStatus>, String>) {
        self.git = status.ok().flatten();
        if self.git_loading.take_again() {
            self.load_git();
        }
    }

    /// Lists the files off the runtime; a big tree takes a while to walk.
    /// The walk checks its token between entries, since a blocking task
    /// can't be aborted and quitting shouldn't wait for it.
    pub(super) fn index_files(&mut self) {
        let root = self.cwd.clone();
        self.indexing.start_or_queue(|cancel| {
            tokio::task::spawn_blocking(move || mention::walk(&root, &cancel))
        });
    }

    fn indexed(&mut self, files: Vec<String>) {
        self.files = files;
        if self.indexing.take_again() {
            self.index_files();
        }
        if matches!(self.completion, Some(Completion::File { .. })) {
            self.refresh_completion();
        }
    }

    /// Writes the whole history in the background; recalling never waits
    /// on it.
    fn save_history(&mut self) {
        let Some(path) = self.history.saved_at().map(Path::to_path_buf) else {
            return;
        };
        let jsonl = self.history.to_jsonl();
        self.history_saving.start_or_queue(|_| {
            tokio::spawn(async move {
                if let Some(dir) = path.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::write(&path, jsonl).await
            })
        });
    }

    /// A failed save is told once; the history stays in memory for this run.
    pub(super) fn history_saved(&mut self, saved: std::io::Result<()>) {
        if self.history_saving.take_again() {
            self.save_history();
        }
        if let Err(e) = saved {
            let path = self.history.saved_at().map(|p| p.display().to_string());
            self.chat.transcript.push_error(format!(
                "prompt history not saved to {}: {e}",
                path.unwrap_or_default()
            ));
            self.history.forget_path();
        }
    }

    fn run_command(&mut self, command: Command) {
        match command {
            // Dropping the app aborts a running turn.
            Command::Exit => self.quit = true,
            // Mid-turn the session is in the turn task, so there is nothing
            // to replace yet.
            Command::Clear if self.is_busy() => {}
            Command::Clear => {
                // Same directory, so the same instruction files and skills.
                let mut session = Session::new(self.model.clone(), self.cwd.clone())
                    .with_context(self.context.clone());
                session.effort = self.effort;
                session.max_steps = self.max_steps;
                self.session = Some(session);
                self.chat = Chat::new(self.cwd.clone());
                self.usage = None;
            }
            Command::Models => self.open_llm_picker(),
            Command::Resume => self.open_session_picker(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, ModelInfo, Request, StreamEvent};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    struct Idle;

    impl Provider for Idle {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    /// An idle app whose provider must never be asked for anything.
    pub(crate) fn app() -> App {
        let session = Session::new("glm", "/repo".into());
        App::new(session, Arc::new(Idle), Arc::new(Vec::new()))
    }

    /// The context of a project in `dir` with one skill, `fix`, whose body
    /// is `Fix $ARGUMENTS.`.
    pub(crate) fn with_fix_skill(dir: &std::path::Path) -> Arc<ProjectContext> {
        let skill = dir.join(".agents/skills/fix");
        std::fs::create_dir_all(&skill).expect("dirs");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: fix\ndescription: fix what is broken\n---\nFix $ARGUMENTS.\n",
        )
        .expect("writes");
        Arc::new(ProjectContext::discover(dir, &Paths::default()))
    }

    fn rows(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).expect("test backend");
        terminal.draw(|frame| app.draw(frame)).expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn prompt_and_status_rows_never_move() {
        let mut app = app();
        let idle = rows(&mut app);
        assert_eq!(
            idle[0].trim_start(),
            format!("nth {} ", env!("CARGO_PKG_VERSION")),
            "right-aligned inside the margin"
        );
        assert!(
            [1, 8, 13].iter().all(|&i| idle[i].trim().is_empty()),
            "an empty line between bands"
        );
        assert_eq!(idle[9].trim_end(), " ▎ build");
        assert_eq!(idle[10].trim_end(), " ▎ Ask anything.");
        assert_eq!(idle[11].trim_end(), " ▎");
        assert_eq!(idle[12].trim_end(), " ▎");
        assert_eq!(idle[14].trim_end(), " glm · /repo");
        assert!(idle[15].trim().is_empty(), "no hint when idle");

        for i in 0..20 {
            app.chat.transcript.push_user(format!("message {i}"));
        }
        let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        app.prompt.insert_str(&lines.join("\n"));
        app.busy_since = Some(Instant::now());
        let busy = rows(&mut app);

        assert_eq!(busy[0], idle[0], "the header stays on top");
        assert_eq!(busy[7].trim_end(), " ▎ message 19");
        let spinner = busy[9].chars().skip(3).take(4).collect::<String>();
        assert!(
            spinner.chars().all(|c| ('⠀'..='⣿').contains(&c)),
            "spinner in place of the mode: {:?}",
            busy[9]
        );
        assert!(busy[9].trim_end().ends_with("esc to cancel"));
        let text: Vec<&str> = busy[10..=12].iter().map(|r| r.trim_end()).collect();
        assert_eq!(
            text,
            [" ▎ line 8", " ▎ line 9", " ▎ line 10"],
            "scrolled to the cursor"
        );
        assert_eq!(busy[14].trim_end(), " glm · /repo", "no git outside a repo");
        assert!(busy[15].trim().is_empty(), "nothing below while busy");
    }

    /// The scrollbar column (the right margin) of the content panel's rows.
    fn scrollbar(rows: &[String]) -> String {
        rows[2..=7]
            .iter()
            .map(|row| row.chars().nth(39).expect("40 wide"))
            .collect()
    }

    #[test]
    fn a_scrollbar_shows_only_while_scrolled_up() {
        let mut app = app();
        for i in 0..20 {
            app.chat.transcript.push_user(format!("message {i}"));
        }
        assert_eq!(scrollbar(&rows(&mut app)), " ".repeat(6), "following");

        app.chat.scroll_up(10);
        let up = rows(&mut app);
        let bar = scrollbar(&up);
        assert!(bar.contains('┃'), "{bar:?}");
        assert!(
            !bar.starts_with('┃') && !bar.ends_with('┃'),
            "partway: {bar:?}"
        );
        assert!(up[15].trim().is_empty(), "no hint in the status bar");

        app.chat.jump_top();
        assert!(scrollbar(&rows(&mut app)).starts_with('┃'));

        app.chat.jump_bottom();
        assert_eq!(scrollbar(&rows(&mut app)), " ".repeat(6), "following again");
    }

    #[test]
    fn status_shows_the_branch_in_the_git_summary() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("main".into()),
            ahead: 2,
            modified: 1,
            ..GitStatus::default()
        });
        app.busy_since = Some(Instant::now());
        let rows = rows(&mut app);

        assert!(rows[14].starts_with(" glm · /repo "));
        assert!(rows[14].trim_end().ends_with("git · main +2 *1"));
    }

    #[test]
    fn a_narrow_status_line_cuts_the_right_first() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("a-very-long-branch-name".into()),
            modified: 1,
            ..GitStatus::default()
        });
        app.busy_since = Some(Instant::now());
        let rows = rows(&mut app);

        let row = rows[14].trim();
        assert!(row.starts_with("glm · /repo git · a-very"), "{row:?}");
        assert!(!row.ends_with("*1"), "the counts are cut: {row:?}");
    }

    #[tokio::test]
    async fn a_second_git_load_waits_for_the_first() {
        let mut app = app();
        app.load_git();
        app.load_git();

        let status = app.git_loading.join().await.expect("loads");
        app.git_loaded(status);
        assert!(app.git_loading.is_running(), "the queued load starts");
        assert!(!app.git_loading.take_again(), "only once");
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_git_load() {
        let mut app = app();
        app.load_git();
        let loading = app.git_loading.abort_handle().expect("loading");

        drop(app);
        tokio::task::yield_now().await;
        assert!(loading.is_finished());
    }

    #[test]
    fn the_context_bar_fills_with_usage() {
        let mut app = app();
        let place = rows(&mut app)[14].trim_end().to_string();
        assert!(
            place.ends_with("/repo"),
            "no bar while the window is unknown"
        );

        let mut glm = crate::llm_picker::tests::model("glm", true);
        glm.context = Some(1000);
        app.llms = Some(vec![glm]);
        let empty = rows(&mut app);
        assert!(
            empty[14].starts_with(&format!("{place} {} ", " ".repeat(13))),
            "an empty bar before the first reply: {:?}",
            empty[14]
        );

        app.on_session(Event::Usage(Usage {
            input: 1000,
            output: 0,
        }));
        let full = rows(&mut app);
        assert!(full[14].starts_with(&format!("{place} {}", "⣿".repeat(13))));
    }

    #[tokio::test]
    async fn tool_output_stays_in_the_transcript() {
        let mut app = app();
        app.chat.transcript.push_user("test it".into());
        let bash = nth_protocol::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        };
        app.on_session(Event::ToolStarted(bash.clone()));
        app.on_session(Event::ToolOutput {
            call_id: "1".into(),
            text: "running 3 tests\n".into(),
        });
        let running = rows(&mut app);

        assert_eq!(running[2].trim_end(), " ▎ test it");
        assert_eq!(running[4].trim_end(), " ▎ $ bash   cargo test");
        assert_eq!(running[5].trim_end(), " ▎   running 3 tests");

        app.on_session(Event::ToolFinished {
            call: bash,
            result: Ok(String::new()),
        });
        let finished = rows(&mut app);
        assert_eq!(finished[4].trim_end(), " ▎ $ bash   cargo test");
        assert_eq!(finished[5].trim_end(), " ▎   running 3 tests", "kept");
        assert!(
            app.git_loading.is_running(),
            "bash may have changed the tree"
        );
    }

    #[test]
    fn exit_command_quits() {
        let mut app = app();
        app.prompt.insert_str("/exit");
        app.submit();
        assert!(app.quit);
    }

    #[test]
    fn clear_command_starts_a_fresh_session() {
        let mut app = app();
        let old = app.session.as_ref().expect("idle").id;
        app.session
            .as_mut()
            .expect("idle")
            .messages
            .push(nth_protocol::Message::User("hi".into()));
        app.chat.transcript.push_user("hi".into());

        app.prompt.insert_str("/clear");
        app.submit();

        let session = app.session.as_ref().expect("idle");
        assert_ne!(session.id, old);
        assert_eq!(session.messages.len(), 1, "only the system prompt");
        assert!(app.chat.transcript.is_empty());
        assert!(app.prompt.is_empty());
        assert!(!app.is_busy());
    }

    #[test]
    fn completion_pops_over_the_content_panel() {
        let mut app = app();
        app.apply(keys::Action::Insert('/'));
        let rows = rows(&mut app);

        assert!(rows[6].starts_with("    /clear "));
        assert!(rows[7].starts_with("    /exit "));
        assert!(rows[8].starts_with("    /models "));
        assert!(
            rows[9].starts_with(" ▎  /resume "),
            "right above the cursor"
        );
        assert!(rows[10].starts_with(" ▎ /"));
    }

    #[tokio::test]
    async fn a_second_walk_waits_for_the_first() {
        let mut app = app();
        app.index_files();
        app.index_files();

        let files = app.indexing.join().await.expect("walks");
        app.indexed(files);
        assert!(app.indexing.is_running(), "the queued walk starts");
        assert!(!app.indexing.take_again(), "only once");
    }

    #[tokio::test]
    async fn dropping_the_app_cancels_the_walk() {
        let mut app = app();
        app.index_files();
        let cancel = app.indexing.token().expect("walking");

        drop(app);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn file_popup_opens_mid_prompt() {
        let mut app = app();
        app.files = vec!["src/app/keys.rs".into(), "src/lib.rs".into()];
        for c in "see @ke".chars() {
            app.apply(keys::Action::Insert(c));
        }
        let rows = rows(&mut app);

        assert!(rows[8].trim().is_empty());
        assert!(
            rows[9].starts_with(" ▎ buil src/app/keys.rs "),
            "lined up with the @"
        );
        assert!(rows[10].starts_with(" ▎ see @ke"));
    }

    #[test]
    fn popup_follows_the_cursor_down_and_shifts_left_at_the_edge() {
        let mut app = app();
        app.files = vec!["src/app/keys.rs".into()];
        app.apply(keys::Action::Newline);
        app.apply(keys::Action::Newline);
        for c in format!("{} @ke", "x".repeat(30)).chars() {
            app.apply(keys::Action::Insert(c));
        }
        let rows = rows(&mut app);

        let popup = format!(" ▎{}src/app/keys.rs  ", " ".repeat(21));
        assert_eq!(rows[11], popup, "above the third row, against the margin");
        assert!(rows[12].starts_with(" ▎ xxx"));
    }

    /// An idle app whose model list is already in, so opening the picker
    /// asks the provider for nothing.
    pub(crate) fn llm_listed_app() -> App {
        let mut app = app();
        app.llms = Some(vec![
            crate::llm_picker::tests::model("glm", true),
            crate::llm_picker::tests::model("plain", false),
        ]);
        app
    }

    fn picker(app: &App) -> &crate::llm_picker::LlmPicker {
        match &app.input {
            Input::LlmPicker(picker) => picker,
            _ => panic!("picker not open"),
        }
    }

    #[test]
    fn the_picker_draws_in_place_of_the_prompt() {
        let mut app = llm_listed_app();
        app.prompt.insert_str("/models");
        app.submit();
        let rows = rows(&mut app);

        assert!(rows[5].starts_with(" ▎ switch model"), "{:?}", rows[5]);
        assert!(rows[6].starts_with(" ▎ → glm   ✓"), "{:?}", rows[6]);
        assert!(rows[6].trim_end().ends_with("◂ default ▸"));
        assert!(rows[7].starts_with(" ▎   plain"));
        assert!(
            rows[8..13].iter().all(|r| r.trim().is_empty()),
            "seven model rows"
        );
        assert!(
            rows.iter().all(|r| !r.contains("Ask anything")),
            "no prompt"
        );
        assert_eq!(rows[14].trim_end(), " glm · /repo", "status stays");
    }

    #[test]
    fn the_picker_switches_model_and_effort() {
        let mut app = llm_listed_app();
        app.apply(keys::Action::LlmPicker);
        app.apply(keys::Action::Right);
        app.apply(keys::Action::Right);
        app.apply(keys::Action::Submit);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!((app.model.as_str(), app.effort), ("glm", Effort::Medium));
        assert_eq!(rows(&mut app)[14].trim_end(), " glm · medium · /repo");

        app.apply(keys::Action::LlmPicker);
        app.apply(keys::Action::SelectNext);
        app.apply(keys::Action::Submit);
        assert_eq!((app.model.as_str(), app.effort), ("plain", Effort::Default));
    }

    #[test]
    fn esc_leaves_the_picker_unchanged() {
        let mut app = llm_listed_app();
        app.apply(keys::Action::LlmPicker);
        app.apply(keys::Action::SelectNext);
        app.apply(keys::Action::Interrupt);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.model, "glm");
        assert!(!app.quit);
    }

    #[tokio::test]
    async fn the_next_turn_runs_the_chosen_model() {
        let mut app = llm_listed_app();
        app.apply(keys::Action::LlmPicker);
        app.apply(keys::Action::Right);
        app.apply(keys::Action::Submit);

        app.prompt.insert_str("go");
        app.submit();
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);

        let session = app.session.as_ref().expect("idle");
        assert_eq!(
            (session.model.as_str(), session.effort),
            ("glm", Effort::Low)
        );
    }

    #[tokio::test]
    async fn the_list_loads_into_the_open_picker() {
        let mut app = app();
        app.apply(keys::Action::LlmPicker);
        assert!(picker(&app).chosen().is_none(), "still loading");

        let listing = app.llm_listing.take().expect("listing");
        app.llms_listed(Ok(vec![crate::llm_picker::tests::model("glm", true)]));
        listing.abort();

        assert_eq!(picker(&app).chosen(), Some(("glm".into(), Effort::Default)));
        assert!(app.llms.is_some());
    }

    #[tokio::test]
    async fn a_failed_list_is_asked_for_again() {
        let mut app = app();
        app.apply(keys::Action::LlmPicker);
        let listing = app.llm_listing.take().expect("listing");
        listing.abort();
        app.llms_listed(Err("offline".into()));
        assert!(rows(&mut app)[6].contains("✗ offline"));

        app.apply(keys::Action::Interrupt);
        app.apply(keys::Action::LlmPicker);
        assert!(app.llm_listing.is_running(), "asked again");
    }

    /// A provider whose model list never arrives, and records when the
    /// request for it is dropped.
    struct SlowList(Arc<std::sync::atomic::AtomicBool>);

    struct SetOnDrop(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Provider for SlowList {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            let guard = SetOnDrop(self.0.clone());
            async move {
                let _guard = guard;
                std::future::pending().await
            }
            .boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_listing() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(
            session,
            Arc::new(SlowList(dropped.clone())),
            Arc::new(Vec::new()),
        );
        app.apply(keys::Action::LlmPicker);
        tokio::task::yield_now().await;

        drop(app);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// An ask for `questions`, and where its reply lands.
    fn ask(
        questions: Vec<nth_protocol::Question>,
    ) -> (Ask, tokio::sync::oneshot::Receiver<nth_protocol::Reply>) {
        let (reply, replied) = tokio::sync::oneshot::channel();
        let ask = Ask {
            call_id: "1".into(),
            questions,
            reply,
        };
        (ask, replied)
    }

    #[test]
    fn a_question_takes_the_prompts_place_and_answers_the_tool() {
        use crate::question::tests::question;

        let mut app = app();
        app.prompt.insert_str("half typed");
        let (ask, mut replied) = ask(vec![question("auth", false, &["oauth", "key"])]);
        app.on_ask(ask);
        let asking = rows(&mut app);

        assert_eq!(
            asking[14].trim_end(),
            " glm · /repo",
            "the status bar stays"
        );
        let panel: Vec<&str> = asking[8..13].iter().map(|r| r.trim_end()).collect();
        assert_eq!(
            panel,
            [
                " ▎ question      ↑↓ · 1-2 · enter · esc",
                " ▎ Which auth?",
                " ▎ → 1. oauth",
                " ▎   2. key",
                " ▎   3. Type your own answer…",
            ]
        );

        app.apply(keys::Action::SelectNext);
        app.apply(keys::Action::Submit);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.prompt.text(), "half typed", "the prompt kept its text");
        assert_eq!(
            replied.try_recv(),
            Ok(nth_protocol::Reply::Answered(vec![nth_protocol::Answer {
                picked: vec!["key".into()],
                typed: None,
            }]))
        );
    }

    #[test]
    fn esc_declines_and_the_next_question_follows() {
        use crate::question::tests::question;

        let mut app = app();
        let (first, mut declined) = ask(vec![question("auth", false, &["a", "b"])]);
        let (second, _) = ask(vec![question("checks", true, &["a", "b"])]);
        app.on_ask(first);
        app.on_ask(second);

        app.apply(keys::Action::Interrupt);

        assert_eq!(declined.try_recv(), Ok(nth_protocol::Reply::Declined));
        match &app.input {
            Input::Question(panel) => assert_eq!(panel.questions()[0].header, "checks"),
            _ => panic!("the queued question shows"),
        }

        app.drop_asks();
        assert!(matches!(app.input, Input::Prompt), "gone with the turn");
    }
}
