//! The app: its state, the loop that drives it, and the layout of the
//! three bands: the content panel under its tab header, the input panel
//! and the status bar. Row heights never depend on content, so nothing
//! shifts while a turn runs.

mod agent_picker;
mod completion;
mod connect;
mod content;
mod context;
mod curios;
mod diagnostics;
mod dirs;
mod editor;
mod file;
mod files;
mod finished;
mod flash;
mod git;
mod hint;
mod history;
mod input;
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
mod search;
mod session_picker;
mod settings;
mod subagent;
#[cfg(test)]
pub(crate) mod tests;
mod turn;
mod usage;
mod wakeup;
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
use crossterm::event::{Event as TermEvent, KeyEventKind};
use diagnostics::lsp_changed;
use finished::Finished;
use futures::StreamExt;
use input::Input;
use monitor::due;
use nth_config::{Selection, Target};
use nth_context::{Context as ProjectContext, Paths};
use nth_lsp::{ProcessUsage, ServerStatus};
use nth_notify::Notifier;
use nth_protocol::{
    Ask, BoxError, ContextSnapshot, Event, Listing, Llm, Mode, MonitorEvent, Monitors, Provider,
    Show, Tool, Usage, tool_names,
};
use nth_session::{
    Inbox, Ledger, Session, Store, Subagents, Summary, TurnEnv, store,
    subagent::{SubagentEvent, SubagentId},
};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style},
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinError,
    time::MissedTickBehavior,
};
use turn::{Ended, Queued, TurnQueue};

use crate::{
    Checkers, PreparedRuntime, Reload,
    chat::Chat,
    command::Command,
    diagnostics::DiagnosticsTab,
    file::FileRead,
    git::GitBar,
    header, hero,
    history::History,
    job::Job,
    keymaps::Keymaps,
    llm_picker::usage::LlmUsage,
    prompt::{self, Prompt},
    settings::ChatSettings,
    spinner, status,
    subagent::SubagentView,
    terminal::Screen,
    usage::UsageTab,
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
    /// What the next turn runs as, the mode or an agent in its place, and
    /// what it, its subagents and its clerk run on, with the profiles
    /// ctrl+p cycles.
    pub selection: Selection,
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
    /// The config the runtime in use was prepared from; `None` until one
    /// is installed. What a resumed runtime's config is compared with.
    config: Option<nth_config::Config>,
    /// The terminal width from which the content panel splits, chat on the
    /// left; 0 never splits. `[layout] split_width`.
    split_width: u16,
    /// Whether the last draw was that wide.
    wide: bool,
    /// Saved like the history, one write at a time.
    llm_usage_saving: Job<std::io::Result<()>>,
    /// What waits for the running turn, or for the save between turns, and
    /// whether notices may wake the model.
    pub(crate) turns: TurnQueue,
    /// How the chat's last turn ended, for its tab: done, failed, or idle
    /// when you stopped it or there was none.
    last_turn: TabState,
    /// The content panel's tabs, and which one fills it.
    content: Content,
    diagnostics: DiagnosticsTab,
    context_tab: context::ContextTab,
    usage_tab: UsageTab,
    keymaps: Keymaps,
    /// What checks the tools' writes, for the diagnostics tab to look up;
    /// `None` when nothing does.
    pub(crate) checkers: Option<Checkers>,
    /// What fills the input panel.
    input: Input,
    /// The row Enter chose in search, waiting for the source to be shown
    /// with its final width.
    search_jump: Option<search::Jump>,
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
    curios: crate::curios::CuriosTab,
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
    /// The working tree's git state and the branch's pull request, for
    /// the status bar.
    pub git: GitBar,
    /// The language servers the tools started, as last reported.
    pub servers: Vec<ServerStatus>,
    /// Where `servers` comes from; `None` once its sender is gone.
    lsp: Option<watch::Receiver<Vec<ServerStatus>>>,
    lsp_usage: Option<watch::Receiver<Vec<ProcessUsage>>>,
    /// What the last model reply used; `None` until the first one.
    pub usage: Option<Usage>,
    /// What the session had spent when its turn's task took it, drawn
    /// while the task holds it; while the session is here, its own ledger
    /// is (see `App::spent`).
    ledger: Ledger,
    /// The ledger with what the running turn, or compaction, has spent so
    /// far on top, as its last `Event::Spend` said; `None` between turns.
    running: Option<Ledger>,
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
    /// What `/resume` prepares another session's runtime with.
    reload: Option<Reload>,
    /// The session's instruction files and skills; kept here too, since
    /// the session is in the turn task while one runs and `/` still has to
    /// list the skills.
    context: Arc<ProjectContext>,
    /// The configured step limit, tool ceiling and `[context]` policy,
    /// which every session the app moves on to takes as its env: a session
    /// never saves them, so a loaded one would otherwise fall back to the
    /// defaults. `adopt_llm` fills in the model's share before each turn.
    env: TurnEnv,
    session_listing: Job<Result<Vec<Summary>, store::Error>>,
    /// The session chosen in the session picker, being read.
    session_loading: Job<anyhow::Result<session_picker::Loaded>>,
    /// A save between turns, after a change no turn would save, such as
    /// `/add-dir`; one asked for mid-save runs after it.
    session_saving: Job<Result<(), store::Error>>,
    /// Held here between turns; moved into the turn task while one runs.
    session: Option<Session>,
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    /// Runs the commands typed after `!`.
    shell: Arc<dyn Tool>,
    channels: TurnChannels,
    /// Questions waiting behind the ones in the input panel.
    asks: VecDeque<Ask>,
    turn: Job<Ended>,
    /// The commands the model left running, shared with every turn's tools.
    monitors: Monitors,
    wakeup: nth_protocol::Wakeup,
    wakeup_rx: watch::Receiver<Option<tokio::time::Instant>>,
    /// What the model has not heard yet from the monitors and subagents,
    /// and the prompts on their way into a running turn.
    pub(crate) inbox: Inbox,
    monitor_rx: mpsc::Receiver<MonitorEvent>,
    /// Each monitor's tab, and which monitors still speak to the model.
    monitor_tabs: monitor::MonitorTabs,
    /// The subagents the task tool started, shared with it.
    subagents: Subagents,
    subagent_rx: mpsc::Receiver<SubagentEvent>,
    /// Each subagent's tab, open from its start until you close it.
    subagent_views: BTreeMap<SubagentId, SubagentView>,
    file_tabs: file::FileTabs,
    plan: plan::PlanTab,
    work: work::WorkTab,
    /// A word on what the last key did or did not do, on the status bar
    /// until the next key or for a few seconds.
    pub hint: Option<hint::Hint>,
    /// Where the last draw put the header, so a click knows it landed there.
    areas: mouse::Areas,
    /// How a right click's text reaches the clipboard; tests put it nowhere.
    clipboard: fn(&str) -> std::io::Result<()>,
    /// How a link reaches the desktop's opener; tests put it nowhere.
    opener: fn(&str) -> std::io::Result<()>,
    /// Where ctrl+g writes the copy your editor opens.
    editor_dir: PathBuf,
    /// ctrl+g asked for the editor, on this target; the loop opens it
    /// after this step.
    pending_editor: Option<editor::Target>,
    /// The plan copy your editor has open; the app draws nothing while it
    /// runs.
    editing: Option<editor::Editing>,
    editor: Job<std::io::Result<std::process::ExitStatus>>,
    notifier: Notifier,
    notify_errors: watch::Receiver<Option<String>>,
    /// Whether the terminal has focus, as it last reported; nth starts
    /// focused because you just started it. Notifications go out only
    /// without it.
    focused: bool,
    /// ctrl+c was pressed once with a turn, monitors or subagents
    /// running; again quits.
    quit_armed: bool,
    quit: bool,
    /// nth is on its way out: a turn that ends now only hands the session
    /// back to be saved, and starts nothing that would outlive the exit.
    quitting: bool,
    /// What the exit could not do, said on stderr once the terminal is
    /// back, as the chat is no longer drawn by then.
    pub(crate) exit_notes: Vec<String>,
}

/// Everything the chat starts with: the runtime, installed as `/resume`
/// installs one, and what outlives a `/resume`: the process's subagents,
/// where sessions are saved, the account `/connect` signs in to, how
/// `/resume` prepares another runtime, what earlier runs recalled, and how
/// text reaches the clipboard and links the desktop.
pub struct AppConfig {
    pub runtime: PreparedRuntime,
    /// The subagents the task tool starts, which report over `subagent_rx`.
    pub subagents: Subagents,
    pub subagent_rx: mpsc::Receiver<SubagentEvent>,
    /// Where sessions are saved after every turn and monitors log; `None`
    /// keeps sessions in memory only.
    pub store: Option<Store>,
    /// The ChatGPT login `/connect` signs in through; `None` leaves
    /// nothing to connect.
    pub chatgpt: Option<nth_auth::ChatGpt>,
    /// What `/resume` prepares another session's runtime with; `None`
    /// keeps the current runtime for a resumed session.
    pub reload: Option<Reload>,
    pub history: History,
    pub llm_usage: LlmUsage,
    /// How a right click's text reaches the clipboard.
    pub clipboard: fn(&str) -> std::io::Result<()>,
    /// How a link reaches the desktop's opener.
    pub opener: fn(&str) -> std::io::Result<()>,
    /// Where ctrl+g writes the copy your editor opens.
    pub editor_dir: PathBuf,
}

impl AppConfig {
    /// `runtime` with the process's subagents, and nothing saved, recalled,
    /// connected or resumed; text and links go to the desktop.
    pub fn new(
        runtime: PreparedRuntime,
        subagents: Subagents,
        subagent_rx: mpsc::Receiver<SubagentEvent>,
    ) -> Self {
        Self {
            runtime,
            subagents,
            subagent_rx,
            store: None,
            chatgpt: None,
            reload: None,
            history: History::default(),
            llm_usage: LlmUsage::default(),
            clipboard: crate::terminal::copy,
            opener: nth_auth::open_browser,
            editor_dir: std::env::temp_dir().join("nth"),
        }
    }
}

/// One channel whose two ends the app keeps: every turn gets a clone of the
/// sender, and the loop reads the one receiver.
struct Channel<T> {
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<T>,
}

impl<T> Channel<T> {
    fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self { tx, rx }
    }
}

/// Where a running turn reaches the app: the session's events, the tools'
/// questions and the tabs they show.
struct TurnChannels {
    events: Channel<Event>,
    asks: Channel<Ask>,
    screen: Channel<Show>,
}

impl TurnChannels {
    fn new() -> Self {
        Self {
            events: Channel::new(256),
            asks: Channel::new(4),
            screen: Channel::new(4),
        }
    }
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
async fn next_input<I>(input: &mut Option<I>) -> Option<std::io::Result<TermEvent>>
where
    I: futures::Stream<Item = std::io::Result<TermEvent>> + Unpin,
{
    match input {
        Some(input) => input.next().await,
        None => std::future::pending().await,
    }
}

enum Step {
    Curio(crate::curios::CurioStep),
    Terminal(Option<std::io::Result<TermEvent>>),
    Session(Event),
    Asked(Ask),
    Show(Show),
    FileRead(FileRead),
    Monitor(MonitorEvent),
    Subagent(SubagentEvent),
    NoticesDue,
    WakeupChanged,
    WakeupDue,
    HintExpired,
    TurnEnded(Box<Result<Ended, JoinError>>),
    /// The running turn published a context preview.
    Preview(ContextSnapshot),
    Job(Finished),
    LoginUrl(String),
    EditorClosed(Result<std::io::Result<std::process::ExitStatus>, JoinError>),
    /// Whether the servers' states changed; `false` when the sender is gone.
    LspChanged(bool),
    LspUsageChanged(bool),
    /// The notification backend failed for the first time.
    NotifyFailed,
    /// The one-second tick: a stale context tab.
    Second,
    Tick,
}

impl App {
    /// The chat on `config`'s runtime, installed as `/resume` installs one,
    /// so the two cannot set up a runtime differently.
    pub fn new(config: AppConfig) -> Self {
        let AppConfig {
            runtime,
            subagents,
            subagent_rx,
            store,
            chatgpt,
            reload,
            history,
            llm_usage,
            clipboard,
            opener,
            editor_dir,
        } = config;
        let session = &runtime.session;
        let (monitor_tx, monitor_rx) = mpsc::channel(256);
        // Each session's monitors log beside its save; a chat that saves
        // nothing, as in tests, logs in a temporary folder rather than the
        // working directory.
        let log_dir = store.as_ref().map_or_else(
            || std::env::temp_dir().join("nth-monitors"),
            |store| store.monitor_dir(session.id),
        );
        let monitors = Monitors::new(monitor_tx, log_dir);
        let wakeup = nth_protocol::Wakeup::new();
        let wakeup_rx = wakeup.subscribe().expect("enabled wakeup");
        let home = std::env::var("HOME").ok();
        // `install` below takes the runtime's config, which turns it on.
        let notifier = Notifier::off();
        let plan_path = session.plan_path();
        let work_dir = session.work_dir();
        let mut chat = Chat::replay(session.cwd.clone(), &session.messages);
        chat.warn(&session.context().warnings);
        let mut app = Self {
            chat,
            prompt: Prompt::default(),
            // `install` below puts the runtime's in, on what the session
            // runs as and on.
            selection: runtime.selection.clone(),
            history,
            history_saving: Job::default(),
            llm_usage,
            llm_usage_saving: Job::default(),
            settings: ChatSettings::default(),
            config: None,
            split_width: 0,
            wide: false,
            turns: TurnQueue::default(),
            last_turn: TabState::Idle,
            content: Content::default(),
            diagnostics: DiagnosticsTab::default(),
            usage_tab: UsageTab::default(),
            context_tab: context::ContextTab::default(),
            keymaps: Keymaps::default(),
            checkers: None,
            input: Input::Prompt,
            search_jump: None,
            cwd: session.cwd.clone(),
            place: status::place(&session.cwd, home.as_deref()),
            home,
            extra_dirs: session.extra_dirs.clone(),
            busy_since: None,
            session_since: session.created_at,
            session_id: session.id,
            curios: crate::curios::CuriosTab::new(session.state_root()),
            hero_since: Instant::now(),
            flash: None,
            pointer: None,
            completion: None,
            files: Vec::new(),
            indexing: Job::default(),
            git: GitBar::default(),
            servers: Vec::new(),
            lsp: None,
            lsp_usage: None,
            usage: None,
            ledger: Ledger::default(),
            running: None,
            llms: None,
            llm_listing: Job::default(),
            chatgpt,
            login: Job::default(),
            connect_url: None,
            store: store.map(Arc::new),
            paths: Paths::default(),
            reload,
            context: session.context().clone(),
            env: configured(&session.env),
            session_listing: Job::default(),
            session_loading: Job::default(),
            session_saving: Job::default(),
            session: None,
            provider: runtime.provider.clone(),
            // `install` moves the runtime's list in.
            tools: Vec::new(),
            shell: runtime.shell.clone(),
            channels: TurnChannels::new(),
            asks: VecDeque::new(),
            turn: Job::default(),
            monitors,
            wakeup,
            wakeup_rx,
            inbox: runtime.inbox.clone(),
            monitor_rx,
            monitor_tabs: monitor::MonitorTabs::default(),
            subagents,
            subagent_rx,
            subagent_views: BTreeMap::new(),
            file_tabs: file::FileTabs::new(),
            plan: plan::PlanTab::new(plan_path),
            work: work::WorkTab::new(work_dir),
            hint: None,
            areas: mouse::Areas::default(),
            clipboard,
            opener,
            editor_dir,
            pending_editor: None,
            editing: None,
            editor: Job::default(),
            notify_errors: notifier.errors(),
            notifier,
            focused: true,
            quit_armed: false,
            quit: false,
            quitting: false,
            exit_notes: Vec::new(),
        };
        let (session, warnings) = app.install(runtime);
        app.session = Some(session);
        app.chat.warn(&warnings);
        app
    }

    /// Takes on `runtime`, all but its session and warnings, which it
    /// hands back for the caller to place: `new` builds the app around the
    /// session, `/resume` switches to it. The llms are installed as they
    /// are: the session they came with runs on its mode's, or its agent's,
    /// already.
    fn install(&mut self, runtime: PreparedRuntime) -> (Session, Vec<String>) {
        let PreparedRuntime {
            session,
            inbox,
            paths,
            provider,
            model_tools,
            shell,
            checkers,
            selection,
            warnings,
            config,
        } = runtime;
        self.apply_config(config);
        self.paths = paths;
        self.provider = provider;
        // Whatever the old provider listed is no guide to the new one's.
        self.llms = None;
        self.llm_listing = Default::default();
        self.tools = model_tools;
        // The one those tools post to, so their answers wake this chat.
        self.inbox = inbox;
        self.shell = shell;
        // Whatever prepared it, it runs as and on what the session does.
        self.selection = mode::of_session(selection, &session);
        let mut lsp = checkers.lsp.status();
        self.servers = lsp.borrow_and_update().clone();
        self.lsp = Some(lsp);
        self.lsp_usage = None;
        // The servers of the runtime being left get to leave on their own.
        if let Some(old) = self.checkers.replace(checkers) {
            tokio::spawn(async move { old.lsp.shutdown().await });
        }
        // What the last look found applies to the runtime being left.
        self.diagnostics = Default::default();
        (session, warnings)
    }

    /// Takes on `config`'s `[chat]`, `[layout]` and `[notify]`. A section
    /// equal to the last config's is left alone, so the `/settings` flips of
    /// this run outlive a `/resume` whose config says the same, and the
    /// notifier is not built again for nothing; one that differs starts
    /// over from the new config. These sections are global only, so on a
    /// `/resume` they differ when the global config was edited since start.
    fn apply_config(&mut self, config: nth_config::Config) {
        let old = self.config.as_ref();
        if old.is_none_or(|old| old.chat != config.chat) {
            self.settings = ChatSettings::from(&config.chat);
        }
        if old.is_none_or(|old| old.notify != config.notify) {
            self.set_notifier(config.notify.notifier());
        }
        self.split_width = config.layout.split_width;
        self.config = Some(config);
    }

    /// Runs the chat on `screen`, reading `input`, until you quit or the
    /// input ends; what runs in the background is stopped on the way out.
    pub async fn run<S: Screen>(&mut self, screen: &mut S, input: S::Input) -> Result<()> {
        // `None` while your editor has the terminal.
        let mut input = Some(input);
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.index_files();
        self.load_git();
        self.load_pr();
        self.list_llms();
        self.plan.read();
        self.work.read();
        self.poll_curios();
        let mut second = tokio::time::interval(Duration::from_secs(1));

        while !self.quit {
            let showing_diagnostics = self.panes().shows(Tab::Diagnostics);
            if showing_diagnostics && self.lsp_usage.is_none() {
                self.lsp_usage = self.checkers.as_ref().map(|c| c.lsp.usage());
            } else if !showing_diagnostics {
                self.lsp_usage = None;
            }
            if self.pending_editor.is_some() {
                self.open_editor(screen, &mut input).await;
            }
            if !self.is_editing() {
                screen.draw(self)?;
            }
            let ticking = self.is_busy()
                || self.running_monitors() > 0
                || self.running_subagents() > 0
                || self.showing_hero()
                || self.flash.as_ref().is_some_and(flash::Flash::running)
                || self.hint.as_ref().is_some_and(hint::Hint::moving);
            let wakeup_deadline = *self.wakeup_rx.borrow_and_update();
            let step = tokio::select! {
                event = next_input(&mut input) => Step::Terminal(event),
                step = self.curios.next() => Step::Curio(step),
                ended = self.editor.join() => Step::EditorClosed(ended),
                Some(event) = self.channels.events.rx.recv() => Step::Session(event),
                Some(ask) = self.channels.asks.rx.recv() => Step::Asked(ask),
                Ok(()) = self.notify_errors.changed() => Step::NotifyFailed,
                Some(show) = self.channels.screen.rx.recv() => Step::Show(show),
                Some(read) = self.file_tabs.next() => Step::FileRead(read),
                Some(event) = self.monitor_rx.recv() => Step::Monitor(event),
                Some(event) = self.subagent_rx.recv() => Step::Subagent(event),
                _ = due(self.turns.due()) => Step::NoticesDue,
                _ = self.wakeup_rx.changed() => Step::WakeupChanged,
                _ = due(wakeup_deadline) => Step::WakeupDue,
                _ = due(self.hint.as_ref().map(hint::Hint::expires)) => Step::HintExpired,
                // The status bar's session time changes on the minute.
                _ = due(Some(next_minute())) => Step::Tick,
                ended = self.turn.join() => Step::TurnEnded(Box::new(ended)),
                snapshot = self.context_tab.next() => Step::Preview(snapshot),
                files = self.indexing.join() => Step::Job(Finished::Indexed(files)),
                loaded = self.git.next() => Step::Job(Finished::Git(loaded)),
                llms = self.llm_listing.join() => Step::Job(Finished::LlmsListed(llms)),
                Some(url) = login_url(&mut self.connect_url) => Step::LoginUrl(url),
                account = self.login.join() => Step::Job(Finished::Connected(account)),
                sessions = self.session_listing.join() => Step::Job(Finished::SessionsListed(sessions)),
                session = self.session_loading.join() => Step::Job(Finished::SessionLoaded(session)),
                saved = self.history_saving.join() => Step::Job(Finished::HistorySaved(saved)),
                saved = self.session_saving.join() => Step::Job(Finished::SessionSaved(saved)),
                saved = self.llm_usage_saving.join() => Step::Job(Finished::LlmUsageSaved(saved)),
                plan = self.plan.next() => Step::Job(Finished::PlanRead(plan)),
                plan = self.work.next() => Step::Job(Finished::WorkRead(plan)),
                found = self.diagnostics.next() => Step::Job(Finished::Diagnosed(found)),
                tools = self.usage_tab.next() => Step::Job(Finished::ToolsFound(tools)),
                changed = lsp_changed(&mut self.lsp) => Step::LspChanged(changed),
                changed = diagnostics::usage_changed(&mut self.lsp_usage) => Step::LspUsageChanged(changed),
                _ = tick.tick(), if ticking => Step::Tick,
                _ = second.tick() => Step::Second,
            };
            match step {
                Step::Curio(step) => self.curio_step(step),
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
                Step::WakeupChanged => {}
                Step::WakeupDue => self.wakeup_due(),
                Step::HintExpired => self.hint_expired(),
                Step::TurnEnded(ended) => match *ended {
                    Ok(ended) => self.end_turn(ended),
                    Err(e) => self.turn_task_failed(e),
                },
                Step::Preview(snapshot) => self.context_tab.show(snapshot),
                Step::Job(finished) => self.job_finished(finished),
                Step::LoginUrl(url) => self.login_url(url),
                Step::EditorClosed(ended) => self.editor_closed(screen, &mut input, ended).await?,
                Step::LspChanged(changed) => self.servers_changed(changed),
                Step::LspUsageChanged(changed) => {
                    if changed {
                        if let Some(usage) = &mut self.lsp_usage {
                            usage.borrow_and_update();
                        }
                    } else {
                        self.lsp_usage = None;
                    }
                }
                Step::Second => self.every_second(),
                // Nothing changed but time: the redraw advances the timers.
                Step::Tick => {}
            }
        }
        // Stopping the turn may take seconds, which the last frame, still
        // asking for another ctrl+c, would sit through frozen.
        if self.is_busy() && !self.is_editing() {
            self.hint = Some(hint::Hint::at_once("stopping the turn…"));
            screen.draw(self)?;
        }
        self.stop_background_for_exit().await;
        Ok(())
    }

    /// What follows from the step just applied and must hold before the
    /// next draw, which only draws: the tabs of subagents that answered
    /// close once neither pane shows them.
    pub(super) fn settle(&mut self) {
        self.close_answered_subagents();
    }

    /// The work of the loop's one-second tick, the curios tab keeping a
    /// tick of its own. The context tab is rebuilt only when something
    /// marked it stale or it came into view, since each rebuild projects
    /// the whole history.
    fn every_second(&mut self) {
        self.refresh_stale_context();
    }

    /// One frame after a step: what the step leads to is settled first,
    /// then drawn, so a draw only draws.
    pub(crate) fn frame<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        self.settle();
        terminal.draw(|frame| self.draw(frame))?;
        Ok(())
    }

    /// The model shows you a tab; one it switches to comes in after a blank, so the
    /// switch does not go unnoticed.
    fn show(&mut self, show: Show) {
        let before = self.content.active();
        match show {
            Show::Panel(panel) => self.open_content(panel.into()),
            Show::File(path) => self.show_file(path),
            Show::Curio {
                session_id,
                curio_id,
                revision,
            } => self.open_curio(session_id, curio_id, revision),
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
        let status_rows = status::ROWS.min(area.height);
        let remaining = area.height - status_rows;
        let header_rows = header::ROWS.min(remaining);
        let remaining = remaining - header_rows;
        let requested = self.input.rows(frame.area());
        let spacing = if remaining >= requested.saturating_add(3) {
            3
        } else {
            0
        };
        let input_rows = requested.min(remaining - spacing);
        let [header, content, input, status] = Layout::vertical([
            Constraint::Length(header_rows),
            Constraint::Min(0),
            Constraint::Length(input_rows),
            Constraint::Length(status_rows),
        ])
        .spacing(if spacing == 3 { 1 } else { 0 })
        .areas(area);

        self.areas = mouse::Areas::default();
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
        self.apply_search_jump(&panes);
        for &(tab, area) in &panes {
            self.draw_tab(frame, tab, area);
        }
        self.refresh_search(&panes);
        let focused = panes
            .iter()
            .find(|(tab, _)| *tab == self.content.active())
            .map_or(content, |&(_, area)| area);
        self.areas.panes = panes;
        let tabs = self.tab_labels();
        let split = self.areas.header_split();
        header::draw(frame, header, &tabs, split);
        for (index, area) in header::tab_areas(&tabs, header, split)
            .into_iter()
            .enumerate()
        {
            self.areas.target(mouse::Target::Tab(index), area);
        }
        if let Some(flash) = self.flash.as_ref().filter(|r| r.running()) {
            flash.draw(frame.buffer_mut(), focused);
        }
        // The links' places are kept for the pointer to land on.
        let hits = status::draw(frame, status, self);
        if let Some(area) = hits.place {
            self.areas.target(mouse::Target::Place, area);
        }
        if let Some(area) = hits.branch {
            self.areas.target(mouse::Target::Branch, area);
        }
        if let Some(area) = hits.pr {
            self.areas.target(mouse::Target::PullRequest, area);
        }
        if let Some(area) = hits.curio {
            self.areas.target(mouse::Target::Curio, area);
        }
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
                    self.selection.mode,
                    prompt::Runs {
                        spinner,
                        target,
                        agent: self.selection.agent.as_deref(),
                        llm: &self.selection.current(),
                        profile: self.selection.profile_name(),
                    },
                );
                // Last, so it pops over the content panel; it sits right
                // above the row being typed, lined up with the token it
                // completes.
                if let Some(completion) = &self.completion {
                    let at = prompt::position(input, &self.prompt, completion.start());
                    let popup = completion.draw(frame, area, at);
                    self.areas.cover(popup);
                }
            }
            Input::Search(picker) => picker.draw(frame, input),
            Input::LlmPicker(picker) => crate::llm_picker::draw(frame, input, picker),
            Input::SessionPicker(picker) => crate::session_picker::draw(frame, input, picker),
            Input::AgentPicker(picker) => crate::agent_picker::draw(frame, input, picker),
            Input::Curios(picker) => picker.draw(frame, input, self.session_id),
            Input::Question(panel) => crate::question::draw(frame, input, panel),
            Input::Settings(panel) => crate::settings::draw(frame, input, panel, self.settings),
            Input::Connect(panel) => {
                if let Some(area) = crate::connect::draw(frame, input, panel) {
                    self.areas.target(mouse::Target::LoginUrl, area);
                }
            }
        }
        // Last, over whatever ended up on top.
        self.draw_hover(frame);
    }

    /// `tab`'s view into `area`, with its scrollbar.
    fn draw_tab(&mut self, frame: &mut Frame, tab: Tab, area: Rect) {
        match tab {
            Tab::Context => {
                self.context_tab.view.draw(frame, area);
                if let Some(state) = self.context_tab.view.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Monitor(id) => {
                if let Some(view) = self.monitor_tabs.views.get_mut(&id) {
                    view.draw(frame, area, self.home.as_deref());
                }
            }
            Tab::Subagent(id) => {
                if let Some(view) = self.subagent_views.get_mut(&id) {
                    let queued = self.subagents.queued(id);
                    let chat = view.draw(frame, area, self.settings, queued);
                    if let Some(state) = view.chat.scrollbar() {
                        draw_scrollbar(frame, chat, state);
                    }
                }
            }
            Tab::Plan => {
                let place = self.plan_place();
                self.plan.view.draw(frame, area, &place);
                if let Some(state) = self.plan.view.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Work => {
                let place = self.work_place();
                let live = self.live_work();
                self.work.view.draw(frame, area, &place, &live);
                if let Some(state) = self.work.view.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::File(id) => {
                let place = self.file_place(id);
                if let Some(view) = self.file_tabs.views.get_mut(&id) {
                    view.draw(frame, area, &place);
                    if let Some(state) = view.pager.scrollbar() {
                        draw_scrollbar(frame, area, state);
                    }
                }
            }
            Tab::Keymaps => {
                self.keymaps.draw(frame, area);
                if let Some(state) = self.keymaps.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Diagnostics => {
                let selection = &self.selection;
                let row = |label, llm: &Llm, current, inherits| crate::diagnostics::ModeModel {
                    label,
                    effort: llm.effort.wire(),
                    context_window: self.context_window_of(&llm.model),
                    model: llm.model.clone(),
                    current,
                    inherits,
                };
                let [plan, act] = Mode::ALL.map(|mode| {
                    let current = mode == selection.mode && selection.agent.is_none();
                    row(mode.label(), selection.slots().mode(mode), current, None)
                });
                let subagent = row(
                    "subagent",
                    &selection.llm(Target::Subagent),
                    false,
                    selection.slots().subagent.is_none().then_some("the mode"),
                );
                let clerk = row(
                    "clerk",
                    &selection.llm(Target::Clerk),
                    false,
                    selection.slots().clerk.is_none().then_some("subagent"),
                );
                let usage = self
                    .lsp_usage
                    .as_ref()
                    .map(|rx| rx.borrow().clone())
                    .unwrap_or_default();
                let agent_label = |caller: &nth_lsp::Caller| {
                    if caller.session == self.session_id.to_string() {
                        self.selection
                            .agent
                            .as_ref()
                            .map_or_else(|| "main session".into(), |name| format!("main @{name}"))
                    } else if let Some((id, agent)) =
                        self.subagents.describe_session(&caller.session)
                    {
                        format!("@{agent} #{id}")
                    } else if caller.is_subagent {
                        format!("subagent {}", caller.session)
                    } else {
                        format!("session {}", caller.session)
                    }
                };
                let facts = crate::diagnostics::Facts {
                    models: [plan, act, subagent, clerk],
                    llms: self.llms.as_ref().map(|l| l.models.len()),
                    failed: self.llms.as_ref().map_or(&[], |l| l.failed.as_slice()),
                    listing: self.llm_listing.is_running(),
                    checks: self.checkers.is_some(),
                    rtk: self.checkers.as_ref().is_some_and(|checkers| checkers.rtk),
                    running: &self.servers,
                    lsp_usage: &usage,
                    agent_label: &agent_label,
                    context: &self.context,
                    home: self.home.as_deref(),
                };
                self.diagnostics.draw(frame, area, &facts);
                if let Some(state) = self.diagnostics.view.pager.scrollbar() {
                    draw_scrollbar(frame, area, state);
                }
            }
            Tab::Usage => {
                // The listing alone, so the view can be drawn into.
                let llms = self.llms.as_ref();
                let cost = |model: &str| llms?.cost_of(model);
                self.usage_tab.draw(
                    frame,
                    area,
                    usage::spent(self.session.as_ref(), &self.ledger, self.running.as_ref()),
                    &self.llm_usage.ranked(),
                    &cost,
                );
                if let Some(state) = self.usage_tab.view.pager.scrollbar() {
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
        let before = (
            self.runs_as(),
            self.extra_dirs.clone(),
            self.content.panes(self.wide),
        );
        match event {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            TermEvent::FocusGained => {
                self.focused = true;
                // Instruction files may have been edited while away. Best
                // effort: one edited while the terminal keeps the focus
                // shows once something else marks the tab stale.
                self.context_stale();
            }
            TermEvent::FocusLost => self.focused = false,
            TermEvent::Paste(text) if matches!(self.input, Input::Prompt) => {
                self.prompt.paste(&text);
                self.refresh_completion();
            }
            TermEvent::Paste(text) => {
                if let Input::Search(picker) = &mut self.input {
                    picker.insert_str(&text);
                }
            }
            _ => {}
        }
        let after = (
            self.runs_as(),
            self.extra_dirs.clone(),
            self.content.panes(self.wide),
        );
        if before != after {
            self.refresh_context();
        }
    }

    fn on_session(&mut self, event: Event) {
        match &event {
            // Quitting: the tree and the place are not shown again, and a
            // read started now would be cut off by the exit.
            Event::ToolFinished { .. } | Event::Moved(_) if self.quitting => {}
            // A write or a command may have changed the tree.
            Event::ToolFinished { call, .. }
                if tool_names::writes_files(&call.name) || call.name == "bash" =>
            {
                self.load_git();
                if call.name != "bash" {
                    self.plan.read();
                }
            }
            Event::ToolFinished { call, .. } if tool_names::touches_work(&call.name) => {
                self.work.read()
            }
            Event::Usage(usage) => self.usage = Some(*usage),
            Event::Spend(spend) => self.spend_reported(spend.as_ref().clone()),
            Event::Moved(cwd) => self.follow(cwd.clone()),
            _ => {}
        }
        self.chat.apply(&event);
    }

    fn run_command(&mut self, command: Command) {
        match command {
            // As ctrl+c: asked twice while a turn runs, which quitting
            // stops and saves.
            Command::Exit => self.ask_quit(),
            Command::Keymaps => self.open_content(Tab::Keymaps),
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
            Command::Agents => self.open_agent_picker(),
            Command::Connect => self.open_connect(),
            Command::Context => self.open_content(Tab::Context),
            Command::Resume => self.open_session_picker(),
            Command::Diagnostics => self.open_content(Tab::Diagnostics),
            Command::Usage => self.open_content(Tab::Usage),
            Command::Plan => self.open_content(Tab::Plan),
            Command::Work => self.open_content(Tab::Work),
            Command::Approve => self.approve(String::new()),
            Command::Compact => self.compact(),
            Command::Close => self.close_content(),
            Command::Copy => self.copy_answer(),
            Command::CopyId => self.copy_session_id(),
            Command::Settings => self.open_settings(),
            Command::Memory => self.show_memory(),
            Command::Curios => self.open_curios_picker(),
        }
    }

    /// `/copy`: the latest completed answer of the prompt's target.
    fn copy_answer(&mut self) {
        let (transcript, running) = match self.showing_subagent() {
            Some(id) => match self.subagent_views.get(&id) {
                Some(view) => (&view.chat.transcript, view.is_running()),
                None => {
                    self.hint = Some("nothing to copy".into());
                    return;
                }
            },
            None => (&self.chat.transcript, self.is_busy()),
        };
        let mut skip = running;
        let text = transcript.entries().rev().find_map(|entry| match entry {
            crate::chat::Entry::Compacted(text) if !text.trim().is_empty() => Some(text.clone()),
            crate::chat::Entry::Answer(text) if !skip && !text.trim().is_empty() => {
                Some(text.clone())
            }
            crate::chat::Entry::TurnError(_) | crate::chat::Entry::Interrupted { .. } => {
                skip = true;
                None
            }
            crate::chat::Entry::TurnDone { .. } | crate::chat::Entry::User(_) => {
                skip = false;
                None
            }
            _ => None,
        });
        match text {
            Some(text) => self.copy(text),
            None => self.hint = Some("nothing to copy".into()),
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
        let Llm { model, effort } = self.selection.current();
        let mut session = Session::new(model, self.cwd.clone()).with_context(self.context.clone());
        session.effort = effort;
        session.mode = self.selection.mode;
        session.set_agent(self.selection.agent.clone());
        session.env = self.env.clone();
        session.set_extra_dirs(self.extra_dirs.clone());
        let plan_path = session.plan_path();
        let work_dir = session.work_dir();
        if matches!(self.input, Input::Search(_)) {
            self.input = Input::Prompt;
        }
        self.search_jump = None;
        self.session_since = session.created_at;
        self.session_id = session.id;
        self.session = Some(session);
        self.context_tab.view.update(None);
        self.refresh_context();
        self.usage = None;
        self.running = None;
        self.plan.for_session(plan_path);
        self.work.for_session(work_dir);
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
        if tab == Tab::Context {
            if self.is_busy() {
                self.follow_running_turn();
            }
            self.refresh_context();
        }
    }
}

/// The config's share of `env`, which every session the app moves on to
/// keeps; the model's share is the listing's, which `adopt_llm` hands over.
fn configured(env: &TurnEnv) -> TurnEnv {
    TurnEnv {
        compaction: env.compaction,
        max_steps: env.max_steps,
        tool_timeout: env.tool_timeout,
        ..TurnEnv::default()
    }
}
