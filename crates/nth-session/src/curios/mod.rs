//! The rules a front-end delivers Curios review feedback and opens curios
//! by: who delivers a session's feedback, which batches wake the model, and
//! whose store serves a curio opened from another session. Store calls spawn
//! git or wait on another process's lock, so what here touches a store is
//! sync and meant for a blocking thread; the decisions are plain values a
//! front-end applies on its own loop.
mod delivery;
#[cfg(test)]
mod fixture;
mod open;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
};

pub use delivery::{Claim, Delivery, Poll, Polled, Read};
use nth_curios::{Batch, Store};
use nth_protocol::CurioNotice;
pub use open::{How, resolve};

/// One store handle per session id for this run: building one spawns git
/// and takes the lock, which a poll every second must not repeat. Locked
/// only on blocking threads, and never while a store is built.
#[derive(Clone, Default)]
pub struct Stores(Arc<Mutex<BTreeMap<String, Store>>>);

impl Stores {
    /// `session`'s store under `root`, built on first use.
    pub fn get(&self, root: &Path, session: &str) -> nth_curios::Result<Store> {
        if let Some(store) = self.0.lock().expect("stores lock poisoned").get(session) {
            return Ok(store.clone());
        }
        let store = Store::new(root, session)?;
        self.0
            .lock()
            .expect("stores lock poisoned")
            .insert(session.to_owned(), store.clone());
        Ok(store)
    }
}

/// The pending `batches` the session has not recorded: a recorded one is
/// in its history already, and posting it again would wake the model with
/// nothing new.
pub fn fresh(batches: Vec<Batch>, recorded: &BTreeSet<String>) -> Vec<CurioNotice> {
    batches
        .into_iter()
        .filter(|batch| !recorded.contains(&batch.id))
        .map(|batch| CurioNotice { batch })
        .collect()
}
