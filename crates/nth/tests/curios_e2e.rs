//! Independent real-CLI acceptance checks. Set CURIOS_E2E_KEEP to retain a
//! browser-ready repository and fixture.json; no provider is contacted.
mod support;

use std::process::Command;

use nth_protocol::Message;
use nth_session::{Session, Store};
use serde_json::json;
use support::Env;

#[tokio::test]
async fn real_cli_review_revisions_pagination_and_deletion() {
    let env = Env::new();
    let repo = env.dir("repo");
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
    Store::at(env.sessions()).save(&session).await.unwrap();
    let sid = session.id.to_string();
    let pub1 = env.json(
        &repo,
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
        env.json(&repo, &["curio", "list", "--limit", "1", "--json"])["total"],
        1
    );
    let comment = env.json(
        &repo,
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
        env.json(&repo, &["curio", "read", id, "--view", "batches", "--json"])["total"],
        0
    );
    let sent = env.json(
        &repo,
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
    let threads = env.json(&repo, &["curio", "read", id, "--view", "threads", "--json"]);
    let version = threads["items"][0]["version"].to_string();
    // Simulated agent response, explicitly authored by CLI, not inference.
    let reply = env.json(
        &repo,
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
    let resolved = env.json(
        &repo,
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
    let reopened = env.json(
        &repo,
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
    let pub2 = env.json(
        &repo,
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
        env.json(&repo, &["curio", "read", id, "--view", "threads", "--json"])["items"][0]["attachment"],
        "detached"
    );
    assert_eq!(
        env.json(&repo, &["curio", "read", id, "--revision", "1", "--json"])["revision"]["number"],
        1
    );
    let page = env.json(
        &repo,
        &[
            "curio", "read", id, "--view", "messages", "--thread", tid, "--limit", "1", "--json",
        ],
    );
    assert_eq!(page["total"], 2);
    assert_eq!(page["next_offset"], 1);
    assert_eq!(
        env.json(
            &repo,
            &[
                "curio", "read", id, "--view", "messages", "--thread", tid, "--offset", "1",
                "--limit", "1", "--json"
            ]
        )["items"][0]["author"],
        "agent"
    );
    for invalid in ["0", "../state.json", "missing"] {
        assert!(
            !env.run(&repo, &["curio", "read", invalid, "--json"])
                .status
                .success()
        );
    }
    for limit in ["0", "101"] {
        assert!(
            !env.run(&repo, &["curio", "list", "--limit", limit])
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
        let data = env.data().to_path_buf();
        let path = env.keep();
        std::fs::write(path.join("fixture.json"), serde_json::to_vec_pretty(&json!({"repo":repo,"data":data,"session":sid,"curio":id,"thread":tid,"source":source})).unwrap()).unwrap();
        println!("browser fixture: {}", path.display());
    } else {
        let project = repo.join("project.html");
        std::fs::write(&project, "project source survives deletion").unwrap();
        let output = env.run(&repo, &["session", "delete", &sid[..8]]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!scope.source_dir().exists());
        assert!(project.exists());
        assert_eq!(env.json(&repo, &["curio", "list", "--json"])["total"], 0);
    }
}
