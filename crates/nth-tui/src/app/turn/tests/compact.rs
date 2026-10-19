//! `/compact`: a summary request run as a turn.

use super::*;

#[tokio::test]
async fn a_compaction_summarizes_the_session_and_shows_a_divider() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "go");
    end(&mut app).await;

    send(&mut app, "/compact");
    assert!(app.is_busy(), "the compaction runs as a turn");
    end(&mut app).await;

    assert!(!app.is_busy());
    assert!(app.queue.is_empty());
    let session = app.session.as_ref().expect("session came back");
    assert!(
        matches!(session.messages.last(), Some(Message::Compaction { .. })),
        "{:?}",
        session.messages.last()
    );
    assert_eq!(
        app.chat.transcript.entries().last(),
        Some(&Entry::Compacted("ok".into())),
        "the divider, with no footer under it"
    );
    let icon = nth_icons::icons().context;
    let rows = crate::app::tests::rows(&mut app);
    assert!(
        rows.iter()
            .any(|row| row.contains(&format!("{icon} compacted · ok"))),
        "drawn where the divider landed: {rows:?}"
    );
}

#[tokio::test]
async fn a_compaction_after_a_model_pick_asks_the_picked_model_and_limits() {
    let provider = Arc::new(Scripted::new([vec![StreamEvent::TextDelta(
        "the summary".into(),
    )]]));
    let mut session = Session::new("old", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.messages.push(Message::User("old history".into()));
    let mut app = App::new(session, provider.clone(), Arc::new(Vec::new()));
    app.llms = Some(Listing::from(vec![listed("new", 3_000, 1_000)]));
    app.model = "new".into();

    send(&mut app, "/compact");
    end(&mut app).await;

    assert_eq!(
        provider.asked(),
        ["new"],
        "the summary runs on the model picked since the last turn"
    );
    let session = app.session.as_ref().expect("session came back");
    assert_eq!(session.model, "new");
    assert_eq!(
        (session.context_window, session.max_output),
        (Some(3_000), Some(1_000)),
        "the session carries the picked model's limits"
    );
}

#[tokio::test]
async fn a_compaction_sent_while_busy_runs_after_the_turn() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "go");
    send(&mut app, "/compact");

    assert_eq!(app.queue, [Queued::Compact], "it waits for the turn");

    end(&mut app).await;
    assert!(app.is_busy(), "the compaction runs next");
    end(&mut app).await;

    assert!(!app.is_busy());
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::Compacted(_))
    ));
}

#[tokio::test]
async fn esc_interrupts_a_compaction_and_keeps_the_session() {
    let hang = Arc::new(Scripted::hang());
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, hang, Arc::new(Vec::new()));
    app.session
        .as_mut()
        .expect("idle")
        .messages
        .push(Message::User("old".into()));

    send(&mut app, "/compact");
    tokio::task::yield_now().await;
    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    assert!(!app.is_busy());
    let session = app.session.as_ref().expect("session came back");
    assert!(
        matches!(session.messages.last(), Some(Message::User(text)) if text == "old"),
        "nothing was summarized"
    );
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::Interrupted { .. })
    ));
}

#[tokio::test]
async fn an_interrupted_turn_discards_a_queued_compaction() {
    let (mut app, _) = busy_app().await;
    send(&mut app, "/compact");
    assert_eq!(app.queue, [Queued::Compact]);

    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    assert!(app.queue.is_empty());
    assert!(app.prompt.is_empty(), "no text of yours to give back");
    assert_eq!(app.hint.as_deref(), Some("compaction discarded"));
}

#[tokio::test]
async fn compacting_nothing_asks_nothing_and_says_nothing() {
    let provider = Arc::new(Scripted::default());
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, provider.clone(), Arc::new(Vec::new()));

    send(&mut app, "/compact");
    assert!(app.is_busy(), "a compaction runs as a turn");
    end(&mut app).await;

    assert!(!app.is_busy());
    assert!(app.queue.is_empty());
    assert!(
        app.chat.transcript.entries().next().is_none(),
        "no divider and no footer for a session with nothing to summarize"
    );
    assert!(provider.sent().is_empty(), "the model was never asked");
    assert!(app.session.as_ref().expect("session came back").is_empty());
}

#[tokio::test]
async fn a_manual_compaction_shows_only_the_summarys_first_line() {
    let provider = Arc::new(Scripted::new([
        vec![StreamEvent::TextDelta("ok".into())],
        vec![StreamEvent::TextDelta(
            "Objective: ship\n\n## Details\nmore".into(),
        )],
    ]));
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, provider, Arc::new(Vec::new()));
    send(&mut app, "go");
    end(&mut app).await;

    send(&mut app, "/compact");
    end(&mut app).await;

    assert_eq!(
        app.chat.transcript.entries().last(),
        Some(&Entry::Compacted(
            "Objective: ship\n\n## Details\nmore".into()
        )),
        "the whole summary is kept for the clipboard and the model"
    );
    let rows = crate::app::tests::rows(&mut app);
    assert!(
        rows.iter()
            .any(|row| row.contains("compacted · Objective: ship")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("## Details")),
        "one row, whatever the summary holds: {rows:?}"
    );
}

#[tokio::test]
async fn a_request_after_a_compaction_starts_at_the_checkpoint() {
    let provider = Arc::new(Scripted::new([
        vec![StreamEvent::TextDelta("first answer".into())],
        vec![StreamEvent::TextDelta("the summary".into())],
        vec![StreamEvent::TextDelta("second answer".into())],
    ]));
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, provider.clone(), Arc::new(Vec::new()));
    send(&mut app, "go");
    end(&mut app).await;
    send(&mut app, "/compact");
    end(&mut app).await;

    send(&mut app, "next");
    end(&mut app).await;

    let sent = provider.sent();
    assert_eq!(sent.len(), 3, "the turn, the summary, the turn after");
    assert_eq!(sent[1].len(), 1, "the summary request is one user message");
    let Message::User(head) = &sent[1][0] else {
        panic!("the summary request: {:?}", sent[1]);
    };
    assert!(head.contains("[User]: go"), "{head}");

    let after = &sent[2];
    assert!(matches!(after.first(), Some(Message::System(_))));
    let Message::User(checkpoint) = &after[1] else {
        panic!("the checkpoint: {:?}", after[1]);
    };
    assert!(
        checkpoint.starts_with("<conversation-checkpoint>")
            && checkpoint.contains("<summary>\nthe summary\n</summary>")
            && checkpoint.contains("[Assistant]: first answer"),
        "the marker is read as one user message, the latest reply kept: {checkpoint}"
    );
    assert_eq!(after[2], Message::User("next".into()));
    assert_eq!(after.len(), 3, "the summarized messages are not sent again");
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::TurnDone { .. })
    ));
}

#[tokio::test]
async fn esc_during_a_compaction_leaves_a_session_that_still_runs() {
    let provider = Arc::new(Scripted::replying([Reply::Hang]));
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, provider, Arc::new(Vec::new()));
    app.session
        .as_mut()
        .expect("idle")
        .messages
        .push(Message::User("old".into()));

    send(&mut app, "/compact");
    tokio::task::yield_now().await;
    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    assert!(!app.is_busy());
    let session = app.session.as_ref().expect("session came back");
    assert!(
        matches!(session.messages.last(), Some(Message::User(text)) if text == "old"),
        "nothing was summarized"
    );
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::Interrupted { .. })
    ));

    // The session the interrupted compaction held is valid to go on.
    send(&mut app, "next");
    end(&mut app).await;

    assert!(!app.is_busy());
    let session = app.session.as_ref().expect("session came back");
    assert!(matches!(
        session.messages.last(),
        Some(Message::Assistant(reply)) if reply.text == "ok"
    ));
}

#[tokio::test]
async fn a_queued_compaction_shows_its_label_until_the_turn_ends() {
    let provider = Arc::new(Scripted::new([
        vec![StreamEvent::TextDelta("ok".into())],
        vec![StreamEvent::TextDelta("the summary".into())],
    ]));
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, provider, Arc::new(Vec::new()));
    send(&mut app, "go");
    send(&mut app, "/compact");

    let rows = crate::app::tests::rows(&mut app);
    assert!(
        rows[15].starts_with(" > 1 queued · /compact"),
        "{:?}",
        rows[15]
    );

    end(&mut app).await;
    assert!(app.is_busy(), "the compaction runs next");
    end(&mut app).await;

    assert!(!app.is_busy());
    assert_eq!(
        app.chat.transcript.entries().last(),
        Some(&Entry::Compacted("the summary".into()))
    );
}
