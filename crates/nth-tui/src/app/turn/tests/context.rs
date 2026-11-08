//! The context tab against running turns: which turns it follows, and when
//! its idle preview is rebuilt: at once for your changes, once a tick for
//! what reaches the inbox, never for the clock alone.

use std::path::Path;

use crossterm::event::Event as TermEvent;
use nth_protocol::{Phase, Preview, Tool};
use nth_session::SubagentEvent;

use super::*;
use crate::{
    app::{
        Tab,
        tests::{rows, rows_of},
    },
    command::Command,
};

/// An idle app with the context tab open and shown, built once.
fn open_tab() -> App {
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::idle()), Arc::new(Vec::new()));
    app.run_command(Command::Context);
    assert_eq!(app.context_tab.rebuilds, 1, "opening it builds once");
    app
}

/// A monitor's stdout line, into the inbox and to the app.
fn monitor_line(app: &mut App, line: &str) {
    app.inbox
        .monitor_line(1, "build", Path::new("/tmp/build.log"), line.into());
    app.on_monitor(MonitorEvent::Output {
        id: 1,
        line: line.into(),
        stream: Stream::Stdout,
    });
}

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
    monitor_line(&mut app, "compiled");
    app.every_second();
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
    let mut app = open_tab();
    // The loop's one-second tick dispatches here; it cannot run without a
    // real terminal.
    for _ in 0..5 {
        app.every_second();
        rows(&mut app);
    }
    assert_eq!(app.context_tab.rebuilds, 1);
}

#[tokio::test]
async fn what_reaches_the_inbox_rebuilds_once_a_tick_and_only_when_shown() {
    let mut app = open_tab();
    for n in 0..50 {
        monitor_line(&mut app, &format!("line {n}"));
        app.on_subagent(SubagentEvent::Session {
            id: 1,
            event: Event::TextDelta("token".into()),
        });
    }
    assert_eq!(app.context_tab.rebuilds, 1, "nothing per line");
    app.every_second();
    app.every_second();
    assert_eq!(app.context_tab.rebuilds, 2, "one for all of them");

    app.run_command(Command::Keymaps);
    monitor_line(&mut app, "while hidden");
    app.every_second();
    assert_eq!(app.context_tab.rebuilds, 2, "a hidden tab is not rebuilt");
}

#[tokio::test]
async fn a_tab_that_comes_into_view_without_input_is_rebuilt_on_the_tick() {
    // A tab closing by itself, as an answered subagent's or a gone plan's does.
    let mut app = open_tab();
    app.run_command(Command::Keymaps);
    app.every_second();
    app.content.remove(Tab::Keymaps);
    assert_eq!(app.content.active(), Tab::Context);
    app.every_second();
    assert_eq!(app.context_tab.rebuilds, 2);

    // A resize that splits the panel puts it beside the chat.
    let session = Session::new("glm", "/repo".into());
    let mut app =
        App::new(session, Arc::new(Scripted::idle()), Arc::new(Vec::new())).with_split_width(80);
    app.run_command(Command::Context);
    app.content.select(0);
    rows_of(&mut app, 40);
    app.every_second();
    let rebuilt = app.context_tab.rebuilds;
    rows_of(&mut app, 120);
    app.every_second();
    assert_eq!(app.context_tab.rebuilds, rebuilt + 1);

    // Coming back to the terminal: instruction files may have changed.
    app.on_terminal(TermEvent::FocusGained);
    app.every_second();
    assert_eq!(app.context_tab.rebuilds, rebuilt + 2);
}

#[tokio::test]
async fn closing_the_tab_mid_turn_stops_the_turn_building() {
    let hang = Arc::new(Scripted::hang());
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, hang.clone(), Arc::new(Vec::new()));
    app.run_command(Command::Context);
    send(&mut app, "go");
    hang.asked.notified().await;
    assert!(app.context_tab.slot.builds());

    app.close_content();
    assert!(!app.context_tab.slot.builds());
}

#[tokio::test]
async fn opening_the_tab_mid_turn_shows_the_turn_so_far() {
    let hang = Arc::new(Scripted::hang());
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, hang.clone(), Arc::new(Vec::new()));
    app.run_command(Command::Context);
    assert!(app.context_tab.view.preview().is_some());
    app.close_content();
    send(&mut app, "go");
    hang.asked.notified().await;

    app.run_command(Command::Context);
    assert!(
        app.context_tab.view.preview().is_none(),
        "nothing built yet, and the idle preview is from before the turn"
    );
    assert!(
        rows(&mut app)
            .iter()
            .any(|row| row.contains("waiting for the turn's next step"))
    );

    app.close_content();
    // As the `context` tool asking would have had one built.
    let mut built = Session::new("glm", "/repo".into()).context_preview(&[], &app.inbox);
    built.phase = Phase::PendingReply;
    let built = Arc::new(built);
    app.context_tab.slot.publish(built.clone());
    app.run_command(Command::Context);
    assert_eq!(app.context_tab.view.preview(), Some(&*built));
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

#[tokio::test]
async fn a_later_turns_first_context_call_reads_nothing_of_the_last() {
    let call = StreamEvent::ToolCall(ToolCall {
        id: "1".into(),
        name: "context".into(),
        arguments: "{}".into(),
    });
    let provider = Scripted::new([
        vec![StreamEvent::TextDelta("first".into())],
        vec![call],
        vec![StreamEvent::TextDelta("done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(nth_tools::ContextPreview)];
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(provider), Arc::new(tools));
    app.run_command(Command::Context);
    send(&mut app, "one");
    end(&mut app).await;
    app.close_content();

    send(&mut app, "two");
    end(&mut app).await;
    let session = app.session.as_ref().expect("session came back");
    let answer = session
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .expect("the call was answered");
    assert!(answer.contains("call this tool again"), "{answer}");
}

#[tokio::test]
async fn a_failed_turn_task_forgets_what_it_asked() {
    let (mut app, _) = busy_app().await;
    // As the `context` tool does mid-turn.
    app.context_tab.slot.want();
    app.turn
        .start(|_| tokio::spawn(async { panic!("tool bug") }));
    let Err(error) = app.turn.join().await else {
        panic!("the task panicked");
    };
    app.turn_task_failed(error);
    assert!(!app.context_tab.slot.builds());
}
