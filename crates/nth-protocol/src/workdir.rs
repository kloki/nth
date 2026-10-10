use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Where a tool moved the session's working directory during a turn, and
/// the directory it came from while it works in a worktree. Shared by the
/// calls of a turn: a later step runs in the new directory, and the session
/// takes it on when the turn ends.
#[derive(Debug, Clone, Default)]
pub struct Workdir(Arc<Mutex<State>>);

#[derive(Debug, Default)]
struct State {
    /// `None` until a tool moves the session.
    moved_to: Option<PathBuf>,
    /// The directory the session left for a worktree; `None` outside one.
    origin: Option<PathBuf>,
}

impl Workdir {
    /// `origin` is where the session came from when it is already in a
    /// worktree.
    pub fn new(origin: Option<PathBuf>) -> Self {
        Self(Arc::new(Mutex::new(State {
            moved_to: None,
            origin,
        })))
    }

    /// Where a tool moved the session this turn, if it did.
    pub fn moved_to(&self) -> Option<PathBuf> {
        self.state().moved_to.clone()
    }

    pub fn origin(&self) -> Option<PathBuf> {
        self.state().origin.clone()
    }

    /// Moves from `from` into the worktree at `to`. From one worktree into
    /// another, the origin stays the directory the first was entered from.
    pub fn enter(&self, from: &Path, to: PathBuf) {
        let mut state = self.state();
        state.origin.get_or_insert_with(|| from.to_path_buf());
        state.moved_to = Some(to);
    }

    /// Moves back to the origin and returns it; `None` outside a worktree.
    pub fn exit(&self) -> Option<PathBuf> {
        let mut state = self.state();
        let origin = state.origin.take()?;
        state.moved_to = Some(origin.clone());
        Some(origin)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().expect("workdir lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_moved_until_a_tool_moves() {
        let workdir = Workdir::default();

        assert_eq!(workdir.moved_to(), None);
        assert_eq!(workdir.exit(), None);
        assert_eq!(workdir.moved_to(), None);
    }

    #[test]
    fn exit_goes_back_where_the_first_worktree_was_entered_from() {
        let workdir = Workdir::default();

        workdir.enter(Path::new("/repo/src"), "/repo/.nth/worktrees/a".into());
        workdir.enter(
            Path::new("/repo/.nth/worktrees/a"),
            "/repo/.nth/worktrees/b".into(),
        );
        assert_eq!(workdir.moved_to(), Some("/repo/.nth/worktrees/b".into()));

        assert_eq!(workdir.exit(), Some("/repo/src".into()));
        assert_eq!(workdir.moved_to(), Some("/repo/src".into()));
        assert_eq!(workdir.origin(), None);
    }
}
