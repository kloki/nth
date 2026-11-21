//! `/rename <title>`: naming the session by hand. Between turns the name
//! goes on the session and is saved; while a turn holds the session it
//! waits and is shown at once, and goes on when the turn hands the session
//! back, after anything the turn named it, so yours wins.

use super::App;

impl App {
    pub(super) fn rename_session(&mut self, text: &str) {
        if text.trim().is_empty() {
            self.hint = Some("usage: /rename <title>".into());
            return;
        }
        let name = match nth_session::title::validate(text) {
            Ok(name) => name,
            Err(e) => {
                self.hint = Some(format!("could not rename session: {e}").into());
                return;
            }
        };
        match &mut self.session {
            Some(session) => {
                self.hint = Some(format!("renamed {name}").into());
                session.name = Some(name);
                self.save_session();
            }
            None => {
                self.hint = Some(format!("rename pending: {name}").into());
                self.pending_name = Some(name);
            }
        }
    }

    /// A failed task took its session with it. Rename only that session's
    /// last save, never the fresh session the app starts afterward.
    pub(super) async fn recover_pending_name(&mut self) -> Option<Result<String, String>> {
        let name = self.pending_name.take()?;
        let id = self.session_id;
        let result = match &self.store {
            Some(store) => store.rename(id, &name).await.map(|_| ()),
            None => {
                return Some(Err(format!(
                    "could not rename session {id} to {name}: no session store"
                )));
            }
        };
        Some(match result {
            Ok(()) => Ok(format!("renamed saved session {id} to {name}")),
            Err(e) => Err(format!("could not rename session {id} to {name}: {e}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use nth_protocol::Message;
    use nth_session::{Session, Store};

    use crate::app::{
        App, AppConfig,
        tests::{Scripted, rows},
    };

    /// An app on `provider` in `dir`, whose sessions save there too.
    fn saving(dir: &Path, provider: Arc<Scripted>, said: bool) -> App {
        let mut session = Session::new("glm", dir.into());
        if said {
            session.messages.push(Message::User("first".into()));
        }
        let mut config = AppConfig::test(session, provider);
        config.store = Some(Store::at(dir.join("sessions")));
        App::new(config)
    }

    async fn saved_name(app: &mut App, dir: &Path) -> Option<String> {
        app.session_saving
            .join()
            .await
            .expect("saves")
            .expect("saved");
        let store = Store::at(dir.join("sessions"));
        store.load(app.session_id).await.expect("loads").name
    }

    #[tokio::test]
    async fn a_rename_between_turns_is_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = saving(dir.path(), Arc::new(Scripted::idle()), true);
        app.prompt.insert_str("/rename  Fix the build ");
        app.submit();

        assert_eq!(app.session_title().as_deref(), Some("Fix the build"));
        assert!(rows(&mut app)[0].contains("Fix the build"));
        assert_eq!(
            saved_name(&mut app, dir.path()).await.as_deref(),
            Some("Fix the build")
        );
    }

    #[tokio::test]
    async fn a_title_that_cannot_name_a_session_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = saving(dir.path(), Arc::new(Scripted::idle()), true);
        app.prompt.insert_str("/rename");
        app.submit();
        assert!(
            app.hint.as_deref().unwrap().contains("usage"),
            "{:?}",
            app.hint
        );
        app.rename_session(&"x".repeat(101));
        assert!(app.hint.as_deref().unwrap().contains("at most 100"));
        assert_eq!(app.session.as_ref().unwrap().name, None);
    }

    #[tokio::test]
    async fn a_rename_during_a_cancelled_turn_is_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = saving(dir.path(), Arc::new(Scripted::hang()), false);

        app.prompt.insert_str("the build fails");
        app.submit();
        assert!(app.is_busy());
        assert_eq!(app.session_title().as_deref(), Some("the build fails"));
        app.prompt.insert_str("/rename Mine");
        app.submit();
        assert_eq!(app.session_title().as_deref(), Some("Mine"), "at once");

        app.interrupt();
        let ended = app.turn.join().await.expect("ends");
        app.end_turn(ended);
        assert_eq!(app.session.as_ref().unwrap().name.as_deref(), Some("Mine"));
        assert_eq!(app.session_title().as_deref(), Some("Mine"));
        assert_eq!(
            saved_name(&mut app, dir.path()).await.as_deref(),
            Some("Mine")
        );
    }

    #[tokio::test]
    async fn quitting_during_a_renamed_turn_saves_the_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = saving(dir.path(), Arc::new(Scripted::hang()), false);
        app.prompt.insert_str("first prompt");
        app.submit();
        app.rename_session("Exit name");

        app.stop_background_for_exit().await;

        let store = Store::at(dir.path().join("sessions"));
        assert_eq!(
            store.load(app.session_id).await.unwrap().name.as_deref(),
            Some("Exit name")
        );
    }

    #[tokio::test]
    async fn failed_turn_renames_its_saved_session_not_the_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = saving(dir.path(), Arc::new(Scripted::hang()), true);
        let store = Store::at(dir.path().join("sessions"));
        let id = app.session_id;
        store.save(app.session.as_ref().unwrap()).await.unwrap();
        app.start_turn("go".into());
        app.rename_session("Recovered");
        app.turn.abort_handle().unwrap().abort();
        let error = match app.turn.join().await {
            Err(error) => error,
            Ok(_) => panic!("turn should have been aborted"),
        };
        app.turn_task_failed(error).await;

        assert_eq!(
            store.load(id).await.unwrap().name.as_deref(),
            Some("Recovered")
        );
        assert_ne!(app.session_id, id);
        assert_eq!(app.session_title(), None);
        assert_eq!(app.pending_name, None);
    }

    #[tokio::test]
    async fn failed_first_turn_reports_the_rename_could_not_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = saving(dir.path(), Arc::new(Scripted::hang()), false);
        let id = app.session_id;
        app.start_turn("go".into());
        app.rename_session("Lost name");
        app.turn.abort_handle().unwrap().abort();
        let error = match app.turn.join().await {
            Err(error) => error,
            Ok(_) => panic!("turn should have been aborted"),
        };
        app.turn_task_failed(error).await;

        let entries = format!("{:?}", app.chat.transcript.entries().collect::<Vec<_>>());
        assert!(entries.contains("could not rename session"), "{entries}");
        assert!(entries.contains("Lost name"), "{entries}");
        assert_ne!(app.session_id, id);
        assert_eq!(app.session_title(), None);
        assert_eq!(app.pending_name, None);
    }

    #[tokio::test]
    async fn quitting_after_a_failed_turn_recovers_the_pending_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = saving(dir.path(), Arc::new(Scripted::hang()), true);
        let store = Store::at(dir.path().join("sessions"));
        let id = app.session_id;
        store.save(app.session.as_ref().unwrap()).await.unwrap();
        app.start_turn("go".into());
        app.rename_session("On exit");
        app.turn.abort_handle().unwrap().abort();

        app.stop_background_for_exit().await;

        assert_eq!(
            store.load(id).await.unwrap().name.as_deref(),
            Some("On exit")
        );
        assert!(
            app.exit_notes
                .iter()
                .any(|note| note.contains("renamed saved session"))
        );
    }
}
