//! The tests of running turns, by what the turn meets: the ones here
//! interrupt it, fill in skills and save the history.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossterm::event::{KeyCode, KeyEvent};
use nth_protocol::{
    Event, INTERRUPTED, Listing, Message, ModelInfo, MonitorEvent, Provider, Stream, StreamEvent,
    ToolCall, Usage, failed,
};
use tokio::sync::Notify;

use super::*;
use crate::{
    app::tests::{Reply, Scripted},
    chat::Entry,
};

mod compact;
mod moved;
mod notices;
mod shell;
mod steer;

/// An app whose turn is running against a provider that never
/// answers, and the flag that shows when that turn's request was
/// dropped.
async fn busy_app() -> (App, Arc<AtomicBool>) {
    let hang = Arc::new(Scripted::hang());
    let dropped = hang.dropped.clone();
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, hang, Arc::new(Vec::new()));
    app.prompt.insert_str("go");
    app.submit();
    tokio::task::yield_now().await;
    assert!(app.is_busy());
    (app, dropped)
}

/// Lets the running turn task finish before the app hears of it, as
/// when the loop is busy drawing.
async fn settle(app: &App) {
    let handle = app.turn.abort_handle().expect("running");
    while !handle.is_finished() {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn end_turn_drains_tail_events_before_finishing_transcript() {
    let (mut app, _) = busy_app().await;
    app.interrupt();
    let ended = app.turn.join().await.expect("turn ends");
    app.events_tx
        .send(Event::TextDelta("tail reply".into()))
        .await
        .expect("event queued");
    app.events_tx
        .send(Event::Usage(Usage {
            input: 123,
            ..Usage::default()
        }))
        .await
        .expect("usage queued");
    app.end_turn(ended);
    let entries: Vec<_> = app.chat.transcript.entries().collect();
    assert!(format!("{entries:?}").contains("tail reply"), "{entries:?}");
    assert!(matches!(entries.last(), Some(Entry::Interrupted { .. })));
    assert!(app.events_rx.try_recv().is_err());
    assert_eq!(
        app.spent
            .ledger
            .spends()
            .last()
            .expect("spend")
            .tokens
            .input,
        123
    );
}

#[tokio::test]
async fn esc_interrupts_the_turn_and_keeps_the_session() {
    let (mut app, dropped) = busy_app().await;

    app.on_key(KeyEvent::from(KeyCode::Esc));
    let ended = app.turn.join().await.expect("turn task finished");
    app.end_turn(ended);

    assert!(!app.is_busy());
    assert!(dropped.load(Ordering::SeqCst), "request kept running");
    let session = app.session.as_ref().expect("session came back");
    assert_eq!(session.messages.last(), Some(&Message::User("go".into())));
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::Interrupted { .. })
    ));
}

#[tokio::test]
async fn a_skill_command_sends_the_filled_in_skill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hang = Arc::new(Scripted::hang());
    let session = Session::new("glm", dir.path().to_path_buf())
        .with_context(crate::app::tests::with_fix_skill(dir.path()));
    let mut app = App::new(session, hang.clone(), Arc::new(Vec::new()));

    app.prompt.insert_str("/fix the build");
    app.submit();
    // The skill is read first; Esc before the request would lose it.
    hang.asked.notified().await;
    app.on_key(KeyEvent::from(KeyCode::Esc));
    let ended = app.turn.join().await.expect("ends");
    app.end_turn(ended);

    assert_eq!(
        app.chat.transcript.entries().next(),
        Some(&Entry::User("/fix the build".into()))
    );
    let session = app.session.as_ref().expect("session came back");
    let Some(Message::User(sent)) = session.messages.last() else {
        panic!("the prompt was sent");
    };
    assert!(
        sent.starts_with(
            "/fix the build\n\n<skill_content name=\"fix\">\n# Skill: fix\n\nFix the build.\n"
        ),
        "{sent}"
    );
    assert_eq!(session.title().as_deref(), Some("/fix the build"));
}

#[tokio::test]
async fn a_skill_that_cannot_be_read_fails_the_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let context = crate::app::tests::with_fix_skill(dir.path());
    std::fs::remove_file(dir.path().join(".agents/skills/fix/SKILL.md")).expect("removes");
    let session = Session::new("glm", dir.path().to_path_buf()).with_context(context);
    let mut app = App::new(session, Arc::new(Scripted::hang()), Arc::new(Vec::new()));

    app.prompt.insert_str("/fix it");
    app.submit();
    let ended = app.turn.join().await.expect("ends");
    app.end_turn(ended);

    let Some(Entry::TurnError(e)) = app.chat.transcript.entries().last() else {
        panic!("the turn failed");
    };
    assert!(
        e.starts_with("could not run the skill: cannot read "),
        "{e}"
    );
}

#[tokio::test]
async fn a_sent_prompt_is_saved_to_the_history_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nth/prompt-history.jsonl");
    let session = Session::new("glm", "/repo".into());
    let mut app = App::new(session, Arc::new(Scripted::hang()), Arc::new(Vec::new()))
        .with_history(crate::history::History::load(path.clone()).await);

    app.prompt.insert_str("two\nlines");
    app.submit();
    let saved = app.history_saving.join().await.expect("save finished");
    app.history_saved(saved);

    assert_eq!(
        std::fs::read_to_string(&path).expect("saved"),
        "\"two\\nlines\"\n"
    );
    let mut loaded = crate::history::History::load(path).await;
    assert_eq!(loaded.prev("").as_deref(), Some("two\nlines"));
}

fn send(app: &mut App, text: &str) {
    app.prompt.insert_str(text);
    app.submit();
}

async fn end(app: &mut App) {
    let ended = app.turn.join().await.expect("turn task finished");
    app.end_turn(ended);
}

fn last_user(app: &App) -> Option<&str> {
    app.chat
        .transcript
        .entries()
        .rev()
        .find_map(|entry| match entry {
            Entry::User(text) => Some(text.as_str()),
            _ => None,
        })
}

/// A listing entry with the given context window and output limit.
fn listed(id: &str, context: u64, output: u64) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        name: None,
        context: Some(context),
        output: Some(output),
        efforts: Vec::new(),
        origin: None,
        cost: None,
        takes_images: None,
    }
}

/// An idle app in `dir` that runs commands with the real bash.
fn shell_app(dir: &std::path::Path) -> App {
    let session = Session::new("glm", dir.to_path_buf());
    App::new(session, Arc::new(Scripted::default()), Arc::new(Vec::new()))
        .with_shell(Arc::new(nth_tools::Bash::untimed(Default::default())))
}

fn command(app: &mut App, text: &str) {
    app.apply(crate::app::keys::Action::Insert('!'));
    send(app, text);
}
