//! `nth session`: the saved sessions, listed newest first, and deleting
//! one with what it left on disk.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use jiff::Timestamp;
use nth_icons::icons;
use nth_protocol::monitor_log_dir;
use nth_session::{Session, Store, Summary, store};
use owo_colors::OwoColorize;
use serde_json::{Value, json};

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
    /// Delete a saved session with its plan, work items and monitor logs
    Delete {
        /// The session, by its id or the start of it
        session: String,
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
            let shown = select(&sessions, (!all).then_some(cwd.as_path()), max_count);
            let mut out = io::stdout().lock();
            match json || !out.is_terminal() {
                true => writeln!(out, "{}", list_json(&shown))?,
                false => list_text(&mut out, &shown, all, SystemTime::now())?,
            }
            Ok(())
        }
        SessionCommand::Delete { session } => {
            let session = pick(&store, Some(&session)).await?;
            let removed = delete(&store, &session, &store::data_dir()?).await?;
            let mut out = io::stdout().lock();
            let title = session.title().unwrap_or_default();
            writeln!(
                out,
                "{} deleted {}  {}",
                icons().ok.green().bold(),
                title.bold(),
                session.id.to_string().dimmed()
            )?;
            for path in removed {
                writeln!(out, "  {}", path.display().dimmed())?;
            }
            Ok(())
        }
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
    let mut found = sessions.iter().filter(|s| s.id.to_string().starts_with(id));
    match (found.next(), found.next()) {
        (Some(summary), None) => Ok(summary),
        (None, _) => bail!("no saved session starts with {id}"),
        (Some(_), Some(_)) => bail!("more than one saved session starts with {id}"),
    }
}

/// The sessions that ran in `cwd`, or all of them, the first `max` of them.
fn select<'a>(sessions: &'a [Summary], cwd: Option<&Path>, max: Option<usize>) -> Vec<&'a Summary> {
    sessions
        .iter()
        .filter(|s| cwd.is_none_or(|cwd| s.cwd == cwd))
        .take(max.unwrap_or(usize::MAX))
        .collect()
}

/// Removes `session`'s save, then whatever else it kept: its plan file,
/// its work items and its monitor logs, each only if it is there. Its
/// worktrees are left alone, since they may hold work not merged yet.
/// Returns what went besides the save.
async fn delete(store: &Store, session: &Session, data_dir: &Path) -> Result<Vec<PathBuf>> {
    store.delete(session.id).await?;
    let mut removed = Vec::new();
    let plan = session.plan_path();
    if gone(tokio::fs::remove_file(&plan).await, &plan)? {
        removed.push(plan);
    }
    let dirs = [
        session.work_dir(),
        monitor_log_dir(data_dir, &session.id.to_string()),
    ];
    for dir in dirs {
        if gone(tokio::fs::remove_dir_all(&dir).await, &dir)? {
            removed.push(dir);
        }
    }
    Ok(removed)
}

/// Whether a removal took something away; one that was never there is
/// not an error.
fn gone(result: io::Result<()>, path: &Path) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn list_text(
    out: &mut impl Write,
    sessions: &[&Summary],
    all: bool,
    now: SystemTime,
) -> Result<()> {
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

/// How long ago `then` was, in its largest whole unit, as the resume
/// picker says it: `now`, `5m`, `3h`, `2d`.
fn age(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Message;

    use super::*;

    fn summary(id: &str, cwd: &str, at_secs: u64) -> Summary {
        Summary {
            id: id.parse().expect("a uuid"),
            title: format!("in {cwd}"),
            cwd: cwd.into(),
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

    /// The text without its colours.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|c| *c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn finds_a_session_by_the_start_of_its_id() {
        let sessions = sessions();

        assert_eq!(find(&sessions, "2").expect("one").cwd, Path::new("/a"));
        assert_eq!(find(&sessions, "12").expect("one").cwd, Path::new("/b"));
        let both = find(&sessions, "1").expect_err("two").to_string();
        assert!(both.contains("more than one"), "{both}");
        assert!(find(&sessions, "3").is_err());
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
    }

    #[test]
    fn the_text_has_a_line_per_session_and_its_place_with_all() {
        let sessions = sessions();
        let shown: Vec<&Summary> = sessions.iter().take(1).collect();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3 + 2 * 3_600);
        let mut out = Vec::new();

        list_text(&mut out, &shown, true, now).unwrap();

        assert_eq!(
            plain(&String::from_utf8(out).unwrap()),
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

    #[tokio::test]
    async fn delete_removes_the_save_plan_work_and_monitor_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let data = dir.path().join("data");
        let mut session = Session::new("glm", dir.path().join("repo"));
        session.messages.push(Message::User("fix it".into()));
        store.save(&session).await.expect("saves");
        let other = Session::new("glm", dir.path().join("repo"));
        let plan = session.plan_path();
        let work = session.work_dir();
        let monitors = monitor_log_dir(&data, &session.id.to_string());
        let kept = other.plan_path();
        for file in [
            &plan,
            &work.join("01-a/item.md"),
            &monitors.join("1.log"),
            &kept,
        ] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x").unwrap();
        }

        let removed = delete(&store, &session, &data).await.expect("deletes");

        assert_eq!(removed, [plan.clone(), work.clone(), monitors.clone()]);
        assert!(store.list().await.expect("lists").is_empty());
        assert!(!plan.exists() && !work.exists() && !monitors.exists());
        assert!(kept.exists(), "another session's plan stays");
        assert!(delete(&store, &session, &data).await.is_err(), "gone");
    }

    #[tokio::test]
    async fn delete_without_a_plan_or_work_removes_only_the_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let mut session = Session::new("glm", dir.path().join("repo"));
        session.messages.push(Message::User("fix it".into()));
        store.save(&session).await.expect("saves");

        let removed = delete(&store, &session, dir.path()).await.expect("deletes");

        assert!(removed.is_empty(), "{removed:?}");
    }
}
