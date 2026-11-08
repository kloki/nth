//! A session's side of Curios: where its store is, the review feedback it
//! records and acknowledges, and the turn that feedback starts on its own.
//! Every store build spawns git or waits on another process's lock, so
//! the async methods run it on a blocking thread.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use nth_curios::Store;
use nth_protocol::{Event, Mode, Provider, Tool};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::CurioNotice;
use crate::{Error, FrontEnd, Session, agent_loop::emit, plan, session::TurnInput};

/// The batches a session recorded, as a turn adds to them: a batch is
/// recorded in the same step that puts its notice in the history, so the
/// saved receipts and the saved history always agree.
pub(crate) struct Receipts<'a> {
    session: &'a str,
    recorded: &'a mut BTreeSet<String>,
}

impl<'a> Receipts<'a> {
    pub(crate) fn new(session: &'a str, recorded: &'a mut BTreeSet<String>) -> Self {
        Self { session, recorded }
    }

    /// Whether `notice` is new feedback for this session, recording it if
    /// so: another session's, or one already recorded, is not.
    pub(crate) fn record(&mut self, notice: &CurioNotice) -> bool {
        self.session == notice.batch.session_id && self.recorded.insert(notice.batch.id.clone())
    }
}

/// The store of session `id`: at `root` when it kept one, else in the main
/// checkout of `state_root`. A subagent has none, since its feedback would
/// go to a session nobody reviews.
fn open(
    subagent: bool,
    root: Option<PathBuf>,
    state_root: &Path,
    id: &str,
) -> nth_curios::Result<Store> {
    if subagent {
        return Err(nth_curios::Error::Invalid(
            "Curios is unavailable to this session".into(),
        ));
    }
    match root {
        Some(root) => Store::new(&root, id),
        None => Store::open(state_root, id),
    }
}

/// [`Session::acknowledge_curios`]'s work, on a blocking thread.
fn acknowledge(root: &Path, session: &str, receipts: &[String]) -> nth_curios::Result<()> {
    // Saved history remains usable when its repository has gone away.
    // Only repository absence is optional; state and write errors still fail.
    match root.try_exists() {
        Ok(false) => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(true) => {}
    }
    match nth_curios::repository_root(root) {
        Err(nth_curios::Error::RepoRequired) => return Ok(()),
        Err(error) => return Err(error),
        Ok(found) if found != root.canonicalize()? => return Ok(()),
        Ok(_) => {}
    }
    Store::new(root, session)?.acknowledge_recorded(receipts)
}

impl Session {
    /// This session's Curios store, for a front-end that delivers its
    /// feedback or a command that publishes for it. Builds the store, so an
    /// async caller runs it under [`nth_curios::offload`].
    pub fn curio_store(&self) -> nth_curios::Result<Store> {
        open(
            self.subagent,
            self.curio_root.clone(),
            &self.state_root(),
            &self.id.to_string(),
        )
    }

    pub fn curio_source_dir(&self) -> Option<PathBuf> {
        self.curio_root.as_ref().map(|r| {
            nth_directories::Directories::from_root(r.clone()).curio_source(&self.id.to_string())
        })
    }

    /// Explicit preparation stores ownership for save/resume.
    pub fn prepare_curios(&mut self) -> nth_curios::Result<Store> {
        let store = self.curio_store()?;
        self.curio_root = Some(store.repo_root().to_path_buf());
        Ok(store)
    }

    /// Idle wakeup entry point. Never routes submitted text through prompt,
    /// skill or agent-mention parsing. Caller owns the delivery lease and save.
    pub async fn deliver_curios(
        &mut self,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        if cancel.is_cancelled() {
            return Err(Error::Interrupted);
        }
        if self.tools_mode() == Mode::Plan {
            plan::ensure_dir(&self.state_root())
                .await
                .map_err(Error::State)?;
        }
        self.ready_curios().await;
        let Some(notices) = self.take_curios(front_end, events, cancel).await else {
            return Ok(());
        };
        self.prepared_turn(
            TurnInput::Feedback(notices),
            provider,
            tools,
            front_end,
            events,
            cancel,
        )
        .await
    }

    /// This session's review feedback waiting in `front_end`'s inbox,
    /// recorded and shown, as the notices that lead the next user message.
    pub(crate) async fn take_curios(
        &mut self,
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Option<String> {
        let id = self.id.to_string();
        let mut receipts = (!self.subagent).then(|| Receipts::new(&id, &mut self.curio_receipts));
        let mut notices = Vec::new();
        for notice in front_end.inbox.take_curios(&id) {
            if receipts.as_mut().is_some_and(|r| r.record(&notice)) {
                let text = notice.render();
                emit(events, cancel, Event::Notice(text.clone())).await;
                notices.push(text);
            }
        }
        // One element per line, as `split_notices` reads them back.
        (!notices.is_empty()).then(|| notices.join("\n"))
    }

    /// Keeps the root of the store the tools use, once a turn ended, so
    /// that acknowledging feedback and deleting the session find what the
    /// turn's tools made. Only the root is found, not the store built: its
    /// folder waits for the tools, and the delivery poll for the folder.
    pub(crate) async fn find_curio_root(&mut self) {
        if self.curio_root.is_some() {
            return;
        }
        let state_root = self.state_root();
        if let Ok(root) = nth_curios::offload(move || nth_curios::owner_root(&state_root)).await {
            self.curio_root = Some(root);
        }
    }

    /// Readies this session's Curios store and keeps its root.
    pub(crate) async fn ready_curios(&mut self) {
        let (subagent, root, state_root, id) = (
            self.subagent,
            self.curio_root.clone(),
            self.state_root(),
            self.id.to_string(),
        );
        if let Ok(root) = nth_curios::offload(move || {
            Ok(open(subagent, root, &state_root, &id)?
                .repo_root()
                .to_path_buf())
        })
        .await
        {
            self.curio_root = Some(root);
        }
    }

    /// Marks the Curios feedback this session recorded as recorded in its
    /// store too. Call once the session is saved; one missed is caught up by
    /// the next, since it covers every receipt. The future owns what it
    /// reads, so a front-end can run it as a task of its own.
    pub fn acknowledge_curios(
        &self,
    ) -> impl Future<Output = nth_curios::Result<()>> + Send + 'static {
        let receipts: Vec<_> = self.curio_receipts.iter().cloned().collect();
        let root = self.curio_root.clone();
        let id = self.id.to_string();
        async move {
            match root {
                Some(root) if !receipts.is_empty() => {
                    nth_curios::offload(move || acknowledge(&root, &id, &receipts)).await
                }
                _ => Ok(()),
            }
        }
    }
}
