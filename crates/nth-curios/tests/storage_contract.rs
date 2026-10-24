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
    let store = Store::new(repo.path(), "acceptance-owner").unwrap();
    (repo, store)
}

fn publish(key: &str) -> Publish {
    Publish {
        request_id: key.into(),
        curio_id: None,
        expected_revision: None,
        title: Some("Original title".into()),
        html: "<h1 id='summary'>Summary</h1>".into(),
    }
}

fn comment(key: &str) -> Comment {
    Comment {
        request_id: key.into(),
        revision: 1,
        body: "Please explain the price".into(),
        target: Target {
            element_id: Some("summary".into()),
            kind: TargetKind::Element,
            rect: None,
            label: "Summary".into(),
            quote: "Summary".into(),
        },
    }
}

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn reopen_preserves_saved_queued_recorded_and_answered_messages_and_batch_snapshot() {
    let (repo, store) = setup();
    let cid = store.publish(publish("publish")).unwrap().curio.id;
    let thread = store.comment(&cid, comment("comment")).unwrap();
    drop(store);
    let store = Store::new(repo.path(), "acceptance-owner").unwrap();
    let saved = store.snapshot(&cid, None).unwrap();
    assert_eq!(json(&saved.threads[0]), json(&thread));
    assert!(store.batches(false).unwrap().is_empty());
    let send = Send {
        request_id: "send".into(),
        message_ids: vec![thread.messages[0].id.clone()],
    };
    let batch = store.send(&cid, send.clone()).unwrap();
    let immutable_batch = json(&batch.messages);
    let store = Store::new(repo.path(), "acceptance-owner").unwrap();
    assert_eq!(json(store.send(&cid, send).unwrap()), json(&batch));
    assert_eq!(
        store.snapshot(&cid, None).unwrap().threads[0].messages[0].delivery,
        Delivery::Queued
    );
    assert_eq!(
        store.batches(true).unwrap()[0].session_id,
        "acceptance-owner"
    );
    store
        .acknowledge_recorded(std::slice::from_ref(&batch.id))
        .unwrap();
    let store = Store::new(repo.path(), "acceptance-owner").unwrap();
    assert!(store.batches(true).unwrap().is_empty());
    let recorded = store.snapshot(&cid, None).unwrap().threads.remove(0);
    assert_eq!(recorded.messages[0].delivery, Delivery::Recorded);
    let reply = Reply {
        request_id: "reply".into(),
        expected_version: recorded.version,
        body: "Explained in revision two".into(),
        mark_addressed: true,
        revision: Some(1),
    };
    let answer = store
        .reply(&cid, &thread.id, reply.clone(), Author::Agent)
        .unwrap();
    let version = store.state_version().unwrap();
    assert_eq!(
        json(store.reply(&cid, &thread.id, reply, Author::Agent).unwrap()),
        json(&answer)
    );
    assert_eq!(store.state_version().unwrap(), version);
    let store = Store::new(repo.path(), "acceptance-owner").unwrap();
    let current = store.snapshot(&cid, None).unwrap().threads.remove(0);
    assert_eq!(current.status, Status::Addressed);
    assert_eq!(current.messages.len(), 2);
    assert_eq!(current.messages[0].delivery, Delivery::Answered);
    assert_eq!(
        json(&store.batches(false).unwrap()[0].messages),
        immutable_batch
    );
    let resolved = store
        .set_status(&cid, &thread.id, current.version, Status::Resolved)
        .unwrap();
    let reopened = store
        .set_status(&cid, &thread.id, resolved.version, Status::Open)
        .unwrap();
    assert_eq!(reopened.status, Status::Open);
    assert!(
        Store::new(repo.path(), "other-owner")
            .unwrap()
            .list()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn historical_metadata_and_source_capture_are_immutable_after_edit_and_republish() {
    let (repo, store) = setup();
    let source = store.source_dir().join("document.html");
    let original = "<h1 id='summary'>Summary</h1>";
    fs::write(&source, original).unwrap();
    let first = store.publish_file(&source, publish("first")).unwrap();
    let cid = &first.curio.id;
    let revision_path = store.path().join(cid).join("revisions/1/revision.json");
    let metadata = fs::read(&revision_path).unwrap();
    let thread = store.comment(cid, comment("comment")).unwrap();
    fs::write(&source, "<h1 id='different'>Different meaning</h1>").unwrap();
    assert_eq!(store.content(cid, 1).unwrap(), original);
    let mut update = publish("second");
    update.curio_id = Some(cid.clone());
    update.expected_revision = Some(1);
    update.title = Some("New title".into());
    let second = store.publish_file(&source, update).unwrap();
    assert_eq!(second.detached_thread_ids, vec![thread.id]);
    let store = Store::new(repo.path(), "acceptance-owner").unwrap();
    let old = store.snapshot(cid, Some(1)).unwrap();
    assert_eq!(json(old.revision), json(first.revision));
    assert_eq!(old.threads[0].attachment, "attached");
    assert_eq!(
        store.snapshot(cid, None).unwrap().threads[0].attachment,
        "detached"
    );
    assert_eq!(fs::read(revision_path).unwrap(), metadata);
    assert_eq!(store.content(cid, 1).unwrap(), original);
}

#[test]
fn stale_mutations_and_mismatched_receipts_leave_state_unchanged() {
    let (_repo, store) = setup();
    let cid = store.publish(publish("publish")).unwrap().curio.id;
    let thread = store.comment(&cid, comment("comment")).unwrap();
    let version = store.state_version().unwrap();
    let mut changed = publish("publish");
    changed.title = Some("Changed".into());
    assert!(matches!(store.publish(changed), Err(Error::Conflict(_))));
    let mut stale = publish("stale");
    stale.curio_id = Some(cid.clone());
    stale.expected_revision = Some(0);
    assert!(matches!(store.publish(stale), Err(Error::Conflict(_))));
    let mut changed = comment("comment");
    changed.body = "Different".into();
    assert!(matches!(
        store.comment(&cid, changed),
        Err(Error::Conflict(_))
    ));
    let stale_reply = Reply {
        request_id: "stale-reply".into(),
        expected_version: 0,
        body: "Stale".into(),
        mark_addressed: false,
        revision: None,
    };
    assert!(matches!(
        store.reply(&cid, &thread.id, stale_reply, Author::Agent),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        store.set_status(&cid, &thread.id, 0, Status::Resolved),
        Err(Error::Conflict(_))
    ));
    assert_eq!(store.state_version().unwrap(), version);
    let send = Send {
        request_id: "send".into(),
        message_ids: vec![thread.messages[0].id.clone()],
    };
    store.send(&cid, send).unwrap();
    let version = store.state_version().unwrap();
    assert!(matches!(
        store.send(
            &cid,
            Send {
                request_id: "send".into(),
                message_ids: vec!["unknown".into()]
            }
        ),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        store.send(
            &cid,
            Send {
                request_id: "send-again".into(),
                message_ids: vec![thread.messages[0].id.clone()]
            }
        ),
        Err(Error::Conflict(_))
    ));
    assert_eq!(store.state_version().unwrap(), version);
}

#[test]
fn failed_partial_send_does_not_queue_any_message() {
    let (_repo, store) = setup();
    let cid = store.publish(publish("publish")).unwrap().curio.id;
    let thread = store.comment(&cid, comment("comment")).unwrap();
    let version = store.state_version().unwrap();
    assert!(
        store
            .send(
                &cid,
                Send {
                    request_id: "send".into(),
                    message_ids: vec![thread.messages[0].id.clone(), "missing".into()]
                }
            )
            .is_err()
    );
    assert_eq!(store.state_version().unwrap(), version);
    assert_eq!(
        store.snapshot(&cid, None).unwrap().threads[0].messages[0].delivery,
        Delivery::Saved
    );
    assert!(store.batches(false).unwrap().is_empty());
}
