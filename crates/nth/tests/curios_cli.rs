//! Exercise the real binary without provider credentials or the user's data.
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
};

use nth_protocol::Message;
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
