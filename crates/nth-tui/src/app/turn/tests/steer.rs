//! Prompts sent while a turn runs: steered into it, queued behind it, or given back.

use super::*;

#[tokio::test]
async fn enter_while_busy_steers_the_prompt_into_the_turn() {
    let (mut app, _) = busy_app().await;

    send(&mut app, "next");

    assert!(app.prompt.is_empty());
    assert!(app.queue.is_empty());
    assert_eq!(app.inbox.prompts_waiting(), Some((1, "next".into())));
    assert_eq!(last_user(&app), Some("go"), "shown once the model gets it");
    let token = app.turn.token().expect("still running");
    assert!(!token.is_cancelled());
}

#[tokio::test]
async fn prompts_sent_mid_turn_join_the_turn_that_picks_them_up() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "one");
    send(&mut app, "two");
    send(&mut app, "three");

    end(&mut app).await;
    assert!(app.is_busy(), "the steered prompts' own turn");
    assert_eq!(last_user(&app), Some("two\n\nthree"));
    end(&mut app).await;
    assert!(!app.is_busy());
    assert!(app.queue.is_empty());
    let session = app.session.as_ref().expect("session came back");
    let sent: Vec<_> = session
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, ["one", "two\n\nthree"]);
}

#[tokio::test]
async fn a_prompt_sent_mid_turn_preempts_the_tool_calls() {
    let gate = Arc::new(Notify::new());
    // Mid-work when you steer it: a tool call once the gate opens.
    let call = StreamEvent::ToolCall(ToolCall {
        id: "1".into(),
        name: "bash".into(),
        arguments: "{}".into(),
    });
    let provider = Scripted::replying([Reply::HangUntil(gate.clone(), vec![call])])
        .with_fallback(Reply::text("changed plans"));
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(provider), Arc::new(Vec::new()));
    send(&mut app, "go");
    send(&mut app, "stop, just name them");

    gate.notify_one();
    end(&mut app).await;

    assert!(!app.is_busy());
    assert!(app.queue.is_empty());
    let session = app.session.as_ref().expect("session came back");
    assert!(matches!(
        &session.messages[2],
        Message::Assistant(reply) if reply.tool_calls.len() == 1
    ));
    assert!(matches!(
        &session.messages[3],
        Message::ToolResult { content, .. }
            if *content == failed(INTERRUPTED)
    ));
    let Message::User(steered) = &session.messages[4] else {
        panic!("the prompt was steered in: {:?}", session.messages[4]);
    };
    assert!(
        steered.starts_with("stop, just name them\n\n<system-reminder>\nThe user interrupted"),
        "{steered}"
    );
    assert!(matches!(
        &session.messages[5],
        Message::Assistant(reply) if reply.text == "changed plans"
    ));
    // The chat shows the call it skipped and what you typed, in the
    // order the model saw them.
    let entries: Vec<_> = app.chat.transcript.entries().collect();
    let at = entries
        .iter()
        .position(|entry| matches!(entry, Entry::Tool { call, .. } if call.name == "bash"))
        .expect("the skipped call shows");
    assert!(matches!(
        &entries[at],
        Entry::Tool { state: crate::chat::ToolState::Failed(why), .. }
            if why == "interrupted by the user"
    ));
    assert!(matches!(
        &entries[at + 1],
        Entry::User(text) if text == "stop, just name them"
    ));
}

#[tokio::test]
async fn a_prompt_after_a_mode_switch_waits_for_its_own_turn() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "go");
    app.set_mode(nth_protocol::Mode::Plan);
    send(&mut app, "plan it");

    assert_eq!(app.queue, [Queued::Prompt("plan it".into())]);
    assert_eq!(app.inbox.prompts_waiting(), None, "not steered");
    end(&mut app).await;
    assert_eq!(last_user(&app), Some("plan it"));
    end(&mut app).await;
    let session = app.session.as_ref().expect("session came back");
    assert_eq!(session.mode, nth_protocol::Mode::Plan);
}

#[tokio::test]
async fn a_prompt_after_a_model_pick_waits_for_its_own_turn() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "go");
    app.model = "kimi".into();
    send(&mut app, "you now");

    assert_eq!(app.queue, [Queued::Prompt("you now".into())]);
    end(&mut app).await;
    end(&mut app).await;
    let session = app.session.as_ref().expect("session came back");
    assert_eq!(session.model, "kimi");
}

#[tokio::test]
async fn a_steered_prompt_runs_before_what_queued_behind_it() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "go");
    send(&mut app, "next");
    command(&mut app, "make");
    assert_eq!(app.inbox.prompts_waiting(), Some((1, "next".into())));

    end(&mut app).await;
    assert!(app.is_busy(), "the steered prompt's own turn");
    assert_eq!(last_user(&app), Some("next"));
    assert_eq!(app.queue, [Queued::Shell("make".into())], "still waits");
}

#[tokio::test]
async fn a_prompt_sent_while_a_command_runs_waits_for_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = shell_app(dir.path());
    command(&mut app, "echo hi");
    send(&mut app, "hello");

    assert_eq!(app.queue, [Queued::Prompt("hello".into())]);
    assert_eq!(
        app.inbox.prompts_waiting(),
        None,
        "nothing would pick it up"
    );
}

#[tokio::test]
async fn the_status_bar_counts_prompts_on_their_way_into_the_turn() {
    let (mut app, _) = busy_app().await;
    send(&mut app, "next");

    let rows = crate::app::tests::rows(&mut app);
    assert!(
        crate::app::tests::status_row(&rows, 1).starts_with(" > 1 queued · next"),
        "{:?}",
        crate::app::tests::status_row(&rows, 1)
    );

    // A command waiting behind it keeps its place; the prompt on its
    // way in is still the next thing the model hears.
    command(&mut app, "make");
    let rows = crate::app::tests::rows(&mut app);
    assert!(
        crate::app::tests::status_row(&rows, 1).starts_with(" > 2 queued · next"),
        "{:?}",
        crate::app::tests::status_row(&rows, 1)
    );
}

#[tokio::test]
async fn esc_gives_queued_prompts_back_instead_of_sending_them() {
    let (mut app, _) = busy_app().await;
    send(&mut app, "two");
    send(&mut app, "three");
    app.prompt.insert_str("typing");

    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    assert!(!app.is_busy());
    assert!(app.queue.is_empty());
    assert_eq!(app.prompt.text(), "two\n\nthree\n\ntyping");
}

#[tokio::test]
async fn esc_drops_queued_plan_edits_and_approval_with_a_hint() {
    let (mut app, _) = busy_app().await;
    app.plan_edited("# Plan\n", Ok("# Plan\nmore\n".into()));
    assert!(matches!(app.queue.front(), Some(Queued::PlanEdits { .. })));

    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    assert!(app.queue.is_empty());
    assert!(app.prompt.is_empty(), "{:?}", app.prompt.text());
    assert_eq!(app.hint.as_deref(), Some("plan edits discarded"));

    let (mut app, _) = busy_app().await;
    app.queue.push_back(Queued::Approve);
    app.queue.push_back(Queued::Prompt("then this".into()));
    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    assert_eq!(app.prompt.text(), "then this");
    assert_eq!(app.hint.as_deref(), Some("plan approval discarded"));
}

#[tokio::test]
async fn a_usage_report_at_the_tail_of_a_turn_still_counts() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "one");
    settle(&app).await;
    let usage = Usage {
        input: 10,
        output: 2,
        ..Usage::default()
    };
    app.events_tx.try_send(Event::Usage(usage)).expect("room");

    end(&mut app).await;

    assert_eq!(app.usage, Some(usage));
}

#[tokio::test]
async fn a_turn_task_that_panics_leaves_the_app_running() {
    let (mut app, _) = busy_app().await;
    send(&mut app, "next");
    // In place of the real task, whose session is lost with it.
    app.turn
        .start(|_| tokio::spawn(async { panic!("tool bug") }));

    let Err(error) = app.turn.join().await else {
        panic!("the task panicked");
    };
    app.turn_task_failed(error);

    assert!(!app.is_busy());
    assert_eq!(app.prompt.text(), "next", "the queue comes back");
    let session = app.session.as_ref().expect("a fresh session");
    assert!(session.is_empty(), "nothing of the lost one");
    assert_eq!(session.cwd, std::path::Path::new("/repo"));
    let Some(Entry::TurnError(e)) = app.chat.transcript.entries().last() else {
        panic!(
            "the turn failed: {:?}",
            app.chat.transcript.entries().last()
        );
    };
    assert!(e.starts_with("turn task failed: "), "{e}");
    assert!(e.contains("/resume"), "{e}");
}

#[tokio::test]
async fn esc_after_the_reply_ended_still_holds_the_queue_back() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()));
    send(&mut app, "one");
    send(&mut app, "two");
    settle(&app).await;

    app.interrupt();
    end(&mut app).await;

    assert!(!app.is_busy());
    assert_eq!(app.prompt.text(), "two");
    assert!(!app.interrupted, "the next turn starts clean");
}
