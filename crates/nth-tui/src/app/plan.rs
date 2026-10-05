//! The plan tab, wired to the session's plan file: read in the background
//! after anything that may have written it, shown in a tab while it
//! exists, and approved with `/approve`.

use std::path::PathBuf;

use nth_protocol::Mode;
use nth_session::plan;

use super::{App, Queued, Tab};

impl App {
    /// Points the plan tab at the plan of the session the app is on, and
    /// reads it with nothing marked as changed.
    pub(super) fn plan_for_session(&mut self, path: PathBuf) {
        self.plan_path = path;
        self.plan_settle = true;
        self.read_plan();
    }

    /// Reads the plan file in the background; the tools may just have
    /// written it.
    pub(super) fn read_plan(&mut self) {
        let path = self.plan_path.clone();
        self.plan_reading.start_or_queue(|_| {
            // A plan that can't be read shows as none, rather than as an
            // error on every turn.
            tokio::spawn(async move { tokio::fs::read_to_string(path).await.ok() })
        });
    }

    pub(super) fn plan_read(&mut self, text: Option<String>) {
        let settle = std::mem::take(&mut self.plan_settle);
        let created = !settle && !self.plan.exists() && text.is_some();
        if settle {
            self.plan.settle(text);
        } else {
            self.plan.update(text);
        }
        if created {
            // The first plan is what you will want to read.
            self.content.open(Tab::Plan);
        } else if self.plan.exists() {
            // A revision opens the tab unfocused, so the reply the model is
            // writing stays in view; the tab's label says what changed.
            self.content.add(Tab::Plan);
        } else {
            self.content.remove(Tab::Plan);
        }
        if self.plan_reading.take_again() {
            self.read_plan();
        }
    }

    /// The plan file as the tab names it: relative to the working
    /// directory.
    pub(super) fn plan_place(&self) -> String {
        self.plan_path
            .strip_prefix(&self.cwd)
            .unwrap_or(&self.plan_path)
            .display()
            .to_string()
    }

    /// `/approve`: act on the plan. The mode switches now and the chat
    /// shows; the approval runs as its own turn, queued behind one that is
    /// running.
    pub(super) fn approve(&mut self) {
        if !self.plan.exists() {
            self.hint = Some("no plan to approve".into());
            return;
        }
        self.set_mode(Mode::Act);
        self.plan.accept();
        // Acting happens in the chat.
        self.content.select(0);
        self.send(Queued::Approve);
    }

    /// What the model gets for `/approve`.
    pub(super) fn approval(&self) -> String {
        format!(
            "/approve{}{}\n</system-reminder>",
            REMINDER_OPEN,
            plan::approved(&self.plan_path)
        )
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

    use super::*;
    use crate::{
        app::{keys::Action, tests::Idle},
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
        let text = app.plan_reading.join().await.expect("read");
        app.plan_read(text);
    }

    fn write_plan(app: &App, text: &str) {
        std::fs::create_dir_all(app.plan_path.parent().expect("dir")).expect("dirs");
        std::fs::write(&app.plan_path, text).expect("writes");
    }

    #[tokio::test]
    async fn the_first_plan_shows_and_a_revision_only_opens_the_tab() {
        let (mut app, _dir) = app_with_plan_dir();
        app.read_plan();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat]);

        write_plan(&app, "# Plan\nstep\n");
        app.read_plan();
        read(&mut app).await;
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Plan]);
        assert_eq!(app.content.active(), Tab::Plan, "the first plan shows");
        assert_eq!(
            app.tab_label(Tab::Plan),
            "plan",
            "a first plan has no marks"
        );

        app.content.select(0);
        app.plan.turn_started();
        write_plan(&app, "# Plan\nstep 1\n");
        app.read_plan();
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
        app.read_plan();
        read(&mut app).await;

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Plan]);
        assert_eq!(app.content.active(), Tab::Chat);
    }

    #[tokio::test]
    async fn the_tab_shows_the_plan_with_its_changes() {
        let (mut app, _dir) = app_with_plan_dir();
        app.read_plan();
        read(&mut app).await;
        write_plan(&app, "# Plan\nstep\n");
        app.read_plan();
        read(&mut app).await;
        app.plan.turn_started();
        write_plan(&app, "# Plan\nstep 1\n");
        app.read_plan();
        read(&mut app).await;

        let rows = crate::app::tests::rows(&mut app);

        assert!(rows[0].starts_with(" 1 chat  2 plan +1 -1 "), "{rows:#?}");
        assert!(rows[2].starts_with(" .nth/plans/"));
        assert_eq!(rows[4].trim_end(), "   Plan");
        assert_eq!(rows[5].trim_end(), " - step");
        assert_eq!(rows[6].trim_end(), " + step 1");
    }

    #[tokio::test]
    async fn a_plan_there_on_resume_shows_no_changes() {
        let (mut app, _dir) = app_with_plan_dir();
        write_plan(&app, "# Plan\n");
        app.plan_for_session(app.plan_path.clone());
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
        app.read_plan();
        read(&mut app).await;
        write_plan(&app, "# Plan\n");
        app.read_plan();
        read(&mut app).await;
        assert_eq!(app.content.active(), Tab::Plan);

        app.prompt.insert_str("/approve");
        app.apply(Action::Submit);

        assert_eq!(app.content.active(), Tab::Chat, "acting shows in the chat");
        assert_eq!(app.mode, Mode::Act);
        assert_eq!(app.tab_label(Tab::Plan), "plan");
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
                app.plan_path.display()
            )),
            "{sent}"
        );
        assert_eq!(
            app.chat.transcript.entries().next(),
            Some(&Entry::User("/approve".into()))
        );
    }
}
