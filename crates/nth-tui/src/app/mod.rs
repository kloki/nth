//! The chat app: its state, the loop that drives it, and the layout of
//! three bands: chat history, the input panel and the status bar. Row
//! heights never depend on content, so nothing shifts while a turn runs.

mod keys;
mod panel;
mod turn;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind, MouseEventKind};
use futures::StreamExt;
use nth_protocol::{BoxError, Effort, Event, ModelInfo, Provider, Tool};
use nth_session::{CancellationToken, Session};
use panel::Panel;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Rect},
};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
    time::MissedTickBehavior,
};
use turn::{Ended, Running};

use crate::{
    chat::Chat,
    command::Command,
    git::{self, GitStatus},
    llm_picker, mention,
    popup::{self, Popup},
    prompt::{self, Mode, Prompt},
    spinner, status,
};

const WHEEL_LINES: usize = 3;
/// How often a running turn redraws, so the spinner shows every frame and
/// the live reasoning timer advances.
const TICK: Duration = spinner::FRAME;

pub struct App {
    pub chat: Chat,
    pub prompt: Prompt,
    pub mode: Mode,
    /// What fills the rows under the chat.
    panel: Panel,
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
    indexing: Option<Indexing>,
    /// Set when a turn ends mid-walk, so its files get picked up by one
    /// more walk rather than a second one racing the first.
    reindex: bool,
    /// The working tree's git state; `None` outside a repository or until
    /// the first load lands.
    pub git: Option<GitStatus>,
    git_loading: Option<JoinHandle<Result<Option<GitStatus>, String>>>,
    /// Set when the tree may have changed mid-load, like `reindex`.
    reload_git: bool,
    /// The LLMs the endpoint serves, kept once listed; after a failure the
    /// next open asks again.
    llms: Option<Vec<ModelInfo>>,
    llm_listing: Option<JoinHandle<Result<Vec<ModelInfo>, BoxError>>>,
    /// Held here between turns; moved into the turn task while one runs.
    session: Option<Session>,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    turn: Option<Running>,
    quit: bool,
}

/// A file walk in flight. The walk checks `cancel` between entries, since a
/// blocking task can't be aborted and quitting shouldn't wait for it.
struct Indexing {
    handle: JoinHandle<Vec<String>>,
    cancel: CancellationToken,
}

/// The open completion popup.
enum Completion {
    Command(Popup<Command>),
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

    fn draw(&self, frame: &mut Frame, area: Rect, anchor: Rect) {
        let (rows, selected): (Vec<(String, &str)>, _) = match self {
            Completion::Command(popup) => (
                popup.items().iter().map(|c| c.row()).collect(),
                popup.selected_index(),
            ),
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
    TurnEnded(Result<Ended, JoinError>),
    Indexed(Result<Vec<String>, JoinError>),
    GitLoaded(Result<Result<Option<GitStatus>, String>, JoinError>),
    LlmsListed(Result<Result<Vec<ModelInfo>, BoxError>, JoinError>),
    Tick,
}

impl App {
    pub fn new(
        session: Session,
        provider: Arc<dyn Provider>,
        tools: Arc<Vec<Box<dyn Tool>>>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(256);
        let home = std::env::var("HOME").ok();
        Self {
            chat: Chat::new(session.cwd.clone()),
            prompt: Prompt::default(),
            mode: Mode::default(),
            panel: Panel::Prompt,
            model: session.model.clone(),
            effort: session.effort,
            cwd: session.cwd.clone(),
            place: status::place(&session.cwd, home.as_deref()),
            busy_since: None,
            completion: None,
            files: Vec::new(),
            indexing: None,
            reindex: false,
            git: None,
            git_loading: None,
            reload_git: false,
            llms: None,
            llm_listing: None,
            session: Some(session),
            provider,
            tools,
            events_tx,
            events_rx,
            turn: None,
            quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut input = EventStream::new();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.index_files();
        self.load_git();

        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            let busy = self.is_busy();
            let turn = self.turn.as_mut().map(|running| &mut running.handle);
            let indexing = self.indexing.is_some();
            let index = self.indexing.as_mut().map(|indexing| &mut indexing.handle);
            let git_loading = self.git_loading.is_some();
            let git = self.git_loading.as_mut();
            let llm_listing = self.llm_listing.is_some();
            let llms = self.llm_listing.as_mut();
            let step = tokio::select! {
                event = input.next() => Step::Terminal(event),
                Some(event) = self.events_rx.recv() => Step::Session(event),
                ended = async {
                    match turn {
                        Some(turn) => turn.await,
                        None => std::future::pending().await,
                    }
                }, if busy => Step::TurnEnded(ended),
                files = async {
                    match index {
                        Some(index) => index.await,
                        None => std::future::pending().await,
                    }
                }, if indexing => Step::Indexed(files),
                status = async {
                    match git {
                        Some(git) => git.await,
                        None => std::future::pending().await,
                    }
                }, if git_loading => Step::GitLoaded(status),
                llms = async {
                    match llms {
                        Some(llms) => llms.await,
                        None => std::future::pending().await,
                    }
                }, if llm_listing => Step::LlmsListed(llms),
                _ = tick.tick(), if busy => Step::Tick,
            };
            match step {
                Step::Terminal(None) => break,
                Step::Terminal(Some(event)) => {
                    self.on_terminal(event.context("reading terminal input")?)
                }
                Step::Session(event) => self.on_session(event),
                Step::TurnEnded(ended) => self.end_turn(ended.context("turn task failed")?),
                Step::Indexed(files) => self.indexed(files.context("listing files failed")?),
                Step::GitLoaded(status) => {
                    self.git_loaded(status.context("reading git status failed")?)
                }
                Step::LlmsListed(llms) => self.llms_listed(llms.context("listing LLMs failed")?),
                // Nothing changed but time: the redraw advances the reasoning timer.
                Step::Tick => {}
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(1, 0));
        let [chat, panel, status] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(self.panel.rows()),
            Constraint::Length(status::ROWS),
        ])
        .areas(area);

        let banner = format!("nth · {} · {}", self.model, self.place);
        self.chat.draw(frame, chat, &banner);
        status::draw(frame, status, self);
        match &self.panel {
            Panel::Prompt => {
                let spinner = self.busy_since.map(|since| spinner::frame(since.elapsed()));
                prompt::draw(frame, panel, &self.prompt, self.mode, spinner);
                // Last, so it pops over the chat.
                if let Some(completion) = &self.completion {
                    completion.draw(frame, area, panel);
                }
            }
            Panel::LlmPicker(picker) => llm_picker::draw(frame, panel, picker),
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
            TermEvent::Paste(text) if matches!(self.panel, Panel::Prompt) => {
                self.prompt
                    .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_completion();
            }
            _ => {}
        }
    }

    fn on_session(&mut self, event: Event) {
        // A write or a command may have changed the tree.
        if let Event::ToolFinished { call, .. } = &event
            && matches!(call.name.as_str(), "write" | "bash")
        {
            self.load_git();
        }
        self.chat.transcript.apply(&event);
    }

    /// Reads git status in the background; git is slow on a big tree.
    pub(super) fn load_git(&mut self) {
        if self.git_loading.is_some() {
            self.reload_git = true;
            return;
        }
        let cwd = self.cwd.clone();
        self.git_loading = Some(tokio::spawn(async move { git::load(&cwd).await }));
    }

    /// A failed load (no git installed, say) shows no git state rather
    /// than stopping the app.
    fn git_loaded(&mut self, status: Result<Option<GitStatus>, String>) {
        self.git_loading = None;
        self.git = status.ok().flatten();
        if std::mem::take(&mut self.reload_git) {
            self.load_git();
        }
    }

    /// Lists the files off the runtime; a big tree takes a while to walk.
    pub(super) fn index_files(&mut self) {
        if self.indexing.is_some() {
            self.reindex = true;
            return;
        }
        let root = self.cwd.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::task::spawn_blocking(move || mention::walk(&root, &token));
        self.indexing = Some(Indexing { handle, cancel });
    }

    fn indexed(&mut self, files: Vec<String>) {
        self.files = files;
        self.indexing = None;
        if std::mem::take(&mut self.reindex) {
            self.index_files();
        }
        if matches!(self.completion, Some(Completion::File { .. })) {
            self.refresh_completion();
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
                let mut session = Session::new(self.model.clone(), self.cwd.clone());
                session.effort = self.effort;
                self.session = Some(session);
                self.chat = Chat::new(self.cwd.clone());
            }
            Command::Models => self.open_llm_picker(),
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
        assert_eq!(idle[11].trim_end(), " ▎");
        assert_eq!(idle[12].trim_end(), " ▎ BUILD  > Ask anything.");
        assert_eq!(idle[13].trim_end(), " ▎");
        assert!(idle[14].trim_end().ends_with("glm /repo"));
        assert!(idle[15].trim().is_empty(), "no hint or git outside a repo");

        for i in 0..20 {
            app.chat.transcript.push_user(format!("message {i}"));
        }
        let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        app.prompt.insert_str(&lines.join("\n"));
        app.busy_since = Some(Instant::now());
        let busy = rows(&mut app);

        assert_eq!(busy[10].trim_end(), " ▎ message 19");
        let spinner = busy[12].chars().skip(3).take(4).collect::<String>();
        assert!(
            spinner.chars().all(|c| ('⠀'..='⣿').contains(&c)),
            "spinner in place of the mode: {:?}",
            busy[12]
        );
        assert_eq!(
            busy[12].chars().skip(7).collect::<String>().trim_end(),
            "   > line 10",
            "> stays put; scrolled to the cursor"
        );
        assert!(busy[14].starts_with(" thinking"));
        assert!(busy[14].trim_end().ends_with("glm /repo"));
        assert!(busy[15].trim_end().ends_with("esc to interrupt"));
    }

    #[test]
    fn status_shows_branch_and_git_summary_on_the_right() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("main".into()),
            ahead: 2,
            modified: 1,
            ..GitStatus::default()
        });
        app.busy_since = Some(Instant::now());
        let rows = rows(&mut app);

        assert!(rows[14].starts_with(" thinking"));
        assert!(rows[14].trim_end().ends_with("glm /repo main"));
        assert!(rows[15].starts_with(" esc to interrupt"));
        assert!(rows[15].trim_end().ends_with("+2 *1"));
    }

    #[test]
    fn a_narrow_status_line_cuts_the_left_first() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("a-very-long-branch-name".into()),
            ..GitStatus::default()
        });
        app.busy_since = Some(Instant::now());
        let rows = rows(&mut app);

        assert_eq!(rows[14].trim(), "thin glm /repo a-very-long-branch-name");
    }

    #[tokio::test]
    async fn a_second_git_load_waits_for_the_first() {
        let mut app = app();
        app.load_git();
        app.load_git();
        assert!(app.reload_git);

        app.git_loaded(Ok(None));
        assert!(!app.reload_git);
        assert!(app.git_loading.is_some(), "the queued load starts");
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_git_load() {
        let mut app = app();
        app.load_git();
        let loading = app.git_loading.as_ref().expect("loading").abort_handle();

        drop(app);
        tokio::task::yield_now().await;
        assert!(loading.is_finished());
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
    fn completion_pops_over_the_chat() {
        let mut app = app();
        app.apply(keys::Action::Insert('/'));
        let rows = rows(&mut app);

        assert!(rows[8].starts_with("  /clear"));
        assert!(rows[9].starts_with("  /exit"));
        assert!(rows[10].starts_with("  /models"));
        assert!(rows[12].starts_with(" ▎ BUILD  > /"));
    }

    #[tokio::test]
    async fn a_second_walk_waits_for_the_first() {
        let mut app = app();
        app.index_files();
        app.index_files();
        assert!(app.reindex);

        app.indexed(Vec::new());
        assert!(!app.reindex);
        assert!(app.indexing.is_some(), "the queued walk starts");
    }

    #[tokio::test]
    async fn dropping_the_app_cancels_the_walk() {
        let mut app = app();
        app.index_files();
        let cancel = app.indexing.as_ref().expect("walking").cancel.clone();

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

        assert!(rows[10].starts_with("  src/app/keys.rs"));
        assert!(rows[12].starts_with(" ▎ BUILD  > see @ke"));
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
        match &app.panel {
            Panel::LlmPicker(picker) => picker,
            Panel::Prompt => panic!("picker not open"),
        }
    }

    #[test]
    fn the_picker_draws_in_place_of_the_prompt() {
        let mut app = llm_listed_app();
        app.prompt.insert_str("/models");
        app.submit();
        let rows = rows(&mut app);

        assert!(rows[6].starts_with(" ▎ switch model"), "{:?}", rows[6]);
        assert!(rows[7].starts_with(" ▎ → glm   ✓"), "{:?}", rows[7]);
        assert!(rows[7].trim_end().ends_with("◂ default ▸"));
        assert!(rows[8].starts_with(" ▎   plain"));
        assert!(
            rows[9..14].iter().all(|r| r.trim().is_empty()),
            "seven model rows"
        );
        assert!(
            rows.iter().all(|r| !r.contains("Ask anything")),
            "no prompt"
        );
        assert!(rows[14].trim_end().ends_with("glm /repo"), "status stays");
    }

    #[test]
    fn the_picker_switches_model_and_effort() {
        let mut app = llm_listed_app();
        app.apply(keys::Action::LlmPicker);
        app.apply(keys::Action::Right);
        app.apply(keys::Action::Right);
        app.apply(keys::Action::Submit);

        assert!(matches!(app.panel, Panel::Prompt));
        assert_eq!((app.model.as_str(), app.effort), ("glm", Effort::Medium));
        assert!(
            rows(&mut app)[14]
                .trim_end()
                .ends_with("glm · medium /repo")
        );

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

        assert!(matches!(app.panel, Panel::Prompt));
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
        let running = app.turn.take().expect("turn started");
        app.end_turn(running.handle.await.expect("turn task finished"));

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
        assert!(rows(&mut app)[7].contains("✗ offline"));

        app.apply(keys::Action::Interrupt);
        app.apply(keys::Action::LlmPicker);
        assert!(app.llm_listing.is_some(), "asked again");
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
}
