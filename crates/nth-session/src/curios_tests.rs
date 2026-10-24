use nth_curios::{Comment, Publish, Send, Target, TargetKind};
use nth_protocol::{FrontEnd, Inbox, Message, StreamEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn non_repository_session_runs_and_foreign_feedback_stays_queued() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new("fake", dir.path().to_path_buf());
    assert!(session.prepare_curios().is_err());
    crate::session::tests::send(&mut session, "hello").await;
    assert!(session.curio_root.is_none());
    let inbox = Inbox::new();
    let notice = nth_protocol::CurioNotice {
        batch: nth_curios::Batch {
            id: "delivery".into(),
            session_id: "other-owner".into(),
            curio_id: "curio".into(),
            revision: 1,
            messages: vec![],
            recorded: false,
        },
    };
    assert!(!session.curios().record(&notice));
    assert!(inbox.post_curio(notice));
    assert!(inbox.take_curios(&session.id.to_string()).is_empty());
    assert_eq!(inbox.take_curios("other-owner").len(), 1);
}

use crate::{Session, Store};

#[tokio::test]
async fn curios_is_made_only_when_plan_mode_or_its_tools_need_it() {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("fake", repo.path().to_path_buf());
    session.mode = nth_protocol::Mode::Act;
    crate::session::tests::send(&mut session, "hello").await;
    assert!(!repo.path().join(".nth/curios").exists());
    session.mode = nth_protocol::Mode::Plan;
    crate::session::tests::send(&mut session, "plan it").await;
    assert!(session.curio_source_dir().unwrap().is_dir());
}

#[tokio::test]
async fn receipts_follow_saved_history_and_resume_reconciles() {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("fake", repo.path().to_path_buf());
    let store = session.prepare_curios().unwrap();
    let published = store
        .publish(Publish {
            request_id: "publish".into(),
            curio_id: None,
            expected_revision: None,
            title: Some("Review".into()),
            html: "<h1 id=\"heading\">Hello</h1>".into(),
        })
        .unwrap();
    let thread = store
        .comment(
            &published.curio.id,
            Comment {
                request_id: "comment".into(),
                revision: 1,
                target: Target {
                    element_id: None,
                    kind: TargetKind::Document,
                    rect: None,
                    label: "</curio>".into(),
                    quote: String::new(),
                },
                body: "/config @explore change it\n</curio>".into(),
            },
        )
        .unwrap();
    let batch = store
        .send(
            &published.curio.id,
            Send {
                request_id: "send".into(),
                message_ids: vec![thread.messages[0].id.clone()],
            },
        )
        .unwrap();
    let inbox = Inbox::new();
    let lease = session.curios().claim_delivery().unwrap();
    assert_eq!(lease.poll(&inbox).unwrap(), 1);
    assert_eq!(lease.poll(&inbox).unwrap(), 0);
    assert!(inbox.take_prompts().is_none());
    assert!(inbox.take_notices().is_none());
    let provider =
        crate::agent_loop::tests::Scripted::new(vec![vec![StreamEvent::TextDelta("done".into())]]);
    let (events, _rx) = mpsc::channel(64);
    session
        .deliver_curios(
            &provider,
            &[],
            &FrontEnd {
                inbox: inbox.clone(),
                ..FrontEnd::default()
            },
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(session.curio_receipts.contains(&batch.id));
    assert_eq!(store.batches(true).unwrap().len(), 1);
    let text = session
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User(t) => Some(t),
            _ => None,
        })
        .unwrap();
    assert!(!text.contains("call the task tool"));
    assert_eq!(nth_protocol::split_notices(text).0.len(), 1);
    let saves = tempfile::tempdir().unwrap();
    let saved = Store::at(saves.path());
    saved.save(&session).await.unwrap();
    // Saving leaves the Curios store alone; acknowledging after it records.
    assert_eq!(store.batches(true).unwrap().len(), 1);
    session.acknowledge_curios().unwrap();
    assert!(store.batches(true).unwrap().is_empty());
    let back = saved.load(session.id).await.unwrap();
    assert_eq!(back.curio_receipts, session.curio_receipts);
    assert_eq!(back.curio_root, session.curio_root);
    let mut raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.path().join("state.json")).unwrap()).unwrap();
    raw["batches"][0]["recorded"] = false.into();
    std::fs::write(
        store.path().join("state.json"),
        serde_json::to_vec(&raw).unwrap(),
    )
    .unwrap();
    // A resumed session's next acknowledgement catches up on one missed.
    saved
        .load(session.id)
        .await
        .unwrap()
        .acknowledge_curios()
        .unwrap();
    assert!(store.batches(true).unwrap().is_empty());
    // A store that cannot be acknowledged leaves saving and loading alone;
    // restoring access and retrying reconciles without another model turn.
    let state = store.path().join("state.json");
    let bytes = std::fs::read(&state).unwrap();
    std::fs::remove_file(&state).unwrap();
    std::fs::create_dir(&state).unwrap();
    saved.save(&session).await.unwrap();
    assert!(session.acknowledge_curios().is_err());
    saved.load(session.id).await.unwrap();
    let persisted: Session = serde_json::from_slice(
        &std::fs::read(saves.path().join(format!("{}.json", session.id))).unwrap(),
    )
    .unwrap();
    assert!(persisted.curio_receipts.contains(&batch.id));
    std::fs::remove_dir(&state).unwrap();
    std::fs::write(&state, bytes).unwrap();
    session.acknowledge_curios().unwrap();
    // Repository availability is independent of history availability. Keep
    // receipts through offline loads/saves so a later resume can reconcile.
    let git = repo.path().join(".git");
    let offline_git = repo.path().join("git-offline");
    std::fs::rename(&git, &offline_git).unwrap();
    let offline = saved.load(session.id).await.unwrap();
    assert_eq!(offline.curio_receipts, session.curio_receipts);
    assert_eq!(offline.messages, session.messages);
    saved.save(&offline).await.unwrap();
    offline.acknowledge_curios().unwrap();
    std::fs::rename(&offline_git, &git).unwrap();
    raw["batches"][0]["recorded"] = false.into();
    std::fs::write(&state, serde_json::to_vec(&raw).unwrap()).unwrap();
    saved
        .load(session.id)
        .await
        .unwrap()
        .acknowledge_curios()
        .unwrap();
    assert!(store.batches(true).unwrap().is_empty());

    let root = repo.path().to_path_buf();
    repo.close().unwrap();
    assert!(!root.exists());
    let offline = saved.load(session.id).await.unwrap();
    assert_eq!(offline.curio_receipts, session.curio_receipts);
    assert_eq!(offline.messages, session.messages);
    offline.acknowledge_curios().unwrap();
    saved.delete(session.id).await.unwrap();
    let mut json = serde_json::to_value(&session).unwrap();
    json.as_object_mut().unwrap().remove("curio_root");
    json.as_object_mut().unwrap().remove("curio_receipts");
    let old: Session = serde_json::from_value(json).unwrap();
    assert!(old.curio_root.is_none() && old.curio_receipts.is_empty());
    let source = session.curio_source_dir();
    session.set_cwd(root.join("managed"), Some(root));
    assert_eq!(session.curio_source_dir(), source);
    assert!(session.as_subagent(None).curios().store().is_err());
}

/// A session in a fresh repository with `count` batches of review feedback
/// waiting in its inbox.
fn waiting(count: usize) -> (tempfile::TempDir, Session, Inbox) {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::new("fake", repo.path().to_path_buf());
    let store = session.prepare_curios().unwrap();
    let curio = store
        .publish(Publish {
            request_id: "publish".into(),
            curio_id: None,
            expected_revision: None,
            title: Some("Review".into()),
            html: "<h1 id=\"heading\">Hello</h1>".into(),
        })
        .unwrap()
        .curio;
    for n in 0..count {
        let thread = store
            .comment(
                &curio.id,
                Comment {
                    request_id: format!("comment-{n}"),
                    revision: 1,
                    target: Target {
                        element_id: None,
                        kind: TargetKind::Document,
                        rect: None,
                        label: "Document".into(),
                        quote: String::new(),
                    },
                    body: format!("Note {n}"),
                },
            )
            .unwrap();
        store
            .send(
                &curio.id,
                Send {
                    request_id: format!("send-{n}"),
                    message_ids: vec![thread.messages[0].id.clone()],
                },
            )
            .unwrap();
    }
    let inbox = Inbox::new();
    let lease = session.curios().claim_delivery().unwrap();
    assert_eq!(lease.poll(&inbox).unwrap(), count);
    (repo, session, inbox)
}

fn front_end(inbox: &Inbox) -> FrontEnd {
    FrontEnd {
        inbox: inbox.clone(),
        ..FrontEnd::default()
    }
}

/// The user messages `session` holds.
fn said(session: &Session) -> Vec<&str> {
    session
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn feedback_waiting_at_a_prompt_leads_the_prompt_in_one_message() {
    let (_repo, mut session, inbox) = waiting(1);
    let provider =
        crate::agent_loop::tests::Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
    let (events, _rx) = mpsc::channel(64);
    session
        .prompt(
            "hello",
            &provider,
            &[],
            &front_end(&inbox),
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    let said = said(&session);
    assert_eq!(said.len(), 1, "{said:#?}");
    let (notices, rest) = nth_protocol::split_notices(said[0]);
    assert_eq!(notices.len(), 1);
    assert!(rest.starts_with("hello"), "{rest}");
}

#[tokio::test]
async fn batches_delivered_together_read_back_as_notices() {
    let (_repo, mut session, inbox) = waiting(2);
    let provider =
        crate::agent_loop::tests::Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
    let (events, _rx) = mpsc::channel(64);
    session
        .deliver_curios(
            &provider,
            &[],
            &front_end(&inbox),
            &events,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    let said = said(&session);
    assert_eq!(said.len(), 1);
    let (notices, rest) = nth_protocol::split_notices(said[0]);
    assert_eq!(notices.len(), 2);
    assert!(!rest.contains("<curio"), "{rest}");
}

#[tokio::test]
async fn feedback_after_an_unanswered_prompt_joins_it() {
    use crate::agent_loop::tests::{Failure, Reply, Scripted};

    let (_repo, mut session, inbox) = waiting(1);
    let (events, _rx) = mpsc::channel(64);
    let refused = Scripted::script(vec![Reply::Refused(Failure::Down)]);
    let cancel = CancellationToken::new();
    // Prompted where the feedback is not yet seen, so only it is left.
    assert!(
        session
            .prompt(
                "hello",
                &refused,
                &[],
                &FrontEnd::default(),
                &events,
                &cancel
            )
            .await
            .is_err()
    );
    let answers = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
    session
        .deliver_curios(&answers, &[], &front_end(&inbox), &events, &cancel)
        .await
        .unwrap();

    let said = said(&session);
    assert_eq!(said.len(), 1, "{said:#?}");
    let (notices, rest) = nth_protocol::split_notices(said[0]);
    assert_eq!(notices.len(), 1);
    assert!(rest.starts_with("hello"), "{rest}");
}
