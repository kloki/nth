//! The current branch's pull request, as `gh` (GitHub) or `tea`
//! (Gitea/Forgejo) reports it: the number is a link on the status bar.
//! Which of the two to ask is decided by where `origin` points, so no
//! call is made a forge's tool could not answer.

use std::{path::Path, process::Stdio};

use tokio::process::Command;

/// The pull request for the branch, whichever state it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    pub state: State,
}

/// The state the forge reports; the link is shown for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Merged,
    Closed,
}

/// `None` outside a repository, on a detached head, without an `origin`,
/// when no pull request is on the branch, or when the forge's tool is
/// missing or not logged in: all of them show no link rather than an error.
pub async fn load(cwd: &Path) -> Option<Pr> {
    let remote = git(cwd, &["remote", "get-url", "origin"]).await?;
    let host = host(&remote)?;
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if branch == "HEAD" {
        return None; // A detached head has no branch to match.
    }
    if is_github(&host) {
        let json = command(
            cwd,
            "gh",
            &["pr", "view", &branch, "--json", "number,url,state"],
        )
        .await?;
        parse_gh(&json)
    } else {
        let json = command(
            cwd,
            "tea",
            &[
                "pr",
                "list",
                "--state",
                "all",
                "--fields",
                "index,state,head,url",
                "--output",
                "json",
            ],
        )
        .await?;
        parse_tea(&json, &branch)
    }
}

/// The host a remote URL points at: `git@host:…`, `https://host/…` and
/// `ssh://git@host/…` all name one.
pub fn host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    let host = rest.split([':', '/']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// GitHub, or an instance shaped like its own; everything else is asked
/// of `tea`, which speaks Gitea and Forgejo.
fn is_github(host: &str) -> bool {
    host == "github.com" || host.ends_with(".github.com")
}

/// gh's `--json` output, one pull request.
pub fn parse_gh(json: &str) -> Option<Pr> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let pr = Pr {
        number: value.get("number")?.as_u64()?,
        url: value.get("url")?.as_str()?.to_string(),
        state: state(value.get("state")?.as_str()?)?,
    };
    Some(pr)
}

/// tea's `--output json` list, filtered down to the branch's pull request.
/// A fork's head is rendered as `owner:branch`, so only what follows the
/// colon is matched. An open one is preferred over the rest; otherwise the
/// first, as tea lists the newest first.
pub fn parse_tea(json: &str, branch: &str) -> Option<Pr> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let mut other = None;
    for row in rows {
        let Some(pr) = pr(&row, branch) else { continue };
        if pr.state == State::Open {
            return Some(pr);
        }
        other.get_or_insert(pr);
    }
    other
}

/// One row of tea's list, if it is the branch's pull request.
fn pr(row: &serde_json::Value, branch: &str) -> Option<Pr> {
    let head = row.get("head")?.as_str()?;
    if head.rsplit(':').next() != Some(branch) {
        return None;
    }
    // tea renders the index as a number, or as a string when it is asked
    // for other fields alongside it.
    let index = row.get("index")?;
    let number = index
        .as_u64()
        .or_else(|| index.as_str().and_then(|index| index.parse().ok()))?;
    Some(Pr {
        number,
        url: row.get("url")?.as_str()?.to_string(),
        state: state(row.get("state")?.as_str()?)?,
    })
}

fn state(text: &str) -> Option<State> {
    match text.to_ascii_lowercase().as_str() {
        "open" => Some(State::Open),
        "merged" => Some(State::Merged),
        "closed" => Some(State::Closed),
        _ => None,
    }
}

/// git's stdout, or `None` when it fails.
async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        // Quitting mid-load aborts the task; git must not outlive it.
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    stdout(out)
}

/// A forge tool's stdout, or `None` when it fails: not installed, not
/// logged in, or no pull request for the branch.
async fn command(cwd: &Path, program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        // No prompt can be answered from here, so none is asked.
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    stdout(out)
}

fn stdout(out: std::process::Output) -> Option<String> {
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_names_its_host_whatever_the_form() {
        assert_eq!(
            host("git@github.com:kloki/nth.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host("https://gitea.com/a/b.git").as_deref(),
            Some("gitea.com")
        );
        assert_eq!(
            host("ssh://git@host.example/a/b").as_deref(),
            Some("host.example")
        );
        assert_eq!(
            host("ssh://host.example:2222/a/b").as_deref(),
            Some("host.example")
        );
        assert_eq!(host("://"), None);
    }

    #[test]
    fn gh_json_is_one_pull_request() {
        let json = r#"{"number":123,"state":"MERGED","url":"https://x.y/pull/123"}"#;
        assert_eq!(
            parse_gh(json),
            Some(Pr {
                number: 123,
                url: "https://x.y/pull/123".into(),
                state: State::Merged,
            })
        );
        assert_eq!(parse_gh("no pull requests found"), None);
        assert_eq!(parse_gh("{}"), None);
    }

    #[test]
    fn tea_json_is_matched_to_the_branch() {
        let json = r#"[
            {"index":5,"state":"open","head":"other","url":"https://x.y/pulls/5"},
            {"index":9,"state":"open","head":"k:feature","url":"https://x.y/pulls/9"},
            {"index":4,"state":"closed","head":"feature","url":"https://x.y/pulls/4"}
        ]"#;
        assert_eq!(
            parse_tea(json, "feature"),
            Some(Pr {
                number: 9,
                url: "https://x.y/pulls/9".into(),
                state: State::Open,
            }),
            "an open one wins, fork heads included"
        );
    }

    #[test]
    fn tea_json_falls_back_to_the_first_of_the_branch() {
        let json = r#"[
            {"index":7,"state":"merged","head":"feature","url":"https://x.y/pulls/7"},
            {"index":8,"state":"closed","head":"feature","url":"https://x.y/pulls/8"},
            {"index":"8","state":"open","head":"other","url":"https://x.y/pulls/8"}
        ]"#;
        assert_eq!(
            parse_tea(json, "feature"),
            Some(Pr {
                number: 7,
                url: "https://x.y/pulls/7".into(),
                state: State::Merged,
            })
        );
        assert_eq!(parse_tea(json, "main"), None);
    }

    #[tokio::test]
    async fn outside_a_repository_is_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(load(dir.path()).await, None);
    }
}
