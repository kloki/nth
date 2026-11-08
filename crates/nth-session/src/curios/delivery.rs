//! Who delivers a session's review feedback, and what each poll hands the
//! model. One process at a time holds a session's delivery lease, so the
//! viewer's feedback wakes one model; the others wait and try again.
use std::{
    collections::BTreeSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_curios::{Batch, Store};
use nth_directories::Directories;
use nth_protocol::{CurioDelivery, CurioNotice};

use super::Stores;

/// How long after a refused or failed claim the lease is tried again, and
/// after a failed acknowledgment the next: another process that holds the
/// lease may exit, and a failure that builds a store would otherwise repeat
/// every poll.
pub(crate) const CLAIM_RETRY: Duration = Duration::from_secs(10);

/// A session's delivery as its front-end's loop keeps it: the lease while
/// held, and when to try again for what failed. Time comes in as `now`, so
/// the rules never read a clock of their own.
#[derive(Debug)]
pub struct Delivery {
    lease: Option<CurioDelivery>,
    next_claim: Instant,
    /// Whether the front-end was told another process delivers, so it is
    /// told once rather than every retry.
    told_owned: bool,
    next_acknowledge: Instant,
    /// The last acknowledgment failure told, so each is told once.
    told_unacknowledged: Option<String>,
}

/// What a poll reads on a blocking thread: plain values, so a job owns them.
pub struct Poll {
    root: PathBuf,
    me: String,
    claim: bool,
    delivering: Option<Store>,
    stores: Stores,
}

/// What a [`Poll`] read, for [`Delivery::apply`] on the loop.
pub struct Read {
    attempt: Attempt,
    feedback: Feedback,
}

enum Attempt {
    NotTried,
    NoFolder,
    /// This session's store could not be built.
    Failed,
    /// Another process holds the lease; the store's root.
    Refused(PathBuf),
    Lease(CurioDelivery),
}

enum Feedback {
    /// No lease, so nothing was read.
    Unowned,
    Busy,
    Pending(Vec<Batch>),
    Unreadable,
}

/// How a poll's try for the lease went.
#[derive(Debug, PartialEq)]
pub enum Claim {
    /// This session delivers its feedback from now on.
    Claimed,
    /// Another process delivers it; `first` the first time this delivery
    /// found so.
    Owned { first: bool },
    /// Not tried: the lease is held, the retry time has not come, or the
    /// store could not be built and waits for the retry.
    NotYet,
    /// This session never published, so there is no feedback to deliver:
    /// its folder is waited for rather than made.
    NoFolder,
}

/// What the loop does after a poll.
#[derive(Debug)]
pub struct Polled {
    pub claim: Claim,
    /// Where the store another process delivers from is: the session keeps
    /// it, so its acknowledgment finds the store.
    pub root: Option<PathBuf>,
    /// Feedback to post to the inbox, none of it recorded yet.
    pub post: Vec<CurioNotice>,
    /// Pending batches the session already recorded: its acknowledgment did
    /// not reach the store yet, so it is caught up rather than posted again.
    pub unacknowledged: bool,
    /// Another held the store's lock, so nothing was read; the next poll
    /// reads it.
    pub busy: bool,
    /// The pending batches could not be read.
    pub unreadable: bool,
}

impl Delivery {
    pub fn new(now: Instant) -> Self {
        Self {
            lease: None,
            next_claim: now,
            told_owned: false,
            next_acknowledge: now,
            told_unacknowledged: None,
        }
    }

    /// The store the lease is on, while this session delivers.
    pub fn store(&self) -> Option<&Store> {
        self.lease.as_ref().map(CurioDelivery::store)
    }

    /// Lets the lease go, for another process or session to take.
    pub fn release(&mut self) {
        self.lease = None;
    }

    /// Tries for the lease on the next poll: a curio just adopted brings
    /// its feedback here, so it must not wait out the retry.
    pub fn claim_now(&mut self, now: Instant) {
        self.next_claim = now;
    }

    /// The poll to run at `now` for session `me` of the repository at
    /// `root`: it tries for the lease when none is held and the retry time
    /// has come, and reads the pending feedback while one is.
    pub fn poll(&self, root: PathBuf, me: String, stores: Stores, now: Instant) -> Poll {
        Poll {
            root,
            me,
            claim: self.lease.is_none() && now >= self.next_claim,
            delivering: self.store().cloned(),
            stores,
        }
    }

    /// Takes in what a poll read: keeps a lease it won, sets when to retry
    /// one it lost, and leaves out of what it posts the batches `recorded`.
    pub fn apply(&mut self, read: Read, recorded: &BTreeSet<String>, now: Instant) -> Polled {
        let mut root = None;
        let claim = match read.attempt {
            Attempt::NotTried => Claim::NotYet,
            Attempt::NoFolder => Claim::NoFolder,
            Attempt::Failed => {
                self.next_claim = now + CLAIM_RETRY;
                Claim::NotYet
            }
            Attempt::Refused(found) => {
                self.next_claim = now + CLAIM_RETRY;
                root = Some(found);
                Claim::Owned {
                    first: !std::mem::replace(&mut self.told_owned, true),
                }
            }
            Attempt::Lease(lease) => {
                self.lease = Some(lease);
                Claim::Claimed
            }
        };
        let mut polled = Polled {
            claim,
            root,
            post: Vec::new(),
            unacknowledged: false,
            busy: false,
            unreadable: false,
        };
        match read.feedback {
            Feedback::Unowned => {}
            Feedback::Busy => polled.busy = true,
            Feedback::Unreadable => polled.unreadable = true,
            Feedback::Pending(batches) => {
                polled.unacknowledged = batches.iter().any(|b| recorded.contains(&b.id));
                polled.post = super::fresh(batches, recorded);
            }
        }
        polled
    }

    /// Whether a catch-up acknowledgment may start at `now`.
    pub fn acknowledge_due(&self, now: Instant) -> bool {
        now >= self.next_acknowledge
    }

    /// Takes in how an acknowledgment ended, and returns the failure to
    /// tell: each once, until one succeeds. A failure holds the next one
    /// back for [`CLAIM_RETRY`].
    pub fn acknowledged(&mut self, result: Result<(), String>, now: Instant) -> Option<String> {
        let error = match result {
            Ok(()) => {
                self.told_unacknowledged = None;
                return None;
            }
            Err(error) => error,
        };
        self.next_acknowledge = now + CLAIM_RETRY;
        if self.told_unacknowledged.as_ref() == Some(&error) {
            return None;
        }
        self.told_unacknowledged = Some(error.clone());
        Some(error)
    }
}

impl Poll {
    /// Reads only: a front-end whose session is away in a turn tries for
    /// the lease once it is back.
    pub fn without_claim(mut self) -> Self {
        self.claim = false;
        self
    }

    /// Tries for the lease when due and reads the pending feedback. Builds
    /// a store and takes its lock, so it runs on a blocking thread.
    pub fn run(self) -> Read {
        let attempt = if !self.claim {
            Attempt::NotTried
        } else if !Directories::from_root(self.root.clone())
            .curio_owner(&self.me)
            .is_dir()
        {
            Attempt::NoFolder
        } else {
            match self.stores.get(&self.root, &self.me) {
                Ok(store) => {
                    let root = store.repo_root().to_path_buf();
                    CurioDelivery::claim(store).map_or(Attempt::Refused(root), Attempt::Lease)
                }
                Err(_) => Attempt::Failed,
            }
        };
        let batches = match &attempt {
            Attempt::Lease(lease) => Some(lease.poll()),
            _ => self.delivering.map(|store| store.try_batches(true)),
        };
        let feedback = match batches {
            None => Feedback::Unowned,
            Some(Ok(Some(batches))) => Feedback::Pending(batches),
            Some(Ok(None)) => Feedback::Busy,
            Some(Err(_)) => Feedback::Unreadable,
        };
        Read { attempt, feedback }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, time::Instant};

    use nth_curios::{Batch, Discovery, Store};
    use nth_protocol::StreamEvent;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        FrontEnd, Inbox, Session,
        curios::{
            How,
            fixture::{publish, published, send},
            resolve,
        },
    };

    /// One poll of `session`'s delivery at `now`, applied.
    fn tick(delivery: &mut Delivery, session: &Session, stores: &Stores, now: Instant) -> Polled {
        let root = session.curio_root.clone().expect("prepared");
        let read = delivery
            .poll(root, session.id.to_string(), stores.clone(), now)
            .run();
        delivery.apply(read, &session.curio_receipts, now)
    }

    /// Polls until one reads the store: a child another test forks shares
    /// the store's lock until it execs.
    fn settle(delivery: &mut Delivery, session: &Session, stores: &Stores, now: Instant) -> Polled {
        for _ in 0..50 {
            let polled = tick(delivery, session, stores, now);
            if !polled.busy && delivery.store().is_some() {
                return polled;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the Curios store stayed locked");
    }

    async fn deliver(session: &mut Session, notices: Vec<CurioNotice>) {
        let inbox = Inbox::new();
        for notice in notices {
            inbox.post_curio(notice);
        }
        let provider = crate::agent_loop::tests::Scripted::new(vec![vec![StreamEvent::TextDelta(
            "done".into(),
        )]]);
        let (events, _rx) = mpsc::channel(64);
        session
            .deliver_curios(
                &provider,
                &[],
                &FrontEnd {
                    inbox,
                    ..FrontEnd::default()
                },
                &events,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn batch(id: &str) -> Batch {
        Batch {
            id: id.into(),
            session_id: "owner".into(),
            curio_id: "curio".into(),
            revision: 1,
            messages: vec![],
            recorded: false,
        }
    }

    #[test]
    fn poll_posts_only_unrecorded_batches() {
        let now = Instant::now();
        let mut delivery = Delivery::new(now);
        let pending = || Read {
            attempt: Attempt::NotTried,
            feedback: Feedback::Pending(vec![batch("recorded"), batch("new")]),
        };
        let polled = delivery.apply(pending(), &BTreeSet::from(["recorded".into()]), now);
        let posted: Vec<_> = polled.post.iter().map(|n| n.batch.id.as_str()).collect();
        assert_eq!(posted, ["new"]);
        assert!(polled.unacknowledged);
        let polled = delivery.apply(pending(), &BTreeSet::new(), now);
        assert_eq!(polled.post.len(), 2);
        assert!(!polled.unacknowledged);
    }

    #[test]
    fn claim_waits_claim_retry_after_a_refusal() {
        let (_repo, session, _store, _) = published();
        let _other = session.curios().claim_delivery().unwrap();
        let stores = Stores::default();
        let now = Instant::now();
        let mut delivery = Delivery::new(now);
        let polled = tick(&mut delivery, &session, &stores, now);
        assert_eq!(polled.claim, Claim::Owned { first: true });
        assert_eq!(polled.root, session.curio_root);
        let soon = now + CLAIM_RETRY - Duration::from_secs(1);
        assert_eq!(
            tick(&mut delivery, &session, &stores, soon).claim,
            Claim::NotYet
        );
        let later = now + CLAIM_RETRY;
        assert_eq!(
            tick(&mut delivery, &session, &stores, later).claim,
            Claim::Owned { first: false },
            "told once"
        );
        assert!(delivery.store().is_none());
    }

    #[test]
    fn delivery_is_claimed_once_another_owner_lets_go() {
        let (_repo, session, _store, _) = published();
        let other = session.curios().claim_delivery().unwrap();
        let stores = Stores::default();
        let mut now = Instant::now();
        let mut delivery = Delivery::new(now);
        let polled = tick(&mut delivery, &session, &stores, now);
        assert!(delivery.store().is_none());
        assert_eq!(polled.claim, Claim::Owned { first: true });
        drop(other);
        tick(&mut delivery, &session, &stores, now);
        assert!(delivery.store().is_none(), "retries wait a while");
        // A child another test forks shares the lock until it execs.
        for _ in 0..50 {
            now += CLAIM_RETRY;
            tick(&mut delivery, &session, &stores, now);
            if delivery.store().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivery.store().is_some());
    }

    #[tokio::test]
    async fn feedback_on_an_adopted_curio_reaches_this_session() {
        let (repo, mut session, store, _) = published();
        let old = Store::new(repo.path(), "old-owner").unwrap();
        let c = publish(&old, "Old");
        let stores = Stores::default();
        let now = Instant::now();
        let mut delivery = Delivery::new(now);
        settle(&mut delivery, &session, &stores, now);
        let root = session.curio_root.clone().unwrap();
        let me = session.id.to_string();
        let (_, how) = resolve(
            &stores,
            &root,
            &me,
            &c.session_id,
            &c.id,
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(how, How::Adopted("old-owner".into()));
        delivery.claim_now(now);
        // The viewer now writes to this session's store.
        send(&store, &c);
        let polled = settle(&mut delivery, &session, &stores, now);
        assert_eq!(polled.post.len(), 1);
        deliver(&mut session, polled.post).await;
        assert_eq!(session.curio_receipts.len(), 1);
        assert!(old.batches(true).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unacknowledged_receipt_is_caught_up_without_waking() {
        let (_repo, mut session, store, c) = published();
        let stores = Stores::default();
        let now = Instant::now();
        let mut delivery = Delivery::new(now);
        send(&store, &c);
        let polled = settle(&mut delivery, &session, &stores, now);
        deliver(&mut session, polled.post).await;
        // Nothing saved yet, so nothing acknowledged.
        assert_eq!(store.batches(true).unwrap().len(), 1);
        let polled = settle(&mut delivery, &session, &stores, now);
        assert!(polled.post.is_empty(), "the model is not woken again");
        assert!(polled.unacknowledged);
        assert!(delivery.acknowledge_due(now));
        let result = session
            .acknowledge_curios()
            .await
            .map_err(|e| e.to_string());
        assert_eq!(delivery.acknowledged(result, now), None);
        assert!(store.batches(true).unwrap().is_empty());
    }

    #[test]
    fn a_failing_acknowledgment_is_told_once_and_waits_to_retry() {
        let now = Instant::now();
        let mut delivery = Delivery::new(now);
        assert!(delivery.acknowledge_due(now));
        assert_eq!(
            delivery.acknowledged(Err("denied".into()), now),
            Some("denied".into())
        );
        assert!(!delivery.acknowledge_due(now + Duration::from_secs(1)));
        let later = now + CLAIM_RETRY;
        assert!(delivery.acknowledge_due(later));
        assert_eq!(
            delivery.acknowledged(Err("denied".into()), later),
            None,
            "the same failure is not told again"
        );
        assert_eq!(
            delivery.acknowledged(Err("gone".into()), later),
            Some("gone".into())
        );
        assert_eq!(delivery.acknowledged(Ok(()), later), None);
        assert_eq!(
            delivery.acknowledged(Err("gone".into()), later),
            Some("gone".into()),
            "told again after one succeeded"
        );
    }

    #[test]
    fn other_owner_is_discovered_but_never_delivers_here() {
        let (repo, owner, store, c) = published();
        send(&store, &c);
        let mut other = Session::new("fake", repo.path().to_path_buf());
        other.curio_root = owner.curio_root.clone();
        let stores = Stores::default();
        let now = Instant::now();
        let mut delivery = Delivery::new(now);
        let polled = tick(&mut delivery, &other, &stores, now);
        assert_eq!(polled.claim, Claim::NoFolder);
        assert!(polled.post.is_empty());
        assert!(delivery.store().is_none());
        let mut discovery = Discovery::default();
        let items = discovery.refresh(repo.path()).unwrap();
        assert_eq!(items.len(), 1);
        assert_ne!(items[0].session_id, other.id.to_string());
        assert!(
            discovery
                .queued()
                .contains(&(c.session_id.clone(), c.id.clone()))
        );
    }
}
