//! `nth session`: the saved sessions, listed newest first, and deleting
//! one with what it left on disk.

use std::{
    io::{self, Write},
    path::Path,
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use jiff::Timestamp;
use nth_icons::icons;
use nth_session::{Session, Store, Summary, store::age};
use owo_colors::OwoColorize;
use serde_json::{Value, json};

use crate::output::{self, Report};

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List the saved sessions of the working directory, newest first
    List {
        /// Show at most this many
        #[arg(short = 'n', long)]
        max_count: Option<usize>,
        /// Every directory's sessions, not only the working directory's
        #[arg(long)]
        all: bool,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// Delete a saved session with its plan, work, logs, images and repository Curios
    Delete {
        /// The session, by its id or the start of it
        session: String,
    },
    /// Rename a saved session; the next save of an open copy wins
    Rename {
        /// The session, by its id or the start of it
        session: String,
        /// The new title
        title: String,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(command: SessionCommand) -> Result<()> {
    let store = Store::open()?;
    match command {
        SessionCommand::List {
            max_count,
            all,
            json,
        } => {
            let cwd = std::env::current_dir().context("no working directory")?;
            let sessions = store.list().await?;
            let report = SessionListReport {
                sessions: select(&sessions, (!all).then_some(cwd.as_path()), max_count),
                all,
                now: SystemTime::now(),
            };
            output::print(&report, json)
        }
        SessionCommand::Delete { session } => {
            let session = pick(&store, Some(&session)).await?;
            let deleted = store.delete_session(&session).await?;
            let mut out = io::stdout().lock();
            let title = session.title().unwrap_or_default();
            writeln!(
                out,
                "{} deleted {}  {}",
                icons().ok.green().bold(),
                title.bold(),
                session.id.to_string().dimmed()
            )?;
            for path in &deleted.removed {
                writeln!(out, "  {}", path.display().dimmed())?;
            }
            // The save is gone, so a retry cannot find what stayed: name it
            // for removing by hand.
            for (path, error) in &deleted.failed {
                eprintln!(
                    "{} could not remove {}: {error}",
                    icons().fail.red().bold(),
                    path.display()
                );
            }
            match deleted.failed.len() {
                0 => Ok(()),
                n => bail!("{n} of what the session kept could not be removed"),
            }
        }
        SessionCommand::Rename {
            session,
            title,
            json,
        } => {
            let sessions = store.list().await?;
            let id = find(&sessions, &session)?.id;
            let title = store.rename(id, &title).await?;
            output::print(
                &RenameReport {
                    id: id.to_string(),
                    title,
                },
                json,
            )
        }
    }
}

struct RenameReport {
    id: String,
    title: String,
}

impl Report for RenameReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(
            out,
            "{} renamed {}  {}",
            icons().ok.green().bold(),
            self.title.bold(),
            self.id.dimmed()
        )?;
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(
            out,
            "{}",
            json!({"id": self.id, "title": self.title, "outcome": "renamed"})
        )?;
        Ok(())
    }
}

/// The saved session whose id starts with `id`, or the latest one.
pub(crate) async fn pick(store: &Store, id: Option<&str>) -> Result<Session> {
    let sessions = store.list().await?;
    let summary = match id {
        None => sessions.first().context("no saved sessions")?,
        Some(id) => find(&sessions, id)?,
    };
    Ok(store.load(summary.id).await?)
}

/// The one session whose id starts with `id`.
fn find<'a>(sessions: &'a [Summary], id: &str) -> Result<&'a Summary> {
    // Every id starts with nothing, so an empty one (an unset variable)
    // would pick the only session there is.
    if id.is_empty() {
        bail!("no session id given");
    }
    let mut found = sessions.iter().filter(|s| s.id.to_string().starts_with(id));
    match (found.next(), found.next()) {
        (Some(summary), None) => Ok(summary),
        (None, _) => bail!("no saved session starts with {id}"),
        (Some(_), Some(_)) => bail!("more than one saved session starts with {id}"),
    }
}

/// The sessions that ran in `cwd`, also those that moved from it into a
/// worktree, or all of them, the first `max` of them.
fn select<'a>(sessions: &'a [Summary], cwd: Option<&Path>, max: Option<usize>) -> Vec<&'a Summary> {
    sessions
        .iter()
        .filter(|s| cwd.is_none_or(|cwd| s.cwd == cwd || s.origin.as_deref() == Some(cwd)))
        .take(max.unwrap_or(usize::MAX))
        .collect()
}

/// The sessions `nth session list` shows, and what their lines depend on:
/// whether each names its directory, and the time their ages count to.
struct SessionListReport<'a> {
    sessions: Vec<&'a Summary>,
    all: bool,
    now: SystemTime,
}

impl Report for SessionListReport<'_> {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        list_text(out, &self.sessions, self.all, self.now)
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(out, "{}", list_json(&self.sessions))?;
        Ok(())
    }
}

fn list_text(out: &mut dyn Write, sessions: &[&Summary], all: bool, now: SystemTime) -> Result<()> {
    if sessions.is_empty() {
        let what = match all {
            true => "no saved sessions",
            false => "no saved sessions here (--all for every directory)",
        };
        writeln!(out, "{} {}", icons().to.cyan().bold(), what.dimmed())?;
        return Ok(());
    }
    for session in sessions {
        writeln!(
            out,
            "{}  {}  {}  {}",
            session.id.to_string().dimmed(),
            format!("{:>3}", age(now, session.updated_at)).dimmed(),
            session.title.bold(),
            session.model.blue()
        )?;
        if all {
            writeln!(out, "  {}", session.cwd.display().dimmed())?;
        }
    }
    Ok(())
}

fn list_json(sessions: &[&Summary]) -> Value {
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "cwd": s.cwd,
                "model": s.model,
                "updated": Timestamp::try_from(s.updated_at).ok().map(|at| at.to_string()),
            })
        })
        .collect();
    Value::Array(sessions)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::output::testing;

    fn summary(id: &str, cwd: &str, at_secs: u64) -> Summary {
        Summary {
            id: id.parse().expect("a uuid"),
            title: format!("in {cwd}"),
            renamed: false,
            cwd: cwd.into(),
            origin: None,
            model: "glm".into(),
            updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs),
        }
    }

    fn sessions() -> Vec<Summary> {
        vec![
            summary("11111111-0000-4000-8000-000000000000", "/a", 3),
            summary("12222222-0000-4000-8000-000000000000", "/b", 2),
            summary("21111111-0000-4000-8000-000000000000", "/a", 1),
        ]
    }

    /// The time the snapshots are taken at.
    const NOW: u64 = 10 * 86_400;

    /// Sessions of different ages and places, newest first.
    fn aged() -> Vec<Summary> {
        vec![
            summary("11111111-0000-4000-8000-000000000000", "/a", NOW - 30),
            summary(
                "12222222-0000-4000-8000-000000000000",
                "/b/longer",
                NOW - 300,
            ),
            summary(
                "21111111-0000-4000-8000-000000000000",
                "/a",
                NOW - 2 * 86_400,
            ),
        ]
    }

    /// The report of `cwd`'s sessions, or of all of them, at `NOW`.
    fn listed<'a>(sessions: &'a [Summary], cwd: Option<&Path>) -> SessionListReport<'a> {
        SessionListReport {
            sessions: select(sessions, cwd, None),
            all: cwd.is_none(),
            now: SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
        }
    }

    #[test]
    fn the_working_directorys_sessions_list_a_line_each() {
        let sessions = aged();

        let text = testing::text(&listed(&sessions, Some(Path::new("/a"))));

        insta::assert_snapshot!(text);
    }

    #[test]
    fn every_directorys_sessions_list_with_their_place() {
        let sessions = aged();

        let text = testing::text(&listed(&sessions, None));

        insta::assert_snapshot!(text);
    }

    #[test]
    fn no_sessions_here_points_at_all() {
        let sessions = aged();

        let text = testing::text(&listed(&sessions, Some(Path::new("/c"))));

        insta::assert_snapshot!(text);
    }

    #[test]
    fn no_sessions_anywhere_says_so() {
        let text = testing::text(&listed(&[], None));

        insta::assert_snapshot!(text);
    }

    #[test]
    fn the_sessions_are_one_json_array() {
        let sessions = aged();

        let value: Value = serde_json::from_str(&testing::json(&listed(&sessions, None))).unwrap();

        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn finds_a_session_by_the_start_of_its_id() {
        let sessions = sessions();

        assert_eq!(find(&sessions, "2").expect("one").cwd, Path::new("/a"));
        assert_eq!(find(&sessions, "12").expect("one").cwd, Path::new("/b"));
        let both = find(&sessions, "1").expect_err("two").to_string();
        assert!(both.contains("more than one"), "{both}");
        assert!(find(&sessions, "3").is_err());
        assert!(find(&sessions[..1], "").is_err(), "not the only one");
    }

    #[test]
    fn lists_the_working_directorys_sessions_or_all() {
        let sessions = sessions();
        let at = |shown: Vec<&Summary>| -> Vec<u64> {
            let epoch = SystemTime::UNIX_EPOCH;
            let secs = |s: &Summary| s.updated_at.duration_since(epoch).unwrap().as_secs();
            shown.into_iter().map(secs).collect()
        };

        assert_eq!(at(select(&sessions, Some(Path::new("/a")), None)), [3, 1]);
        assert_eq!(at(select(&sessions, None, None)), [3, 2, 1]);
        assert_eq!(at(select(&sessions, None, Some(2))), [3, 2]);

        let mut moved = summary(
            "31111111-0000-4000-8000-000000000000",
            "/a/.nth/worktrees/x",
            4,
        );
        moved.origin = Some("/a".into());
        let sessions = [vec![moved], sessions].concat();
        assert_eq!(
            at(select(&sessions, Some(Path::new("/a")), None)),
            [4, 3, 1]
        );
        assert_eq!(
            at(select(
                &sessions,
                Some(Path::new("/a/.nth/worktrees/x")),
                None
            )),
            [4]
        );
    }

    #[test]
    fn the_text_has_a_line_per_session_and_its_place_with_all() {
        let sessions = sessions();
        let report = SessionListReport {
            sessions: sessions.iter().take(1).collect(),
            all: true,
            now: SystemTime::UNIX_EPOCH + Duration::from_secs(3 + 2 * 3_600),
        };

        assert_eq!(
            testing::text(&report),
            "11111111-0000-4000-8000-000000000000   2h  in /a  glm\n  /a\n"
        );
    }

    #[test]
    fn the_json_is_one_array() {
        let sessions = sessions();
        let shown: Vec<&Summary> = sessions.iter().collect();

        let value = list_json(&shown);

        assert_eq!(value.as_array().map(Vec::len), Some(3));
        assert_eq!(value[1]["id"], "12222222-0000-4000-8000-000000000000");
        assert_eq!(value[1]["title"], "in /b");
        assert_eq!(value[1]["cwd"], "/b");
        assert_eq!(value[1]["model"], "glm");
        assert_eq!(value[1]["updated"], "1970-01-01T00:00:02Z");
    }
}
