//! Repository-scoped review commands. No provider is needed.
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use nth_protocol::nth_curios::{
    self as core, Author, Comment, Publish, Reply, Send, Status, Target, TargetKind,
};
use serde_json::{Value, json};

#[derive(ClapArgs)]
pub struct Args {
    /// Owning session ID or unambiguous prefix (required for a new publication)
    #[arg(long, global = true)]
    session: Option<String>,
    /// Print JSON (also the default when stdout is redirected)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum View {
    Curio,
    Threads,
    Messages,
    Revisions,
    Batches,
}

#[derive(Subcommand)]
enum Command {
    /// List published curios across this repository
    List {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
    /// Publish a file inside the owning session's source workspace
    Publish {
        source: PathBuf,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        expected_revision: Option<u64>,
        #[arg(long)]
        request_id: String,
    },
    /// Read metadata or a bounded page; messages require --thread
    Read {
        id: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long, value_enum, default_value = "curio")]
        view: View,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
    /// Add a human comment, saved locally until Send
    Comment {
        id: String,
        body: String,
        #[arg(long)]
        revision: u64,
        /// Semantic HTML ID; omitted for a document-level comment
        #[arg(long)]
        element: Option<String>,
        /// Target-relative region geometry as JSON: x, y, width, height
        #[arg(long)]
        rect: Option<String>,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long, default_value = "")]
        quote: String,
        #[arg(long)]
        request_id: String,
    },
    /// Reply as a human by default; --agent explicitly records an agent response
    Reply {
        id: String,
        thread: String,
        body: String,
        #[arg(long)]
        expected_version: u64,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        agent: bool,
        #[arg(long, requires = "agent")]
        mark_addressed: bool,
    },
    /// Submit an immutable batch of saved human messages for the owning session
    Send {
        id: String,
        #[arg(long, required = true, num_args = 1..)]
        message_ids: Vec<String>,
        #[arg(long)]
        request_id: String,
    },
    /// Resolve a review thread as the user
    Resolve {
        id: String,
        thread: String,
        #[arg(long)]
        expected_version: u64,
    },
    /// Reopen a review thread as the user
    Reopen {
        id: String,
        thread: String,
        #[arg(long)]
        expected_version: u64,
    },
    /// Serve until Ctrl-C; prints a private browser URL and runs no model
    Serve { id: String },
}

pub async fn run(args: Args) -> Result<()> {
    let root = core::repository_root(&std::env::current_dir().context("no working directory")?)?;
    let json_output = args.json || !io::stdout().is_terminal();
    let value = execute(args, &root).await?;
    print(&value, json_output)
}

fn print(value: &Value, json_output: bool) -> Result<()> {
    let mut out = io::stdout().lock();
    if json_output {
        writeln!(out, "{}", serde_json::to_string(value)?)?;
    } else {
        human(&mut out, value, "")?;
    }
    Ok(())
}

fn human(out: &mut impl Write, value: &Value, indent: &str) -> io::Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if value.is_object() || value.is_array() {
                    writeln!(out, "{indent}{key}:")?;
                    human(out, value, &format!("{indent}  "))?;
                } else {
                    writeln!(
                        out,
                        "{indent}{key}: {}",
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    )?;
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                human(out, item, indent)?;
            }
        }
        _ => writeln!(
            out,
            "{indent}{}",
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        )?,
    }
    Ok(())
}

fn page(items: Vec<Value>, offset: usize, limit: u16) -> Value {
    let total = items.len();
    let items: Vec<_> = items
        .into_iter()
        .skip(offset)
        .take(limit as usize)
        .collect();
    let next = offset.saturating_add(items.len());
    json!({"items": items, "total": total, "next_offset": (next < total).then_some(next)})
}

/// Preserve thread receipt fields without echoing private content or history.
/// Idempotent comment/reply results contain the original mutation's last message.
fn thread_receipt(thread: core::Thread, include_message: bool) -> Value {
    let mut receipt = json!({"id": thread.id, "number": thread.number,
        "version": thread.version, "created_revision": thread.created_revision,
        "status": thread.status, "attachment": thread.attachment,
        "message_count": thread.messages.len()});
    if include_message {
        receipt["messages"] = json!(thread.messages.last().map(|m| json!({
            "id": m.id, "author": m.author, "delivery": m.delivery,
            "created_at": m.created_at
        })).into_iter().collect::<Vec<_>>());
    }
    receipt
}

/// Core discovery is bounded and never consults a global curio store.
async fn owner(root: &Path, prefix: Option<&str>, id: Option<&str>) -> Result<core::Store> {
    if prefix == Some("") {
        bail!("no session id given");
    }
    if id.is_none() && prefix.is_none() {
        bail!("new publication requires --session <owning-session-id>");
    }
    let found = core::discover(root)?;
    let owners: std::collections::BTreeSet<_> = found
        .iter()
        .filter(|c| id.is_none_or(|id| c.id == id))
        .filter(|c| prefix.is_none_or(|p| c.session_id.starts_with(p)))
        .map(|c| c.session_id.clone())
        .collect();
    if owners.len() > 1 {
        bail!("ambiguous owner; supply --session with a longer session ID");
    }
    if let Some(owner) = owners.first() {
        return Ok(core::Store::new(root, owner)?);
    }
    if id.is_some() {
        bail!("curio not found in this repository for the selected session");
    }
    let prefix = prefix.context("new publication requires --session <owning-session-id>")?;
    let store = nth_session::Store::open()?;
    let mut session = crate::session::pick(&store, Some(prefix)).await?;
    let scope = session.prepare_curios()?;
    if scope.repo_root() != root {
        bail!("owning session belongs to a different repository");
    }
    store.save(&session).await?;
    Ok(scope)
}

async fn execute(args: Args, root: &Path) -> Result<Value> {
    let prefix = args.session.as_deref();
    let value = match args.command {
        Command::List { offset, limit } => {
            let items = core::discover(root)?
                .into_iter()
                .filter(|c| prefix.is_none_or(|p| c.session_id.starts_with(p)))
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            page(items, offset, limit)
        }
        Command::Publish {
            source,
            id,
            title,
            expected_revision,
            request_id,
        } => {
            let store = owner(root, prefix, id.as_deref()).await?;
            let published = store.publish_file(
                &source,
                Publish {
                    request_id,
                    curio_id: id,
                    expected_revision,
                    title,
                    html: String::new(),
                },
            )?;
            json!({"curio": published.curio, "revision": {
                "number": published.revision.number, "title": published.revision.title,
                "hash": published.revision.hash, "html_url": published.revision.html_url
            }, "detached_thread_ids": published.detached_thread_ids})
        }
        Command::Read {
            id,
            revision,
            view,
            thread,
            offset,
            limit,
        } => {
            let store = owner(root, prefix, Some(&id)).await?;
            let snapshot = store.snapshot(&id, revision)?;
            match view {
                View::Curio => json!({"curio": snapshot.curio, "revision": snapshot.revision,
                    "state_version": snapshot.state_version, "source_dir": store.source_dir(),
                    "thread_count": snapshot.threads.len(), "revision_count": snapshot.revisions.len()}),
                View::Threads => page(snapshot.threads.into_iter().map(|t| json!({
                    "id":t.id,"number":t.number,"version":t.version,"target":t.target,
                    "status":t.status,"attachment":t.attachment,"created_revision":t.created_revision,
                    "message_count":t.messages.len()})).collect(), offset, limit),
                View::Messages => {
                    let tid = thread.context("--view messages requires --thread")?;
                    let t = snapshot.threads.into_iter().find(|t| t.id == tid).context("thread not found")?;
                    page(t.messages.into_iter().map(serde_json::to_value).collect::<std::result::Result<_,_>>()?, offset, limit)
                }
                View::Revisions => page(snapshot.revisions.into_iter().map(serde_json::to_value).collect::<std::result::Result<_,_>>()?, offset, limit),
                View::Batches => page(store.batches(false)?.into_iter().filter(|b| b.curio_id == id).map(serde_json::to_value).collect::<std::result::Result<_,_>>()?, offset, limit),
            }
        }
        Command::Comment {
            id,
            body,
            revision,
            element,
            rect,
            label,
            quote,
            request_id,
        } => {
            let store = owner(root, prefix, Some(&id)).await?;
            let rect = rect.map(|r| serde_json::from_str(&r)).transpose()?;
            let kind = if rect.is_some() {
                TargetKind::Region
            } else if element.is_some() {
                TargetKind::Element
            } else {
                TargetKind::Document
            };
            thread_receipt(store.comment(
                &id,
                Comment {
                    request_id,
                    revision,
                    target: Target {
                        element_id: element,
                        kind,
                        rect,
                        label,
                        quote,
                    },
                    body,
                },
            )?, true)
        }
        Command::Reply {
            id,
            thread,
            body,
            expected_version,
            request_id,
            revision,
            agent,
            mark_addressed,
        } => {
            let store = owner(root, prefix, Some(&id)).await?;
            thread_receipt(store.reply(
                &id,
                &thread,
                Reply {
                    request_id,
                    expected_version,
                    body,
                    mark_addressed,
                    revision,
                },
                if agent { Author::Agent } else { Author::Human },
            )?, true)
        }
        Command::Send {
            id,
            message_ids,
            request_id,
        } => {
            let batch = owner(root, prefix, Some(&id)).await?.send(
                &id, Send { request_id, message_ids },
            )?;
            json!({"id": batch.id, "session_id": batch.session_id,
                "curio_id": batch.curio_id, "revision": batch.revision,
                "recorded": batch.recorded,
                "messages": batch.messages.iter().map(|m| json!({
                    "thread_id": m.thread_id, "thread_version": m.thread_version,
                    "message": {"id": m.message.id, "delivery": m.message.delivery}
                })).collect::<Vec<_>>()})
        },
        Command::Resolve {
            id,
            thread,
            expected_version,
        } => thread_receipt(owner(root, prefix, Some(&id)).await?.set_status(
            &id,
            &thread,
            expected_version,
            Status::Resolved,
        )?, false),
        Command::Reopen {
            id,
            thread,
            expected_version,
        } => thread_receipt(owner(root, prefix, Some(&id)).await?.set_status(
            &id,
            &thread,
            expected_version,
            Status::Open,
        )?, false),
        Command::Serve { id } => {
            let host = core::Host::start(owner(root, prefix, Some(&id)).await?, &id).await?;
            if args.json {
                print(&json!({"url":host.url()}), true)?;
            } else {
                println!("{}", host.url());
            }
            io::stdout().flush()?;
            tokio::signal::ctrl_c()
                .await
                .context("waiting for Ctrl-C")?;
            host.stop().await?;
            return Ok(json!({"stopped":true}));
        }
    };
    if serde_json::to_vec(&value)?.len() > 256 * 1024 {
        bail!("Curios output exceeds 256 KiB; request a smaller --limit");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn args(words: &[&str]) -> Args {
        let cli =
            crate::Cli::try_parse_from(["nth", "curio"].into_iter().chain(words.iter().copied()))
                .unwrap();
        match cli.command.unwrap() {
            crate::Command::Curio(args) => args,
            _ => unreachable!(),
        }
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        dir
    }

    #[test]
    fn parsing_help_and_bounds() {
        let error = crate::Cli::try_parse_from(["nth", "curio", "--help"])
            .err()
            .unwrap();
        let help = error.to_string();
        for command in [
            "list", "publish", "read", "reply", "comment", "send", "resolve", "reopen", "serve",
        ] {
            assert!(help.contains(command), "{help}");
        }
        assert!(crate::Cli::try_parse_from(["nth", "curio", "list", "--limit", "101"]).is_err());
        assert!(
            crate::Cli::try_parse_from([
                "nth",
                "curio",
                "reply",
                "c",
                "t",
                "body",
                "--expected-version",
                "1",
                "--request-id",
                "r",
                "--mark-addressed"
            ])
            .is_err()
        );
        assert!(args(&["list", "--json"]).json);
    }

    #[tokio::test]
    async fn cli_review_roundtrip_and_pagination() {
        let dir = repo();
        let store = core::Store::new(dir.path(), "owner-111").unwrap();
        let source = store.source_dir().join("document.html");
        std::fs::write(
            &source,
            "<!doctype html><html><body><p id=summary>Review me</p></body></html>",
        )
        .unwrap();
        let published = store
            .publish_file(
                &source,
                Publish {
                    request_id: "initial".into(),
                    curio_id: None,
                    expected_revision: None,
                    title: Some("A document".into()),
                    html: String::new(),
                },
            )
            .unwrap();
        let id = &published.curio.id;
        let root = dir.path();
        let run = |words: Vec<String>| {
            let borrowed: Vec<_> = words.iter().map(String::as_str).collect();
            let args = args(&borrowed);
            async move { execute(args, root).await }
        };
        let listed = run(vec!["list".into()]).await.unwrap();
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["items"][0]["session_id"], "owner-111");
        let comment = run(vec![
            "comment".into(),
            id.clone(),
            "Please revise".into(),
            "--revision".into(),
            "1".into(),
            "--element".into(),
            "summary".into(),
            "--request-id".into(),
            "comment1".into(),
        ])
        .await
        .unwrap();
        let tid = comment["id"].as_str().unwrap();
        let mid = comment["messages"][0]["id"].as_str().unwrap();
        assert_eq!(comment["messages"][0]["delivery"], "saved");
        let batch = run(vec![
            "send".into(),
            id.clone(),
            "--message-ids".into(),
            mid.into(),
            "--request-id".into(),
            "send1".into(),
        ])
        .await
        .unwrap();
        assert_eq!(batch["session_id"], "owner-111");
        assert_eq!(batch["recorded"], false);
        let thread = store.snapshot(id, None).unwrap().threads.remove(0);
        let reply = run(vec![
            "reply".into(),
            id.clone(),
            tid.into(),
            "Revised".into(),
            "--agent".into(),
            "--mark-addressed".into(),
            "--expected-version".into(),
            thread.version.to_string(),
            "--request-id".into(),
            "reply1".into(),
        ])
        .await
        .unwrap();
        assert_eq!(reply["status"], "addressed");
        assert_eq!(reply["messages"][0]["author"], "agent");
        let human_reply = run(vec![
            "reply".into(),
            id.clone(),
            tid.into(),
            "Thanks".into(),
            "--expected-version".into(),
            reply["version"].to_string(),
            "--request-id".into(),
            "human-reply".into(),
        ])
        .await
        .unwrap();
        assert_eq!(human_reply["messages"][0]["author"], "human");
        let update = run(vec![
            "publish".into(),
            source.display().to_string(),
            "--id".into(),
            id.clone(),
            "--expected-revision".into(),
            "1".into(),
            "--request-id".into(),
            "publish2".into(),
        ])
        .await
        .unwrap();
        assert_eq!(update["revision"]["number"], 2);
        let messages = run(vec![
            "read".into(),
            id.clone(),
            "--view".into(),
            "messages".into(),
            "--thread".into(),
            tid.into(),
            "--limit".into(),
            "1".into(),
        ])
        .await
        .unwrap();
        assert_eq!(messages["total"], 3);
        assert_eq!(messages["next_offset"], 1);
        let resolved = run(vec![
            "resolve".into(),
            id.clone(),
            tid.into(),
            "--expected-version".into(),
            human_reply["version"].to_string(),
        ])
        .await
        .unwrap();
        assert_eq!(resolved["status"], "resolved");
        let reopened = run(vec![
            "reopen".into(),
            id.clone(),
            tid.into(),
            "--expected-version".into(),
            resolved["version"].to_string(),
        ])
        .await
        .unwrap();
        assert_eq!(reopened["status"], "open");
        assert!(owner(dir.path(), None, None).await.is_err());
        let second = core::Store::new(dir.path(), "owner-222").unwrap();
        second
            .publish(Publish {
                request_id: "second".into(),
                curio_id: None,
                expected_revision: None,
                title: None,
                html: "<p>Other review</p>".into(),
            })
            .unwrap();
        assert!(
            owner(dir.path(), Some("owner-"), None)
                .await
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        let host = core::Host::start(store, id).await.unwrap();
        let address = host.address();
        assert!(tokio::net::TcpStream::connect(address).await.is_ok());
        host.stop().await.unwrap();
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[test]
    fn no_repository_and_human_output() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            core::repository_root(dir.path()),
            Err(core::Error::RepoRequired)
        ));
        let mut output = Vec::new();
        human(&mut output, &json!({"title":"Review", "revision":2}), "").unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("title: Review"));
        assert!(output.contains("revision: 2"));
    }
}
