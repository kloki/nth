//! The context tab against running turns: which turns it follows, and the
//! events, never the clock, that rebuild its idle preview.

use std::path::Path;

use nth_protocol::{
    Tool,
    context_preview::{Phase, Preview},
};

use super::*;
use crate::{
    app::{Tab, tests::rows},
    command::Command,
};

fn has_user(preview: &Preview, text: &str) -> bool {
    preview
        .entries
        .iter()
        .any(|entry| matches!(&entry.message, Message::User(said) if said.contains(text)))
}

#[tokio::test]
async fn the_tab_follows_the_turn_and_refreshes_on_events() {
    let hang = Arc::new(Scripted::hang());
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, hang.clone(), Arc::new(Vec::new()));

    send(&mut app, "one");
    hang.asked.notified().await;
    assert!(
        !app.context_tab.slot.builds(),
        "a closed tab asks the turn for nothing"
    );

    app.run_command(Command::Context);
    assert!(app.context_tab.watching(), "opened mid-turn, it follows it");
    assert!(app.context_tab.slot.builds());

    app.interrupt();
    end(&mut app).await;
    assert!(!app.context_tab.watching());
    assert!(
        !app.context_tab.slot.builds(),
        "a receiver kept past the turn would have every later turn build"
    );
    let idle = app
        .context_tab
        .view
        .preview()
        .expect("rebuilt as the session came back");
    assert_eq!(idle.phase, Phase::Idle);
    assert!(has_user(idle, "one"));

    send(&mut app, "two");
    let snapshot = app.context_tab.next().await;
    assert_eq!(snapshot.phase, Phase::PendingReply);
    assert!(
        has_user(&snapshot, "two"),
        "open as it starts, it shows the turn"
    );
    app.interrupt();
    end(&mut app).await;
    assert!(!app.context_tab.slot.builds());

    let rebuilt = app.context_tab.rebuilds;
    app.inbox
        .monitor_line(1, "build", Path::new("/tmp/build.log"), "compiled".into());
    app.on_monitor(MonitorEvent::Output {
        id: 1,
        line: "compiled".into(),
        stream: Stream::Stdout,
    });
    assert_eq!(app.context_tab.rebuilds, rebuilt + 1);
    let idle = app.context_tab.view.preview().expect("a preview");
    assert!(
        idle.pending
            .iter()
            .any(|(_, text)| text.contains("compiled")),
        "the monitor's line waits for the next request: {:?}",
        idle.pending
    );

    assert_eq!(app.content.active(), Tab::Context);
    app.close_content();
    send(&mut app, "three");
    assert!(app.is_busy());
    assert!(!app.context_tab.watching(), "a closed tab follows no turn");
    assert!(!app.context_tab.slot.builds());
}

#[tokio::test]
async fn the_tab_does_not_rebuild_on_the_tick() {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::idle()), Arc::new(Vec::new()));
    app.run_command(Command::Context);
    let rebuilt = app.context_tab.rebuilds;
    assert_eq!(rebuilt, 1, "opening it builds once");
    // A second of the loop: its one-second tick and the redraws around it.
    app.every_second();
    for _ in 0..10 {
        rows(&mut app);
    }
    assert_eq!(app.context_tab.rebuilds, rebuilt);
}

#[tokio::test]
async fn a_context_tool_call_asks_for_snapshots_for_its_turn_only() {
    let call = StreamEvent::ToolCall(ToolCall {
        id: "1".into(),
        name: "context".into(),
        arguments: "{}".into(),
    });
    let provider = Scripted::new([vec![call], vec![StreamEvent::TextDelta("done".into())]]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(nth_tools::ContextPreview)];
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(provider), Arc::new(tools));

    send(&mut app, "what do you see");
    end(&mut app).await;
    assert!(
        !app.context_tab.slot.builds(),
        "the app's slot outlives the turn, and so would its want"
    );
}
