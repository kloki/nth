//! Independent acceptance scenarios using durable mutations from a second store handle.
use std::sync::Arc;

use nth_protocol::{
    StreamEvent, ToolCall,
    nth_curios::{self as core, Comment, Publish, Send, Target, TargetKind},
};

use super::{App, Message, Reply, Scripted, Session, rows_of};

fn fixture(provider: Arc<Scripted>) -> (tempfile::TempDir, App, core::Store, core::Curio) {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("fake", dir.path().to_path_buf());
    let store = session.prepare_curios().unwrap();
    let curio = store
        .publish(Publish {
            request_id: "publish".into(),
            curio_id: None,
            expected_revision: None,
            title: Some("Original review".into()),
            html: "<h1 id=heading>Original</h1>".into(),
        })
        .unwrap()
        .curio;
    let mut app = App::new(session, provider, Arc::new(Vec::new()));
    app.poll_curios();
    (dir, app, store, curio)
}

fn comment(store: &core::Store, c: &core::Curio) -> core::Thread {
    store
        .comment(
            &c.id,
            Comment {
                request_id: "comment".into(),
                revision: 1,
                target: Target {
                    element_id: Some("heading".into()),
                    kind: TargetKind::Element,
                    rect: None,
                    label: "Original heading".into(),
                    quote: "Original".into(),
                },
                body: "Please retain this original context".into(),
            },
        )
        .unwrap()
}

fn submit(store: &core::Store, c: &core::Curio, thread: &core::Thread) -> core::Batch {
    store
        .send(
            &c.id,
            Send {
                request_id: "send".into(),
                message_ids: vec![thread.messages[0].id.clone()],
            },
        )
        .unwrap()
}

fn review_text(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User(text) if text.starts_with("<curio ") => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn saved_comment_is_inert_then_external_send_wakes_owner_with_original_snapshot() {
    let provider = Arc::new(Scripted::default());
    let (_dir, mut app, store, c) = fixture(provider.clone());
    let before = app.session.as_ref().unwrap().messages.clone();
    let thread = comment(&store, &c);
    app.poll_curios();
    app.notices_due();
    assert!(!app.is_busy());
    assert!(provider.sent().is_empty());
    assert_eq!(app.session.as_ref().unwrap().messages, before);
    let external = core::Store::new(store.repo_root(), &c.session_id).unwrap();
    let batch = submit(&external, &c, &thread);
    external
        .publish(Publish {
            request_id: "revision-two".into(),
            curio_id: Some(c.id.clone()),
            expected_revision: Some(1),
            title: Some("Changed review".into()),
            html: "<h1 id=new>Replacement</h1>".into(),
        })
        .unwrap();
    app.poll_curios();
    app.notices_due();
    let ended = app.turn.join().await.unwrap();
    app.end_turn(ended);
    let session = app.session.as_ref().unwrap();
    let texts = review_text(&session.messages);
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("revision=\"1\""));
    assert!(texts[0].contains("Original heading"));
    assert!(texts[0].contains(&thread.messages[0].id));
    assert!(session.curio_receipts.contains(&batch.id));
    assert_eq!(
        external.batches(true).unwrap()[0].messages[0].message.body,
        thread.messages[0].body
    );
    assert_eq!(provider.sent().len(), 1);
}

#[tokio::test]
async fn busy_send_waits_for_tool_result_instead_of_interrupting_tool_calls() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let call = ToolCall {
        id: "pending-call".into(),
        name: "missing_fixture_tool".into(),
        arguments: "{}".into(),
    };
    let provider = Arc::new(Scripted::replying([Reply::HangUntil(
        gate.clone(),
        vec![StreamEvent::ToolCall(call)],
    )]));
    let (_dir, mut app, store, c) = fixture(provider.clone());
    app.start_turn("Continue existing work".into());
    provider.asked.notified().await;
    let thread = comment(&store, &c);
    submit(&store, &c, &thread);
    app.poll_curios();
    assert!(app.inbox.prompts_waiting().is_none());
    gate.notify_one();
    let ended = app.turn.join().await.unwrap();
    app.end_turn(ended);
    let sent = provider.sent();
    assert_eq!(sent.len(), 2);
    assert!(review_text(&sent[0]).is_empty());
    let messages = &sent[1];
    let result_at = messages.iter().position(|m| matches!(m, Message::ToolResult { call_id, content, .. } if call_id == "pending-call" && !content.contains("interrupted"))).unwrap();
    let review_at = messages
        .iter()
        .position(|m| matches!(m, Message::User(t) if t.starts_with("<curio ")))
        .unwrap();
    assert!(result_at < review_at);
    assert_eq!(
        review_text(&app.session.as_ref().unwrap().messages).len(),
        1
    );
}

#[tokio::test]
async fn cancellation_holds_later_send_until_deliberate_resumption() {
    let provider = Arc::new(Scripted::replying([Reply::Hang]));
    let (_dir, mut app, store, c) = fixture(provider.clone());
    app.start_turn("Work".into());
    provider.asked.notified().await;
    app.interrupt();
    let ended = app.turn.join().await.unwrap();
    app.end_turn(ended);
    let thread = comment(&store, &c);
    let batch = submit(&store, &c, &thread);
    app.poll_curios();
    app.notices_due();
    assert!(!app.is_busy());
    assert!(app.hold_notices);
    assert!(review_text(&app.session.as_ref().unwrap().messages).is_empty());
    assert!(!store.batches(true).unwrap()[0].recorded);
    app.hold_notices = false;
    app.notices_due();
    let ended = app.turn.join().await.unwrap();
    app.end_turn(ended);
    let session = app.session.as_ref().unwrap();
    assert!(session.curio_receipts.contains(&batch.id));
    assert_eq!(review_text(&session.messages).len(), 1);
}

#[tokio::test]
async fn repository_icon_survives_switch_but_feedback_only_runs_after_owner_resume() {
    let provider = Arc::new(Scripted::default());
    let (_dir, mut app, store, c) = fixture(provider.clone());
    let owner = app.session.as_ref().unwrap().clone();
    app.resume(Session::new("fake", app.cwd.clone()));
    let thread = comment(&store, &c);
    submit(&store, &c, &thread);
    app.poll_curios();
    app.notices_due();
    assert!(!app.is_busy());
    app.place = "repo".into();
    assert!(rows_of(&mut app, 100)[14].contains(nth_icons::icons().curio));
    assert!(app.areas.curio.is_some());
    assert!(provider.sent().is_empty());
    app.resume(owner);
    app.poll_curios();
    app.notices_due();
    let ended = app.turn.join().await.unwrap();
    app.end_turn(ended);
    assert_eq!(app.session_id.to_string(), c.session_id);
    assert_eq!(
        review_text(&app.session.as_ref().unwrap().messages).len(),
        1
    );
}

#[tokio::test]
async fn failed_session_save_leaves_batch_pending_then_restart_deduplicates_saved_history() {
    let provider = Arc::new(Scripted::default());
    let (dir, mut app, store, c) = fixture(provider.clone());
    let blocked = dir.path().join("blocked-sessions");
    std::fs::write(&blocked, "not a directory").unwrap();
    let saves = nth_session::Store::at(&blocked);
    app = app.with_store(saves.clone());
    let thread = comment(&store, &c);
    let batch = submit(&store, &c, &thread);
    app.poll_curios();
    app.notices_due();
    let ended = app.turn.join().await.unwrap();
    app.end_turn(ended);
    assert_eq!(
        review_text(&app.session.as_ref().unwrap().messages).len(),
        1
    );
    assert!(!store.batches(true).unwrap()[0].recorded);
    assert!(!blocked.join(format!("{}.json", app.session_id)).exists());
    app.poll_curios();
    app.notices_due();
    assert!(!app.is_busy());
    assert_eq!(provider.sent().len(), 1);
    std::fs::remove_file(&blocked).unwrap();
    saves.save(app.session.as_ref().unwrap()).await.unwrap();
    let resumed = saves.load(app.session_id).await.unwrap();
    assert!(resumed.curio_receipts.contains(&batch.id));
    assert_eq!(review_text(&resumed.messages).len(), 1);
    assert!(store.batches(true).unwrap().is_empty());
    drop(app);
    let mut restarted = App::new(resumed, provider.clone(), Arc::new(Vec::new()));
    restarted.poll_curios();
    restarted.notices_due();
    assert!(!restarted.is_busy());
    assert_eq!(provider.sent().len(), 1);
    assert_eq!(
        review_text(&restarted.session.as_ref().unwrap().messages).len(),
        1
    );
}

#[tokio::test]
async fn status_open_prefers_last_opened_then_falls_back_after_its_owner_is_removed() {
    let provider = Arc::new(Scripted::default());
    let (dir, mut app, first_store, first) = fixture(provider);
    app.opener = |_| Ok(());
    // A second owner also checks that fallback never changes active session ownership.
    let second_store = core::Store::new(dir.path(), "second-owner").unwrap();
    let second = second_store
        .publish(Publish {
            request_id: "second-publication".into(),
            curio_id: None,
            expected_revision: None,
            title: Some("Second document".into()),
            html: "<h1>Second</h1>".into(),
        })
        .unwrap()
        .curio;
    app.poll_curios();
    app.open_curio(second.session_id.clone(), second.id.clone(), None);
    let opened = app.curios.opening.join().await.unwrap();
    assert_eq!(opened.as_ref().unwrap().1, second.id);
    app.curio_opened(opened);
    app.open_latest_curio();
    assert!(
        !app.curios.opening.is_running(),
        "last-opened host is reused"
    );
    assert!(app.hint.as_deref().unwrap().contains("owning session"));
    std::fs::remove_dir_all(second_store.path()).unwrap();
    app.poll_curios();
    app.open_latest_curio();
    let opened = app.curios.opening.join().await.unwrap();
    assert_eq!(opened.as_ref().unwrap().0, first.session_id);
    assert_eq!(opened.as_ref().unwrap().1, first.id);
    app.curio_opened(opened);
    assert_eq!(app.session_id.to_string(), first.session_id);
    assert_eq!(
        first_store
            .snapshot(&first.id, None)
            .unwrap()
            .revision
            .title,
        "Original review"
    );
}
