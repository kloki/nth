//! Working directories added with `/add-dir`: adding one, and keeping the
//! session and the status bar in step with the ones there are.

use std::path::PathBuf;

use nth_session::store;

use super::App;
use crate::{command, status};

impl App {
    /// Adds the directory `arg` names to the session's working directories,
    /// as `/add-dir` does: the argument expanded (`~` for home, a relative
    /// path against the working directory) and canonicalized, and only when
    /// it is a directory that is not one of them already. A word on the
    /// status bar either way, never a turn. While a turn runs the session is
    /// in its task, so the directory is kept here and reaches it with the
    /// next one, as a switched model does.
    pub(super) fn add_dir(&mut self, arg: &str) {
        let arg = arg.trim();
        if arg.is_empty() {
            self.hint = Some("usage: /add-dir <directory>".into());
            return;
        }
        let spelled = command::expand(&self.cwd, self.home.as_deref(), arg);
        let dir: PathBuf = match std::fs::canonicalize(&spelled) {
            Ok(dir) if dir.is_dir() => dir,
            Ok(_) => {
                self.hint = Some(format!("not a directory: {}", spelled.display()));
                return;
            }
            Err(e) => {
                self.hint = Some(format!("cannot add {}: {e}", spelled.display()));
                return;
            }
        };
        // A directory inside one already there, or around one, would list
        // its files twice for `@` and tell the model of them twice.
        // Compared canonical, as `dir` is: on macOS `/var` is `/private/var`.
        let cwd = std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        let home = self.home.as_deref();
        let dirs = std::iter::once(&cwd).chain(&self.extra_dirs);
        let refused = dirs.enumerate().find_map(|(i, other)| {
            let place = status::place(other, home);
            match (i, dir == *other) {
                (0, true) => Some("already the working directory".to_string()),
                (_, true) => Some(format!("already added: {place}")),
                _ if dir.starts_with(other) => Some(format!("already in {place}")),
                _ if other.starts_with(&dir) => Some(format!("contains {place}")),
                _ => None,
            }
        });
        if refused.is_some() {
            self.hint = refused;
            return;
        }
        // The status bar shows the same place the hint names.
        self.hint = Some(format!(
            "added {}",
            status::place(&dir, self.home.as_deref())
        ));
        self.extra_dirs.push(dir.clone());
        // Between turns no turn's end saves it, so it is saved here, or
        // quitting now would lose it.
        if let Some(session) = &mut self.session {
            session.set_extra_dirs(self.extra_dirs.clone());
            self.save_session();
        }
        // The added directory's files are worth mentioning as much as the
        // working directory's are.
        self.index_files();
    }

    /// Saves the session between turns, in the background.
    fn save_session(&mut self) {
        let (Some(store), Some(session)) = (&self.store, &self.session) else {
            return;
        };
        let store = store.clone();
        let session = session.clone();
        self.session_saving
            .start_or_queue(|_| tokio::spawn(async move { store.save(&session).await }));
    }

    /// A save asked for mid-save runs now, with the session as it is.
    pub(super) fn session_saved(&mut self, saved: Result<(), store::Error>) {
        if self.session_saving.take_again() {
            self.save_session();
        }
        if let Err(e) = saved {
            self.chat
                .transcript
                .push_error(format!("session not saved: {e}"));
        }
    }

    /// `HOME`, as the status bar abbreviates the working directory and the
    /// ones added to it. The field is the app module's own; the status bar
    /// is a module over.
    pub(crate) fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use nth_protocol::Message;
    use nth_session::{Session, Store};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{Idle, rows},
    };

    /// An idle app in `dir`, which exists.
    fn app_in(dir: &Path) -> App {
        App::new(
            Session::new("glm", dir.to_path_buf()),
            Arc::new(Idle),
            Arc::new(Vec::new()),
        )
    }

    /// Adds `name` under `base`, and its path.
    fn dir(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// The app's buffer, drawn wide enough for a status line with a place
    /// and an added directory in it.
    fn wide_buffer(app: &mut App) -> Buffer {
        // macOS's temporary directories are long, and long again canonical.
        let mut terminal = Terminal::new(TestBackend::new(240, 16)).expect("test backend");
        terminal.draw(|frame| app.draw(frame)).expect("draws");
        terminal.backend().buffer().clone()
    }

    /// One row of `buffer` as a string.
    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    /// How the app names `path` in a hint or on the status bar: its place,
    /// canonicalized as `/add-dir` keeps it.
    fn place(app: &App, path: &Path) -> String {
        let path = path.canonicalize().expect("canonical");
        status::place(&path, app.home.as_deref())
    }

    #[tokio::test]
    async fn adds_a_directory_and_tells_the_model() {
        let base = tempfile::tempdir().expect("tempdir");
        let other = dir(base.path(), "other");
        let mut app = app_in(&dir(base.path(), "repo"));

        app.add_dir("../other");

        assert_eq!(app.extra_dirs, [other.canonicalize().expect("canonical")]);
        assert_eq!(
            app.hint.as_deref(),
            Some(format!("added {}", place(&app, &other)).as_str())
        );
        let session = app.session.as_ref().expect("idle");
        assert_eq!(session.extra_dirs, app.extra_dirs);
        let Message::System(prompt) = &session.messages[0] else {
            panic!("starts with the system prompt");
        };
        assert!(prompt.contains("<additional_directories>"), "{prompt}");
    }

    #[tokio::test]
    async fn the_status_bar_shows_the_added_directories() {
        let base = tempfile::tempdir().expect("tempdir");
        let other = dir(base.path(), "other");
        let mut app = app_in(&dir(base.path(), "repo"));

        app.add_dir("../other");
        let added = format!("+{}", place(&app, &other));

        let buffer = wide_buffer(&mut app);
        let line = row(&buffer, 14);
        assert!(line.contains(&added), "{line:?}");
        // The working directory and the added one are magenta, the model
        // bright white before them.
        let at = |needle: &str| u16::try_from(line.find(needle).expect(needle)).expect("fits");
        assert_eq!(buffer[(at("glm"), 14)].fg, Color::White);
        for needle in [app.place.as_str(), added.as_str()] {
            assert_eq!(buffer[(at(needle), 14)].fg, Color::Magenta, "{needle}");
        }
    }

    #[tokio::test]
    async fn refuses_what_is_not_a_directory_to_add() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = dir(base.path(), "repo");
        dir(base.path(), "other");
        let mut app = app_in(&repo);

        app.add_dir("");
        assert_eq!(app.hint.as_deref(), Some("usage: /add-dir <directory>"));

        app.add_dir("../missing");
        assert!(
            app.hint
                .as_deref()
                .is_some_and(|hint| hint.starts_with("cannot add ")),
            "{:?}",
            app.hint
        );

        std::fs::write(base.path().join("file"), "").expect("file");
        app.add_dir("../file");
        assert!(
            app.hint
                .as_deref()
                .is_some_and(|hint| hint.starts_with("not a directory: ")),
            "{:?}",
            app.hint
        );

        // The working directory is already where the session runs.
        app.add_dir(".");
        assert_eq!(app.hint.as_deref(), Some("already the working directory"));

        // One already added, added again.
        app.add_dir("../other");
        app.add_dir("../other");
        assert!(
            app.hint
                .as_deref()
                .is_some_and(|hint| hint.starts_with("already added")),
            "{:?}",
            app.hint
        );
        assert_eq!(app.extra_dirs.len(), 1, "nothing added by a refusal");

        // Inside the working directory or an added one, or around one.
        dir(&repo, "src");
        app.add_dir("src");
        assert_eq!(
            app.hint.as_deref(),
            Some(format!("already in {}", place(&app, &repo)).as_str())
        );
        dir(base.path(), "other/lib");
        app.add_dir("../other/lib");
        assert!(
            app.hint
                .as_deref()
                .is_some_and(|h| h.starts_with("already in "))
        );
        app.add_dir("..");
        assert_eq!(
            app.hint.as_deref(),
            Some(format!("contains {}", place(&app, &repo)).as_str())
        );
        assert_eq!(app.extra_dirs.len(), 1, "nothing added by a refusal");
    }

    #[tokio::test]
    async fn while_a_turn_runs_the_directory_reaches_the_next_one() {
        let base = tempfile::tempdir().expect("tempdir");
        let other = dir(base.path(), "other");
        let repo = dir(base.path(), "repo");
        let mut app = app_in(&repo);

        app.prompt.insert_str("go");
        app.submit();
        assert!(app.is_busy(), "the session is in its task");
        app.add_dir("../other");
        let added = app.extra_dirs.clone();

        // The turn fails (`Idle` never answers), and its session comes back
        // without the directory: it reaches the session with the next turn,
        // as a model switched mid-turn does.
        let ended = app.turn.join().await.expect("turn ends");
        app.end_turn(ended);
        assert!(
            app.session
                .as_ref()
                .expect("session back")
                .extra_dirs
                .is_empty()
        );

        app.prompt.insert_str("again");
        app.submit();
        let ended = app.turn.join().await.expect("turn ends");
        app.end_turn(ended);
        let session = app.session.as_ref().expect("session back");
        assert_eq!(session.extra_dirs, added);
        assert!(
            session
                .extra_dirs
                .contains(&other.canonicalize().expect("canonical"))
        );
    }

    #[tokio::test]
    async fn enter_and_tab_complete_and_run_the_command() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = dir(base.path(), "repo");
        dir(base.path(), "other");
        let mut app = app_in(&repo);

        for c in "/add-dir ../ot".chars() {
            app.apply(Action::Insert(c));
        }
        let row = rows(&mut app);
        assert!(
            row.iter().any(|row| row.contains("../other/")),
            "{row:?}: the directories under the one typed"
        );

        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "/add-dir ../other/");
        assert!(app.completion.is_none(), "filled in, as a mention is");

        app.apply(Action::Submit);
        assert!(app.prompt.is_empty());
        assert_eq!(
            app.hint.as_deref(),
            Some(format!("added {}", place(&app, &dir(base.path(), "other"))).as_str())
        );
    }

    #[tokio::test]
    async fn enter_walks_down_until_a_directory_is_filled_in() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = dir(base.path(), "repo");
        dir(&repo, "src");
        let lib = dir(base.path(), "other/lib");
        let mut app = app_in(&repo);
        app.home = Some(base.path().display().to_string());

        // Nothing typed names the working directory, which Enter fills
        // in from rather than adds.
        for c in "/add-dir ".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Submit);
        assert_eq!(app.prompt.text(), "/add-dir src/");
        assert!(app.extra_dirs.is_empty());

        // `~` alone names home, which Enter fills in from too.
        app.prompt.clear();
        for c in "/add-dir ~".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Submit);
        assert_eq!(app.prompt.text(), "/add-dir ~/other/");
        assert!(app.extra_dirs.is_empty());

        // Filled in, it is added, though it has directories under it.
        app.apply(Action::Submit);
        assert!(app.prompt.is_empty());
        assert_eq!(
            app.extra_dirs,
            [lib.parent()
                .expect("other")
                .canonicalize()
                .expect("canonical")]
        );
    }

    #[tokio::test]
    async fn completing_mid_argument_replaces_all_of_it() {
        let base = tempfile::tempdir().expect("tempdir");
        let repo = dir(base.path(), "repo");
        dir(base.path(), "other");
        let mut app = app_in(&repo);

        // Typing `h` into `../otr` lists what `../oth` completes to.
        for c in "/add-dir ../otr".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Left);
        app.apply(Action::Insert('h'));
        app.apply(Action::Accept);

        assert_eq!(app.prompt.text(), "/add-dir ../other/");
    }

    #[tokio::test]
    async fn an_added_directory_is_saved_between_turns() {
        let base = tempfile::tempdir().expect("tempdir");
        let other = dir(base.path(), "other");
        let sessions = base.path().join("sessions");
        // A session nothing was asked in yet is never saved.
        let mut session = Session::new("glm", dir(base.path(), "repo"));
        session.messages.push(Message::User("go".into()));
        let mut app = App::new(session, Arc::new(Idle), Arc::new(Vec::new()))
            .with_store(Store::at(&sessions));

        app.add_dir("../other");
        let saved = app.session_saving.join().await.expect("saves");
        app.session_saved(saved);

        let loaded = Store::at(&sessions)
            .latest()
            .await
            .expect("loads")
            .expect("one saved");
        assert_eq!(
            loaded.extra_dirs,
            [other.canonicalize().expect("canonical")]
        );
    }

    #[tokio::test]
    async fn a_bare_add_dir_only_says_how_it_is_used() {
        let base = tempfile::tempdir().expect("tempdir");
        let mut app = app_in(&dir(base.path(), "repo"));

        for c in "/add-dir".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Submit);

        assert!(app.prompt.is_empty());
        assert_eq!(app.hint.as_deref(), Some("usage: /add-dir <directory>"));
        assert!(app.extra_dirs.is_empty());
    }
}
