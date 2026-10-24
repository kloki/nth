//! Repository review documents. Capabilities remain inside this front-end.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{
    CurioDelivery,
    nth_curios::{self as core, Curio, Host},
};
use nth_session::Session;
use ratatui::{
    Frame,
    layout::Rect,
    style::Color,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{App, input::Input, keys::Action};
use crate::{
    fuzzy::Filter,
    theme::{dim, panel_row, panel_title},
};

/// How long after a refused claim the delivery lease is tried again.
const CLAIM_RETRY: Duration = Duration::from_secs(10);

pub(super) struct Curios {
    root: Option<PathBuf>,
    items: Vec<Curio>,
    delivery: Option<CurioDelivery>,
    /// When to try for the delivery lease again: another process that holds
    /// it may exit.
    next_claim: Instant,
    /// Whether the hint said another process owns delivery.
    told_owned: bool,
    /// Whether the session's receipts are saved, so its feedback may be
    /// acknowledged: false after a save that failed, until the next works.
    pub(super) saved: bool,
    hosts: BTreeMap<(String, String), Host>,
    pub(super) opening: super::Job<core::Result<(String, String, Host)>>,
    opening_revision: Option<u64>,
    last: Option<(String, String)>,
}

impl Curios {
    pub fn new(session: &Session) -> Self {
        Self {
            root: session
                .curio_root
                .clone()
                .or_else(|| core::repository_root(&session.cwd).ok()),
            items: Vec::new(),
            delivery: None,
            next_claim: Instant::now(),
            told_owned: false,
            saved: true,
            hosts: BTreeMap::new(),
            opening: Default::default(),
            opening_revision: None,
            last: None,
        }
    }
    pub fn invalidate(&mut self) {
        self.hosts.clear();
        self.opening = Default::default();
        self.opening_revision = None;
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
        self.areas.curio = None;
        if matches!(self.input, Input::Curios(_)) {
            self.input = Input::Prompt;
        }
        self.poll_curios();
    }

    pub(super) fn poll_curios(&mut self) {
        let Some(root) = &self.curios.root else {
            return;
        };
        self.curios.items = core::discover(root).unwrap_or_default();
        // The store has no update timestamp API; use its durable state mtime.
        self.curios.items.sort_by_key(|c| {
            std::fs::metadata(
                root.join(".nth/curios")
                    .join(&c.session_id)
                    .join(&c.id)
                    .join("revisions")
                    .join(c.current_revision.to_string())
                    .join("index.html"),
            )
            .and_then(|m| m.modified())
            .ok()
        });
        self.curios.items.reverse();
        if self.curios.items.is_empty() {
            self.areas.curio = None;
        }
        // Feedback exists only once this session published, so its folder
        // is waited for rather than made.
        if self.curios.delivery.is_none()
            && Instant::now() >= self.curios.next_claim
            && let Some(session) = &mut self.session
            && root
                .join(".nth/curios")
                .join(session.id.to_string())
                .is_dir()
        {
            self.curios.next_claim = Instant::now() + CLAIM_RETRY;
            if session.prepare_curios().is_ok() {
                match session.curios().claim_delivery() {
                    Ok(delivery) => self.curios.delivery = Some(delivery),
                    Err(_) if !self.curios.told_owned => {
                        self.curios.told_owned = true;
                        self.hint = Some("Curios delivery is owned by another process".into())
                    }
                    Err(_) => {}
                }
            }
        }
        let Some(delivery) = &self.curios.delivery else {
            return;
        };
        // While a turn runs the session is away; its turn hands over what
        // the inbox holds and its receipts come back with it.
        let recorded = self
            .session
            .as_ref()
            .map(|s| s.curio_receipts.clone())
            .unwrap_or_default();
        match delivery.poll(&self.inbox, &recorded) {
            Ok(polled) => {
                // A batch posted while a turn ran that the turn recorded
                // anyway would otherwise wake the model with nothing.
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
                // An acknowledgment that failed after a save is caught up
                // here, so the viewer stops saying the feedback is on its way.
                if polled.unacknowledged
                    && self.curios.saved
                    && self.store.is_some()
                    && let Some(session) = &self.session
                {
                    let _ = session.acknowledge_curios();
                }
            }
            Err(_) => self.hint = Some("could not read queued Curios feedback".into()),
        }
    }

    pub(super) fn open_curios_picker(&mut self) {
        self.poll_curios();
        self.completion = None;
        let items = self.curios.items.clone();
        let pending = items
            .iter()
            .map(|c| {
                self.curios
                    .root
                    .as_ref()
                    .and_then(|r| core::Store::new(r, &c.session_id).ok())
                    .and_then(|s| s.batches(true).ok())
                    .is_some_and(|bs| bs.iter().any(|b| b.curio_id == c.id))
            })
            .collect();
        self.input = Input::Curios(Picker {
            shown: (0..items.len()).collect(),
            items,
            pending,
            filter: Filter::default(),
            selected: 0,
        });
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

    pub(super) fn open_curio(&mut self, owner: String, id: String, revision: Option<u64>) {
        let key = (owner.clone(), id.clone());
        let Some(root) = self.curios.root.clone() else {
            self.hint = Some("Curios requires a Git repository".into());
            return;
        };
        if self.curios.hosts.contains_key(&key) {
            if core::Store::new(&root, &owner)
                .and_then(|store| store.snapshot(&id, revision))
                .is_err()
            {
                self.hint = Some("could not open Curio revision".into());
                return;
            }
            self.curios.last = Some(key);
            self.present_curio(revision);
            return;
        }
        self.curios.opening_revision = revision;
        self.curios.opening.start(|_| {
            tokio::spawn(async move {
                let store = core::Store::new(&root, &owner)?;
                store.snapshot(&id, revision)?;
                let host = Host::start(store, &id).await?;
                Ok((owner, id, host))
            })
        });
    }

    pub(super) fn curio_opened(&mut self, opened: core::Result<(String, String, Host)>) {
        let revision = self.curios.opening_revision.take();
        match opened {
            Ok((owner, id, host)) => {
                let key = (owner, id);
                self.curios.hosts.insert(key.clone(), host);
                self.curios.last = Some(key);
                self.present_curio(revision);
            }
            Err(_) => self.hint = Some("could not host Curio".into()),
        }
    }

    fn present_curio(&mut self, revision: Option<u64>) {
        let Some(key) = &self.curios.last else {
            return;
        };
        let Some(host) = self.curios.hosts.get(key) else {
            return;
        };
        // Opener errors may contain their arguments: never render them here.
        let url = revision.map_or_else(|| host.url(), |n| host.url_for_revision(n));
        let opened = (self.opener)(&url).is_ok();
        self.hint = Some(
            if !opened {
                "could not open Curio · /curios then ctrl+y to copy link"
            } else if key.0 != self.session_id.to_string() {
                "opened Curio · feedback stays queued for its owning session"
            } else {
                "opened Curio · /curios then ctrl+y to copy link"
            }
            .into(),
        );
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::{
        core::{Comment, Publish, Send, Target, TargetKind},
        *,
    };
    use crate::app::tests::{Reply, Scripted, rows_of};

    #[test]
    fn polling_makes_no_curios_folder_for_a_session_that_never_published() {
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
        app.poll_curios();
        app.poll_curios();
        assert!(!dir.path().join(".nth/curios").exists());
    }

    fn fixture() -> (tempfile::TempDir, App, core::Store, Curio) {
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
        app.poll_curios();
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
        let (_dir, mut app, store, c) = fixture();
        assert!(!app.inbox.wakes());
        send(&store, &c);
        app.poll_curios();
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
        app.poll_curios();
        assert!(!app.inbox.wakes());
    }

    #[tokio::test]
    async fn an_unacknowledged_receipt_is_caught_up_without_waking() {
        let (dir, mut app, store, c) = fixture();
        send(&store, &c);
        app.poll_curios();
        app.notices_due();
        let ended = app.turn.join().await.unwrap();
        // Without a store nothing is saved, so nothing is acknowledged.
        app.end_turn(ended);
        assert_eq!(store.batches(true).unwrap().len(), 1);
        app.store = Some(Arc::new(nth_session::Store::at(
            dir.path().join("sessions"),
        )));
        app.poll_curios();
        assert!(app.notices_due.is_none());
        assert!(!app.inbox.wakes());
        assert!(store.batches(true).unwrap().is_empty());
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
        app.poll_curios();
        assert!(app.curios.delivery.is_none());
        assert!(app.hint.as_deref().unwrap().contains("another process"));
        drop(other);
        app.poll_curios();
        assert!(app.curios.delivery.is_none(), "retries wait a while");
        // A child another test forks shares the lock until it execs.
        for _ in 0..50 {
            app.curios.next_claim = Instant::now();
            app.poll_curios();
            if app.curios.delivery.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.curios.delivery.is_some());
    }

    #[tokio::test]
    async fn held_and_busy_feedback_does_not_preempt() {
        let (_dir, mut app, store, c) = fixture();
        app.hold_notices = true;
        send(&store, &c);
        app.poll_curios();
        app.notices_due();
        assert!(!app.is_busy());
        assert!(app.inbox.wakes());
        app.hold_notices = false;
        let provider = Arc::new(Scripted::replying([Reply::Hang]));
        app.provider = provider.clone();
        app.start_turn("work".into());
        provider.asked.notified().await;
        app.poll_curios();
        app.notices_due();
        assert!(app.inbox.prompts_waiting().is_none());
        assert!(app.is_busy());
        app.turn.cancel();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
    }

    #[tokio::test]
    async fn requested_revision_is_opened_and_validated_with_new_and_reused_hosts() {
        let (_dir, mut app, store, c) = fixture();
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
        assert!(!app.curios.opening.is_running());
        assert!(!app.hint.as_deref().unwrap().contains("127.0.0.1"));
        app.opener = |_| panic!("invalid revision must not reach the reused host opener");
        for revision in [0, 99] {
            app.open_curio(c.session_id.clone(), c.id.clone(), Some(revision));
            assert_eq!(app.hint.as_deref(), Some("could not open Curio revision"));
        }
        app.opener = |url| {
            assert!(!url.contains("?revision="));
            Ok(())
        };
        app.open_curio(c.session_id, c.id, None);
        assert!(app.hint.as_deref().unwrap().starts_with("opened Curio"));
    }

    #[tokio::test]
    async fn status_click_opener_failure_clipping_and_session_invalidation() {
        let (_dir, mut app, _store, _c) = fixture();
        app.place = "repo".into();
        let rows = rows_of(&mut app, 100);
        assert!(rows[14].contains(nth_icons::icons().curio));
        let rect = app.areas.curio.unwrap();
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
        app.place = "x".repeat(200);
        rows_of(&mut app, 20);
        assert!(app.areas.curio.is_none());
        app.start_fresh_session();
        assert!(app.curios.hosts.is_empty());
        assert!(app.curios.last.is_none());
    }

    #[tokio::test]
    async fn other_owner_is_discovered_but_never_delivers_here() {
        let (_dir, mut app, store, c) = fixture();
        app.resume(Session::new("glm", app.cwd.clone()));
        send(&store, &c);
        app.poll_curios();
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
        let (_dir, mut app, _store, _c) = fixture();
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
        let pr = app.areas.pr_link.unwrap();
        let curio = app.areas.curio.unwrap();
        assert!(!pr.intersects(curio));
        // The space before the icon counts, so a near miss still opens it.
        assert_eq!(curio.width, 2);
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
        assert!(app.areas.pr_link.is_none());
    }

    #[tokio::test]
    async fn successful_session_save_acknowledges_receipt() {
        let (dir, mut app, store, c) = fixture();
        let saved = nth_session::Store::at(dir.path().join("sessions"));
        app = app.with_store(saved);
        send(&store, &c);
        app.poll_curios();
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
