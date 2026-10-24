use std::{fs, process::Command};

use nth_curios::*;

fn setup() -> (tempfile::TempDir, Store) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let store = Store::new(repo.path(), "boundary-owner").unwrap();
    (repo, store)
}

fn publish(key: &str) -> Publish {
    Publish {
        request_id: key.into(),
        curio_id: None,
        expected_revision: None,
        title: None,
        html: "<p id='target'>Target</p>".into(),
    }
}

fn comment(key: &str) -> Comment {
    Comment {
        request_id: key.into(),
        revision: 1,
        body: "Review this".into(),
        target: Target {
            element_id: Some("target".into()),
            kind: TargetKind::Element,
            rect: None,
            label: "Target".into(),
            quote: "Target".into(),
        },
    }
}

#[test]
fn child_process_lock_probe() {
    let Ok(root) = std::env::var("NTH_CURIOS_TEST_REPO") else {
        return;
    };
    let store = Store::new(root, "boundary-owner").unwrap();
    match std::env::var("NTH_CURIOS_TEST_ACTION").unwrap().as_str() {
        "lease" => assert!(matches!(store.claim_delivery(), Err(Error::Conflict(_)))),
        "comment" => {
            store
                .comment(
                    &std::env::var("NTH_CURIOS_TEST_CID").unwrap(),
                    comment(&std::env::var("NTH_CURIOS_TEST_KEY").unwrap()),
                )
                .unwrap();
        }
        other => panic!("unexpected action {other}"),
    }
}

#[test]
fn independent_processes_coordinate_writes_and_delivery_leases() {
    let (repo, store) = setup();
    let cid = store.publish(publish("publish")).unwrap().curio.id;
    let executable = std::env::current_exe().unwrap();
    let lease = store.claim_delivery().unwrap();
    assert!(
        Command::new(&executable)
            .args(["--exact", "child_process_lock_probe", "--nocapture"])
            .env("NTH_CURIOS_TEST_REPO", repo.path())
            .env("NTH_CURIOS_TEST_ACTION", "lease")
            .status()
            .unwrap()
            .success()
    );
    drop(lease);
    let children: Vec<_> = (0..6)
        .map(|n| {
            Command::new(&executable)
                .args(["--exact", "child_process_lock_probe", "--nocapture"])
                .env("NTH_CURIOS_TEST_REPO", repo.path())
                .env("NTH_CURIOS_TEST_ACTION", "comment")
                .env("NTH_CURIOS_TEST_CID", &cid)
                .env("NTH_CURIOS_TEST_KEY", format!("child-{n}"))
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let snapshot = store.snapshot(&cid, None).unwrap();
    assert_eq!(snapshot.threads.len(), 6);
    let mut numbers: Vec<_> = snapshot.threads.iter().map(|t| t.number).collect();
    numbers.sort();
    assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6]);
    assert!(store.claim_delivery().is_ok());
}

#[test]
fn all_generated_paths_are_ignored_and_existing_rules_are_preserved() {
    let (repo, store) = setup();
    let cid = store.publish(publish("publish")).unwrap().curio.id;
    let _lease = store.claim_delivery().unwrap();
    for path in [
        store.source_dir().join("templates/quiet-folio.html"),
        store.path().join("state.json"),
        store.path().join("lock"),
        store.path().join("delivery.lock"),
        store.path().join(&cid).join("revisions/1/index.html"),
        store.path().join(&cid).join("revisions/1/revision.json"),
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["check-ignore", "--quiet", "--no-index"])
                .arg(&path)
                .status()
                .unwrap()
                .success(),
            "not ignored: {}",
            path.display()
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let ignore = repo.path().join(".nth/curios/.gitignore");
    fs::write(&ignore, "# keep this comment\n*\n").unwrap();
    Store::new(repo.path(), "boundary-owner").unwrap();
    assert_eq!(
        fs::read_to_string(&ignore).unwrap(),
        "# keep this comment\n*\n"
    );
    fs::write(&ignore, "*\n!*/\n!*.html\n").unwrap();
    assert!(matches!(
        Store::new(repo.path(), "boundary-owner"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(fs::read_to_string(&ignore).unwrap(), "*\n!*/\n!*.html\n");
}

#[test]
fn source_and_identifier_escapes_and_symlinks_are_rejected() {
    let (repo, store) = setup();
    let outside = repo.path().join("outside.html");
    fs::write(&outside, "<p>outside</p>").unwrap();
    assert!(store.publish_file(&outside, publish("outside")).is_err());
    for id in ["../escape", "/tmp/escape", "", "a/b", "a\\b"] {
        assert!(
            Store::new(repo.path(), id).is_err(),
            "accepted session {id:?}"
        );
        assert!(store.content(id, 1).is_err(), "accepted curio {id:?}");
    }
    let other = Store::new(repo.path(), "other-owner").unwrap();
    let source = other.source_dir().join("document.html");
    fs::write(&source, "<p>Other session</p>").unwrap();
    assert!(
        store
            .publish_file(&source, publish("other-session"))
            .is_err()
    );
    #[cfg(unix)]
    {
        let link = store.source_dir().join("linked.html");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(store.publish_file(&link, publish("symlink")).is_err());
    }
}

#[test]
fn invalid_comments_and_regions_do_not_mutate_state() {
    let (_repo, store) = setup();
    let cid = store.publish(publish("publish")).unwrap().curio.id;
    let version = store.state_version().unwrap();
    for body in [" ".into(), "x".repeat(16385)] {
        let mut input = comment("invalid-body");
        input.body = body;
        assert!(store.comment(&cid, input).is_err());
    }
    for rect in [
        Rect {
            x: -1.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
        Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 1.0,
        },
        Rect {
            x: 0.0,
            y: 0.0,
            width: f64::INFINITY,
            height: 1.0,
        },
        Rect {
            x: f64::NAN,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        },
    ] {
        let mut input = comment("invalid-region");
        input.target.kind = TargetKind::Region;
        input.target.rect = Some(rect);
        assert!(store.comment(&cid, input).is_err());
    }
    let mut input = comment("invalid-context");
    input.target.quote = "x".repeat(2049);
    assert!(store.comment(&cid, input).is_err());
    let mut input = comment("invalid-anchor");
    input.target.element_id = Some("absent".into());
    assert!(store.comment(&cid, input).is_err());
    assert_eq!(store.state_version().unwrap(), version);
    assert!(store.snapshot(&cid, None).unwrap().threads.is_empty());
}

#[test]
fn unreadable_state_leaves_no_published_revision_and_retry_recovers() {
    let (repo, store) = setup();
    // A directory cannot be loaded as state. Verify that an I/O failure does
    // not consume the request ID or publish a revision.
    fs::create_dir(store.path().join("state.json")).unwrap();
    assert!(store.publish(publish("failed")).is_err());
    fs::remove_dir(store.path().join("state.json")).unwrap();
    let store = Store::new(repo.path(), "boundary-owner").unwrap();
    assert!(store.list().unwrap().is_empty());
    assert_eq!(store.state_version().unwrap(), 0);
    assert_eq!(store.publish(publish("failed")).unwrap().revision.number, 1);
}

#[test]
fn no_repository_and_nested_start_have_explicit_results() {
    let plain = tempfile::tempdir().unwrap();
    assert!(matches!(
        Store::new(plain.path(), "owner"),
        Err(Error::RepoRequired)
    ));
    let (repo, _store) = setup();
    let nested = repo.path().join("nested/deep");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(repository_root(&nested).unwrap(), repo.path());
    assert!(Store::new(repository_root(&nested).unwrap(), "nested-owner").is_ok());
}
