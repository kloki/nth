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
        self.hint = Some(format!("renamed {name}").into());
        match &mut self.session {
            Some(session) => {
                session.name = Some(name);
                self.save_session();
            }
            None => self.pending_name = Some(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use nth_protocol::Message;
    use nth_session::{Session, Store};

    use crate::app::{
        App, AppConfig,
        tests::{Reply, Scripted, rows},
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

    /// A name you give while the turn runs is shown at once and outlasts
    /// the one the turn's aside made up.
    #[tokio::test]
    async fn a_rename_during_a_turn_wins_over_the_generated_title() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The aside and the turn both get this reply.
        let provider = Arc::new(Scripted::default().with_fallback(Reply::text("Generated")));
        let mut app = saving(dir.path(), provider.clone(), false);
        app.env.auto_title = true;

        app.prompt.insert_str("the build fails");
        app.submit();
        assert!(app.is_busy());
        assert_eq!(app.session_title().as_deref(), Some("the build fails"));
        app.prompt.insert_str("/rename Mine");
        app.submit();
        assert_eq!(app.session_title().as_deref(), Some("Mine"), "at once");

        let ended = app.turn.join().await.expect("ends");
        assert_eq!(provider.sent().len(), 2, "the turn and the aside");
        app.end_turn(ended);
        assert_eq!(app.session.as_ref().unwrap().name.as_deref(), Some("Mine"));
        assert_eq!(app.session_title().as_deref(), Some("Mine"));
        assert_eq!(
            saved_name(&mut app, dir.path()).await.as_deref(),
            Some("Mine")
        );
    }
}
