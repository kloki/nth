//! Applies what the curios tab (`crate::curios`) hands the loop to the rest
//! of the app: feedback to the inbox, the session's receipts and root, the
//! hints, the browser opener and the `/curios` picker in the input panel.
use std::collections::BTreeSet;

use nth_curios as core;
use nth_session::curios::{Claim, How, Polled};
use tokio::task::JoinError;

use super::{App, input::Input, keys::Action};
use crate::curios::{CurioStep, CuriosTab, NotOpened, Opened, Tick};

impl App {
    pub(crate) fn has_curios(&self) -> bool {
        self.curios.has_curios()
    }

    pub(super) fn curio_step(&mut self, step: CurioStep) {
        match step {
            CurioStep::Polled(tick) => self.curio_polled(tick),
            CurioStep::Opened(opened) => self.curio_opened(opened),
            CurioStep::Acknowledged(acknowledged) => self.curio_acknowledged(acknowledged),
            CurioStep::PollDue => self.request_curio_poll(),
        }
    }

    pub(super) fn reset_curios(&mut self) {
        self.curios.invalidate();
        if let Some(session) = &self.session {
            self.curios = CuriosTab::new(session.state_root());
        }
        self.areas.forget(&crate::app::mouse::Target::Curio);
        if matches!(self.input, Input::Curios(_)) {
            self.input = Input::Prompt;
        }
        self.request_curio_poll();
    }

    /// Polls unless one runs already, so a slow one never stacks.
    pub(super) fn poll_curios(&mut self) {
        if !self.curios.polling.is_running() {
            self.request_curio_poll();
        }
    }

    /// Starts a poll, or while one runs, another once it is done: something
    /// changed since it started.
    fn request_curio_poll(&mut self) {
        // While a turn runs the session is away, and with it the root a
        // refused claim would keep.
        let claim = self.session.is_some();
        self.curios.poll(self.session_id.to_string(), claim);
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
        let landed = self.curios.land(tick);
        if let Input::Curios(picker) = &mut self.input {
            picker.mark(self.curios.queued());
        }
        if let Some(skipped) = landed.skipped {
            self.hint = Some(format!("curios: skipped {skipped} unreadable folders").into());
        }
        if !self.curios.has_curios() {
            self.areas.forget(&crate::app::mouse::Target::Curio);
        }
        // While a turn runs the session is away; its turn hands over what
        // the inbox holds and its receipts come back with it.
        let recorded = self
            .session
            .as_ref()
            .map(|s| s.curio_receipts.clone())
            .unwrap_or_default();
        let Some(read) = landed.read else {
            return;
        };
        let polled = self.curios.deliver(read, &recorded);
        if polled.claim == (Claim::Owned { first: true }) {
            self.hint = Some("Curios delivery is owned by another process".into());
        }
        if let Some(session) = &mut self.session {
            // Acknowledging reads the root from the session, which a turn
            // that started after the claim had away.
            if let Some(root) = polled
                .root
                .clone()
                .or_else(|| self.curios.delivering_root())
            {
                session.curio_root = Some(root);
            }
        }
        if polled.unreadable {
            self.hint = Some("could not read queued Curios feedback".into());
        }
        self.deliver_curio_feedback(polled, &recorded);
    }

    fn deliver_curio_feedback(&mut self, polled: Polled, recorded: &BTreeSet<String>) {
        let posted = polled
            .post
            .into_iter()
            .filter(|notice| self.inbox.post_curio(notice.clone()))
            .count();
        // A batch posted while a turn ran that the turn recorded anyway
        // would otherwise wake the model with nothing.
        if self.session.is_some() {
            for notice in self.inbox.take_curios(&self.session_id.to_string()) {
                if !recorded.contains(&notice.batch.id) {
                    self.inbox.post_curio(notice);
                }
            }
        }
        if posted > 0 {
            self.context_stale();
            self.arm_notices();
        }
        // An acknowledgment that failed after a save is caught up here, so
        // the viewer stops saying the feedback is on its way.
        if polled.unacknowledged
            && self.curios.saved
            && self.store.is_some()
            && self.curios.acknowledge_due()
            && let Some(session) = &self.session
        {
            self.curios.acknowledge(session.acknowledge_curios());
        }
    }

    pub(super) fn curio_acknowledged(&mut self, acknowledged: Result<core::Result<()>, JoinError>) {
        if let Some(error) = self.curios.acknowledged(acknowledged) {
            self.hint = Some(format!("curios: could not acknowledge feedback: {error}").into());
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
            if !self.curios.busy() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
        self.input = Input::Curios(self.curios.picker());
    }

    pub(super) fn key_curios(&mut self, action: Action) {
        let Input::Curios(picker) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext => picker.select_next(),
            Action::SelectPrev => picker.select_prev(),
            Action::Insert(c) => picker.push(c),
            Action::Backspace => picker.pop(),
            Action::Submit => {
                if let Some(c) = picker.selected().cloned() {
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
        match self.curios.latest() {
            Some((s, c)) => self.open_curio(s, c, None),
            None => self.hint = Some("no Curio to open".into()),
        }
    }

    /// Opens a curio in the browser, hosting it first.
    pub(super) fn open_curio(&mut self, owner: String, id: String, revision: Option<u64>) {
        let me = self.session_id.to_string();
        match self.curios.open(me, owner, id, revision) {
            Ok(()) => {}
            Err(NotOpened::Looking) => {
                self.hint = Some("Curios is still looking for the repository".into())
            }
            Err(NotOpened::Outside) => self.hint = Some("Curios requires a Git repository".into()),
        }
    }

    pub(super) fn curio_opened(&mut self, opened: core::Result<Opened>) {
        match self.curios.opened(opened) {
            Ok((url, how)) => {
                // Its feedback comes here now, which the tab asked the next
                // poll to claim.
                if matches!(how, How::Adopted(_)) {
                    self.request_curio_poll();
                }
                if let Some(url) = url {
                    self.present_curio(&url, how);
                }
            }
            Err(true) => self.hint = Some("could not open Curio revision".into()),
            Err(false) => self.hint = Some("could not host Curio".into()),
        }
    }

    fn present_curio(&mut self, url: &str, how: How) {
        // Opener errors may contain their arguments: never render them here.
        if (self.opener)(url).is_err() {
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
        let link = match &self.input {
            Input::Curios(picker) => picker.selected().and_then(|c| self.curios.link(c)),
            _ => None,
        };
        self.hint = Some(
            match link {
                Some(link) if (self.clipboard)(&link).is_ok() => "copied Curio link",
                Some(_) => "could not copy Curio link",
                None => "open this Curio first to create its link",
            }
            .into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use nth_curios::Curio;
    use nth_session::Session;

    use super::{
        core::{Comment, Publish, Send, Target, TargetKind},
        *,
    };
    use crate::{
        app::tests::{Reply, Scripted, rows_of},
        curios::Root,
    };

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
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
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
    async fn the_tab_tick_polls_and_its_end_lands_through_one_step() {
        let (_dir, mut app, store, c) = fixture().await;
        send(&store, &c);
        let step = app.curios.next().await;
        assert!(matches!(step, CurioStep::PollDue));
        app.curio_step(step);
        assert!(app.curios.polling.is_running());
        let step = app.curios.next().await;
        assert!(matches!(step, CurioStep::Polled(_)));
        app.curio_step(step);
        // A child another test forks may hold the store's lock a moment.
        if app.curios.busy() {
            app.poll_curios_through().await;
        }
        assert!(app.inbox.wakes(), "the poll's feedback reached the inbox");
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
        assert!(!p.pending()[0]);
        assert!(app.curios.polling.is_running());
        app.poll_curios_through().await;
        let Input::Curios(p) = &app.input else {
            panic!("picker")
        };
        assert!(p.pending()[0], "the next tick marks it");
    }

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        dir
    }

    #[tokio::test]
    async fn the_poll_finds_the_repository_rather_than_a_cached_root() {
        let dir = repository();
        let stale = tempfile::tempdir().unwrap();
        let mut session = Session::new("glm", dir.path().to_path_buf());
        session.curio_root = Some(stale.path().to_path_buf());
        let state_root = session.state_root();
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        // Finding it runs git, which the loop never waits on.
        assert_eq!(*app.curios.root(), Root::Unknown(state_root));
        app.poll_curios_through().await;
        assert_eq!(
            *app.curios.root(),
            Root::At(dir.path().canonicalize().unwrap())
        );
    }

    #[tokio::test]
    async fn a_claim_replaces_a_stale_cached_root() {
        let dir = repository();
        let stale = tempfile::tempdir().unwrap();
        let mut session = Session::new("glm", dir.path().to_path_buf());
        session.prepare_curios().unwrap();
        session.curio_root = Some(stale.path().to_path_buf());
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.poll_curios_through().await;
        assert!(app.curios.delivery().store().is_some(), "claimed");
        assert_eq!(
            app.session.as_ref().unwrap().curio_root,
            Some(dir.path().canonicalize().unwrap())
        );
    }

    #[tokio::test]
    async fn resuming_a_session_in_another_repository_looks_for_its_root_afresh() {
        let first = repository();
        let second = repository();
        let session = Session::new("glm", first.path().to_path_buf());
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.poll_curios_through().await;
        assert_eq!(
            *app.curios.root(),
            Root::At(first.path().canonicalize().unwrap())
        );
        let other = Session::new("glm", second.path().to_path_buf());
        let state_root = other.state_root();
        app.resume(other);
        assert_eq!(*app.curios.root(), Root::Unknown(state_root));
        app.poll_curios_through().await;
        assert_eq!(
            *app.curios.root(),
            Root::At(second.path().canonicalize().unwrap())
        );
    }

    #[tokio::test]
    async fn opening_a_curio_before_the_first_poll_lands_says_so() {
        let dir = repository();
        let session = Session::new("glm", dir.path().to_path_buf());
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.open_curio(app.session_id.to_string(), "review".into(), None);
        assert_eq!(
            app.hint.as_deref(),
            Some("Curios is still looking for the repository")
        );
        assert!(!app.curios.opening.is_running());
    }

    #[tokio::test]
    async fn a_session_outside_a_repository_stops_polling() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new("glm", dir.path().to_path_buf());
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.poll_curios_through().await;
        assert_eq!(*app.curios.root(), Root::Outside);
        app.poll_curios();
        assert!(!app.curios.polling.is_running());
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
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
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
    async fn a_failing_acknowledgment_is_told_as_a_hint() {
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
        // The retry waits, so the next poll starts none.
        let started = app.curios.acknowledgments();
        app.poll_curios_through().await;
        assert_eq!(app.curios.acknowledgments(), started);
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
    async fn another_process_delivering_is_told_once() {
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
        let _other =
            nth_session::curios::CurioDelivery::claim(session.curio_store().unwrap()).unwrap();
        let mut app = App::bare(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
        app.poll_curios_through().await;
        assert!(app.curios.delivery().store().is_none());
        assert_eq!(
            app.hint.as_deref(),
            Some("Curios delivery is owned by another process")
        );
        app.hint = None;
        app.curios.claim_now();
        app.poll_curios_through().await;
        assert!(app.curios.delivery().store().is_none());
        assert!(app.hint.is_none(), "told once");
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
        assert_eq!(app.curios.hosts(), 0);

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
        assert_eq!(app.curios.hosts(), 1);
        assert!(!app.hint.as_deref().unwrap().contains("127.0.0.1"));

        app.open_curio(c.session_id.clone(), c.id.clone(), Some(1));
        let opened = app.curios.opening.join().await.unwrap();
        assert!(
            opened.as_ref().unwrap().host.is_none(),
            "the host is reused"
        );
        app.curio_opened(opened);
        assert_eq!(app.curios.hosts(), 1);
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
        assert_eq!(app.curios.hosts(), 1);
        app.hint = Some("x".repeat(200).into());
        rows_of(&mut app, 20);
        assert!(app.areas.find(&crate::app::mouse::Target::Curio).is_none());
        app.start_fresh_session();
        assert_eq!(app.curios.hosts(), 0);
        assert!(app.curios.last().is_none());
    }

    #[tokio::test]
    async fn pr_and_curio_hit_areas_are_independent() {
        let (_dir, mut app, _store, _c) = fixture().await;
        app.place = "repo".into();
        app.git.status = Some(crate::git::GitStatus {
            branch: Some("main".into()),
            ..Default::default()
        });
        app.git.pr = Some(crate::git::Pr {
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
