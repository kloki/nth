//! Independent real-CLI acceptance checks. Set CURIOS_E2E_KEEP to retain a
//! browser-ready repository and fixture.json; no provider is contacted.
use std::{path::Path, process::Command};

use nth_protocol::Message;
use nth_session::{Session, Store};
use serde_json::{Value, json};

fn cli(repo: &Path, data: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nth"))
        .current_dir(repo)
        .env("HOME", data)
        .env("XDG_DATA_HOME", data)
        .env("XDG_CONFIG_HOME", data.join("config"))
        .env_remove("NTH_CONFIG")
        .env_remove("NTH_MODEL")
        .args(args)
        .output()
        .unwrap()
}

fn run(repo: &Path, data: &Path, args: &[&str]) -> Value {
    let output = cli(repo, data, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn real_cli_review_revisions_pagination_and_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("unused/no-inference", repo.clone());
    session
        .messages
        .push(Message::User("Independent Curios review fixture".into()));
    let scope = session.prepare_curios().unwrap();
    let source = scope.source_dir().join("roundtrip.html");
    std::fs::write(&source, "<!doctype html><html><head><title>Roundtrip</title><style>body{font:18px system-ui;margin:32px}section{padding:24px;background:#eaf0f5;margin:16px 0}svg{max-width:100%}</style></head><body><h1 id=title>Independent browser review</h1><section id=summary>Stable summary for element and region review.</section><section id=removed>This section disappears in revision two.</section></body></html>").unwrap();
    Store::at(data.join("nth/sessions"))
        .save(&session)
        .await
        .unwrap();
    let sid = session.id.to_string();
    let pub1 = run(
        &repo,
        &data,
        &[
            "curio",
            "publish",
            source.to_str().unwrap(),
            "--session",
            &sid[..8],
            "--request-id",
            "e2e-p1",
            "--json",
        ],
    );
    let id = pub1["curio"]["id"].as_str().unwrap();
    assert_eq!(
        run(&repo, &data, &["curio", "list", "--limit", "1", "--json"])["total"],
        1
    );
    let comment = run(
        &repo,
        &data,
        &[
            "curio",
            "comment",
            id,
            "Please remove this section",
            "--revision",
            "1",
            "--element",
            "removed",
            "--request-id",
            "e2e-c1",
            "--json",
        ],
    );
    let tid = comment["id"].as_str().unwrap();
    let mid = comment["messages"][0]["id"].as_str().unwrap();
    assert_eq!(comment["messages"][0]["delivery"], "saved");
    assert_eq!(
        run(
            &repo,
            &data,
            &["curio", "read", id, "--view", "batches", "--json"]
        )["total"],
        0
    );
    let sent = run(
        &repo,
        &data,
        &[
            "curio",
            "send",
            id,
            "--message-ids",
            mid,
            "--request-id",
            "e2e-send",
            "--json",
        ],
    );
    assert_eq!(sent["recorded"], false);
    let threads = run(
        &repo,
        &data,
        &["curio", "read", id, "--view", "threads", "--json"],
    );
    let version = threads["items"][0]["version"].to_string();
    // Simulated agent response, explicitly authored by CLI, not inference.
    let reply = run(
        &repo,
        &data,
        &[
            "curio",
            "reply",
            id,
            tid,
            "Simulated agent: removed in next revision",
            "--agent",
            "--mark-addressed",
            "--expected-version",
            &version,
            "--request-id",
            "e2e-reply",
            "--json",
        ],
    );
    assert_eq!(reply["status"], "addressed");
    let resolved = run(
        &repo,
        &data,
        &[
            "curio",
            "resolve",
            id,
            tid,
            "--expected-version",
            &reply["version"].to_string(),
            "--json",
        ],
    );
    assert_eq!(resolved["status"], "resolved");
    let reopened = run(
        &repo,
        &data,
        &[
            "curio",
            "reopen",
            id,
            tid,
            "--expected-version",
            &resolved["version"].to_string(),
            "--json",
        ],
    );
    assert_eq!(reopened["status"], "open");
    let original = std::fs::read_to_string(&source).unwrap();
    std::fs::write(
        &source,
        original.replace(
            "<section id=removed>This section disappears in revision two.</section>",
            "",
        ),
    )
    .unwrap();
    let pub2 = run(
        &repo,
        &data,
        &[
            "curio",
            "publish",
            source.to_str().unwrap(),
            "--id",
            id,
            "--expected-revision",
            "1",
            "--request-id",
            "e2e-p2",
            "--json",
        ],
    );
    assert_eq!(pub2["revision"]["number"], 2);
    assert_eq!(
        run(
            &repo,
            &data,
            &["curio", "read", id, "--view", "threads", "--json"]
        )["items"][0]["attachment"],
        "detached"
    );
    assert_eq!(
        run(
            &repo,
            &data,
            &["curio", "read", id, "--revision", "1", "--json"]
        )["revision"]["number"],
        1
    );
    let page = run(
        &repo,
        &data,
        &[
            "curio", "read", id, "--view", "messages", "--thread", tid, "--limit", "1", "--json",
        ],
    );
    assert_eq!(page["total"], 2);
    assert_eq!(page["next_offset"], 1);
    assert_eq!(
        run(
            &repo,
            &data,
            &[
                "curio", "read", id, "--view", "messages", "--thread", tid, "--offset", "1",
                "--limit", "1", "--json"
            ]
        )["items"][0]["author"],
        "agent"
    );
    for invalid in ["0", "../state.json", "missing"] {
        assert!(
            !cli(&repo, &data, &["curio", "read", invalid, "--json"])
                .status
                .success()
        );
    }
    for limit in ["0", "101"] {
        assert!(
            !cli(&repo, &data, &["curio", "list", "--limit", limit])
                .status
                .success()
        );
    }
    let ignored = Command::new("git")
        .current_dir(&repo)
        .args(["check-ignore", "--quiet"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(ignored.success());
    assert!(
        Command::new("git")
            .current_dir(&repo)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap()
            .stdout
            .is_empty()
    );
    if std::env::var_os("CURIOS_E2E_KEEP").is_some() {
        let path = temp.keep();
        std::fs::write(path.join("fixture.json"), serde_json::to_vec_pretty(&json!({"repo":repo,"data":data,"session":sid,"curio":id,"thread":tid,"source":source})).unwrap()).unwrap();
        println!("browser fixture: {}", path.display());
    } else {
        let project = repo.join("project.html");
        std::fs::write(&project, "project source survives deletion").unwrap();
        let output = cli(&repo, &data, &["session", "delete", &sid[..8]]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!scope.source_dir().exists());
        assert!(project.exists());
        assert_eq!(run(&repo, &data, &["curio", "list", "--json"])["total"], 0);
    }
}
