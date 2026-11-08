//! Repository review documents. Capabilities remain inside this front-end.
//!
//! Every store call spawns git or waits on a lock another process may hold,
//! so none runs on the loop: polls, opening and acknowledgments are jobs on
//! blocking threads, and the loop applies what they found.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use nth_protocol::{
    CurioDelivery, Polled,
    nth_curios::{self as core, Batch, Curio, Host},
};
use nth_session::Session;
use ratatui::{
    Frame,
    layout::Rect,
    style::Color,
    text::{Line, Span},
    widgets::Paragraph,
};
use tokio::task::JoinError;

use super::{App, input::Input, keys::Action};
use crate::{
    fuzzy::Filter,
    theme::{dim, panel_row, panel_title},
};

/// How long after a refused claim the delivery lease is tried again.
const CLAIM_RETRY: Duration = Duration::from_secs(10);

/// A curio the opening job hosts.
pub(super) struct Opened {
    pub(super) owner: String,
    pub(super) id: String,
    /// `None` when the host already serving it was checked and kept.
    pub(super) host: Option<Host>,
    pub(super) how: How,
}

/// Whose a curio was when it was opened.
#[derive(Debug, PartialEq)]
pub(super) enum How {
    /// This session's.
    Own,
    /// Another session's, adopted from it.
    Adopted(String),
    /// Another session's that is running, so it stays there.
    Running,
}

/// One store handle per session id for this run: building one spawns git
/// and takes the lock, which a poll every second must not repeat. Locked
/// only on blocking threads, and never while a store is built.
#[derive(Clone, Default)]
struct Stores(Arc<Mutex<BTreeMap<String, core::Store>>>);

impl Stores {
    fn get(&self, root: &Path, session: &str) -> core::Result<core::Store> {
        if let Some(store) = self.0.lock().expect("stores lock poisoned").get(session) {
            return Ok(store.clone());
        }
        let store = core::Store::new(root, session)?;
        self.0
            .lock()
            .expect("stores lock poisoned")
            .insert(session.to_owned(), store.clone());
        Ok(store)
    }
}

/// What one poll found, read on a blocking thread for the loop to apply.
pub(super) struct Tick {
    /// Handed back, since it remembers what it read before.
    discovery: core::Discovery,
    /// Newest first.
    items: Vec<Curio>,
    /// The curios with feedback queued, as session and curio id.
    queued: BTreeSet<(String, String)>,
    claim: Claim,
    feedback: Feedback,
}

/// How a poll's try for the delivery lease went.
enum Claim {
    /// Not this tick: one is held, the retry time has not come, or this
    /// session never published.
    NotTried,
    /// This session's store could not be built.
    Failed,
    /// Another process delivers this session's feedback; the store's root.
    Refused(PathBuf),
    Lease(CurioDelivery),
}

/// What the delivery store's pending batches read as.
enum Feedback {
    /// No lease, so nothing was read.
    Unowned,
    /// Another held the store's lock; the next tick reads it.
    Busy,
    Pending(Vec<Batch>),
    Unreadable,
}

/// What a poll is started with: plain values, so the job owns them.
struct Poll {
    root: PathBuf,
    me: String,
    claim: bool,
    delivering: Option<core::Store>,
    stores: Stores,
}

impl Poll {
    fn run(self, mut discovery: core::Discovery) -> Tick {
        let mut items = discovery.refresh(&self.root).unwrap_or_default();
        items.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        let queued = discovery.queued();
        // Feedback exists only once this session published, so its folder
        // is waited for rather than made.
        let claim = if self.claim && self.root.join(".nth/curios").join(&self.me).is_dir() {
            match self.stores.get(&self.root, &self.me) {
                Ok(store) => {
                    let root = store.repo_root().to_path_buf();
                    CurioDelivery::claim(store).map_or(Claim::Refused(root), Claim::Lease)
                }
                Err(_) => Claim::Failed,
            }
        } else {
            Claim::NotTried
        };
        let delivering = match &claim {
            Claim::Lease(lease) => Some(lease.store().clone()),
            _ => self.delivering,
        };
        let feedback = match delivering.map(|store| store.try_batches(true)) {
            None => Feedback::Unowned,
            Some(Ok(Some(batches))) => Feedback::Pending(batches),
            Some(Ok(None)) => Feedback::Busy,
            Some(Err(_)) => Feedback::Unreadable,
        };
        Tick {
            discovery,
            items,
            queued,
            claim,
            feedback,
        }
    }
}

pub(super) struct Curios {
    root: Option<PathBuf>,
    /// From the last poll, as are `queued` and `skipped`.
    items: Vec<Curio>,
    queued: BTreeSet<(String, String)>,
    /// Away in the poll while one runs.
    discovery: core::Discovery,
    /// How many folders the last refresh skipped, so the hint shows once
    /// per change rather than every tick.
    skipped: usize,
    /// Whether the last poll found the store locked and read nothing; the
    /// next tick reads it.
    busy: bool,
    delivery: Option<CurioDelivery>,
    /// When to try for the delivery lease again: another process that holds
    /// it may exit.
    next_claim: Instant,
    /// Whether the hint said another process owns delivery.
    told_owned: bool,
    /// When a catch-up acknowledgment may run again: one that failed
    /// builds a store each time, and would fail every tick the same way.
    next_acknowledge: Instant,
    /// The last acknowledgment failure the hint showed, so each shows once.
    told_unacknowledged: Option<String>,
    /// Whether the session's receipts are saved, so its feedback may be
    /// acknowledged: false after a save that failed, until the next works.
    pub(super) saved: bool,
    stores: Stores,
    pub(super) polling: super::Job<Tick>,
    pub(super) acknowledging: super::Job<core::Result<()>>,
    hosts: BTreeMap<(String, String), Host>,
    pub(super) opening: super::Job<core::Result<Opened>>,
    opening_revision: Option<u64>,
    /// Whether the opening job checks a host already serving, so a failure
    /// is the revision's rather than the host's.
    opening_reused: bool,
    last: Option<(String, String)>,
}

impl Curios {
    pub fn new(session: &Session) -> Self {
        Self {
            root: session
                .curio_root
                .clone()
                .or_else(|| nth_protocol::Curios::repository_root(&session.state_root()).ok()),
            items: Vec::new(),
            queued: BTreeSet::new(),
            discovery: core::Discovery::default(),
            skipped: 0,
            busy: false,
            delivery: None,
            next_claim: Instant::now(),
            told_owned: false,
            next_acknowledge: Instant::now(),
            told_unacknowledged: None,
            saved: true,
            stores: Stores::default(),
            polling: Default::default(),
            acknowledging: Default::default(),
            hosts: BTreeMap::new(),
            opening: Default::default(),
            opening_revision: None,
            opening_reused: false,
            last: None,
        }
    }
    pub fn invalidate(&mut self) {
        self.hosts.clear();
        self.opening = Default::default();
        self.opening_revision = None;
        // A poll running holds the discovery and may bring back a lease.
        self.polling = Default::default();
        self.discovery = core::Discovery::default();
        self.delivery = None;
    }
}

#[derive(Debug)]
pub(super) struct Picker {
    items: Vec<Curio>,
    pending: Vec<bool>,
    filter: Filter,
    shown: Vec<usize>,
    selected: usize,
}

impl Picker {
    fn refilter(&mut self) {
        let names: Vec<_> = self
            .items
            .iter()
            .map(|c| format!("{} {}", c.title, c.session_id))
            .collect();
        self.shown = self.filter.rank(names.iter().map(String::as_str));
        self.selected = 0;
    }
    /// Marks the curios `queued` names, from the latest poll.
    fn mark(&mut self, queued: &BTreeSet<(String, String)>) {
        self.pending = self
            .items
            .iter()
            .map(|c| queued.contains(&(c.session_id.clone(), c.id.clone())))
            .collect();
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect, owner: uuid::Uuid) {
        let mut lines = vec![
            panel_title("curios · enter open · ctrl+y copy link · esc", Color::Cyan),
            panel_row(
                Color::Cyan,
                [Span::styled(self.filter.query().to_string(), dim())],
            ),
        ];
        if self.items.is_empty() {
            lines.push(Line::from("  no published curios in this repository"));
        }
        let height = usize::from(area.height.saturating_sub(2));
        let first = self.selected.saturating_sub(height.saturating_sub(1));
        for (row, &i) in self.shown.iter().enumerate().skip(first).take(height) {
            let c = &self.items[i];
            let mark = if row == self.selected {
                nth_icons::icons().pick
            } else {
                " "
            };
            let own = if c.session_id == owner.to_string() {
                "this session"
            } else {
                &c.session_id
            };
            lines.push(panel_row(
                Color::Cyan,
                [Span::raw(format!(
                    "{mark} {} · r{} · {}{}",
                    c.title,
                    c.current_revision,
                    own,
                    if self.pending[i] { " · queued" } else { "" }
                ))],
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

impl App {
    pub(crate) fn has_curios(&self) -> bool {
        !self.curios.items.is_empty()
    }

    pub(super) fn reset_curios(&mut self) {
        self.curios.invalidate();
        if let Some(session) = &self.session {
            self.curios = Curios::new(session);
        }
        self.areas.forget(&crate::app::mouse::Target::Curio);
        if matches!(self.input, Input::Curios(_)) {
            self.input = Input::Prompt;
        }
        self.request_curio_poll();
    }

    /// The one-second tick: a poll still running is left to finish, so a
    /// slow one never stacks.
    pub(super) fn poll_curios(&mut self) {
        if !self.curios.polling.is_running() {
            self.request_curio_poll();
        }
    }

    /// Starts a poll, or while one runs, another once it is done: something
    /// changed since it started.
    fn request_curio_poll(&mut self) {
        let Some(root) = self.curios.root.clone() else {
            return;
        };
        let poll = Poll {
            root,
            me: self.session_id.to_string(),
            claim: self.curios.delivery.is_none()
                && Instant::now() >= self.curios.next_claim
                && self.session.is_some(),
            delivering: self.curios.delivery.as_ref().map(|d| d.store().clone()),
            stores: self.curios.stores.clone(),
        };
        let curios = &mut self.curios;
        curios.polling.start_or_queue(|_| {
            let discovery = std::mem::take(&mut curios.discovery);
            tokio::task::spawn_blocking(move || poll.run(discovery))
        });
    }

    pub(super) fn curio_polled(&mut self, tick: Result<Tick, JoinError>) {
        match tick {
            Ok(tick) => self.apply_curio_tick(tick),
            Err(_) => self.hint = Some("could not read Curios".into()),
        }
        if self.curios.polling.take_again() {
            self.request_curio_poll();
        }
    }

    fn apply_curio_tick(&mut self, tick: Tick) {
        let Tick {
            discovery,
            items,
            queued,
            claim,
            feedback,
        } = tick;
        let skipped = discovery.skipped.len();
        self.curios.discovery = discovery;
        self.curios.items = items;
        self.curios.queued = queued;
        if let Input::Curios(picker) = &mut self.input {
            picker.mark(&self.curios.queued);
        }
        if skipped != self.curios.skipped {
            self.curios.skipped = skipped;
            if skipped > 0 {
                self.hint = Some(format!("curios: skipped {skipped} unreadable folders").into());
            }
        }
        if self.curios.items.is_empty() {
            self.areas.forget(&crate::app::mouse::Target::Curio);
        }
        match claim {
            Claim::NotTried => {}
            Claim::Failed => self.curios.next_claim = Instant::now() + CLAIM_RETRY,
            Claim::Refused(root) => {
                self.curios.next_claim = Instant::now() + CLAIM_RETRY;
                if let Some(session) = &mut self.session {
                    session.curio_root = Some(root);
                }
                if !self.curios.told_owned {
                    self.curios.told_owned = true;
                    self.hint = Some("Curios delivery is owned by another process".into());
                }
            }
            Claim::Lease(lease) => {
                self.curios.next_claim = Instant::now() + CLAIM_RETRY;
                self.curios.delivery = Some(lease);
            }
        }
        // Acknowledging reads the root from the session, which a turn that
        // started after the claim had away.
        if let (Some(session), Some(lease)) = (&mut self.session, &self.curios.delivery)
            && session.curio_root.is_none()
        {
            session.curio_root = Some(lease.store().repo_root().to_path_buf());
        }
        self.curios.busy = matches!(feedback, Feedback::Busy);
        match feedback {
            Feedback::Unowned | Feedback::Busy => {}
            Feedback::Unreadable => {
                self.hint = Some("could not read queued Curios feedback".into())
            }
            Feedback::Pending(batches) => self.deliver_curio_feedback(batches),
        }
    }

    fn deliver_curio_feedback(&mut self, batches: Vec<Batch>) {
        // While a turn runs the session is away; its turn hands over what
        // the inbox holds and its receipts come back with it.
        let recorded = self
            .session
            .as_ref()
            .map(|s| s.curio_receipts.clone())
            .unwrap_or_default();
        let polled = Polled::deliver(batches, &self.inbox, &recorded);
        // A batch posted while a turn ran that the turn recorded anyway
        // would otherwise wake the model with nothing.
        if self.session.is_some() {
            for notice in self.inbox.take_curios(&self.session_id.to_string()) {
                if !recorded.contains(&notice.batch.id) {
                    self.inbox.post_curio(notice);
                }
            }
        }
        if polled.posted > 0 {
            self.arm_notices();
        }
        // An acknowledgment that failed after a save is caught up here, so
        // the viewer stops saying the feedback is on its way.
        if polled.unacknowledged
            && self.curios.saved
            && self.store.is_some()
            && !self.curios.acknowledging.is_running()
            && Instant::now() >= self.curios.next_acknowledge
            && let Some(session) = &self.session
        {
            let acknowledge = session.acknowledge_curios();
            self.curios
                .acknowledging
                .start(|_| tokio::spawn(acknowledge));
        }
    }

    pub(super) fn curio_acknowledged(&mut self, acknowledged: Result<core::Result<()>, JoinError>) {
        let error = match acknowledged {
            Ok(Ok(())) => {
                self.curios.told_unacknowledged = None;
                return;
            }
            Ok(Err(error)) => error.to_string(),
            Err(error) => error.to_string(),
        };
        self.curios.next_acknowledge = Instant::now() + CLAIM_RETRY;
        if self.curios.told_unacknowledged.as_ref() != Some(&error) {
            self.hint = Some(format!("curios: could not acknowledge feedback: {error}").into());
            self.curios.told_unacknowledged = Some(error);
        }
    }

    /// Drives polls to the end until one reads the store: a child another
    /// test forks shares the store's lock until it execs. A poll started
    /// before the caller's change is finished first, since it may have read
    /// too early.
    #[cfg(test)]
    pub(super) async fn poll_curios_through(&mut self) {
        self.settle_curio_polls().await;
        for _ in 0..50 {
            self.request_curio_poll();
            self.settle_curio_polls().await;
            if self.curios.acknowledging.is_running() {
                let acknowledged = self.curios.acknowledging.join().await;
                self.curio_acknowledged(acknowledged);
            }
            if !self.curios.busy {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the Curios store stayed locked");
    }

    #[cfg(test)]
    async fn settle_curio_polls(&mut self) {
        while self.curios.polling.is_running() {
            let tick = self.curios.polling.join().await;
            self.curio_polled(tick);
        }
    }

    pub(super) fn open_curios_picker(&mut self) {
        // The picker shows the last poll at once; one running marks it
        // again when it lands.
        self.poll_curios();
        self.completion = None;
        let items = self.curios.items.clone();
        let mut picker = Picker {
            shown: (0..items.len()).collect(),
            items,
            pending: Vec::new(),
            filter: Filter::default(),
            selected: 0,
        };
        picker.mark(&self.curios.queued);
        self.input = Input::Curios(picker);
    }

    pub(super) fn key_curios(&mut self, action: Action) {
        let Input::Curios(picker) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext if !picker.shown.is_empty() => {
                picker.selected = (picker.selected + 1) % picker.shown.len()
            }
            Action::SelectPrev if !picker.shown.is_empty() => {
                picker.selected = (picker.selected + picker.shown.len() - 1) % picker.shown.len()
            }
            Action::Insert(c) => {
                picker.filter.push(c);
                picker.refilter();
            }
            Action::Backspace => {
                picker.filter.pop();
                picker.refilter();
            }
            Action::Submit => {
                if let Some(c) = picker
                    .shown
                    .get(picker.selected)
                    .map(|&i| picker.items[i].clone())
                {
                    self.input = Input::Prompt;
                    self.open_curio(c.session_id, c.id, None);
                }
            }
            Action::CopyCurio => self.copy_curio_url(),
            Action::Interrupt | Action::ClearOrQuit | Action::LlmPicker => {
                self.input = Input::Prompt
            }
            _ => {}
        }
    }

    pub(super) fn open_latest_curio(&mut self) {
        let chosen = self
            .curios
            .last
            .clone()
            .filter(|(s, c)| {
                self.curios
                    .items
                    .iter()
                    .any(|i| &i.session_id == s && &i.id == c)
            })
            .or_else(|| {
                self.curios
                    .items
                    .first()
                    .map(|c| (c.session_id.clone(), c.id.clone()))
            });
        match chosen {
            Some((s, c)) => self.open_curio(s, c, None),
            None => self.hint = Some("no Curio to open".into()),
        }
    }

    /// Opens a curio in the browser. One from another session is adopted
    /// first, so the feedback written on it reaches this session; one whose
    /// session is running elsewhere stays there.
    pub(super) fn open_curio(&mut self, owner: String, id: String, revision: Option<u64>) {
        let me = self.session_id.to_string();
        let Some(root) = self.curios.root.clone() else {
            self.hint = Some("Curios requires a Git repository".into());
            return;
        };
        let reused = owner == me && self.curios.hosts.contains_key(&(owner.clone(), id.clone()));
        self.curios.opening_revision = revision;
        self.curios.opening_reused = reused;
        let stores = self.curios.stores.clone();
        self.curios.opening.start(|cancel| {
            tokio::spawn(async move {
                let (store, how, id) = core::offload(move || {
                    let (store, how) = resolve(&stores, &root, &me, owner, &id, &cancel)?;
                    store.snapshot(&id, revision)?;
                    Ok((store, how, id))
                })
                .await?;
                let owner = store.session_id().to_string();
                let host = match reused {
                    true => None,
                    false => Some(Host::start(store, &id).await?),
                };
                Ok(Opened {
                    owner,
                    id,
                    host,
                    how,
                })
            })
        });
    }

    pub(super) fn curio_opened(&mut self, opened: core::Result<Opened>) {
        let revision = self.curios.opening_revision.take();
        let reused = std::mem::take(&mut self.curios.opening_reused);
        match opened {
            Ok(Opened {
                owner,
                id,
                host,
                how,
            }) => {
                if let How::Adopted(from) = &how {
                    // Its old host serves a store that no longer has it.
                    self.curios.hosts.remove(&(from.clone(), id.clone()));
                    // Listed under its new owner at once, as the poll below
                    // will, so opening the latest finds it meanwhile.
                    for curio in self
                        .curios
                        .items
                        .iter_mut()
                        .filter(|c| &c.session_id == from && c.id == id)
                    {
                        curio.session_id = owner.clone();
                        curio.adopted_from = Some(from.clone());
                    }
                    // Its feedback comes here now, so delivery is needed.
                    self.curios.next_claim = Instant::now();
                    self.request_curio_poll();
                }
                let key = (owner, id);
                if let Some(host) = host {
                    self.curios.hosts.insert(key.clone(), host);
                }
                self.curios.last = Some(key);
                self.present_curio(revision, how);
            }
            Err(_) if reused => self.hint = Some("could not open Curio revision".into()),
            Err(_) => self.hint = Some("could not host Curio".into()),
        }
    }

    fn present_curio(&mut self, revision: Option<u64>, how: How) {
        let Some(key) = &self.curios.last else {
            return;
        };
        let Some(host) = self.curios.hosts.get(key) else {
            return;
        };
        // Opener errors may contain their arguments: never render them here.
        let url = revision.map_or_else(|| host.url(), |n| host.url_for_revision(n));
        if (self.opener)(&url).is_err() {
            self.hint = Some("could not open Curio · /curios then ctrl+y to copy link".into());
            return;
        }
        self.hint = Some(match how {
            How::Own => "opened Curio · /curios then ctrl+y to copy link".into(),
            How::Adopted(from) => format!(
                "adopted Curio from session {} · its feedback comes here",
                &from[..from.len().min(8)]
            )
            .into(),
            How::Running => {
                "opened Curio · its session runs elsewhere and gets its feedback".into()
            }
        });
    }

    fn copy_curio_url(&mut self) {
        let host = match &self.input {
            Input::Curios(p) => p
                .shown
                .get(p.selected)
                .and_then(|&i| p.items.get(i))
                .and_then(|c| self.curios.hosts.get(&(c.session_id.clone(), c.id.clone()))),
            _ => None,
        };
        self.hint = Some(
            match host {
                Some(host) if (self.clipboard)(&host.url()).is_ok() => "copied Curio link",
                Some(_) => "could not copy Curio link",
                None => "open this Curio first to create its link",
            }
            .into(),
        );
    }
}

/// Whose store serves `owner`'s curio `id` for session `me`: its own, this
/// one's after adopting it from an idle session, or a running session's,
/// which keeps it. Builds stores, so it runs on a blocking thread.
fn resolve(
    stores: &Stores,
    root: &Path,
    me: &str,
    owner: String,
    id: &str,
    cancel: &nth_session::CancellationToken,
) -> core::Result<(core::Store, How)> {
    let store = stores.get(root, me)?;
    if owner == me {
        return Ok((store, How::Own));
    }
    // Building the store took a while: a `/clear` or a newer open since
    // must not adopt the curio into the session left behind.
    if cancel.is_cancelled() {
        return Err(core::Error::Invalid("Curio opening cancelled".into()));
    }
    match store.adopt(&owner, id) {
        Ok(_) => Ok((store, How::Adopted(owner))),
        Err(core::Error::Conflict(_)) => Ok((stores.get(root, &owner)?, How::Running)),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::{
        core::{Comment, Publish, Send, Target, TargetKind},
        *,
    };
    use crate::app::tests::{Reply, Scripted, rows_of};

    #[tokio::test]
    async fn polling_makes_no_curios_folder_for_a_session_that_never_published() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let session = Session::new("glm", dir.path().to_path_buf());
        let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.poll_curios_through().await;
        app.poll_curios_through().await;
        assert!(!dir.path().join(".nth/curios").exists());
    }

    #[tokio::test]
    async fn the_tick_starts_no_second_poll_while_one_runs() {
        let (_dir, mut app, _store, _c) = fixture().await;
        app.poll_curios();
        let first = app.curios.polling.abort_handle().expect("a poll runs");
        let token = app.curios.polling.token().expect("a poll runs");
        app.poll_curios();
        assert!(app.curios.polling.is_running());
        let still = app.curios.polling.abort_handle().expect("a poll runs");
        assert_eq!(first.id(), still.id(), "the tick left the running poll");
        assert!(!token.is_cancelled());
        assert!(
            !app.curios.polling.take_again(),
            "a tick does not queue another"
        );
        let tick = app.curios.polling.join().await;
        app.curio_polled(tick);
        assert!(!app.curios.polling.is_running());
    }

    #[tokio::test]
    async fn the_picker_marks_queued_from_the_last_tick() {
        let (_dir, mut app, store, c) = fixture().await;
        send(&store, &c);
        // A poll the picker started has not landed yet, and the picker
        // reads no store of its own, so the feedback is not marked.
        app.open_curios_picker();
        let Input::Curios(p) = &app.input else {
            panic!("picker")
        };
        assert!(!p.pending[0]);
        assert!(app.curios.polling.is_running());
        app.poll_curios_through().await;
        let Input::Curios(p) = &app.input else {
            panic!("picker")
        };
        assert!(p.pending[0], "the next tick marks it");
    }

    async fn fixture() -> (tempfile::TempDir, App, core::Store, Curio) {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let mut session = Session::new("glm", dir.path().to_path_buf());
        let store = session.prepare_curios().unwrap();
        let c = store
            .publish(Publish {
                request_id: "publish".into(),
                curio_id: None,
                expected_revision: None,
                title: Some("Review".into()),
                html: "<h1 id=title>Review</h1>".into(),
            })
            .unwrap()
            .curio;
        let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.opener = |_| Ok(());
        app.poll_curios_through().await;
        (dir, app, store, c)
    }

    fn send(store: &core::Store, c: &Curio) {
        let thread = store
            .comment(
                &c.id,
                Comment {
                    request_id: "comment".into(),
                    revision: 1,
                    target: Target {
                        element_id: None,
                        kind: TargetKind::Document,
                        rect: None,
                        label: "Review".into(),
                        quote: "".into(),
                    },
                    body: "/config @explore revise this".into(),
                },
            )
            .unwrap();
        store
            .send(
                &c.id,
                Send {
                    request_id: "send".into(),
                    message_ids: vec![thread.messages[0].id.clone()],
                },
            )
            .unwrap();
    }

    #[tokio::test]
    async fn durable_send_records_once_without_command_parsing() {
        let (_dir, mut app, store, c) = fixture().await;
        assert!(!app.inbox.wakes());
        send(&store, &c);
        app.poll_curios_through().await;
        assert!(app.notices_due.is_some());
        app.notices_due();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
        let session = app.session.as_ref().unwrap();
        assert_eq!(session.curio_receipts.len(), 1);
        assert!(session.messages.iter().any(|m| matches!(m, nth_protocol::Message::User(t) if t.starts_with("<curio ") && t.contains("/config @explore"))));
        assert_eq!(
            store.batches(true).unwrap().len(),
            1,
            "no save means no acknowledgment"
        );
        app.poll_curios_through().await;
        assert!(!app.inbox.wakes());
    }

    #[tokio::test]
    async fn an_unacknowledged_receipt_is_caught_up_without_waking() {
        let (dir, mut app, store, c) = fixture().await;
        send(&store, &c);
        app.poll_curios_through().await;
        app.notices_due();
        let ended = app.turn.join().await.unwrap();
        // Without a store nothing is saved, so nothing is acknowledged.
        app.end_turn(ended);
        assert_eq!(store.batches(true).unwrap().len(), 1);
        app.store = Some(Arc::new(nth_session::Store::at(
            dir.path().join("sessions"),
        )));
        app.poll_curios_through().await;
        assert!(app.notices_due.is_none());
        assert!(!app.inbox.wakes());
        assert!(store.batches(true).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_acknowledgment_is_told_once_and_waits_to_retry() {
        let (dir, mut app, store, c) = fixture().await;
        send(&store, &c);
        app.poll_curios_through().await;
        app.notices_due();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
        app.store = Some(Arc::new(nth_session::Store::at(
            dir.path().join("sessions"),
        )));
        // A rule exposing the session's state makes a fresh store refuse,
        // while the poll's handle, built before, still reads the batches.
        let ignore = dir.path().join(".nth/curios/.gitignore");
        std::fs::write(
            &ignore,
            format!("*\n!{0}/\n!{0}/state.json\n", app.session_id),
        )
        .unwrap();
        app.poll_curios_through().await;
        assert!(
            app.hint
                .as_deref()
                .unwrap()
                .starts_with("curios: could not acknowledge feedback: ")
        );
        assert_eq!(store.batches(true).unwrap().len(), 1);
        app.hint = None;
        let retry = app.curios.next_acknowledge;
        app.poll_curios_through().await;
        assert_eq!(
            app.curios.next_acknowledge, retry,
            "no retry before CLAIM_RETRY"
        );
        assert!(app.hint.is_none());
        // The same failure again is not told again.
        app.curios.next_acknowledge = Instant::now();
        app.poll_curios_through().await;
        assert!(app.hint.is_none());
        std::fs::write(&ignore, "*\n").unwrap();
        app.curios.next_acknowledge = Instant::now();
        app.poll_curios_through().await;
        assert!(store.batches(true).unwrap().is_empty());
        assert!(app.curios.told_unacknowledged.is_none());
    }

    #[tokio::test]
    async fn delivery_is_claimed_once_another_owner_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let mut session = Session::new("glm", dir.path().to_path_buf());
        session.prepare_curios().unwrap();
        let other = session.curios().claim_delivery().unwrap();
        let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.poll_curios_through().await;
        assert!(app.curios.delivery.is_none());
        assert!(app.hint.as_deref().unwrap().contains("another process"));
        drop(other);
        app.poll_curios_through().await;
        assert!(app.curios.delivery.is_none(), "retries wait a while");
        // A child another test forks shares the lock until it execs.
        for _ in 0..50 {
            app.curios.next_claim = Instant::now();
            app.poll_curios_through().await;
            if app.curios.delivery.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(app.curios.delivery.is_some());
    }

    #[tokio::test]
    async fn feedback_on_an_adopted_curio_reaches_this_session() {
        let (dir, mut app, store, _) = fixture().await;
        let old = core::Store::new(dir.path(), "old-owner").unwrap();
        let c = old
            .publish(Publish {
                request_id: "old".into(),
                curio_id: None,
                expected_revision: None,
                title: Some("Old".into()),
                html: "<h1 id=title>Old</h1>".into(),
            })
            .unwrap()
            .curio;
        app.open_curio(c.session_id.clone(), c.id.clone(), None);
        let opened = app.curios.opening.join().await.unwrap();
        app.curio_opened(opened);
        // The viewer now writes to this session's store.
        send(&store, &c);
        app.poll_curios_through().await;
        assert!(app.notices_due.is_some());
        app.notices_due();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
        assert_eq!(app.session.as_ref().unwrap().curio_receipts.len(), 1);
        assert!(old.batches(true).unwrap().is_empty());
    }

    #[tokio::test]
    async fn held_and_busy_feedback_does_not_preempt() {
        let (_dir, mut app, store, c) = fixture().await;
        app.hold_notices = true;
        send(&store, &c);
        app.poll_curios_through().await;
        app.notices_due();
        assert!(!app.is_busy());
        assert!(app.inbox.wakes());
        app.hold_notices = false;
        let provider = Arc::new(Scripted::replying([Reply::Hang]));
        app.provider = provider.clone();
        app.start_turn("work".into());
        provider.asked.notified().await;
        app.poll_curios_through().await;
        app.notices_due();
        assert!(app.inbox.prompts_waiting().is_none());
        assert!(app.is_busy());
        app.turn.cancel();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
    }

    #[tokio::test]
    async fn requested_revision_is_opened_and_validated_with_new_and_reused_hosts() {
        let (_dir, mut app, store, c) = fixture().await;
        store
            .publish(Publish {
                request_id: "publish-2".into(),
                curio_id: Some(c.id.clone()),
                expected_revision: Some(1),
                title: None,
                html: "<h1 id=title>Updated review</h1>".into(),
            })
            .unwrap();
        app.opener = |_| panic!("invalid revision must not reach the opener");
        app.open_curio(c.session_id.clone(), c.id.clone(), Some(99));
        let opened = app.curios.opening.join().await.unwrap();
        assert!(opened.is_err());
        app.curio_opened(opened);
        assert!(app.curios.hosts.is_empty());

        app.opener = |url| {
            let (public, capability) = url.split_once('#').unwrap();
            assert!(public.ends_with("/?revision=1"));
            assert!(!capability.is_empty());
            // Even an opener that echoes its arguments cannot leak the URL.
            Err(std::io::Error::other(url.to_string()))
        };
        app.open_curio(c.session_id.clone(), c.id.clone(), Some(1));
        let opened = app.curios.opening.join().await.unwrap();
        app.curio_opened(opened);
        assert_eq!(app.curios.hosts.len(), 1);
        assert!(!app.hint.as_deref().unwrap().contains("127.0.0.1"));

        app.open_curio(c.session_id.clone(), c.id.clone(), Some(1));
        let opened = app.curios.opening.join().await.unwrap();
        assert!(
            opened.as_ref().unwrap().host.is_none(),
            "the host is reused"
        );
        app.curio_opened(opened);
        assert_eq!(app.curios.hosts.len(), 1);
        assert!(!app.hint.as_deref().unwrap().contains("127.0.0.1"));
        app.opener = |_| panic!("invalid revision must not reach the reused host opener");
        for revision in [0, 99] {
            app.open_curio(c.session_id.clone(), c.id.clone(), Some(revision));
            let opened = app.curios.opening.join().await.unwrap();
            app.curio_opened(opened);
            assert_eq!(app.hint.as_deref(), Some("could not open Curio revision"));
        }
        app.opener = |url| {
            assert!(!url.contains("?revision="));
            Ok(())
        };
        app.open_curio(c.session_id, c.id, None);
        let opened = app.curios.opening.join().await.unwrap();
        app.curio_opened(opened);
        assert!(app.hint.as_deref().unwrap().starts_with("opened Curio"));
    }

    #[tokio::test]
    async fn status_click_opener_failure_clipping_and_session_invalidation() {
        let (_dir, mut app, _store, _c) = fixture().await;
        app.place = "repo".into();
        let rows = rows_of(&mut app, 100);
        assert!(crate::app::tests::status_row(&rows, 1).contains(nth_icons::icons().curio));
        let rect = app.areas.find(&crate::app::mouse::Target::Curio).unwrap();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        let opened = app.curios.opening.join().await.unwrap();
        app.opener = |url| {
            assert!(url.starts_with("http://127.0.0.1:"));
            assert!(url.contains('#'));
            Err(std::io::Error::other(url.to_string()))
        };
        app.curio_opened(opened);
        assert!(
            app.hint
                .as_deref()
                .unwrap()
                .starts_with("could not open Curio")
        );
        assert!(!app.hint.as_deref().unwrap().contains("127.0.0.1"));
        assert_eq!(app.curios.hosts.len(), 1);
        app.hint = Some("x".repeat(200).into());
        rows_of(&mut app, 20);
        assert!(app.areas.find(&crate::app::mouse::Target::Curio).is_none());
        app.start_fresh_session();
        assert!(app.curios.hosts.is_empty());
        assert!(app.curios.last.is_none());
    }

    #[tokio::test]
    async fn other_owner_is_discovered_but_never_delivers_here() {
        let (_dir, mut app, store, c) = fixture().await;
        app.resume(Session::new("glm", app.cwd.clone()));
        send(&store, &c);
        app.poll_curios_through().await;
        assert!(app.has_curios());
        assert!(!app.inbox.wakes());
        app.open_curios_picker();
        let Input::Curios(p) = &app.input else {
            panic!("picker")
        };
        assert!(p.pending[0]);
        assert_ne!(p.items[0].session_id, app.session_id.to_string());
    }

    #[tokio::test]
    async fn pr_and_curio_hit_areas_are_independent() {
        let (_dir, mut app, _store, _c) = fixture().await;
        app.place = "repo".into();
        app.git = Some(crate::git::GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        });
        app.pr = Some(crate::git::Pr {
            number: 123,
            url: "https://example.test/pull/123".into(),
            state: crate::git::State::Open,
        });
        rows_of(&mut app, 100);
        let pr = app
            .areas
            .find(&crate::app::mouse::Target::PullRequest)
            .unwrap();
        let curio = app.areas.find(&crate::app::mouse::Target::Curio).unwrap();
        assert_eq!(curio.y, pr.y + 1);
        assert!(!pr.intersects(curio));
        // The icon is clickable on its new row.
        assert_eq!(curio.width, 1);
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: curio.x,
            row: curio.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.curios.opening.is_running());
        app.curios.invalidate();
        app.opener = |url| {
            assert_eq!(url, "https://example.test/pull/123");
            Ok(())
        };
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pr.x,
            row: pr.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.curios.opening.is_running());
        assert!(app.hint.as_deref().unwrap().contains("example.test"));
        app.pr_loaded(None);
        assert!(
            app.areas
                .find(&crate::app::mouse::Target::PullRequest)
                .is_none()
        );
    }

    #[tokio::test]
    async fn successful_session_save_acknowledges_receipt() {
        let (dir, mut app, store, c) = fixture().await;
        let saved = nth_session::Store::at(dir.path().join("sessions"));
        app = app.with_store(saved);
        send(&store, &c);
        app.poll_curios_through().await;
        app.notices_due();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
        assert!(store.batches(true).unwrap().is_empty());
        let saved = app
            .store
            .as_ref()
            .unwrap()
            .load(app.session_id)
            .await
            .unwrap();
        assert_eq!(saved.curio_receipts.len(), 1);
        // The store keeps the real path, which a temporary folder may not be.
        assert_eq!(saved.curio_root, Some(dir.path().canonicalize().unwrap()));
    }
}
