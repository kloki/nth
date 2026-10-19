//! The plan tab, wired to the session's plan file: read in the background
//! after anything that may have written it, shown in a tab while it
//! exists, and approved with `/approve`.

use std::path::{Path, PathBuf};

use nth_protocol::Mode;
use nth_session::plan;
use tokio::task::JoinError;

use super::{App, Queued, Tab, content::Change, job::Job};
use crate::plan::PlanView;

/// The session's plan file and what the plan tab shows of it.
pub(super) struct PlanTab {
    pub(super) view: PlanView,
    path: PathBuf,
    pub(super) reading: Job<Option<String>>,
    /// The next read is of a session just opened, so it marks nothing as
    /// changed.
    settle: bool,
    /// A turn that ended in plan mode, waiting for the plan to be read to
    /// say whether it left a plan to approve.
    pub(super) notice: Option<nth_notify::Context>,
}

impl PlanTab {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            view: PlanView::default(),
            path,
            reading: Job::default(),
            settle: true,
            notice: None,
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Points the tab at the plan of the session the app is on, and reads
    /// it with nothing marked as changed.
    pub(super) fn for_session(&mut self, path: PathBuf) {
        self.path = path;
        self.settle = true;
        self.read();
    }

    /// Reads the plan file in the background; the tools may just have
    /// written it.
    pub(super) fn read(&mut self) {
        let path = self.path.clone();
        self.reading.start_or_queue(|_| {
            // A plan that can't be read shows as none, rather than as an
            // error on every turn.
            tokio::spawn(async move { tokio::fs::read_to_string(path).await.ok() })
        });
    }

    /// The read running, once it is back; never while none runs.
    pub(super) async fn next(&mut self) -> Result<Option<String>, JoinError> {
        self.reading.join().await
    }

    /// Takes in the plan as read, and says what that did to it and
    /// whether another read was asked for meanwhile.
    fn took(&mut self, text: Option<String>) -> (Change, bool) {
        let settle = std::mem::take(&mut self.settle);
        let created = !settle && !self.view.exists() && text.is_some();
        if settle {
            self.view.settle(text);
        } else {
            self.view.update(text);
        }
        let change = if created {
            Change::Created
        } else if self.view.exists() {
            Change::Present
        } else {
            Change::Gone
        };
        (change, self.reading.take_again())
    }

    /// The plan file as the tab names it: relative to `cwd`.
    fn place(&self, cwd: &Path) -> String {
        self.path
            .strip_prefix(cwd)
            .unwrap_or(&self.path)
            .display()
            .to_string()
    }

    /// What the model gets for `/approve`.
    pub(super) fn approval(&self) -> String {
        format!(
            "/approve{}{}\n</system-reminder>",
            REMINDER_OPEN,
            plan::approved(&self.path)
        )
    }
}

impl App {
    /// The plan as read: its tab shows while the file exists, and the
    /// label says what the latest turn changed.
    pub(super) fn plan_read(&mut self, text: Option<String>) {
        let (change, again) = self.plan.took(text);
        self.tab_read(Tab::Plan, change);
        if again {
            self.plan.read();
        } else {
            // Only once the read that came after the turn is back.
            self.notify_plan_read();
        }
    }

    /// The plan file as the tab names it: relative to the working
    /// directory.
    pub(super) fn plan_place(&self) -> String {
        self.plan.place(&self.cwd)
    }

    /// `/approve`: act on the plan. The mode switches now and the chat
    /// shows; the approval runs as its own turn, queued behind one that is
    /// running.
    pub(super) fn approve(&mut self) {
        if !self.plan.view.exists() {
            self.hint = Some("no plan to approve".into());
            return;
        }
        self.set_mode(Mode::Act);
        self.plan.view.approve();
        // Acting happens in the chat.
        self.content.select(0);
        self.send(Queued::Approve);
    }
}

/// The approval goes to the model inside a reminder, so the chat shows only
/// `/approve`, live and when the session is resumed.
const REMINDER_OPEN: &str = "\n\n<system-reminder>\n";

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_protocol::Message;
    use nth_session::Session;
    use ratatui::style::Color;

    use super::*;
    use crate::{
        app::{
            keys::Action,
            tests::{Idle, tab_colour},
        },
        chat::Entry,
    };

    /// An app on a session in a temporary directory, so its plan file is
    /// real, and that directory.
    fn app_with_plan_dir() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = Session::new("glm", dir.path().to_path_buf());
        let app = App::new(session, Arc::new(Idle), Arc::new(Vec::new()));
        (app, dir)
    }

    async fn read(app: &mut App) {
        let text = app.plan.reading.join().await.expect("read");
        app.plan_read(text);
    }

    fn write_plan(app: &App, text: &str) {
        std::fs::create_dir_all(app.plan.path().parent().expect("dir")).expect("dirs");
        std::fs::write(app.plan.path(), text).expect("writes");
    }

    #[tokio::test]
    async fn the_first_plan_shows_and_a_revision_only_opens_the_tab() {
        let (mut app, _dir) = app_with_plan_dir();
        app.plan.read();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat]);

        write_plan(&app, "# Plan\nstep\n");
        app.plan.read();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Plan]);
        assert_eq!(app.content.active(), Tab::Plan, "the first plan shows");
        assert!(app.flash.is_some(), "and flashes");
        assert_eq!(
            app.tab_label(Tab::Plan),
            "plan",
            "a first plan has no marks"
        );

        app.content.select(0);
        app.plan.view.turn_started();
        write_plan(&app, "# Plan\nstep 1\n");
        app.plan.read();
        read(&mut app).await;
        assert_eq!(
            app.content.active(),
            Tab::Chat,
            "a revision leaves the chat"
        );
        assert_eq!(app.tab_label(Tab::Plan), "plan +1 -1");
    }

    #[tokio::test]
    async fn a_plan_there_on_start_opens_its_tab_unfocused() {
        let (mut app, _dir) = app_with_plan_dir();
        write_plan(&app, "# Plan\n");
        app.plan.read();
        read(&mut app).await;

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Plan]);
        assert_eq!(app.content.active(), Tab::Chat);
    }

    #[tokio::test]
    async fn the_tab_closes_and_plan_or_a_revision_brings_it_back() {
        let (mut app, _dir) = app_with_plan_dir();
        app.plan.read();
        read(&mut app).await;
        write_plan(&app, "# Plan\n");
        app.plan.read();
        read(&mut app).await;
        assert_eq!(app.content.active(), Tab::Plan);

        app.close_content();
        assert_eq!(app.content.tabs(), [Tab::Chat], "dismissed");

        app.prompt.insert_str("/plan");
        app.apply(Action::Submit);
        assert_eq!(app.content.active(), Tab::Plan, "/plan shows it");

        app.close_content();
        write_plan(&app, "# Plan\nstep\n");
        app.plan.read();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Plan]);
        assert_eq!(app.content.active(), Tab::Chat, "a revision, unfocused");
    }

    #[tokio::test]
    async fn the_tab_shows_the_plan_with_its_changes() {
        let (mut app, _dir) = app_with_plan_dir();
        app.plan.read();
        read(&mut app).await;
        write_plan(&app, "# Plan\nstep\n");
        app.plan.read();
        read(&mut app).await;
        app.plan.view.turn_started();
        write_plan(&app, "# Plan\nstep 1\n");
        app.plan.read();
        read(&mut app).await;
        // The plan as drawn, not the blank it comes in after.
        app.flash = None;

        let rows = crate::app::tests::rows(&mut app);

        assert!(rows[0].starts_with("  › chat [≡ plan +1 -1] "), "{rows:#?}");
        assert!(rows[2].starts_with(" .nth/plans/"));
        assert_eq!(rows[4].trim_end(), "   Plan");
        assert_eq!(rows[5].trim_end(), " - step");
        assert_eq!(rows[6].trim_end(), " + step 1");
    }

    #[tokio::test]
    async fn a_plan_there_on_resume_shows_no_changes() {
        let (mut app, _dir) = app_with_plan_dir();
        write_plan(&app, "# Plan\n");
        app.plan.for_session(app.plan.path().to_path_buf());
        read(&mut app).await;

        assert_eq!(app.tab_label(Tab::Plan), "plan");
    }

    #[test]
    fn approve_without_a_plan_only_hints() {
        let (mut app, _dir) = app_with_plan_dir();
        app.prompt.insert_str("/approve");
        app.apply(Action::Submit);

        assert_eq!(app.hint.as_deref(), Some("no plan to approve"));
        assert!(!app.is_busy());
    }

    #[tokio::test]
    async fn approve_acts_on_the_plan_and_clears_its_marks() {
        let (mut app, _dir) = app_with_plan_dir();
        app.set_mode(Mode::Plan);
        app.plan.read();
        read(&mut app).await;
        write_plan(&app, "# Plan\n");
        app.plan.read();
        read(&mut app).await;
        assert_eq!(app.content.active(), Tab::Plan);

        app.prompt.insert_str("/approve");
        app.apply(Action::Submit);

        assert_eq!(app.content.active(), Tab::Chat, "acting shows in the chat");
        assert_eq!(app.mode, Mode::Act);
        assert_eq!(app.tab_label(Tab::Plan), "plan");
        assert_eq!(tab_colour(&mut app, "≡ plan"), Color::Green, "approved");
        assert!(app.is_busy());
        app.interrupt();
        let ended = app.turn.join().await.expect("ends");
        app.end_turn(ended);
        let session = app.session.as_ref().expect("came back");
        let Some(Message::User(sent)) = session.messages.last() else {
            panic!("sent");
        };
        assert!(
            sent.starts_with(&format!(
                "/approve\n\n<system-reminder>\nThe plan at {} has been approved",
                app.plan.path().display()
            )),
            "{sent}"
        );
        assert_eq!(
            app.chat.transcript.entries().next(),
            Some(&Entry::User("/approve".into()))
        );
    }
}
