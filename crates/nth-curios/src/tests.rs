use std::process::Command;

use crate::*;

fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let store = Store::new(dir.path(), "session-one").unwrap();
    (dir, store)
}
fn publication(request: &str) -> Publish {
    Publish {
        request_id: request.into(),
        curio_id: None,
        expected_revision: None,
        title: Some("Review".into()),
        html: "<!doctype html><h1 id='heading'>Hello</h1>".into(),
    }
}
fn comment(request: &str) -> Comment {
    Comment {
        request_id: request.into(),
        revision: 1,
        target: Target {
            element_id: Some("heading".into()),
            kind: TargetKind::Element,
            rect: None,
            label: "Heading".into(),
            quote: "Hello".into(),
        },
        body: "Please revise".into(),
    }
}

#[test]
fn durable_review_retry_and_history() {
    let (dir, store) = setup();
    let published = store.publish(publication("publish-one")).unwrap();
    let cid = &published.curio.id;
    assert_eq!(
        store.publish(publication("publish-one")).unwrap().curio.id,
        *cid
    );
    let mut conflicting = publication("publish-one");
    conflicting.html.push_str("changed");
    assert!(matches!(
        store.publish(conflicting),
        Err(Error::Conflict(_))
    ));
    let thread = store.comment(cid, comment("comment-one")).unwrap();
    assert_eq!(
        store.comment(cid, comment("comment-one")).unwrap().id,
        thread.id
    );
    assert!(store.batches(true).unwrap().is_empty());
    let batch = store
        .send(
            cid,
            Send {
                request_id: "send-one".into(),
                message_ids: vec![thread.messages[0].id.clone()],
            },
        )
        .unwrap();
    let resumed = Store::new(dir.path(), "session-one").unwrap();
    assert_eq!(resumed.batches(true).unwrap()[0].id, batch.id);
    resumed.acknowledge_recorded(&[batch.id]).unwrap();
    let current = resumed.snapshot(cid, None).unwrap().threads.remove(0);
    assert_eq!(current.messages[0].delivery, Delivery::Recorded);
    let replied = resumed
        .reply(
            cid,
            &thread.id,
            Reply {
                request_id: "reply-one".into(),
                expected_version: current.version,
                body: "Updated".into(),
                mark_addressed: true,
                revision: Some(1),
            },
            Author::Agent,
        )
        .unwrap();
    assert_eq!(replied.status, Status::Addressed);
    assert_eq!(replied.messages[0].delivery, Delivery::Answered);
    let mut update = publication("publish-two");
    update.curio_id = Some(cid.clone());
    update.expected_revision = Some(1);
    update.html = "<p id='new'>New</p>".into();
    resumed.publish(update).unwrap();
    assert_eq!(
        resumed.snapshot(cid, None).unwrap().threads[0].attachment,
        "detached"
    );
    assert_eq!(
        resumed.snapshot(cid, Some(1)).unwrap().threads[0].attachment,
        "attached"
    );
    assert!(resumed.content(cid, 1).unwrap().contains("Hello"));
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["check-ignore", "--quiet"])
            .arg(resumed.path().join("state.json"))
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(discover(dir.path()).unwrap().len(), 1);
}

#[test]
fn receipts_keep_one_message_so_state_grows_with_the_thread() {
    let (_dir, store) = setup();
    let cid = store.publish(publication("publish")).unwrap().curio.id;
    let mut thread = store.comment(&cid, comment("comment")).unwrap();
    let body = "x".repeat(1000);
    let mut first = None;
    for n in 0..150 {
        thread = store
            .reply(
                &cid,
                &thread.id,
                Reply {
                    request_id: format!("reply-{n}"),
                    expected_version: thread.version,
                    body: body.clone(),
                    mark_addressed: false,
                    revision: Some(1),
                },
                Author::Agent,
            )
            .unwrap();
        first.get_or_insert_with(|| thread.clone());
    }
    let state = std::fs::metadata(store.path().join("state.json")).unwrap();
    assert!(state.len() < 3 * 150 * 1000, "state is {} bytes", state.len());
    // A retry still gets the thread as that request left it.
    let retried = store
        .reply(
            &cid,
            &thread.id,
            Reply {
                request_id: "reply-0".into(),
                expected_version: 1,
                body: body.clone(),
                mark_addressed: false,
                revision: Some(1),
            },
            Author::Agent,
        )
        .unwrap();
    let first = first.unwrap();
    assert_eq!(retried.version, first.version);
    assert_eq!(
        serde_json::to_value(&retried.messages).unwrap(),
        serde_json::to_value(&first.messages).unwrap()
    );
}

#[test]
fn actual_uuid_owner_ignore_exceptions_are_rejected_without_rewriting() {
    let (dir, _) = setup();
    let owner = "d7811ce2-b442-4bd5-a2e4-bb0dc390d95d";
    let ignore = dir.path().join(".nth/curios/.gitignore");
    for exception in [
        format!("!{owner}/\n!{owner}/source/\n!{owner}/source/*.html\n"),
        format!("!{owner}/\n!{owner}/state.json\n"),
        format!("!{owner}/\n!{owner}/lock\n!{owner}/delivery.lock\n"),
        format!("!{owner}/\n!{owner}/*/\n!{owner}/*/revisions/\n"),
    ] {
        let rules = format!("*\n{exception}");
        std::fs::write(&ignore, &rules).unwrap();
        assert!(matches!(
            Store::new(dir.path(), owner),
            Err(Error::Conflict(_))
        ));
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), rules);
        assert!(
            !dir.path()
                .join(".nth/curios")
                .join(owner)
                .join("state.json")
                .exists()
        );
    }
}

#[test]
fn existing_generated_files_are_checked_even_when_tracked() {
    let (dir, store) = setup();
    let cid = store.publish(publication("p")).unwrap().curio.id;
    let generated = store.path().join(&cid).join("revisions/1/index.html");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "--force"])
            .arg(&generated)
            .status()
            .unwrap()
            .success()
    );
    Store::new(dir.path(), "session-one").unwrap();
    let rules = format!(
        "*\n!session-one/\n!session-one/{cid}/\n!session-one/{cid}/revisions/\n!session-one/{cid}/revisions/1/\n!session-one/{cid}/revisions/1/index.html\n"
    );
    let ignore = dir.path().join(".nth/curios/.gitignore");
    std::fs::write(&ignore, &rules).unwrap();
    assert!(matches!(
        Store::new(dir.path(), "another-owner"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(std::fs::read_to_string(ignore).unwrap(), rules);
    assert!(generated.exists());
}

#[test]
fn reply_before_ack_is_durable_without_answering_unsent_or_later_messages() {
    let (dir, store) = setup();
    let cid = store.publish(publication("p")).unwrap().curio.id;
    let thread = store.comment(&cid, comment("c")).unwrap();
    let send = Send {
        request_id: "send".into(),
        message_ids: vec![thread.messages[0].id.clone()],
    };
    let batch = store.send(&cid, send.clone()).unwrap();
    let unsent = store
        .reply(
            &cid,
            &thread.id,
            Reply {
                request_id: "unsent".into(),
                expected_version: 2,
                body: "Unsent follow-up".into(),
                mark_addressed: false,
                revision: Some(1),
            },
            Author::Human,
        )
        .unwrap();
    let reply = Reply {
        request_id: "answer".into(),
        expected_version: unsent.version,
        body: "Answer".into(),
        mark_addressed: false,
        revision: Some(1),
    };
    let answered = store
        .reply(&cid, &thread.id, reply.clone(), Author::Agent)
        .unwrap();
    assert_eq!(answered.messages[0].delivery, Delivery::Queued);
    assert_eq!(answered.messages[1].delivery, Delivery::Saved);
    let later = store
        .reply(
            &cid,
            &thread.id,
            Reply {
                request_id: "later".into(),
                expected_version: answered.version,
                body: "Later question".into(),
                mark_addressed: false,
                revision: Some(1),
            },
            Author::Human,
        )
        .unwrap();
    let later_batch = store
        .send(
            &cid,
            Send {
                request_id: "send-later".into(),
                message_ids: vec![later.messages[1].id.clone(), later.messages[3].id.clone()],
            },
        )
        .unwrap();
    // An idempotent retry must not answer newly submitted messages.
    store.reply(&cid, &thread.id, reply, Author::Agent).unwrap();
    assert_eq!(store.send(&cid, send).unwrap().id, batch.id);
    let resumed = Store::new(dir.path(), "session-one").unwrap();
    crate::store::FAIL_STATE_COMMIT.with(|fail| fail.set(true));
    assert!(
        resumed
            .acknowledge_recorded(std::slice::from_ref(&batch.id))
            .is_err()
    );
    assert_eq!(
        resumed.snapshot(&cid, None).unwrap().threads[0].messages[0].delivery,
        Delivery::Queued
    );
    resumed
        .acknowledge_recorded(&[batch.id.clone(), later_batch.id])
        .unwrap();
    let snapshot = resumed.snapshot(&cid, None).unwrap();
    let current = &snapshot.threads[0];
    assert_eq!(current.messages[0].delivery, Delivery::Answered);
    assert_eq!(current.messages[1].delivery, Delivery::Recorded);
    assert_eq!(current.messages[3].delivery, Delivery::Recorded);
    let version = current.version;
    resumed.acknowledge_recorded(&[batch.id]).unwrap();
    assert_eq!(
        resumed.snapshot(&cid, None).unwrap().threads[0].version,
        version
    );
    let after_ack = resumed
        .reply(
            &cid,
            &thread.id,
            Reply {
                request_id: "answer-after-ack".into(),
                expected_version: version,
                body: "Follow-up answer".into(),
                mark_addressed: false,
                revision: Some(1),
            },
            Author::Agent,
        )
        .unwrap();
    assert_eq!(after_ack.messages[1].delivery, Delivery::Answered);
    assert_eq!(after_ack.messages[3].delivery, Delivery::Answered);
}

#[test]
fn mixed_historical_submissions_keep_each_messages_reviewed_revision() {
    let (dir, store) = setup();
    let cid = store.publish(publication("p1")).unwrap().curio.id;
    let mut update = publication("p2");
    update.curio_id = Some(cid.clone());
    update.expected_revision = Some(1);
    update.html = "<p id='replacement'>Replacement</p>".into();
    store.publish(update).unwrap();
    let thread = store.comment(&cid, comment("historical")).unwrap();
    let replied = store
        .reply(
            &cid,
            &thread.id,
            Reply {
                request_id: "current-reply".into(),
                expected_version: thread.version,
                body: "Looking at revision two".into(),
                mark_addressed: false,
                revision: Some(2),
            },
            Author::Human,
        )
        .unwrap();
    let resumed = Store::new(dir.path(), "session-one").unwrap();
    let batch = resumed
        .send(
            &cid,
            Send {
                request_id: "send".into(),
                message_ids: replied.messages.iter().map(|m| m.id.clone()).collect(),
            },
        )
        .unwrap();
    assert_eq!(batch.revision, 2);
    assert_eq!(batch.messages[0].reviewed_revision, Some(1));
    assert_eq!(batch.messages[1].reviewed_revision, Some(2));
    assert_eq!(
        batch.messages[0].target.element_id.as_deref(),
        Some("heading")
    );
    let original = resumed
        .snapshot(&cid, batch.messages[0].reviewed_revision)
        .unwrap();
    assert_eq!(original.revision.number, 1);
    assert_eq!(original.threads[0].attachment, "attached");
    assert_eq!(
        resumed.snapshot(&cid, None).unwrap().threads[0].attachment,
        "detached"
    );
    let mut legacy = serde_json::to_value(&batch.messages[0]).unwrap();
    legacy.as_object_mut().unwrap().remove("reviewed_revision");
    let legacy: SubmittedMessage = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.reviewed_revision, None);
    assert_eq!(
        Store::new(dir.path(), "session-one")
            .unwrap()
            .batches(true)
            .unwrap()[0]
            .messages[0]
            .reviewed_revision,
        Some(1)
    );
}

#[test]
fn historical_human_replies_preserve_explicit_or_unknown_reviewed_revision() {
    let (dir, store) = setup();
    let cid = store.publish(publication("p1")).unwrap().curio.id;
    let thread = store.comment(&cid, comment("comment-r1")).unwrap();
    let mut update = publication("p2");
    update.curio_id = Some(cid.clone());
    update.expected_revision = Some(1);
    update.html = "<p id='replacement'>Replacement</p>".into();
    store.publish(update).unwrap();
    assert_eq!(
        store.snapshot(&cid, None).unwrap().threads[0].attachment,
        "detached"
    );
    assert_eq!(
        store.snapshot(&cid, Some(1)).unwrap().threads[0].attachment,
        "attached"
    );

    let reply = Reply {
        request_id: "reply-r1".into(),
        expected_version: thread.version,
        body: "Still viewing revision one".into(),
        mark_addressed: false,
        revision: Some(1),
    };
    let replied = store
        .reply(&cid, &thread.id, reply.clone(), Author::Human)
        .unwrap();
    let retried = store
        .reply(&cid, &thread.id, reply.clone(), Author::Human)
        .unwrap();
    assert_eq!(
        retried.messages.last().unwrap().id,
        replied.messages.last().unwrap().id
    );
    for revision in [Some(2), None] {
        assert!(matches!(
            store.reply(
                &cid,
                &thread.id,
                Reply {
                    revision,
                    ..reply.clone()
                },
                Author::Human
            ),
            Err(Error::Conflict(_))
        ));
    }
    assert!(matches!(
        store.reply(
            &cid,
            &thread.id,
            Reply {
                request_id: "missing-revision".into(),
                expected_version: replied.version,
                revision: Some(3),
                ..reply.clone()
            },
            Author::Human
        ),
        Err(Error::NotFound(_))
    ));
    let unknown: Reply = serde_json::from_value(serde_json::json!({
        "request_id": "unknown-revision",
        "expected_version": replied.version,
        "body": "No viewed revision supplied"
    }))
    .unwrap();
    let replied = store
        .reply(&cid, &thread.id, unknown, Author::Human)
        .unwrap();
    let resumed = Store::new(dir.path(), "session-one").unwrap();
    let batch = resumed
        .send(
            &cid,
            Send {
                request_id: "send-replies".into(),
                message_ids: replied.messages.iter().map(|m| m.id.clone()).collect(),
            },
        )
        .unwrap();
    assert_eq!(batch.revision, 2);
    assert_eq!(batch.messages.len(), 3);
    assert_eq!(batch.messages[0].reviewed_revision, Some(1));
    assert_eq!(batch.messages[1].reviewed_revision, Some(1));
    assert_eq!(batch.messages[2].reviewed_revision, None);
}

#[test]
fn static_boundary_and_anchor_validation() {
    for html in [
        "<script>alert(1)</script>",
        "<img src='https://example.org/x'>",
        "<div onclick='x()'>x</div>",
        "<div id='x'></div><p id='x'>x</p>",
        "<style>@import 'https://example.org';</style>",
        "<style>div{background:u\\72l(https://example.org)}</style>",
        "<meta http-equiv='refresh' content='0;url=/api/state'>",
        "<svg><image href='https://example.org'/></svg>",
        "<a href='javascript:alert(1)'>go</a>",
        "<iframe srcdoc='hi'></iframe>",
    ] {
        assert!(validate(html).is_err(), "accepted {html}");
    }
    assert!(
        validate("<style>h1 {color:red}</style><svg><rect id='box' width='10'/></svg>").is_ok()
    );
}

#[test]
fn ownership_conflicts_and_bounds() {
    let (_dir, store) = setup();
    let lease = store.claim_delivery().unwrap();
    assert!(matches!(store.claim_delivery(), Err(Error::Conflict(_))));
    drop(lease);
    assert!(store.claim_delivery().is_ok());
    let cid = store.publish(publication("p")).unwrap().curio.id;
    let thread = store.comment(&cid, comment("c")).unwrap();
    assert!(matches!(
        store.set_status(&cid, &thread.id, 0, Status::Resolved),
        Err(Error::Conflict(_))
    ));
    assert!(
        store
            .set_status(&cid, &thread.id, 1, Status::Addressed)
            .is_err()
    );
    assert!(store.content("../outside", 1).is_err());
    let mut too_long = comment("long");
    too_long.body = "a".repeat(16385);
    assert!(store.comment(&cid, too_long).is_err());
}

#[test]
fn failed_commit_is_unpublished_and_recovered() {
    let (dir, store) = setup();
    let first = store.publish(publication("first")).unwrap();
    let mut update = publication("second");
    update.curio_id = Some(first.curio.id.clone());
    update.expected_revision = Some(1);
    crate::store::FAIL_STATE_COMMIT.with(|fail| fail.set(true));
    assert!(store.publish(update.clone()).is_err());
    assert_eq!(
        store
            .snapshot(&first.curio.id, None)
            .unwrap()
            .curio
            .current_revision,
        1
    );
    let orphan = store.path().join(&first.curio.id).join("revisions/2");
    assert!(orphan.exists());
    let resumed = Store::new(dir.path(), "session-one").unwrap();
    assert!(!orphan.exists());
    assert!(resumed.content(&first.curio.id, 1).is_ok());
    assert_eq!(resumed.publish(update).unwrap().revision.number, 2);
    assert!(
        validate(&std::fs::read_to_string(resumed.prepare_template().unwrap()).unwrap()).is_ok()
    );
}

#[test]
fn concurrent_writers_preserve_all_comments() {
    let (_dir, store) = setup();
    let cid = store.publish(publication("publish")).unwrap().curio.id;
    let handles: Vec<_> = (0..12)
        .map(|n| {
            let store = store.clone();
            let cid = cid.clone();
            std::thread::spawn(move || {
                store
                    .comment(&cid, comment(&format!("comment-{n}")))
                    .unwrap()
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let snapshot = store.snapshot(&cid, None).unwrap();
    assert_eq!(snapshot.threads.len(), 12);
    let mut numbers: Vec<_> = snapshot.threads.iter().map(|t| t.number).collect();
    numbers.sort();
    assert_eq!(numbers, (1..=12).collect::<Vec<_>>());
}

#[test]
fn source_escape_and_nested_ignore_conflict() {
    let (dir, store) = setup();
    let outside = dir.path().join("outside.html");
    std::fs::write(&outside, "<p>Outside</p>").unwrap();
    assert!(store.publish_file(&outside, publication("escape")).is_err());
    let nested = store.source_dir().join(".gitignore");
    std::fs::write(nested, "!document.html\n").unwrap();
    assert!(matches!(
        Store::new(dir.path(), "session-one"),
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn handoff_cookie_refresh_mutation_guards_and_host_isolation() {
    let (_dir, store) = setup();
    let cid = store.publish(publication("p")).unwrap().curio.id;
    let host = Host::start(store.clone(), &cid).await.unwrap();
    let url = host.url();
    let (base, token) = url.split_once('#').unwrap();
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}api/handoff"))
        .header("X-Curios-Token", token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let cookie_header = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie_header.contains("HttpOnly; SameSite=Strict"));
    assert!(cookie_header.contains("Path=/api/"));
    assert!(!cookie_header.contains(token));
    assert!(!cookie_header.contains("Domain="));
    let cookie = cookie_header.split(';').next().unwrap();
    assert!(response.bytes().await.unwrap().is_empty());
    for route in ["api/state", "api/events"] {
        let denied = client
            .get(format!("{base}{route}"))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 403);
    }
    let content = client.get(format!("{base}content/1")).send().await.unwrap();
    assert_eq!(content.status(), 200);
    assert!(!content.headers().contains_key("set-cookie"));
    let state = client
        .get(format!("{base}api/state"))
        .header("Cookie", cookie)
        .header("X-Curios-Request", "viewer")
        .send()
        .await
        .unwrap();
    assert_eq!(state.status(), 200);
    let nav = client
        .get(format!("{base}api/state"))
        .header("Cookie", cookie)
        .header("Sec-Fetch-Dest", "iframe")
        .header("X-Curios-Request", "viewer")
        .send()
        .await
        .unwrap();
    assert_eq!(nav.status(), 403);
    let body = comment("cookie-comment");
    let without_origin = client
        .post(format!("{base}api/comments"))
        .header("Cookie", cookie)
        .header("X-Curios-Request", "viewer")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(without_origin.status(), 403);
    let form = client
        .post(format!("{base}api/comments"))
        .header("Cookie", cookie)
        .header("Origin", base.trim_end_matches('/'))
        .header("Content-Type", "text/plain")
        .header("X-Curios-Request", "viewer")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), 403);
    let saved = client
        .post(format!("{base}api/comments"))
        .header("Cookie", cookie)
        .header("Origin", base.trim_end_matches('/'))
        .header("Sec-Fetch-Dest", "empty")
        .header("Sec-Fetch-Mode", "cors")
        .header("Sec-Fetch-Site", "same-origin")
        .header("X-Curios-Request", "viewer")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), 200);
    let sse = client
        .get(format!("{base}api/events"))
        .header("Cookie", cookie)
        .header("X-Curios-Request", "viewer")
        .send()
        .await
        .unwrap();
    assert_eq!(sse.status(), 200);
    drop(sse);
    let other = Host::start(store, &cid).await.unwrap();
    let stale = client
        .get(format!("http://{}/api/state", other.address()))
        .header("Cookie", cookie)
        .header("X-Curios-Request", "viewer")
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 403);
    other.stop().await.unwrap();
    host.stop().await.unwrap();
}

#[tokio::test]
async fn host_capability_csp_and_shutdown() {
    let (_dir, store) = setup();
    let cid = store.publish(publication("p")).unwrap().curio.id;
    let host = Host::start(store, &cid).await.unwrap();
    let url = host.url();
    let (base, token) = url.split_once('#').unwrap();
    let client = reqwest::Client::new();
    let unauthorized = client.get(format!("{base}api/state")).send().await.unwrap();
    assert_eq!(unauthorized.status(), 403);
    let authorized = client
        .get(format!("{base}api/state"))
        .header("X-Curios-Token", token)
        .send()
        .await
        .unwrap();
    assert!(authorized.status().is_success());
    let state: Snapshot = authorized.json().await.unwrap();
    assert_eq!(state.curio.id, cid);
    let remote = client
        .get(format!("{base}api/state"))
        .header("X-Curios-Token", token)
        .header("Origin", "https://evil.invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(remote.status(), 403);
    let frame = client.get(format!("{base}content/1")).send().await.unwrap();
    let csp = frame.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("script-src 'none'"));
    assert!(csp.contains("sandbox allow-same-origin"));
    host.stop().await.unwrap();
    assert!(client.get(base).send().await.is_err());
}
