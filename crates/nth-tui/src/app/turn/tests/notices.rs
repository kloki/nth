//! Notices from monitors and subagents, and when they wake the model.

use super::*;

/// Has a new monitor print `line`, as its task would, and lets the app
/// hear of it.
async fn monitor_says(app: &mut App, line: &str) {
    let m = app
        .monitors
        .register("ci", "watch")
        .await
        .expect("front-end");
    let output = MonitorEvent::Output {
        id: m.id,
        line: line.into(),
        stream: Stream::Stdout,
    };
    assert!(app.monitors.event(output).await);
    while let Ok(event) = app.monitor_rx.try_recv() {
        app.on_monitor(event);
    }
}

fn sent(app: &App) -> Vec<&str> {
    let session = app.session.as_ref().expect("session came back");
    session
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

const NOTICE: &str =
    "<monitor id=\"1\" description=\"ci\" log=\"/tmp/logs/1.log\">\nbuild failed\n</monitor>";

fn app_logging_to_tmp(provider: Arc<dyn Provider>) -> App {
    let app = App::new(
        Session::new("glm", "/repo".into()),
        provider,
        Arc::new(Vec::new()),
    );
    app.monitors.set_log_dir("/tmp/logs".into());
    app
}

#[tokio::test]
async fn notices_wake_an_idle_agent() {
    let mut app = app_logging_to_tmp(Arc::new(Scripted::default()));
    monitor_says(&mut app, "build failed").await;
    assert!(app.notices_due.is_some(), "waits for more lines first");

    app.notices_due();
    assert!(app.is_busy());
    end(&mut app).await;

    assert_eq!(sent(&app), [NOTICE]);
    assert!(matches!(
        app.chat.transcript.entries().next(),
        Some(Entry::Notice(nth_protocol::NoticeSummary::Monitor {
            lines: 1,
            ..
        }))
    ));
    assert_eq!(last_user(&app), None, "nothing typed");
}

#[tokio::test]
async fn notices_during_a_turn_follow_it() {
    let mut app = app_logging_to_tmp(Arc::new(Scripted::default()));
    send(&mut app, "one");
    monitor_says(&mut app, "build failed").await;
    app.notices_due();

    end(&mut app).await;
    assert!(app.is_busy(), "the notices' own turn");
    end(&mut app).await;

    assert_eq!(sent(&app), ["one", NOTICE]);
}

#[tokio::test]
async fn after_esc_notices_wait_for_the_next_prompt() {
    let (mut app, _) = busy_app().await;
    app.monitors.set_log_dir("/tmp/logs".into());
    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;

    monitor_says(&mut app, "build failed").await;
    app.notices_due();
    assert!(!app.is_busy(), "you stopped the agent");

    send(&mut app, "fix it");
    app.on_key(KeyEvent::from(KeyCode::Esc));
    end(&mut app).await;
    // The interrupted "go" was never answered, so the next prompt joins
    // it rather than following it as a second user message.
    assert_eq!(sent(&app), [format!("go\n\n{NOTICE}\n\nfix it")]);
    assert_eq!(last_user(&app), Some("fix it"));
}

#[tokio::test]
async fn after_a_failed_turn_a_notice_wakes_the_model() {
    let mut app = app_logging_to_tmp(Arc::new(crate::app::tests::Scripted::idle()));
    send(&mut app, "go");
    end(&mut app).await;
    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::TurnError(_))
    ));

    monitor_says(&mut app, "build failed").await;
    app.notices_due();

    assert!(app.is_busy(), "a failure is not a stop");
    end(&mut app).await;
    // The failed "go" was never answered, so the notice joins it.
    assert_eq!(sent(&app), [format!("go\n\n{NOTICE}")]);
}

#[tokio::test]
async fn a_notice_during_a_failed_turn_wakes_the_model_after_it() {
    let mut app = app_logging_to_tmp(Arc::new(crate::app::tests::Scripted::idle()));
    send(&mut app, "go");
    monitor_says(&mut app, "build failed").await;
    app.notices_due();
    end(&mut app).await;

    assert!(app.notices_due.is_some(), "nothing else will arm it");
    app.notices_due();
    assert!(app.is_busy());
    end(&mut app).await;
    assert_eq!(sent(&app), [format!("go\n\n{NOTICE}")]);
}

#[tokio::test]
async fn a_skill_that_cannot_be_read_keeps_the_notices() {
    let dir = tempfile::tempdir().expect("tempdir");
    let context = crate::app::tests::with_fix_skill(dir.path());
    std::fs::remove_file(dir.path().join(".agents/skills/fix/SKILL.md")).expect("removes");
    let session = Session::new("glm", dir.path().to_path_buf()).with_context(context);
    let mut app = App::new(session, Arc::new(Scripted::hang()), Arc::new(Vec::new()));
    app.monitors.set_log_dir("/tmp/logs".into());
    monitor_says(&mut app, "build failed").await;

    send(&mut app, "/fix it");
    end(&mut app).await;

    assert!(matches!(
        app.chat.transcript.entries().last(),
        Some(Entry::TurnError(_))
    ));
    assert!(
        app.inbox.has_notices(),
        "not taken by a turn that never ran"
    );
    assert!(app.notices_due.is_some(), "and still wake the model");
    assert_eq!(sent(&app), Vec::<&str>::new());
}

#[tokio::test]
async fn clear_stops_the_monitors() {
    let mut app = app_logging_to_tmp(Arc::new(Scripted::default()));
    monitor_says(&mut app, "build failed").await;

    app.run_command(Command::Clear);

    assert_eq!(app.monitors.running(), 0);
    assert!(!app.inbox.has_notices());
}

#[tokio::test]
async fn closing_the_picker_leaves_the_turn_running() {
    let (mut app, _) = busy_app().await;
    app.llms = Some(Listing::default());
    app.apply(crate::app::keys::Action::LlmPicker);

    app.on_key(KeyEvent::from(KeyCode::Esc));

    assert!(matches!(app.input, crate::app::input::Input::Prompt));
    let token = app.turn.token().expect("still running");
    assert!(!token.is_cancelled());
}

#[tokio::test]
async fn dropping_the_app_aborts_the_running_turn() {
    let (app, dropped) = busy_app().await;

    drop(app);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        dropped.load(Ordering::SeqCst),
        "turn kept running after quit"
    );
}
