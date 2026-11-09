//! What the subagents share with the session that started them: what their
//! turns spent, until that session takes it, and which of them were
//! forgotten when it was left. One lock guards both, and whatever a
//! subagent tells that session's model is posted under it, so nothing of a
//! left session lands in the next one's inbox or ledger: the front-end
//! forgets the subagents before it clears the inbox, and a post either
//! comes before, and is cleared with the rest, or sees it was forgotten.

use std::sync::{Arc, Mutex, MutexGuard};

use super::SubagentId;
use crate::Spend;

#[derive(Default)]
struct Spent {
    /// Every id up to this one was forgotten with its session.
    forgotten: SubagentId,
    /// Tagged with the subagent that spent it, so a spend that lands after
    /// its session was left is dropped by its id rather than by when it
    /// came.
    spends: Vec<(SubagentId, Spend)>,
}

/// The registry's half: cheap to clone, and holding no registry, so an
/// actor or a tool can keep it without keeping the subagents alive.
#[derive(Clone, Default)]
pub(super) struct Spending(Arc<Mutex<Spent>>);

impl Spending {
    fn lock(&self) -> MutexGuard<'_, Spent> {
        self.0.lock().expect("subagent spend lock poisoned")
    }

    /// Every subagent up to `id` belonged to a session that was left: what
    /// they spent and nobody took goes, and they tell no model from now on.
    pub(super) fn forget_up_to(&self, id: SubagentId) {
        let mut spent = self.lock();
        spent.forgotten = id;
        spent.spends.retain(|(by, _)| *by > id);
    }

    pub(super) fn forgotten(&self, id: SubagentId) -> bool {
        id <= self.lock().forgotten
    }

    /// What the subagents' turns spent since this was last called.
    pub(super) fn take(&self) -> Vec<Spend> {
        let mut spent = self.lock();
        let forgotten = spent.forgotten;
        std::mem::take(&mut spent.spends)
            .into_iter()
            .filter(|(by, _)| *by > forgotten)
            .map(|(_, spend)| spend)
            .collect()
    }

    pub(super) fn parent(&self, id: SubagentId) -> Parent {
        Parent {
            id,
            spending: self.clone(),
        }
    }
}

/// How subagent `id` reaches the session that started it, for as long as
/// that session was not left.
#[derive(Clone)]
pub(crate) struct Parent {
    id: SubagentId,
    spending: Spending,
}

impl Parent {
    pub(crate) fn id(&self) -> SubagentId {
        self.id
    }

    /// Runs `post`, which puts a notice in the parent's inbox, unless the
    /// subagent was forgotten; `false` when it was. `post` runs under the
    /// lock forgetting takes, so it must not wait.
    pub(crate) fn tell(&self, post: impl FnOnce()) -> bool {
        let spent = self.spending.lock();
        if self.id <= spent.forgotten {
            return false;
        }
        post();
        true
    }

    /// Hands what a turn spent to the parent, which takes it with
    /// [`Subagents::take_spent`](super::Subagents::take_spent).
    pub(crate) fn record(&self, spends: impl IntoIterator<Item = Spend>) {
        let mut spent = self.spending.lock();
        spent
            .spends
            .extend(spends.into_iter().map(|spend| (self.id, spend)));
    }

    #[cfg(test)]
    pub(crate) fn forgotten(&self) -> bool {
        self.spending.forgotten(self.id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::TryLockError;

    use super::*;

    #[test]
    fn a_notice_is_told_under_the_lock_its_session_is_left_under() {
        let spending = Spending::default();
        let parent = spending.parent(1);

        let told = parent.tell(|| {
            // Leaving the session waits for this post, so it cannot come
            // between seeing the subagent listened to and posting.
            assert!(matches!(
                spending.0.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
        });

        assert!(told);
        spending.forget_up_to(1);
        assert!(
            !parent.tell(|| panic!("told after its session was left")),
            "refused once forgotten"
        );
        assert!(!spending.parent(2).forgotten(), "a later one still tells");
    }
}
