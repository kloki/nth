//! What starts while a save between turns is in flight: it waits for the
//! save, or the save of an older clone could land after the turn's own.

use tokio::sync::oneshot;

use super::{notices::monitor_says, *};
use crate::app::tests::{rows, status_row};

/// A save between turns, as `/add-dir` starts, that lands once the
/// returned sender is sent to or dropped.
fn gate_save(app: &mut App) -> oneshot::Sender<()> {
    let (open, gate) = oneshot::channel();
    app.session_saving.start(|_| {
        tokio::spawn(async move {
            let _ = gate.await;
            Ok(())
        })
    });
    open
}

/// Opens the gate and lets the app hear the save landed.
async fn land(app: &mut App, open: oneshot::Sender<()>) {
    open.send(()).expect("the save waits");
    let saved = app.session_saving.join().await.expect("saves");
    app.session_saved(saved);
}

fn queued_row(app: &mut App) -> String {
    status_row(&rows(app), 1).trim_end().to_string()
}

fn hang_app() -> App {
    let session = Session::new("glm", "/repo".into());
    App::bare(session, Arc::new(Scripted::hang()), Arc::new(Vec::new()))
}

#[tokio::test]
async fn a_turn_waits_for_a_pending_save() {
    let mut app = hang_app();
    let open = gate_save(&mut app);

    app.start_turn("go".into());
    send(&mut app, "then");
    assert!(!app.is_busy(), "the turn waits for the save");
    assert!(app.session.is_some(), "the session stays for the save");
    assert_eq!(queued_row(&mut app), " > 2 queued · go");

    land(&mut app, open).await;
    assert!(app.is_busy(), "the turn starts once the save landed");
    assert_eq!(last_user(&app), Some("go"));
    assert_eq!(*app.turns.queued(), [Queued::Prompt("then".into())]);
}

#[tokio::test]
async fn an_approval_waits_as_itself() {
    let mut app = hang_app();
    let _open = gate_save(&mut app);

    app.start(Queued::Approve("skip it".into()));

    assert_eq!(app.turns.held(), Some(&Queued::Approve("skip it".into())));
    assert_eq!(queued_row(&mut app), " > 1 queued · /approve skip it");
}

#[tokio::test]
async fn a_held_wake_gives_way_to_what_you_send() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (sent, held) in [
        ("hi", Queued::Prompt("hi".into())),
        ("!echo hi", Queued::Shell("echo hi".into())),
    ] {
        let mut app = shell_app(dir.path());
        let _open = gate_save(&mut app);
        monitor_says(&mut app, "build failed").await;
        app.notices_due();
        assert_eq!(app.turns.held(), Some(&Queued::Prompt(String::new())));
        assert!(
            !queued_row(&mut app).contains("queued"),
            "nobody sent the wake"
        );

        match sent.strip_prefix('!') {
            Some(text) => command(&mut app, text),
            None => send(&mut app, sent),
        }
        assert_eq!(app.turns.held(), Some(&held));
        assert!(app.turns.queued().is_empty());
    }
}

#[tokio::test]
async fn a_wake_behind_a_held_command_comes_after_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = shell_app(dir.path());
    let open = gate_save(&mut app);
    command(&mut app, "echo hi");
    monitor_says(&mut app, "build failed").await;
    app.notices_due();
    assert_eq!(app.turns.held(), Some(&Queued::Shell("echo hi".into())));
    assert!(app.turns.queued().is_empty(), "the wake is not queued");

    land(&mut app, open).await;
    assert!(app.is_busy(), "the command runs");
    end(&mut app).await;
    assert!(app.is_busy(), "then the notices wake the model");
    end(&mut app).await;

    let session = app.session.as_ref().expect("session came back");
    assert!(
        session
            .messages
            .iter()
            .any(|m| matches!(m, Message::User(sent) if sent.contains("build failed"))),
        "the notices went to the model: {:?}",
        session.messages
    );
}

#[tokio::test]
async fn esc_hands_a_held_turn_back() {
    let mut app = hang_app();
    let open = gate_save(&mut app);
    send(&mut app, "go");
    send(&mut app, "then");

    app.apply(crate::app::keys::Action::Interrupt);

    assert_eq!(app.prompt.text(), "go\n\nthen");
    assert!(app.turns.held().is_none() && app.turns.queued().is_empty());
    land(&mut app, open).await;
    assert!(!app.is_busy(), "nothing left to start");
}

#[tokio::test]
async fn clearing_the_session_hands_a_held_turn_back() {
    let mut app = hang_app();
    let open = gate_save(&mut app);
    let old = app.session_id;
    send(&mut app, "go");

    send(&mut app, "/clear");

    assert_ne!(app.session_id, old);
    assert_eq!(app.prompt.text(), "go");
    assert!(app.turns.held().is_none());
    land(&mut app, open).await;
    assert!(!app.is_busy(), "nothing runs in the new session");
}

#[tokio::test]
async fn esc_on_a_held_prompt_holds_the_notices_too() {
    let mut app = hang_app();
    let open = gate_save(&mut app);
    monitor_says(&mut app, "build failed").await;
    send(&mut app, "go");

    app.apply(crate::app::keys::Action::Interrupt);
    land(&mut app, open).await;
    app.notices_due();

    assert!(!app.is_busy(), "the notices wait for your next prompt");
    assert!(app.inbox.wakes());
    assert_eq!(app.prompt.text(), "go");
}

#[tokio::test]
async fn esc_on_a_held_wake_leaves_the_notices_to_wake_the_model() {
    let mut app = hang_app();
    let open = gate_save(&mut app);
    monitor_says(&mut app, "build failed").await;
    app.notices_due();
    assert_eq!(app.turns.held(), Some(&Queued::Prompt(String::new())));

    app.apply(crate::app::keys::Action::Interrupt);
    assert!(app.turns.held().is_none());
    assert!(app.turns.due().is_some(), "armed again");
    land(&mut app, open).await;
    app.notices_due();

    assert!(app.is_busy(), "the notices woke the model");
}

#[tokio::test]
async fn a_turn_waits_for_a_save_asked_for_during_the_one_it_waited_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = hang_app().with_store(nth_session::Store::at(dir.path()));
    let open = gate_save(&mut app);
    send(&mut app, "go");
    // As `/add-dir` mid-save does: the save runs again once this one ends.
    app.save_session();

    land(&mut app, open).await;
    assert!(app.session_saving.is_running(), "the second save");
    assert!(!app.is_busy(), "the turn waits for it too");
    assert_eq!(app.turns.held(), Some(&Queued::Prompt("go".into())));

    let saved = app.session_saving.join().await.expect("saves");
    app.session_saved(saved);
    assert!(app.is_busy(), "the turn starts once both landed");
}

#[tokio::test]
async fn a_panicking_save_still_starts_the_held_turn() {
    let mut app = hang_app();
    app.session_saving
        .start(|_| tokio::spawn(async { panic!("save exploded") }));
    app.start_turn("go".into());
    assert!(!app.is_busy(), "the turn waits for the save");

    let saved = app.session_saving.join().await;
    app.job_finished(crate::app::finished::Finished::SessionSaved(saved));
    assert!(app.is_busy(), "the turn starts all the same");
    assert_eq!(last_user(&app), Some("go"));
}
