//! Exercise the real binary without provider credentials or the user's data.
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
};

use nth_protocol::{
    Message,
    nth_curios::{self as core, Author, Comment, Publish, Reply, Target, TargetKind},
};
use nth_session::{Session, Store};
use serde_json::Value;

fn command(repo: &std::path::Path, data: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nth"));
    command
        .current_dir(repo)
        .env("XDG_DATA_HOME", data)
        .env("HOME", data)
        .env_remove("NTH_CONFIG")
        .env_remove("NTH_MODEL");
    command
}

fn json(repo: &std::path::Path, data: &std::path::Path, args: &[&str]) -> Value {
    let output = command(repo, data).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn long_thread_mutations_return_bounded_retry_safe_receipts() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let store = core::Store::new(repo.path(), "long-thread-owner").unwrap();
    let source = store.source_dir().join("review.html");
    std::fs::write(&source, "<p id=title>Review</p>").unwrap();
    let published = store
        .publish_file(
            &source,
            Publish {
                request_id: "publish".into(),
                curio_id: None,
                expected_revision: None,
                title: None,
                html: String::new(),
            },
        )
        .unwrap();
    let id = &published.curio.id;
    let body = "private-review-content".repeat(800);
    let body = &body[..16384];
    let mut thread = store
        .comment(
            id,
            Comment {
                request_id: "comment".into(),
                revision: 1,
                target: Target {
                    element_id: None,
                    kind: TargetKind::Document,
                    rect: None,
                    label: String::new(),
                    quote: String::new(),
                },
                body: body.into(),
            },
        )
        .unwrap();
    for n in 1..16 {
        thread = store
            .reply(
                id,
                &thread.id,
                Reply {
                    request_id: format!("seed-{n}"),
                    expected_version: thread.version,
                    body: body.into(),
                    mark_addressed: false,
                    revision: None,
                },
                Author::Human,
            )
            .unwrap();
    }
    assert!(serde_json::to_vec(&thread).unwrap().len() > 256 * 1024);
    let tid = &thread.id;
    let run = |args: &[&str]| {
        let output = command(repo.path(), data.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.len() < 4096,
            "mutation receipt exceeded 4 KiB"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-review-content"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("http://"));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let reply_args = [
        "curio",
        "reply",
        id,
        tid,
        body,
        "--expected-version",
        "16",
        "--request-id",
        "cli-reply",
        "--json",
    ];
    let reply = run(&reply_args);
    assert_eq!(reply["version"], 17);
    assert_eq!(reply["message_count"], 17);
    assert_eq!(reply["messages"].as_array().unwrap().len(), 1);
    assert_eq!(reply["messages"][0]["delivery"], "saved");
    assert_eq!(run(&reply_args), reply);
    let persisted = store.snapshot(id, None).unwrap();
    assert_eq!(persisted.threads[0].messages.len(), 17);
    assert_eq!(
        persisted.threads[0].messages[16].id,
        reply["messages"][0]["id"]
    );

    let resolved = run(&["curio", "resolve", id, tid, "--expected-version", "17"]);
    assert_eq!(resolved["status"], "resolved");
    assert!(resolved.get("messages").is_none());
    let reopened = run(&["curio", "reopen", id, tid, "--expected-version", "18"]);
    assert_eq!(reopened["status"], "open");

    let mut send_args = vec!["curio", "send", id, "--message-ids"];
    send_args.extend(persisted.threads[0].messages.iter().map(|m| m.id.as_str()));
    send_args.extend(["--request-id", "cli-send", "--json"]);
    let sent = run(&send_args);
    assert_eq!(sent["messages"].as_array().unwrap().len(), 17);
    assert_eq!(sent["messages"][0]["message"]["delivery"], "queued");
    assert_eq!(sent["recorded"], false);
    assert_eq!(run(&send_args), sent);
    assert_eq!(store.batches(false).unwrap().len(), 1);

    // Retrying after later mutations still identifies the original message/version.
    assert_eq!(run(&reply_args), reply);
    assert_eq!(
        store.snapshot(id, None).unwrap().threads[0].messages.len(),
        17
    );
    let update_args = [
        "curio",
        "publish",
        source.to_str().unwrap(),
        "--id",
        id,
        "--expected-revision",
        "1",
        "--request-id",
        "cli-update",
    ];
    let update = run(&update_args);
    assert_eq!(update["revision"]["number"], 2);
    assert!(update["revision"].get("anchors").is_none());
    assert_eq!(run(&update_args), update);
}

#[tokio::test]
async fn isolated_binary_publish_read_and_missing_repository() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("unused/model", repo.clone());
    session.messages.push(Message::User("review".into()));
    let scope = session.prepare_curios().unwrap();
    let source = scope.source_dir().join("review.html");
    std::fs::write(
        &source,
        "<!doctype html><html><body><h1 id=title>Review</h1></body></html>",
    )
    .unwrap();
    Store::at(data.join("nth/sessions"))
        .save(&session)
        .await
        .unwrap();
    let session_id = session.id.to_string();
    let published = json(
        &repo,
        &data,
        &[
            "curio",
            "publish",
            source.to_str().unwrap(),
            "--session",
            &session_id[..8],
            "--request-id",
            "publish-1",
            "--json",
        ],
    );
    let id = published["curio"]["id"].as_str().unwrap();
    let read = json(&repo, &data, &["curio", "read", id, "--json"]);
    assert_eq!(read["revision"]["number"], 1);
    assert_eq!(json(&repo, &data, &["curio", "list", "--json"])["total"], 1);
    let comment = json(
        &repo,
        &data,
        &[
            "curio",
            "comment",
            id,
            "Change title",
            "--revision",
            "1",
            "--element",
            "title",
            "--request-id",
            "comment-1",
            "--json",
        ],
    );
    assert_eq!(comment["messages"][0]["delivery"], "saved");
    let mid = comment["messages"][0]["id"].as_str().unwrap();
    let sent = json(
        &repo,
        &data,
        &[
            "curio",
            "send",
            id,
            "--message-ids",
            mid,
            "--request-id",
            "send-1",
            "--json",
        ],
    );
    assert_eq!(sent["session_id"], session_id);
    assert_eq!(sent["recorded"], false);

    #[cfg(unix)]
    {
        let mut child = command(&repo, &data)
            .args(["curio", "serve", id, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let opening: Value = serde_json::from_str(&line).unwrap();
        let address = opening["url"]
            .as_str()
            .unwrap()
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap();
        assert!(std::net::TcpStream::connect(address).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                panic!("serve did not stop after Ctrl-C");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(std::net::TcpStream::connect(address).is_err());
    }
    let no_repo = tempfile::tempdir().unwrap();
    let output = command(no_repo.path(), &data)
        .args(["curio", "list", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a Git repository"));
}
