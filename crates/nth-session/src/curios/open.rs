//! Whose store serves a curio a session opens. Feedback goes to the store
//! the viewer writes to, so opening another session's curio decides where
//! its feedback lands from then on.
use std::path::Path;

use nth_curios::{Error, Result, Store};
use tokio_util::sync::CancellationToken;

use super::Stores;

/// Whose a curio was when it was opened.
#[derive(Debug, PartialEq)]
pub enum How {
    /// This session's.
    Own,
    /// Another session's, adopted from it.
    Adopted(String),
    /// Another session's that is running, so it stays there.
    Running,
}

/// Whose store serves `owner`'s curio `id` for session `me`, checked at
/// `revision` (the current one when `None`): `me`'s own, `me`'s after
/// adopting it from an idle session, or a running session's, which keeps
/// it. Builds stores, so it runs on a blocking thread.
pub fn resolve(
    stores: &Stores,
    root: &Path,
    me: &str,
    owner: &str,
    id: &str,
    revision: Option<u64>,
    cancel: &CancellationToken,
) -> Result<(Store, How)> {
    let store = stores.get(root, me)?;
    let (store, how) = if owner == me {
        (store, How::Own)
    } else {
        // Building the store took a while: a `/clear` or a newer open since
        // must not adopt the curio into the session left behind.
        if cancel.is_cancelled() {
            return Err(Error::Invalid("Curio opening cancelled".into()));
        }
        // Adopting is refused while the owner holds its delivery lease,
        // which only a running session does.
        match store.adopt(owner, id) {
            Ok(_) => (store, How::Adopted(owner.to_owned())),
            Err(Error::Conflict(_)) => (stores.get(root, owner)?, How::Running),
            Err(error) => return Err(error),
        }
    };
    store.snapshot(id, revision)?;
    Ok((store, how))
}

#[cfg(test)]
mod tests {
    use nth_curios::Store;

    use super::*;
    use crate::curios::fixture::{publish, published};

    #[test]
    fn resolve_adopts_an_idle_sessions_curio_and_leaves_a_running_one() {
        let (repo, session, _store, own) = published();
        let root = session.curio_root.clone().unwrap();
        let me = session.id.to_string();
        let stores = Stores::default();
        let cancel = CancellationToken::new();
        let (store, how) = resolve(&stores, &root, &me, &me, &own.id, None, &cancel).unwrap();
        assert_eq!((store.session_id(), how), (me.as_str(), How::Own));

        let idle = Store::new(repo.path(), "idle-owner").unwrap();
        let c = publish(&idle, "Idle");
        let (store, how) =
            resolve(&stores, &root, &me, "idle-owner", &c.id, None, &cancel).unwrap();
        assert_eq!(how, How::Adopted("idle-owner".into()));
        assert_eq!(store.session_id(), me);
        assert!(idle.list().unwrap().is_empty());

        let running = Store::new(repo.path(), "running-owner").unwrap();
        let c = publish(&running, "Running");
        let _lease = running.claim_delivery().unwrap();
        let (store, how) =
            resolve(&stores, &root, &me, "running-owner", &c.id, None, &cancel).unwrap();
        assert_eq!(how, How::Running);
        assert_eq!(store.session_id(), "running-owner");
        assert_eq!(running.list().unwrap().len(), 1);
    }

    #[test]
    fn a_cancelled_open_adopts_nothing() {
        let (repo, session, _store, _) = published();
        let root = session.curio_root.clone().unwrap();
        let idle = Store::new(repo.path(), "idle-owner").unwrap();
        let c = publish(&idle, "Idle");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let me = session.id.to_string();
        let opened = resolve(
            &Stores::default(),
            &root,
            &me,
            "idle-owner",
            &c.id,
            None,
            &cancel,
        );
        assert!(opened.is_err());
        assert_eq!(idle.list().unwrap().len(), 1);
    }
}
