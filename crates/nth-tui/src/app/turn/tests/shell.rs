//! Commands typed after `!`, run without the model.

use super::*;

#[tokio::test]
async fn a_command_runs_without_the_model_and_lands_in_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = shell_app(dir.path());

    command(&mut app, "echo hi");
    assert!(app.is_busy());
    assert!(!app.prompt.shell(), "the prompt is back to the mode");
    end(&mut app).await;

    let entries: Vec<_> = app.chat.transcript.entries().collect();
    assert!(
        matches!(&entries[..], [Entry::Tool { call, output, .. }] if call.name == "bash" && output == &["hi"]),
        "{entries:?}"
    );
    let session = app.session.as_ref().expect("session came back");
    assert!(matches!(
        session.messages.last(),
        Some(Message::ToolResult { content, .. }) if content == "hi\n"
    ));
    assert_eq!(app.history.prev("").as_deref(), Some("!echo hi"));
}

#[tokio::test]
async fn a_command_sent_while_busy_runs_after_the_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = shell_app(dir.path());
    send(&mut app, "hello");
    command(&mut app, "echo later");
    assert_eq!(app.queue, [Queued::Shell("echo later".into())]);

    end(&mut app).await;
    assert!(app.is_busy(), "the command runs next");
    end(&mut app).await;

    let session = app.session.as_ref().expect("session came back");
    assert!(matches!(
        session.messages.last(),
        Some(Message::ToolResult { content, .. }) if content == "later\n"
    ));
}

#[tokio::test]
async fn esc_stops_a_running_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = shell_app(dir.path());
    command(&mut app, "sleep 30");
    tokio::task::yield_now().await;

    app.apply(crate::app::keys::Action::Interrupt);
    end(&mut app).await;

    assert!(!app.is_busy());
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::Interrupted { .. })
    ));
}
