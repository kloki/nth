//! The app: its state, the loop that drives it, and the layout of the
//! three bands: the content panel under its tab header, the input panel
//! and the status bar. Row heights never depend on content, so nothing
//! shifts while a turn runs.

mod completion;
mod connect;
mod content;
mod diagnostics;
mod dirs;
mod editor;
mod file;
mod files;
mod flash;
mod git;
mod hint;
mod history;
mod input;
mod job;
mod keys;
mod llm_picker;
mod mode;
mod monitor;
mod mouse;
mod notify;
mod plan;
mod profiles;
mod question;
mod quit;
mod session_picker;
mod settings;
mod subagent;
#[cfg(test)]
pub(crate) mod tests;
mod turn;
mod usage;
mod work;

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use completion::Completion;
pub(crate) use content::{Content, Panes, Tab, TabState};
use crossterm::{
    event::{Event as TermEvent, EventStream, KeyEventKind},
    execute, queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use diagnostics::lsp_changed;
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
    Ask, BoxError, Compaction, Effort, Event, Inbox, Listing, Mode, MonitorEvent, MonitorId,
    Monitors, Provider, Show, Tool, Usage, monitor_log_dir, tool_names,
};
use nth_session::{
    Session, Store, Subagents, Summary, ToolUse, store,
    subagent::{SubagentEvent, SubagentId},
};
pub use profiles::Profile;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style},
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinError,
    time::MissedTickBehavior,
};
use turn::{Ended, Queued};
use usage::Spent;

use crate::{
    Checkers,
    chat::Chat,
    command::Command,
    diagnostics::{Diagnostics, Program},
    file::{FileId, FileRead, FileView},
    git::{GitStatus, Pr},
    header, hero,
    history::History,
    llm_picker::usage::LlmUsage,
    monitor::MonitorView,
    plan::PlanView,
    prompt::{self, Prompt},
    settings::ChatSettings,
    spinner, status,
    subagent::SubagentView,
    usage::UsageView,
    work::WorkView,
};

/// How often the app redraws with nothing else happening, while a turn
/// runs or a monitor does: the spinner shows every frame, and the
/// reasoning timer and the monitors' running time advance.
const TICK: Duration = spinner::FRAME;
/// How long notices wait for more lines before an idle app sends them, so
/// one burst of output reaches the model as one message.
const NOTICE_DELAY: Duration = Duration::from_millis(200);

/// The next whole minute, when the status bar's session time changes; `run`
/// wakes with `due` then, so the bar stays true while nothing else happens.
fn next_minute() -> tokio::time::Instant {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    tokio::time::Instant::now() + Duration::from_secs(60 - now.as_secs() % 60)
}

pub struct App {
    pub chat: Chat,
    pub prompt: Prompt,
    /// The mode the next turn runs in.
    pub mode: Mode,
    /// The model and effort each mode runs with; the current mode's are
    /// `model` and `effort`, which it is synced with on a switch.
    mode_llms: ModeLlms,
    /// The profiles ctrl+p cycles, in config order; empty when the config
    /// lists none.
    profiles: Vec<Profile>,
    /// The profile ctrl+p is on. A model picked by hand hides its name
    /// from the label until the next ctrl+p puts the profile back.
    profile: Option<usize>,
    /// Prompts sent before, recalled with Up and Down.
    history: History,
    /// Each save writes the whole history; one asked for mid-save runs
    /// after it, since an aborted task can't stop a write already on the
    /// blocking pool.
    history_saving: Job<std::io::Result<()>>,
    /// Turns run on each model, which orders the picker and graphs in the
    /// usage tab.
    llm_usage: LlmUsage,
    /// How the chats show what the model did: the config's `[chat]` to
    /// start, `/settings` to change it for this run.
    settings: ChatSettings,
    /// The terminal width from which the content panel splits, chat on the
    /// left; 0 never splits. `[layout] split_width`.
    split_width: u16,
    /// Whether the last draw was that wide.
    wide: bool,
    /// Saved like the history, one write at a time.
    llm_usage_saving: Job<std::io::Result<()>>,
    /// Prompts sent while a turn runs, oldest first; each runs as its own
    /// turn once the one before ends well. Always empty while idle.
    pub queue: VecDeque<Queued>,
    /// The mode, model and effort the running model turn has, which a
    /// prompt sent mid-turn steers into only while they are still yours;
    /// `None` while idle or running a command.
    steerable: Option<(Mode, mode::Llm)>,
    /// Esc was pressed during the running turn, which may have finished
    /// before it saw the cancel.
    interrupted: bool,
    /// How the chat's last turn ended, for its tab: done, failed, or idle
    /// when you stopped it or there was none.
    last_turn: TabState,
    /// The content panel's tabs, and which one fills it.
    content: Content,
    diagnostics: Diagnostics,
    usage_view: UsageView,
    /// Every saved session's tool totals, most called first; empty until
    /// the usage tab is opened.
    all_time_tools: Vec<(String, ToolUse)>,
    /// Reads every saved session's tool totals for the usage tab.
    tools_lookup: Job<Result<Vec<(String, ToolUse)>, store::Error>>,
    /// What checks the tools' writes, for the diagnostics tab to look up;
    /// `None` when nothing does.
    checkers: Option<Checkers>,
    servers_lookup: Job<Vec<ServerInfo>>,
    formatters_lookup: Job<Vec<FormatterStatus>>,
    rtk_lookup: Job<Option<PathBuf>>,
    browser_lookup: Job<Option<PathBuf>>,
    /// What fills the input panel.
    input: Input,
    /// The model and effort the next turn runs with.
    pub model: String,
    pub effort: Effort,
    pub cwd: PathBuf,
    /// The working directory as shown in the status bar, `~` for home.
    pub place: String,
    home: Option<String>,
    /// Working directories added with `/add-dir`, which the tools may read
    /// and edit like `cwd` itself.
    pub extra_dirs: Vec<PathBuf>,
    /// When the running turn started; `None` while idle.
    pub busy_since: Option<Instant>,
    /// When the current session was created, for the status bar's time;
    /// from the session, so it carries across /resume and starts over with
    /// /clear.
    pub session_since: SystemTime,
    /// The current session's id, kept here because the session itself is
    /// in the turn task while a turn runs.
    session_id: uuid::Uuid,
    /// When the empty chat's field started moving, and where the mouse
    /// last was, for its ripple.
    hero_since: Instant,
    /// The blank before a tab the model just showed.
    flash: Option<flash::Flash>,
    pointer: Option<Position>,
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
    /// The current branch's pull request, as gh or tea found it; `None`
    /// until a load lands, and whenever the branch has none.
    pub pr: Option<Pr>,
    /// The language servers the tools started, as last reported.
    pub servers: Vec<ServerStatus>,
    /// Where `servers` comes from; `None` once its sender is gone.
    lsp: Option<watch::Receiver<Vec<ServerStatus>>>,
    /// What the last model reply used; `None` until the first one.
    pub usage: Option<Usage>,
    /// What the session spent, its subagents included, as heard so far.
    pub spent: Spent,
    /// Queued again when the tree may have changed mid-load, like `indexing`.
    git_loading: Job<Result<Option<GitStatus>, String>>,
    /// The pull request on the branch, loaded with the git status.
    pr_loading: Job<Option<Pr>>,
    /// The LLMs the endpoint serves, kept once listed; after a failure the
    /// next open asks again.
    llms: Option<Listing>,
    llm_listing: Job<Result<Listing, BoxError>>,
    /// The ChatGPT login `/connect` signs in through; `None` leaves
    /// nothing to connect.
    chatgpt: Option<nth_auth::ChatGpt>,
    /// The login `/connect` started, and where its URL comes first.
    login: connect::Login,
    connect_url: Option<oneshot::Receiver<String>>,
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
    /// The configured `[context]` policy, which every session the app moves
    /// on to takes, as it does `max_steps`: a session never saves it.
    compaction: Compaction,
    session_listing: Job<Result<Vec<Summary>, store::Error>>,
    /// The session chosen in the session picker, being read.
    session_loading: Job<Result<Session, store::Error>>,
    /// A save between turns, after a change no turn would save, such as
    /// `/add-dir`; one asked for mid-save runs after it.
    session_saving: Job<Result<(), store::Error>>,
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
    screen_tx: mpsc::Sender<Show>,
    screen_rx: mpsc::Receiver<Show>,
    turn: Job<Ended>,
    /// The commands the model left running, shared with every turn's tools.
    monitors: Monitors,
    /// What the model has not heard yet from the monitors and subagents,
    /// and the prompts on their way into a running turn.
    pub(crate) inbox: Inbox,
    monitor_rx: mpsc::Receiver<MonitorEvent>,
    /// Each monitor's tab, open from its start until you close it.
    monitor_views: BTreeMap<MonitorId, MonitorView>,
    /// The subagents the task tool started, shared with it.
    subagents: Subagents,
    subagent_rx: mpsc::Receiver<SubagentEvent>,
    /// Each subagent's tab, open from its start until you close it.
    subagent_views: BTreeMap<SubagentId, SubagentView>,
    /// Each file tab, open from when the model showed it until you close
    /// it, and the id the next one gets.
    file_views: BTreeMap<FileId, FileView>,
    next_file: FileId,
    /// Where the file tabs' watchers send what they read.
    file_tx: mpsc::Sender<FileRead>,
    file_rx: mpsc::Receiver<FileRead>,
    /// The session's plan file and what the plan tab shows of it.
    plan: PlanView,
    plan_path: PathBuf,
    plan_reading: Job<Option<String>>,
    /// The next read is of a session just opened, so it marks nothing as
    /// changed.
    plan_settle: bool,
    /// The session's work plan and what the work tab shows of it.
    work: WorkView,
    work_dir: PathBuf,
    /// Reads of the work plan, with the directory each read.
    work_reading: Job<(PathBuf, Option<nth_session::work::Plan>)>,
    /// The next read is of a session just opened, so a plan it finds
    /// opens its tab without showing it.
    work_settle: bool,
    /// Where monitors log, one folder per session under it.
    monitor_root: PathBuf,
    /// When the notices waiting start a turn, if the app is idle by then.
    notices_due: Option<tokio::time::Instant>,
    /// Notices wait for your next prompt rather than start a turn, after
    /// you stopped one.
    hold_notices: bool,
    /// A word on what the last key did or did not do, on the status bar
    /// until the next key or for a few seconds.
    pub hint: Option<hint::Hint>,
    /// Where the last draw put the header, so a click knows it landed there.
    areas: mouse::Areas,
    /// How a right click's text reaches the clipboard; tests put it nowhere.
    clipboard: fn(&str) -> std::io::Result<()>,
    /// How a link reaches the desktop's opener; tests put it nowhere.
    opener: fn(&str) -> std::io::Result<()>,
    /// ctrl+g asked for the editor, on this target; the loop opens it
    /// after this step.
    pending_editor: Option<editor::Target>,
    /// The plan copy your editor has open; the app draws nothing while it
    /// runs.
    editing: Option<editor::Editing>,
    editor: Job<std::io::Result<std::process::ExitStatus>>,
    notifier: Notifier,
    notify_errors: watch::Receiver<Option<String>>,
    /// A turn that ended in plan mode, waiting for the plan to be read to
    /// say whether it left a plan to approve.
    plan_notice: Option<nth_notify::Context>,
    /// Whether the terminal has focus, as it last reported; nth starts
    /// focused because you just started it. Notifications go out only
    /// without it.
    focused: bool,
    /// ctrl+c was pressed once with monitors running; again quits.
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

/// The login's URL once it has one, or never while there is no login.
async fn login_url(rx: &mut Option<oneshot::Receiver<String>>) -> Option<String> {
    let Some(url) = rx else {
        return std::future::pending().await;
    };
    let url = url.await.ok();
    // A finished receiver must not be polled again.
    *rx = None;
    url
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
    Show(Show),
    FileRead(FileRead),
    Monitor(MonitorEvent),
    Subagent(SubagentEvent),
    NoticesDue,
    HintExpired,
    TurnEnded(Result<Ended, JoinError>),
    Indexed(Result<Vec<String>, JoinError>),
    GitLoaded(Result<Result<Option<GitStatus>, String>, JoinError>),
    PrLoaded(Result<Option<Pr>, JoinError>),
    LlmsListed(Result<Result<Listing, BoxError>, JoinError>),
    LoginUrl(String),
    Connected(Result<Result<nth_auth::Account, nth_auth::Error>, JoinError>),
    SessionsListed(Result<Result<Vec<Summary>, store::Error>, JoinError>),
    SessionLoaded(Result<Result<Session, store::Error>, JoinError>),
    HistorySaved(Result<std::io::Result<()>, JoinError>),
    SessionSaved(Result<Result<(), store::Error>, JoinError>),
    LlmUsageSaved(Result<std::io::Result<()>, JoinError>),
    EditorClosed(Result<std::io::Result<std::process::ExitStatus>, JoinError>),
    PlanRead(Result<Option<String>, JoinError>),
    WorkRead(Result<(PathBuf, Option<nth_session::work::Plan>), JoinError>),
    ServersFound(Result<Vec<ServerInfo>, JoinError>),
    ToolsFound(Result<Result<Vec<(String, ToolUse)>, store::Error>, JoinError>),
    FormattersFound(Result<Vec<FormatterStatus>, JoinError>),
    RtkFound(Result<Option<PathBuf>, JoinError>),
    BrowserFound(Result<Option<PathBuf>, JoinError>),
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
        let (file_tx, file_rx) = mpsc::channel(16);
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
        let work_dir = session.work_dir();
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
            profiles: Vec::new(),
            profile: None,
            history: History::default(),
            history_saving: Job::default(),
            llm_usage: LlmUsage::default(),
            llm_usage_saving: Job::default(),
            settings: ChatSettings::default(),
            split_width: 0,
            wide: false,
            queue: VecDeque::new(),
            steerable: None,
            interrupted: false,
            last_turn: TabState::Idle,
            content: Content::default(),
            diagnostics: Diagnostics::default(),
            usage_view: UsageView::default(),
            all_time_tools: Vec::new(),
            tools_lookup: Job::default(),
            checkers: None,
            servers_lookup: Job::default(),
            formatters_lookup: Job::default(),
            rtk_lookup: Job::default(),
            browser_lookup: Job::default(),
            input: Input::Prompt,
            model: session.model.clone(),
            effort: session.effort,
            cwd: session.cwd.clone(),
            place: status::place(&session.cwd, home.as_deref()),
            home,
            extra_dirs: session.extra_dirs.clone(),
            busy_since: None,
            session_since: session.created_at,
            session_id: session.id,
            hero_since: Instant::now(),
            flash: None,
            pointer: None,
            completion: None,
            files: Vec::new(),
            indexing: Job::default(),
            git: None,
            pr: None,
            servers: Vec::new(),
            lsp: None,
            usage: None,
            spent: Spent::new(session.usage.clone()),
            git_loading: Job::default(),
            pr_loading: Job::default(),
            llms: None,
            llm_listing: Job::default(),
            chatgpt: None,
            login: Job::default(),
            connect_url: None,
            store: None,
            paths: Paths::default(),
            context: session.context().clone(),
            max_steps: session.max_steps,
            compaction: session.compaction,
            session_listing: Job::default(),
            session_loading: Job::default(),
            session_saving: Job::default(),
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
            file_views: BTreeMap::new(),
            next_file: 0,
            file_tx,
            file_rx,
            plan: PlanView::default(),
            plan_path,
            plan_reading: Job::default(),
            plan_settle: true,
            work: WorkView::default(),
            work_dir,
            work_reading: Job::default(),
            work_settle: true,
            monitor_root,
            notices_due: None,
            hold_notices: false,
            hint: None,
            areas: mouse::Areas::default(),
            clipboard: crate::terminal::copy,
            opener: mouse::open,
            pending_editor: None,
            editing: None,
            editor: Job::default(),
            notify_errors: notifier.errors(),
            notifier,
            plan_notice: None,
            focused: true,
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

    pub fn with_llm_usage(mut self, usage: LlmUsage) -> Self {
        self.llm_usage = usage;
        self
    }

    /// The initial thinking and tool-output toggles, from the config's
    /// `[chat]`; `/settings` flips them for the run.
    pub fn with_chat_settings(mut self, settings: ChatSettings) -> Self {
        self.settings = settings;
        self
    }

    /// The terminal width from which the chat gets the left half of the
    /// content panel, from `[layout]`; 0 never splits.
    pub fn with_split_width(mut self, width: u16) -> Self {
        self.split_width = width;
        self
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        // `None` while your editor has the terminal.
        let mut input = Some(EventStream::new());
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.index_files();
        self.load_git();
        self.load_pr();
        self.list_llms();
        self.read_plan();
        self.read_work();

        while !self.quit {
            if self.pending_editor.is_some() {
                self.open_editor(&mut input).await;
            }
            if !self.is_editing() {
                // The terminal holds the frame until it is all written, so
                // a screen that changes everywhere at once (the empty chat's
                // field) never shows half old, half new.
                queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
                terminal.draw(|frame| self.draw(frame))?;
                execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
            }
            let ticking = self.is_busy()
                || self.running_monitors() > 0
                || self.running_subagents() > 0
                || self.showing_hero()
                || self.flash.as_ref().is_some_and(flash::Flash::running)
                || self.hint.as_ref().is_some_and(hint::Hint::flashing);
            let step = tokio::select! {
                event = next_input(&mut input) => Step::Terminal(event),
                ended = self.editor.join() => Step::EditorClosed(ended),
                Some(event) = self.events_rx.recv() => Step::Session(event),
                Some(ask) = self.asks_rx.recv() => Step::Asked(ask),
                Ok(()) = self.notify_errors.changed() => Step::NotifyFailed,
                Some(show) = self.screen_rx.recv() => Step::Show(show),
                Some(read) = self.file_rx.recv() => Step::FileRead(read),
                Some(event) = self.monitor_rx.recv() => Step::Monitor(event),
                Some(event) = self.subagent_rx.recv() => Step::Subagent(event),
                _ = due(self.notices_due) => Step::NoticesDue,
                _ = due(self.hint.as_ref().map(hint::Hint::expires)) => Step::HintExpired,
                // The status bar's session time changes on the minute.
                _ = due(Some(next_minute())) => Step::Tick,
                ended = self.turn.join() => Step::TurnEnded(ended),
                files = self.indexing.join() => Step::Indexed(files),
                status = self.git_loading.join() => Step::GitLoaded(status),
                pr = self.pr_loading.join() => Step::PrLoaded(pr),
                llms = self.llm_listing.join() => Step::LlmsListed(llms),
                Some(url) = login_url(&mut self.connect_url) => Step::LoginUrl(url),
                account = self.login.join() => Step::Connected(account),
                sessions = self.session_listing.join() => Step::SessionsListed(sessions),
                session = self.session_loading.join() => Step::SessionLoaded(session),
                saved = self.history_saving.join() => Step::HistorySaved(saved),
                saved = self.session_saving.join() => Step::SessionSaved(saved),
                saved = self.llm_usage_saving.join() => Step::LlmUsageSaved(saved),
                plan = self.plan_reading.join() => Step::PlanRead(plan),
                plan = self.work_reading.join() => Step::WorkRead(plan),
                servers = self.servers_lookup.join() => Step::ServersFound(servers),
                tools = self.tools_lookup.join() => Step::ToolsFound(tools),
                formatters = self.formatters_lookup.join() => Step::FormattersFound(formatters),
                rtk = self.rtk_lookup.join() => Step::RtkFound(rtk),
                browser = self.browser_lookup.join() => Step::BrowserFound(browser),
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
                Step::Show(show) => self.show(show),
                Step::FileRead((id, text)) => self.file_read(id, text),
                Step::Monitor(event) => self.on_monitor(event),
                Step::Subagent(event) => self.on_subagent(event),
                Step::NoticesDue => self.notices_due(),
                Step::HintExpired => self.hint_expired(),
                Step::TurnEnded(Ok(ended)) => self.end_turn(ended),
                Step::TurnEnded(Err(e)) => self.turn_task_failed(e),
                Step::Indexed(files) => self.indexed(files.context("listing files failed")?),
                Step::GitLoaded(status) => {
                    self.git_loaded(status.context("reading git status failed")?)
                }
                Step::PrLoaded(pr) => {
                    self.pr_loaded(pr.context("reading the pull request failed")?)
                }
                Step::LlmsListed(llms) => self.llms_listed(llms.context("listing LLMs failed")?),
                Step::LoginUrl(url) => self.login_url(url),
                Step::Connected(account) => self.connected(account.context("signing in failed")?),
                Step::SessionsListed(sessions) => {
                    self.sessions_listed(sessions.context("listing sessions failed")?)
                }
                Step::SessionLoaded(session) => {
                    self.session_loaded(session.context("loading the session failed")?)
                }
                Step::HistorySaved(saved) => {
                    self.history_saved(saved.context("saving prompt history failed")?)
                }
                Step::SessionSaved(saved) => {
                    self.session_saved(saved.context("saving the session failed")?)
                }
                Step::LlmUsageSaved(saved) => {
                    self.llm_usage_saved(saved.context("saving model usage failed")?)
                }
                Step::EditorClosed(ended) => {
                    self.editor_closed(terminal, &mut input, ended).await?
                }
                Step::PlanRead(plan) => self.plan_read(plan.context("reading the plan failed")?),
                Step::WorkRead(plan) => {
                    self.work_read(plan.context("reading the work plan failed")?)
                }
                Step::ServersFound(servers) => {
                    self.diagnostics.servers = Some(servers.context("finding servers failed")?)
                }
                Step::ToolsFound(tools) => {
                    self.tools_found(tools.context("reading tool usage failed")?)
                }
                Step::FormattersFound(formatters) => {
                    self.diagnostics.formatters =
                        Some(formatters.context("checking formatters failed")?)
                }
                Step::RtkFound(rtk) => {
                    self.diagnostics.rtk = Some(match rtk.context("finding rtk failed")? {
                        Some(program) => Program::Found(program),
                        None => Program::Missing,
                    });
                }
                Step::BrowserFound(browser) => {
                    self.diagnostics.browser =
                        Some(match browser.context("finding agent-browser failed")? {
                            Some(program) => Program::Found(program),
                            None => Program::Missing,
                        });
                }
                Step::LspChanged(changed) => self.servers_changed(changed),
                // Nothing changed but time: the redraw advances the timers.
                Step::Tick => {}
            }
        }
        self.stop_background_for_exit().await;
        Ok(())
    }

    /// The model shows you a tab; one it switches to comes in after a blank, so the
    /// switch does not go unnoticed.
    fn show(&mut self, show: Show) {
        let before = self.content.active();
        match show {
            Show::Panel(panel) => self.open_content(panel.into()),
            Show::File(path) => self.show_file(path),
        }
        if self.content.active() != before {
            self.flash = Some(flash::Flash::new());
        }
    }

    /// Whether the empty chat's field shows, which moves on every tick.
    fn showing_hero(&self) -> bool {
        self.panes().shows(Tab::Chat) && self.chat.transcript.is_empty()
    }

    /// What the content panel shows, as of the last draw's width.
    pub(super) fn panes(&self) -> Panes {
        self.content.panes(self.wide)
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
        self.areas = mouse::Areas {
            header,
            ..mouse::Areas::default()
        };
        self.wide = self.split_width != 0 && frame.area().width >= self.split_width;
        let panes = match self.panes() {
            Panes::Single(tab) => vec![(tab, content)],
            Panes::Split(side) => {
                // Two columns between them: the chat's scrollbar and a gap.
                let [left, right] =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .spacing(2)
                        .areas(content);
                vec![(Tab::Chat, left), (side, right)]
            }
        };
        for &(tab, area) in &panes {
            self.draw_tab(frame, tab, area);
        }
        let focused = panes
            .iter()
            .find(|(tab, _)| *tab == self.content.active())
            .map_or(content, |&(_, area)| area);
        self.areas.panes = panes;
        let tabs = self.tab_labels();
        header::draw(frame, header, &tabs, self.areas.header_split());
        if let Some(flash) = self.flash.as_ref().filter(|r| r.running()) {
            flash.draw(frame.buffer_mut(), focused);
        }
        // The link's place is kept for a click to land on.
        self.areas.pr_link = status::draw(frame, status, self);
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
                prompt::draw(
                    frame,
                    input,
                    &self.prompt,
                    self.mode,
                    prompt::Runs {
                        spinner,
                        target,
                        llm: &self.llm(self.mode),
                        profile: self.profile_name(),
                    },
                );
                // Last, so it pops over the content panel; it sits right
                // above the row being typed, lined up with the token it
                // completes.
                if let Some(completion) = &self.completion {
                    let at = prompt::position(input, &self.prompt, completion.start());
                    completion.draw(frame, area, at);
                }
            }
            Input::LlmPicker(picker) => crate::llm_picker::draw(frame, input, picker),
            Input::SessionPicker(picker) => crate::session_picker::draw(frame, input, picker),
            Input::Question(panel) => crate::question::draw(frame, input, panel),
            Input::Settings(panel) => crate::settings::draw(frame, input, panel, self.settings),
            Input::Connect(panel) => {
                self.areas.login_url = crate::connect::draw(frame, input, panel)
            }
        }
    }

    /// `tab`'s view into `area`, with its scrollbar.
    fn draw_tab(&mut self, frame: &mut Frame, tab: Tab, area: Rect) {
        match tab {
            Tab::Monitor(id) => {
                if let Some(view) = self.monitor_views.get_mut(&id) {
                    view.draw(frame, area, self.home.as_deref());
                }
            }
            Tab::Subagent(id) => {
                if let Some(view) = self.subagent_views.get_mut(&id) {
                    let chat = view.draw(frame, area, self.settings);
                    if let Some(state) = view.chat.scrollbar() {
                        draw_scrollbar(frame, chat, state);
                    }
                }
            }
            Tab::Plan => {
                let place = self.plan_place();
                self.plan.draw(frame, area, &place);
                if let Some(state) = self.plan.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Work => {
                let place = self.work_place();
                let live = self.live_work();
                self.work.draw(frame, area, &place, &live);
                if let Some(state) = self.work.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::File(id) => {
                let place = self.file_place(id);
                if let Some(view) = self.file_views.get_mut(&id) {
                    view.draw(frame, area, &place);
                    if let Some(state) = view.pager.scrollbar() {
                        draw_scrollbar(frame, area, state);
                    }
                }
            }
            Tab::Diagnostics => {
                let row = |label, llm: Llm, current, inherits| crate::diagnostics::ModeModel {
                    label,
                    effort: llm.effort.wire(),
                    context_window: self.context_window_of(&llm.model),
                    model: llm.model,
                    current,
                    inherits,
                };
                let [plan, act] = Mode::ALL
                    .map(|mode| row(mode.label(), self.llm(mode), mode == self.mode, None));
                let subagent = row(
                    "subagent",
                    self.subagent_llm(),
                    false,
                    self.mode_llms.subagent.is_none().then_some("the mode"),
                );
                let clerk = row(
                    "clerk",
                    self.clerk_llm(),
                    false,
                    self.mode_llms.clerk.is_none().then_some("subagent"),
                );
                let facts = crate::diagnostics::Facts {
                    models: [plan, act, subagent, clerk],
                    llms: self.llms.as_ref().map(|l| l.models.len()),
                    failed: self.llms.as_ref().map_or(&[], |l| l.failed.as_slice()),
                    listing: self.llm_listing.is_running(),
                    checks: self.checkers.is_some(),
                    rtk: self.checkers.as_ref().is_some_and(|checkers| checkers.rtk),
                    running: &self.servers,
                    context: &self.context,
                    home: self.home.as_deref(),
                };
                self.diagnostics.draw(frame, area, &facts);
                if let Some(state) = self.diagnostics.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Usage => {
                // The listing alone, so the view can be drawn into.
                let llms = self.llms.as_ref();
                let cost = |model: &str| llms?.cost_of(model);
                self.usage_view.draw(
                    frame,
                    area,
                    &self.spent.ledger,
                    &self.llm_usage.ranked(),
                    &self.all_time_tools,
                    &cost,
                );
                if let Some(state) = self.usage_view.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Chat => {
                if self.chat.transcript.is_empty() {
                    hero::draw(frame, area, self.hero_since.elapsed(), self.pointer);
                }
                self.chat.draw(frame, area, self.settings);
                if let Some(state) = self.chat.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
        }
    }

    /// The header's tabs as it shows them now.
    fn tab_labels(&mut self) -> Vec<header::TabLabel> {
        let panes = self.panes();
        let split = matches!(panes, Panes::Split(_));
        self.content
            .tabs()
            .to_vec()
            .into_iter()
            .map(|tab| {
                let state = self.tab_state(tab);
                header::TabLabel {
                    icon: self.tab_icon(tab),
                    name: self.tab_label(tab),
                    state: self.content.shown_state(tab, state, panes),
                    shown: panes.shows(tab),
                    focused: split && tab == self.content.active(),
                }
            })
            .collect()
    }

    fn on_terminal(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            TermEvent::FocusGained => self.focused = true,
            TermEvent::FocusLost => self.focused = false,
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
                if tool_names::writes_files(&call.name) || call.name == "bash" =>
            {
                self.load_git();
                if call.name != "bash" {
                    self.read_plan();
                }
            }
            Event::ToolFinished { call, .. } if tool_names::touches_work(&call.name) => {
                self.read_work()
            }
            Event::Usage(usage) => {
                self.usage = Some(*usage);
                self.spent.usage(&self.model, *usage);
            }
            Event::Moved(cwd) => self.follow(cwd.clone()),
            _ => {}
        }
        self.chat.apply(&event);
    }

    fn run_command(&mut self, command: Command) {
        match command {
            // Dropping the app aborts a running turn.
            Command::Exit => self.ask_quit(),
            // `/add-dir` without a directory to add only says how it is used.
            Command::AddDir => self.add_dir(""),
            // Mid-turn the session is in the turn task, so there is nothing
            // to replace yet.
            Command::Clear if self.is_busy() => self.hint = Some("a turn is running".into()),
            Command::Clear => {
                self.chat = Chat::new(self.cwd.clone());
                self.last_turn = TabState::Idle;
                self.start_fresh_session();
            }
            Command::Models => self.open_llm_picker(),
            Command::Connect => self.open_connect(),
            Command::Resume => self.open_session_picker(),
            Command::Diagnostics => self.open_content(Tab::Diagnostics),
            Command::Usage => self.open_content(Tab::Usage),
            Command::Plan => self.open_content(Tab::Plan),
            Command::Work => self.open_content(Tab::Work),
            Command::Approve => self.approve(),
            Command::Compact => self.compact(),
            Command::Close => self.close_content(),
            Command::CopyId => self.copy_session_id(),
            Command::Settings => self.open_settings(),
        }
    }

    /// `/copy-id`: the session's id on the clipboard, for the commands that
    /// take one (`nth usage`, `nth work`, `nth session delete`).
    fn copy_session_id(&mut self) {
        self.hint = Some(match (self.clipboard)(&self.session_id.to_string()) {
            Ok(()) => format!("copied {}", self.session_id).into(),
            Err(e) => format!("could not copy: {e}").into(),
        });
    }

    /// Moves on to a new, empty session in the same directory, so it has
    /// the same instruction files and skills, and keeps the model, effort
    /// and mode picked. The directories added with `/add-dir` are kept too.
    /// The chat is the caller's to clear or keep.
    fn start_fresh_session(&mut self) {
        let mut session =
            Session::new(self.model.clone(), self.cwd.clone()).with_context(self.context.clone());
        session.effort = self.effort;
        session.mode = self.mode;
        session.max_steps = self.max_steps;
        session.compaction = self.compaction;
        session.set_extra_dirs(self.extra_dirs.clone());
        let plan_path = session.plan_path();
        let work_dir = session.work_dir();
        self.session_since = session.created_at;
        self.session_id = session.id;
        self.session = Some(session);
        self.usage = None;
        self.spent = Spent::default();
        self.plan_for_session(plan_path);
        self.work_for_session(work_dir);
        self.left_session();
    }

    /// Shows `tab`. The diagnostics tab looks again at what applies every
    /// time it opens, since servers and formatters may have been installed
    /// since.
    pub(super) fn open_content(&mut self, tab: Tab) {
        if self.content.open(tab) {
            match tab {
                Tab::Diagnostics => self.diagnose(),
                Tab::Usage => self.load_tools(),
                _ => {}
            }
        }
    }
}
