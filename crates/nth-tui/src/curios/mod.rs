//! Repository review documents as the TUI follows them: the curios the last
//! poll listed, delivery of their feedback, the browser hosts serving them,
//! and the jobs and one-second tick that keep these current.
//!
//! Every store call spawns git or waits on a lock another process may hold,
//! so none runs on the loop: polls, opening and acknowledgments are jobs on
//! blocking threads, and `CuriosTab::next` hands the loop what they found.
//! The rules they follow, delivery and whose store an opened curio is on,
//! live in `nth_session::curios`; `app/curios.rs` applies the steps to the
//! session, the inbox, the hints and the input panel.

mod picker;

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_curios::{self as core, Curio, Host};
use nth_session::curios::{Delivery, How, Poll, Polled, Read, Stores};
pub(crate) use picker::Picker;
use tokio::{
    task::JoinError,
    time::{Interval, MissedTickBehavior},
};

use crate::job::Job;

/// How often the tab polls the store for curios and feedback.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// A curio the opening job hosts.
pub(crate) struct Opened {
    pub(crate) owner: String,
    pub(crate) id: String,
    /// `None` when the host already serving it was checked and kept.
    pub(crate) host: Option<Host>,
    pub(crate) how: How,
}

/// What one poll found, read on a blocking thread for the loop to apply.
pub(crate) struct Tick {
    /// Where the repository is as the poll found it: still unknown after
    /// a git failure that may pass, so the next poll looks again.
    root: Root,
    /// Handed back, since it remembers what it read before.
    discovery: core::Discovery,
    /// Newest first.
    items: Vec<Curio>,
    /// The curios with feedback queued, as session and curio id.
    queued: BTreeSet<(String, String)>,
    /// `None` when no repository was read.
    read: Option<Read>,
}

/// Where the repository the session's curios live in is, as the loop knows
/// it. Finding it runs git, so it is found by the first poll, never on the
/// loop.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Root {
    /// Not looked for yet: the main checkout of this state root.
    Unknown(PathBuf),
    At(PathBuf),
    /// The session is in no repository.
    Outside,
}

/// Finds the repository when `root` does not say yet, lists the curios
/// there and runs `poll`: plain values, so the job owns them.
fn read(root: Root, mut discovery: core::Discovery, poll: Poll) -> Tick {
    let found = match root {
        Root::Unknown(state_root) => match core::owner_root(&state_root) {
            Ok(root) => Root::At(root),
            Err(core::Error::RepoRequired) => Root::Outside,
            Err(_) => Root::Unknown(state_root),
        },
        known => known,
    };
    let Root::At(root) = found else {
        return Tick {
            root: found,
            discovery,
            items: Vec::new(),
            queued: BTreeSet::new(),
            read: None,
        };
    };
    let mut items = discovery.refresh(&root).unwrap_or_default();
    items.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    let queued = discovery.queued();
    let read = poll.run(&root);
    Tick {
        root: Root::At(root),
        discovery,
        items,
        queued,
        read: Some(read),
    }
}

/// What the tab hands the loop: a job that ended, or a poll that is due.
pub(crate) enum CurioStep {
    Polled(Result<Tick, JoinError>),
    Opened(core::Result<Opened>),
    Acknowledged(Result<core::Result<()>, JoinError>),
    /// The tick found no poll running in a repository that may have one:
    /// the app starts it, since only it knows whether the session is home
    /// to claim delivery.
    PollDue,
}

/// What a poll that landed changed, for the app to tell or act on.
pub(crate) struct Landed {
    /// How many folders the refresh skipped, when that changed to more
    /// than none, so the hint shows once per change rather than every tick.
    pub(crate) skipped: Option<usize>,
    /// `None` when no repository was read.
    pub(crate) read: Option<Read>,
}

/// Why a curio was not opened.
pub(crate) enum NotOpened {
    /// The first poll has not found the repository yet.
    Looking,
    Outside,
}

pub(crate) struct CuriosTab {
    root: Root,
    /// From the last poll, as are `queued` and `skipped`.
    items: Vec<Curio>,
    queued: BTreeSet<(String, String)>,
    /// Away in the poll while one runs.
    discovery: core::Discovery,
    skipped: usize,
    /// Whether the last poll found the store locked and read nothing; the
    /// next tick reads it.
    busy: bool,
    delivery: Delivery,
    /// Whether the session's receipts are saved, so its feedback may be
    /// acknowledged: false after a save that failed, until the next works.
    pub(crate) saved: bool,
    stores: Stores,
    /// How many acknowledging jobs started, so a test sees the back-off.
    #[cfg(test)]
    acknowledgments: usize,
    polling: Job<Tick>,
    acknowledging: Job<core::Result<()>>,
    hosts: BTreeMap<(String, String), Host>,
    opening: Job<core::Result<Opened>>,
    opening_revision: Option<u64>,
    /// Whether the opening job checks a host already serving, so a failure
    /// is the revision's rather than the host's.
    opening_reused: bool,
    last: Option<(String, String)>,
    /// Made on the first `next`, inside the runtime, since an app is also
    /// built outside one by the tests that only draw it.
    tick: Option<Interval>,
}

impl CuriosTab {
    /// The tab of a session whose state lives under `state_root`.
    pub(crate) fn new(state_root: PathBuf) -> Self {
        Self {
            root: Root::Unknown(state_root),
            items: Vec::new(),
            queued: BTreeSet::new(),
            discovery: core::Discovery::default(),
            skipped: 0,
            busy: false,
            delivery: Delivery::new(Instant::now()),
            saved: true,
            stores: Stores::default(),
            #[cfg(test)]
            acknowledgments: 0,
            polling: Default::default(),
            acknowledging: Default::default(),
            hosts: BTreeMap::new(),
            opening: Default::default(),
            opening_revision: None,
            opening_reused: false,
            last: None,
            tick: None,
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.hosts.clear();
        self.opening = Default::default();
        self.opening_revision = None;
        // A poll running holds the discovery and may bring back a lease.
        self.polling = Default::default();
        self.discovery = core::Discovery::default();
        self.delivery.release();
    }

    /// Resolves with the next job to end, or once a second when a poll is
    /// due. A poll still running is left to finish, so a slow one never
    /// stacks; outside a repository the tick is off, since no poll will
    /// run again. Cancel-safe.
    pub(crate) async fn next(&mut self) -> CurioStep {
        let tick = self.tick.get_or_insert_with(|| {
            let mut tick = tokio::time::interval(POLL_EVERY);
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            tick
        });
        let outside = self.root == Root::Outside;
        loop {
            tokio::select! {
                polled = self.polling.join() => return CurioStep::Polled(polled),
                opened = self.opening.join() => {
                    // A task that panicked or was aborted brought no host
                    // back; telling it as a failed opening shows the hint
                    // rather than leaving the click unanswered.
                    return CurioStep::Opened(opened.unwrap_or_else(|_| {
                        Err(core::Error::Invalid("host unavailable".into()))
                    }));
                }
                acknowledged = self.acknowledging.join() => {
                    return CurioStep::Acknowledged(acknowledged);
                }
                _ = tick.tick(), if !outside => {
                    if !self.polling.is_running() {
                        return CurioStep::PollDue;
                    }
                }
            }
        }
    }

    pub(crate) fn has_curios(&self) -> bool {
        !self.items.is_empty()
    }

    /// Starts a poll as `poll` does unless one runs already, so a slow one
    /// never stacks.
    pub(crate) fn poll_if_idle(&mut self, me: String, claim: bool) {
        if !self.polling.is_running() {
            self.poll(me, claim);
        }
    }

    /// Whether a poll was asked for while the last one ran; clears the mark.
    pub(crate) fn take_again(&mut self) -> bool {
        self.polling.take_again()
    }

    /// Starts a poll for session `me`, or while one runs, another once it
    /// is done: something changed since it started. `claim` is false while
    /// a turn has the session away, and with it the root a refused claim
    /// would keep: the claim waits for it.
    pub(crate) fn poll(&mut self, me: String, claim: bool) {
        let root = self.root.clone();
        if matches!(root, Root::Outside) {
            return;
        }
        let mut poll = self.delivery.poll(me, self.stores.clone(), Instant::now());
        if !claim {
            poll = poll.without_claim();
        }
        let discovery = &mut self.discovery;
        self.polling.start_or_queue(|_| {
            let discovery = std::mem::take(discovery);
            tokio::task::spawn_blocking(move || read(root, discovery, poll))
        });
    }

    /// Takes in what a poll listed, handing back what it read of the
    /// session's feedback.
    pub(crate) fn land(&mut self, tick: Tick) -> Landed {
        let Tick {
            root,
            discovery,
            items,
            queued,
            read,
        } = tick;
        self.root = root;
        let skipped = discovery.skipped.len();
        self.discovery = discovery;
        self.items = items;
        self.queued = queued;
        let changed = skipped != self.skipped;
        self.skipped = skipped;
        Landed {
            skipped: (changed && skipped > 0).then_some(skipped),
            read,
        }
    }

    /// The curios with feedback queued, as the last poll found them.
    pub(crate) fn queued(&self) -> &BTreeSet<(String, String)> {
        &self.queued
    }

    /// Applies what a poll read of the session's feedback to delivery;
    /// `recorded` are the batches the session already holds receipts for.
    pub(crate) fn deliver(&mut self, read: Read, recorded: &BTreeSet<String>) -> Polled {
        let polled = self.delivery.apply(read, recorded, Instant::now());
        self.busy = polled.busy;
        polled
    }

    /// Where the store this process delivers from is, when it holds the
    /// lease.
    pub(crate) fn delivering_root(&self) -> Option<PathBuf> {
        self.delivery
            .store()
            .map(|store| store.repo_root().to_path_buf())
    }

    /// Whether an acknowledgment may start now: none runs and the back-off
    /// after a failed one is over.
    pub(crate) fn acknowledge_due(&self) -> bool {
        !self.acknowledging.is_running() && self.delivery.acknowledge_due(Instant::now())
    }

    pub(crate) fn acknowledge(
        &mut self,
        acknowledge: impl Future<Output = core::Result<()>> + Send + 'static,
    ) {
        self.acknowledging.start(|_| tokio::spawn(acknowledge));
        #[cfg(test)]
        {
            self.acknowledgments += 1;
        }
    }

    /// Records how an acknowledgment went; the failure to tell, once each.
    pub(crate) fn acknowledged(
        &mut self,
        acknowledged: Result<core::Result<()>, JoinError>,
    ) -> Option<String> {
        let result = match acknowledged {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        self.delivery.acknowledged(result, Instant::now())
    }

    /// Asks the next poll to try for the delivery lease, retry time or not.
    pub(crate) fn claim_now(&mut self) {
        self.delivery.claim_now(Instant::now());
    }

    /// A picker over the last poll's curios; one running marks it again
    /// when it lands.
    pub(crate) fn picker(&self) -> Picker {
        Picker::new(self.items.clone(), &self.queued)
    }

    /// The curio opened last while the poll still lists it, else the
    /// newest.
    pub(crate) fn latest(&self) -> Option<(String, String)> {
        self.last
            .clone()
            .filter(|(s, c)| self.items.iter().any(|i| &i.session_id == s && &i.id == c))
            .or_else(|| {
                self.items
                    .first()
                    .map(|c| (c.session_id.clone(), c.id.clone()))
            })
    }

    /// The link to `curio` while a host serves it.
    pub(crate) fn link(&self, curio: &Curio) -> Option<String> {
        self.hosts
            .get(&(curio.session_id.clone(), curio.id.clone()))
            .map(Host::url)
    }

    /// Starts hosting a curio for session `me`. One from another session is
    /// adopted first, so the feedback written on it reaches this session;
    /// one whose session is running elsewhere stays there.
    pub(crate) fn open(
        &mut self,
        me: String,
        owner: String,
        id: String,
        revision: Option<u64>,
    ) -> Result<(), NotOpened> {
        let root = match self.root.clone() {
            Root::At(root) => root,
            Root::Unknown(_) => return Err(NotOpened::Looking),
            Root::Outside => return Err(NotOpened::Outside),
        };
        let reused = owner == me && self.hosts.contains_key(&(owner.clone(), id.clone()));
        self.opening_revision = revision;
        self.opening_reused = reused;
        let stores = self.stores.clone();
        self.opening.start(|cancel| {
            tokio::spawn(async move {
                let (store, how, id) = core::offload(move || {
                    let (store, how) = nth_session::curios::resolve(
                        &stores, &root, &me, &owner, &id, revision, &cancel,
                    )?;
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
        Ok(())
    }

    /// Keeps the host an opening brought and hands back the URL to open
    /// with how its store was chosen, or on failure whether it was a host
    /// already serving that was checked.
    pub(crate) fn opened(
        &mut self,
        opened: core::Result<Opened>,
    ) -> Result<(Option<String>, How), bool> {
        let revision = self.opening_revision.take();
        let reused = std::mem::take(&mut self.opening_reused);
        let Opened {
            owner,
            id,
            host,
            how,
        } = opened.map_err(|_| reused)?;
        if let How::Adopted(from) = &how {
            // Its old host serves a store that no longer has it.
            self.hosts.remove(&(from.clone(), id.clone()));
            // Listed under its new owner at once, as the next poll will,
            // so opening the latest finds it meanwhile.
            for curio in self
                .items
                .iter_mut()
                .filter(|c| &c.session_id == from && c.id == id)
            {
                curio.session_id = owner.clone();
                curio.adopted_from = Some(from.clone());
            }
            // Its feedback comes here now, so delivery is needed.
            self.claim_now();
        }
        let key = (owner, id);
        if let Some(host) = host {
            self.hosts.insert(key.clone(), host);
        }
        let url = self
            .hosts
            .get(&key)
            .map(|host| revision.map_or_else(|| host.url(), |n| host.url_for_revision(n)));
        self.last = Some(key);
        Ok((url, how))
    }
}

#[cfg(test)]
impl CuriosTab {
    pub(crate) fn polling(&mut self) -> &mut Job<Tick> {
        &mut self.polling
    }

    pub(crate) fn acknowledging(&mut self) -> &mut Job<core::Result<()>> {
        &mut self.acknowledging
    }

    pub(crate) fn opening(&mut self) -> &mut Job<core::Result<Opened>> {
        &mut self.opening
    }

    pub(crate) fn root(&self) -> &Root {
        &self.root
    }

    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    pub(crate) fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    pub(crate) fn hosts(&self) -> usize {
        self.hosts.len()
    }

    pub(crate) fn last(&self) -> Option<&(String, String)> {
        self.last.as_ref()
    }

    pub(crate) fn acknowledgments(&self) -> usize {
        self.acknowledgments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `next` resolves within `wait` of paused time.
    async fn resolves(tab: &mut CuriosTab, wait: Duration) -> Option<CurioStep> {
        tokio::time::timeout(wait, tab.next()).await.ok()
    }

    #[tokio::test(start_paused = true)]
    async fn the_tick_asks_for_a_poll_only_while_none_runs() {
        let mut tab = CuriosTab::new("/nowhere".into());
        assert!(matches!(
            resolves(&mut tab, Duration::from_millis(10)).await,
            Some(CurioStep::PollDue)
        ));
        tab.polling
            .start(|_| tokio::spawn(std::future::pending::<Tick>()));
        assert!(
            resolves(&mut tab, Duration::from_millis(2500))
                .await
                .is_none(),
            "two ticks pass a poll still running"
        );
        tab.polling = Default::default();
        assert!(matches!(
            resolves(&mut tab, Duration::from_millis(1500)).await,
            Some(CurioStep::PollDue)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn outside_a_repository_the_tick_is_off() {
        let mut tab = CuriosTab::new("/nowhere".into());
        tab.root = Root::Outside;
        assert!(resolves(&mut tab, Duration::from_secs(5)).await.is_none());
    }

    #[tokio::test]
    async fn finished_jobs_come_through_next() {
        let mut tab = CuriosTab::new("/nowhere".into());
        // The first tick is spent, so only the job can end the wait.
        assert!(matches!(tab.next().await, CurioStep::PollDue));
        tab.acknowledging.start(|_| tokio::spawn(async { Ok(()) }));
        assert!(matches!(
            tab.next().await,
            CurioStep::Acknowledged(Ok(Ok(())))
        ));
        tab.opening.start(|_| tokio::spawn(std::future::pending()));
        tab.opening.abort_handle().expect("opening").abort();
        let CurioStep::Opened(Err(core::Error::Invalid(reason))) = tab.next().await else {
            panic!("a dead opening is an error to tell");
        };
        assert_eq!(reason, "host unavailable");
    }
}
