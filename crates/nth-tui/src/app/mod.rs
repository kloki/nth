//! The app: its state, the loop that drives it, and the layout of the
//! three bands: the content panel under its tab header, the input panel
//! and the status bar. Row heights never depend on content, so nothing
//! shifts while a turn runs.

mod checks;
mod completion;
mod content;
mod editor;
mod files;
mod git;
mod history;
mod input;
mod job;
mod keys;
mod llms;
mod mode;
mod monitor;
mod mouse;
mod notify;
mod plan;
mod resume;
mod subagent;
#[cfg(test)]
pub(crate) mod tests;
mod turn;

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use checks::lsp_changed;
use completion::Completion;
pub(crate) use content::{Content, Tab, TabState};
use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind};
use futures::StreamExt;
use input::Input;
use job::Job;
pub use mode::{Llm, ModeLlms};
use monitor::due;
use nth_context::{Context as ProjectContext, Paths};
use nth_format::FormatterStatus;
use nth_lsp::{ServerInfo, ServerStatus};
use nth_notify::Notifier;
use nth_protocol::{
    Ask, BoxError, Effort, Event, Inbox, Listing, Mode, MonitorEvent, MonitorId, Monitors, Panel,
    Provider, Tool, Usage, monitor_log_dir,
};
use nth_session::{
    Session, Store, Subagents, Summary, store,
    subagent::{SubagentEvent, SubagentId},
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Style},
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinError,
    time::MissedTickBehavior,
};
use turn::{Ended, Queued};

use crate::{
    Checkers,
    chat::Chat,
    command::Command,
    diagnostics::{self, Diagnostics},
    git::GitStatus,
    header,
    history::History,
    llm_picker,
    monitor::MonitorView,
    plan::PlanView,
    prompt::{self, Prompt},
    question, session_picker, spinner, status,
    subagent::SubagentView,
};

/// How often the app redraws with nothing else happening, while a turn
/// runs or a monitor does: the spinner shows every frame, and the
/// reasoning timer and the monitors' running time advance.
const TICK: Duration = spinner::FRAME;
/// How long notices wait for more lines before an idle app sends them, so
/// one burst of output reaches the model as one message.
const NOTICE_DELAY: Duration = Duration::from_millis(200);

pub struct App {
    pub chat: Chat,
    pub prompt: Prompt,
    /// The mode the next turn runs in.
    pub mode: Mode,
    /// The model and effort each mode runs with; the current mode's are
    /// `model` and `effort`, which it is synced with on a switch.
    mode_llms: ModeLlms,
    /// Prompts sent before, recalled with Up and Down.
    history: History,
    /// Each save writes the whole history; one asked for mid-save runs
    /// after it, since an aborted task can't stop a write already on the
    /// blocking pool.
    history_saving: Job<std::io::Result<()>>,
    /// Prompts sent while a turn runs, oldest first; each runs as its own
    /// turn once the one before ends well. Always empty while idle.
    pub queue: VecDeque<Queued>,
    /// Esc was pressed during the running turn, which may have finished
    /// before it saw the cancel.
    interrupted: bool,
    /// How the chat's last turn ended, for its tab: done, failed, or idle
    /// when you stopped it or there was none.
    last_turn: TabState,
    /// The content panel's tabs, and which one fills it.
    content: Content,
    diagnostics: Diagnostics,
    /// What checks the tools' writes, for the diagnostics tab to look up;
    /// `None` when nothing does.
    checkers: Option<Checkers>,
    servers_lookup: Job<Vec<ServerInfo>>,
    formatters_lookup: Job<Vec<FormatterStatus>>,
    /// What fills the input panel.
    input: Input,
    /// The model and effort the next turn runs with.
    pub model: String,
    pub effort: Effort,
    pub cwd: PathBuf,
    /// The working directory as shown in the status bar, `~` for home.
    pub place: String,
    home: Option<String>,
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
    /// The language servers the tools started, as last reported.
    pub servers: Vec<ServerStatus>,
    /// Where `servers` comes from; `None` once its sender is gone.
    lsp: Option<watch::Receiver<Vec<ServerStatus>>>,
    /// What the last model reply used; `None` until the first one.
    pub usage: Option<Usage>,
    /// Queued again when the tree may have changed mid-load, like `indexing`.
    git_loading: Job<Result<Option<GitStatus>, String>>,
    /// The LLMs the endpoint serves, kept once listed; after a failure the
    /// next open asks again.
    llms: Option<Listing>,
    llm_listing: Job<Result<Listing, BoxError>>,
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
    /// Runs the commands typed after `!`; `None` leaves them unrun.
    shell: Option<Arc<dyn Tool>>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    /// Where the running turn's tools send their questions.
    asks_tx: mpsc::Sender<Ask>,
    asks_rx: mpsc::Receiver<Ask>,
    /// Questions waiting behind the ones in the input panel.
    asks: VecDeque<Ask>,
    /// Where the running turn's tools switch the content panel.
    screen_tx: mpsc::Sender<Panel>,
    screen_rx: mpsc::Receiver<Panel>,
    turn: Job<Ended>,
    /// The commands the model left running, shared with every turn's tools.
    monitors: Monitors,
    /// What the model has not heard yet from the monitors and subagents.
    inbox: Inbox,
    monitor_rx: mpsc::Receiver<MonitorEvent>,
    /// Each monitor's tab, open from its start until you close it.
    monitor_views: BTreeMap<MonitorId, MonitorView>,
    /// The subagents the task tool started, shared with it.
    subagents: Subagents,
    subagent_rx: mpsc::Receiver<SubagentEvent>,
    /// Each subagent's tab, open from its start until you close it.
    subagent_views: BTreeMap<SubagentId, SubagentView>,
    /// The session's plan file and what the plan tab shows of it.
    plan: PlanView,
    plan_path: PathBuf,
    plan_reading: Job<Option<String>>,
    /// The next read is of a session just opened, so it marks nothing as
    /// changed.
    plan_settle: bool,
    /// Where monitors log, one folder per session under it.
    monitor_root: PathBuf,
    /// When the notices waiting start a turn, if the app is idle by then.
    notices_due: Option<tokio::time::Instant>,
    /// Notices wait for your next prompt rather than start a turn, after
    /// you stopped one or it failed.
    hold_notices: bool,
    /// A word on what the last key did not do, on the status bar until the
    /// next key.
    pub hint: Option<String>,
    /// Where the last draw put the header, so a click knows it landed there.
    areas: mouse::Areas,
    /// How a right click's text reaches the clipboard; tests put it nowhere.
    clipboard: fn(&str) -> std::io::Result<()>,
    /// ctrl+g asked for the editor, on this target; the loop opens it
    /// after this step.
    pending_editor: Option<editor::Target>,
    /// The plan copy your editor has open; the app draws nothing while it
    /// runs.
    editing: Option<editor::Editing>,
    editor: Job<std::io::Result<std::process::ExitStatus>>,
    /// ctrl+c was pressed once with monitors running; again quits.
    notifier: Notifier,
    notify_errors: watch::Receiver<Option<String>>,
    /// A turn that ended in plan mode, waiting for the plan to be read to
    /// say whether it left a plan to approve.
    plan_notice: Option<nth_notify::Context>,
    quit_armed: bool,
    quit: bool,
}

/// A grey thumb in the right margin of `content`, so the view keeps its
/// width and doesn't rewrap when the bar comes and goes.
fn draw_scrollbar(frame: &mut Frame, content: Rect, mut state: ScrollbarState) {
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

/// The next terminal event, or never while there is no stream.
async fn next_input(input: &mut Option<EventStream>) -> Option<std::io::Result<TermEvent>> {
    match input {
        Some(input) => input.next().await,
        None => std::future::pending().await,
    }
}

enum Step {
    Terminal(Option<std::io::Result<TermEvent>>),
    Session(Event),
    Asked(Ask),
    Show(Panel),
    Monitor(MonitorEvent),
    Subagent(SubagentEvent),
    NoticesDue,
    TurnEnded(Result<Ended, JoinError>),
    Indexed(Result<Vec<String>, JoinError>),
    GitLoaded(Result<Result<Option<GitStatus>, String>, JoinError>),
    LlmsListed(Result<Result<Listing, BoxError>, JoinError>),
    SessionsListed(Result<Result<Vec<Summary>, store::Error>, JoinError>),
    SessionLoaded(Result<Result<Session, store::Error>, JoinError>),
    HistorySaved(Result<std::io::Result<()>, JoinError>),
    EditorClosed(Result<std::io::Result<std::process::ExitStatus>, JoinError>),
    PlanRead(Result<Option<String>, JoinError>),
    ServersFound(Result<Vec<ServerInfo>, JoinError>),
    FormattersFound(Result<Vec<FormatterStatus>, JoinError>),
    /// Whether the servers' states changed; `false` when the sender is gone.
    LspChanged(bool),
    /// The notification backend failed for the first time.
    NotifyFailed,
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
        let (screen_tx, screen_rx) = mpsc::channel(4);
        let (monitor_tx, monitor_rx) = mpsc::channel(256);
        // Replaced by `with_subagents`; until then no subagent reports here.
        let (_, subagent_rx) = mpsc::channel(1);
        let monitor_root = std::env::temp_dir().join("nth");
        let inbox = Inbox::new();
        let monitors = Monitors::new(
            monitor_tx,
            monitor_log_dir(&monitor_root, &session.id.to_string()),
            inbox.clone(),
        );
        let home = std::env::var("HOME").ok();
        // Replaced by `with_notifier`.
        let notifier = Notifier::off();
        let plan_path = session.plan_path();
        let mut chat = Chat::replay(session.cwd.clone(), &session.messages);
        chat.warn(&session.context().warnings);
        Self {
            chat,
            prompt: Prompt::default(),
            mode: session.mode,
            mode_llms: ModeLlms::same(Llm {
                model: session.model.clone(),
                effort: session.effort,
            }),
            history: History::default(),
            history_saving: Job::default(),
            queue: VecDeque::new(),
            interrupted: false,
            last_turn: TabState::Idle,
            content: Content::default(),
            diagnostics: Diagnostics::default(),
            checkers: None,
            servers_lookup: Job::default(),
            formatters_lookup: Job::default(),
            input: Input::Prompt,
            model: session.model.clone(),
            effort: session.effort,
            cwd: session.cwd.clone(),
            place: status::place(&session.cwd, home.as_deref()),
            home,
            busy_since: None,
            completion: None,
            files: Vec::new(),
            indexing: Job::default(),
            git: None,
            servers: Vec::new(),
            lsp: None,
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
            shell: None,
            events_tx,
            events_rx,
            asks_tx,
            asks_rx,
            asks: VecDeque::new(),
            screen_tx,
            screen_rx,
            turn: Job::default(),
            monitors,
            inbox,
            monitor_rx,
            monitor_views: BTreeMap::new(),
            subagents: Subagents::default(),
            subagent_rx,
            subagent_views: BTreeMap::new(),
            plan: PlanView::default(),
            plan_path,
            plan_reading: Job::default(),
            plan_settle: true,
            monitor_root,
            notices_due: None,
            hold_notices: false,
            hint: None,
            areas: mouse::Areas::default(),
            clipboard: crate::terminal::copy,
            pending_editor: None,
            editing: None,
            editor: Job::default(),
            notify_errors: notifier.errors(),
            notifier,
            plan_notice: None,
            quit_armed: false,
            quit: false,
        }
    }

    /// Runs the commands typed after `!` on `shell`.
    pub fn with_shell(mut self, shell: Arc<dyn Tool>) -> Self {
        self.shell = Some(shell);
        self
    }

    /// Hears from the subagents in `subagents`, which the task tool starts,
    /// over `rx`.
    pub fn with_subagents(
        mut self,
        subagents: Subagents,
        rx: mpsc::Receiver<SubagentEvent>,
    ) -> Self {
        self.subagents = subagents;
        self.subagent_rx = rx;
        self
    }

    /// Saves sessions to `store`, and logs monitors next to them.
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = Some(Arc::new(store));
        if let Ok(data) = store::data_dir() {
            self.monitor_root = data;
            self.log_monitors_for_session();
        }
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
        // `None` while your editor has the terminal.
        let mut input = Some(EventStream::new());
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.index_files();
        self.load_git();
        self.list_llms();
        self.read_plan();

        while !self.quit {
            if self.pending_editor.is_some() {
                self.open_editor(&mut input).await;
            }
            if !self.is_editing() {
                terminal.draw(|frame| self.draw(frame))?;
            }
            let ticking =
                self.is_busy() || self.running_monitors() > 0 || self.running_subagents() > 0;
            let step = tokio::select! {
                event = next_input(&mut input) => Step::Terminal(event),
                ended = self.editor.join() => Step::EditorClosed(ended),
                Some(event) = self.events_rx.recv() => Step::Session(event),
                Some(ask) = self.asks_rx.recv() => Step::Asked(ask),
                Ok(()) = self.notify_errors.changed() => Step::NotifyFailed,
                Some(panel) = self.screen_rx.recv() => Step::Show(panel),
                Some(event) = self.monitor_rx.recv() => Step::Monitor(event),
                Some(event) = self.subagent_rx.recv() => Step::Subagent(event),
                _ = due(self.notices_due) => Step::NoticesDue,
                ended = self.turn.join() => Step::TurnEnded(ended),
                files = self.indexing.join() => Step::Indexed(files),
                status = self.git_loading.join() => Step::GitLoaded(status),
                llms = self.llm_listing.join() => Step::LlmsListed(llms),
                sessions = self.session_listing.join() => Step::SessionsListed(sessions),
                session = self.session_loading.join() => Step::SessionLoaded(session),
                saved = self.history_saving.join() => Step::HistorySaved(saved),
                plan = self.plan_reading.join() => Step::PlanRead(plan),
                servers = self.servers_lookup.join() => Step::ServersFound(servers),
                formatters = self.formatters_lookup.join() => Step::FormattersFound(formatters),
                changed = lsp_changed(&mut self.lsp) => Step::LspChanged(changed),
                _ = tick.tick(), if ticking => Step::Tick,
            };
            match step {
                Step::Terminal(None) => break,
                Step::Terminal(Some(event)) => {
                    self.on_terminal(event.context("reading terminal input")?)
                }
                Step::Session(event) => self.on_session(event),
                Step::Asked(ask) => {
                    self.notify_ask(&ask);
                    self.on_ask(ask)
                }
                Step::NotifyFailed => self.notify_failed(),
                Step::Show(panel) => self.open_content(panel.into()),
                Step::Monitor(event) => self.on_monitor(event),
                Step::Subagent(event) => self.on_subagent(event),
                Step::NoticesDue => self.notices_due(),
                Step::TurnEnded(Ok(ended)) => self.end_turn(ended),
                Step::TurnEnded(Err(e)) => self.turn_task_failed(e),
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
                Step::EditorClosed(ended) => {
                    self.editor_closed(terminal, &mut input, ended).await?
                }
                Step::PlanRead(plan) => self.plan_read(plan.context("reading the plan failed")?),
                Step::ServersFound(servers) => {
                    self.diagnostics.servers = Some(servers.context("finding servers failed")?)
                }
                Step::FormattersFound(formatters) => {
                    self.diagnostics.formatters =
                        Some(formatters.context("checking formatters failed")?)
                }
                Step::LspChanged(changed) => self.servers_changed(changed),
                // Nothing changed but time: the redraw advances the timers.
                Step::Tick => {}
            }
        }
        self.stop_background_for_exit().await;
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

        self.close_answered_subagents();
        self.areas = mouse::Areas { header };
        let tabs = self.tab_labels();
        header::draw(frame, header, &tabs);
        match self.content.active() {
            Tab::Monitor(id) => {
                if let Some(view) = self.monitor_views.get_mut(&id) {
                    view.draw(frame, content, self.home.as_deref());
                }
            }
            Tab::Subagent(id) => {
                if let Some(view) = self.subagent_views.get_mut(&id) {
                    let chat = view.draw(frame, content);
                    if let Some(state) = view.chat.scrollbar() {
                        draw_scrollbar(frame, chat, state);
                    }
                }
            }
            Tab::Plan => {
                let place = self.plan_place();
                self.plan.draw(frame, content, &place);
                if let Some(state) = self.plan.scrollbar() {
                    draw_scrollbar(frame, content, state);
                }
            }
            Tab::Diagnostics => {
                let facts = diagnostics::Facts {
                    model: &self.model,
                    effort: self.effort.wire(),
                    context_window: self.context_window(),
                    llms: self.llms.as_ref().map(|l| l.models.len()),
                    failed: self.llms.as_ref().map_or(&[], |l| l.failed.as_slice()),
                    listing: self.llm_listing.is_running(),
                    checks: self.checkers.is_some(),
                    running: &self.servers,
                    context: &self.context,
                    home: self.home.as_deref(),
                };
                self.diagnostics.draw(frame, content, &facts);
            }
            Tab::Chat => {
                let banner = format!("nth · {} · {}", self.model, self.place);
                self.chat.draw(frame, content, &banner);
                if let Some(state) = self.chat.scrollbar() {
                    draw_scrollbar(frame, content, state);
                }
            }
        }
        status::draw(frame, status, self);
        match &self.input {
            Input::Prompt => {
                // On a subagent's tab the prompt is its: the label names it
                // and the spinner follows its turn, not the main session's.
                let target = self
                    .showing_subagent()
                    .and_then(|id| self.subagent_views.get(&id));
                let since = match target {
                    Some(view) => view.running_since(),
                    None => self.busy_since,
                };
                let spinner = since.map(|since| spinner::frame(since.elapsed()));
                let target = target.map(|view| view.agent.as_str());
                prompt::draw(frame, input, &self.prompt, self.mode, spinner, target);
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

    /// The header's tabs as it shows them now.
    fn tab_labels(&mut self) -> Vec<header::TabLabel> {
        self.content
            .tabs()
            .to_vec()
            .into_iter()
            .map(|tab| {
                let state = self.tab_state(tab);
                header::TabLabel {
                    icon: self.tab_icon(tab),
                    name: self.tab_label(tab),
                    state: self.content.shown_state(tab, state),
                    active: tab == self.content.active(),
                }
            })
            .collect()
    }

    fn on_terminal(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            TermEvent::Paste(text) if matches!(self.input, Input::Prompt) => {
                self.prompt.paste(&text);
                self.refresh_completion();
            }
            _ => {}
        }
    }

    fn on_session(&mut self, event: Event) {
        // A write or a command may have changed the tree.
        match &event {
            Event::ToolFinished { call, .. }
                if matches!(
                    call.name.as_str(),
                    "write" | "edit" | "apply_patch" | "bash"
                ) =>
            {
                self.load_git();
                if call.name != "bash" {
                    self.read_plan();
                }
            }
            Event::Usage(usage) => self.usage = Some(*usage),
            _ => {}
        }
        self.chat.apply(&event);
    }

    fn run_command(&mut self, command: Command) {
        match command {
            // Dropping the app aborts a running turn.
            Command::Exit => self.ask_quit(),
            // Mid-turn the session is in the turn task, so there is nothing
            // to replace yet.
            Command::Clear if self.is_busy() => self.hint = Some("a turn is running".into()),
            Command::Clear => {
                self.chat = Chat::new(self.cwd.clone());
                self.last_turn = TabState::Idle;
                self.start_fresh_session();
            }
            Command::Models => self.open_llm_picker(),
            Command::Resume => self.open_session_picker(),
            Command::Diagnostics => self.open_content(Tab::Diagnostics),
            Command::Approve => self.approve(),
            Command::Close => self.close_content(),
        }
    }

    /// Moves on to a new, empty session in the same directory, so it has
    /// the same instruction files and skills, and keeps the model, effort
    /// and mode picked. The chat is the caller's to clear or keep.
    fn start_fresh_session(&mut self) {
        let mut session =
            Session::new(self.model.clone(), self.cwd.clone()).with_context(self.context.clone());
        session.effort = self.effort;
        session.mode = self.mode;
        session.max_steps = self.max_steps;
        let plan_path = session.plan_path();
        self.session = Some(session);
        self.usage = None;
        self.plan_for_session(plan_path);
        self.left_session();
    }

    /// Shows `tab`. The diagnostics tab looks again at what applies every
    /// time it opens, since servers and formatters may have been installed
    /// since.
    pub(super) fn open_content(&mut self, tab: Tab) {
        if self.content.open(tab) && tab == Tab::Diagnostics {
            self.diagnose();
        }
    }
}
