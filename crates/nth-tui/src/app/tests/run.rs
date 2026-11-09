//! The loop itself: `App::run` on a screen that draws into a buffer, fed
//! terminal events through a channel, its frames read as they are drawn.

use std::{future::Future, io, path::PathBuf, sync::Arc, time::Duration};

use anyhow::bail;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use nth_protocol::Message;
use nth_session::{Session, Store};
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::{oneshot, watch};

use super::Scripted;
use crate::{
    app::{App, AppConfig},
    terminal::Screen,
};

/// How long a test waits for the loop to draw what it expects or to end:
/// a guard against a hang, never a pause.
const PATIENCE: Duration = Duration::from_secs(10);

/// A directory that does not exist, under a temporary one already gone: a
/// fixed path such as `/repo` may be where a CI mounts the checkout. The
/// git and forge processes the loop starts in it fail at spawn, so none is
/// in flight to race a paused clock.
fn nowhere() -> PathBuf {
    tempfile::tempdir().expect("tempdir").path().join("gone")
}

/// The chat on `provider` in [`nowhere`], as [`AppConfig::test`] starts it.
fn config_on(provider: Scripted) -> AppConfig {
    AppConfig::test(Session::new("glm", nowhere()), Arc::new(provider))
}

/// [`config_on`] an idle provider, built.
fn idle_app() -> App {
    App::new(config_on(Scripted::idle()))
}

/// Draws into a buffer and sends each frame's rows to the test.
struct TestScreen {
    terminal: Terminal<TestBackend>,
    frames: watch::Sender<Vec<String>>,
}

impl Screen for TestScreen {
    type Input = UnboundedReceiver<io::Result<TermEvent>>;

    fn draw(&mut self, app: &mut App) -> anyhow::Result<()> {
        app.frame(&mut self.terminal)?;
        let buffer = self.terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect();
        self.frames.send_replace(rows);
        Ok(())
    }

    /// Refused, so ctrl+g in a test never waits on `$EDITOR`.
    fn suspend(&mut self, _: &mut Option<Self::Input>) -> anyhow::Result<()> {
        bail!("no terminal to hand over")
    }

    fn resume(&mut self) -> anyhow::Result<Self::Input> {
        bail!("no terminal to take back")
    }
}

/// The test's side of the loop: the keys it types and the frames it sees.
/// Dropping it ends the input, which ends the loop.
struct Driver {
    keys: UnboundedSender<io::Result<TermEvent>>,
    frames: watch::Receiver<Vec<String>>,
}

impl Driver {
    fn press(&self, code: KeyCode, modifiers: KeyModifiers) {
        let key = TermEvent::Key(KeyEvent::new(code, modifiers));
        self.keys.unbounded_send(Ok(key)).expect("the loop reads");
    }

    fn ctrl(&self, c: char) {
        self.press(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    /// Waits for a frame with `text` on one of its rows, and returns it.
    async fn shows(&mut self, text: &str) -> Vec<String> {
        let wanted = async {
            let rows = self
                .frames
                .wait_for(|rows| rows.iter().any(|row| row.contains(text)))
                .await;
            rows.map(|rows| rows.clone())
        };
        match tokio::time::timeout(PATIENCE, wanted).await {
            Ok(Ok(rows)) => rows,
            Ok(Err(_)) => panic!("the loop ended before showing {text:?}"),
            Err(_) => panic!(
                "never showed {text:?}; last frame:\n{}",
                self.frames.borrow().join("\n")
            ),
        }
    }
}

/// Runs `app`'s loop on an 80 by 24 test screen while `script` drives it,
/// and returns what the loop and the script returned once both are done.
/// A script that drops its driver ends the input, and with it the loop;
/// one that returns it keeps the input open, so the loop must end itself.
async fn drive<F, T>(app: &mut App, script: impl FnOnce(Driver) -> F) -> (anyhow::Result<()>, T)
where
    F: Future<Output = T>,
{
    let (keys, input) = unbounded();
    let (frames, seen) = watch::channel(Vec::new());
    let mut screen = TestScreen {
        terminal: Terminal::new(TestBackend::new(80, 24)).expect("test backend"),
        frames,
    };
    let script = script(Driver { keys, frames: seen });
    let both = async { tokio::join!(app.run(&mut screen, input), script) };
    tokio::time::timeout(PATIENCE, both)
        .await
        .expect("the loop and the script end")
}

#[tokio::test]
async fn a_key_is_drawn_once_the_loop_takes_it() {
    let mut app = idle_app();
    drive(&mut app, |mut driver| async move {
        driver.type_text("hello loop");
        driver.shows("hello loop").await;
    })
    .await
    .0
    .expect("runs");
    assert_eq!(app.prompt.text(), "hello loop");
}

#[tokio::test(start_paused = true)]
async fn scheduled_wakeup_runs_once_after_the_countdown() {
    use nth_protocol::{StreamEvent, ToolCall};

    let provider = Arc::new(Scripted::replying([
        super::Reply::Events(vec![StreamEvent::ToolCall(ToolCall {
            id: "schedule".into(),
            name: "schedule_wakeup".into(),
            arguments: r#"{"duration_ms":5000}"#.into(),
        })]),
        super::Reply::text("waiting"),
        super::Reply::text("checked"),
    ]));
    let mut config = AppConfig::test(Session::new("glm", nowhere()), provider.clone());
    config
        .runtime
        .model_tools
        .push(Arc::new(nth_tools::ScheduleWakeup));
    let mut app = App::new(config);
    drive(&mut app, |mut driver| async move {
        driver.type_text("go");
        driver.press(KeyCode::Enter, KeyModifiers::NONE);
        driver.shows("waiting").await;
        driver.shows("wake in").await;
        tokio::time::advance(Duration::from_secs(5)).await;
        driver.shows("checked").await;
    })
    .await
    .0
    .expect("runs");
    let requests = provider.sent();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .iter()
            .any(|message| matches!(message, Message::User(text) if text.contains("<wakeup")))
    );
    assert!(app.wakeup_deadline().is_none());
}

#[tokio::test]
async fn a_panicking_job_is_reported_not_an_exit() {
    let mut app = idle_app();
    app.indexing
        .start(|_| tokio::spawn(async { panic!("the walk broke") }));
    drive(&mut app, |mut driver| async move {
        driver.shows("listing files failed").await;
        // Still reading keys and drawing.
        driver.type_text("still here");
        driver.shows("still here").await;
    })
    .await
    .0
    .expect("a panicked job is no error of the loop");
}

#[tokio::test]
async fn terminal_eof_ends_the_loop() {
    let mut app = App::new(config_on(Scripted::hang()));
    // A poll that runs until the end of the test unless the exit stops it.
    let (_hold, held) = oneshot::channel::<()>();
    app.curios.polling().start(|_| {
        tokio::spawn(async move {
            let _ = held.await;
            panic!("the poll was let go");
        })
    });
    drive(&mut app, |mut driver| async move {
        driver.type_text("go on");
        driver.press(KeyCode::Enter, KeyModifiers::NONE);
        driver.shows("go on").await;
        // Dropping the driver closes the input with the turn still running.
    })
    .await
    .0
    .expect("an ended input is no error");
    assert!(!app.turn.is_running(), "the turn was stopped");
    assert!(app.session.is_some(), "and handed the session back");
    assert!(
        !app.curios.polling().is_running(),
        "the curios poll was stopped"
    );
}

#[tokio::test]
async fn ctrl_g_without_a_terminal_hints_rather_than_waits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = config_on(Scripted::idle());
    config.editor_dir = dir.path().to_path_buf();
    let mut app = App::new(config);
    drive(&mut app, |mut driver| async move {
        driver.type_text("draft");
        driver.ctrl('g');
        driver
            .shows("cannot open the editor: no terminal to hand over")
            .await;
    })
    .await
    .0
    .expect("runs");
    assert!(!app.is_editing());
    assert_eq!(app.prompt.text(), "draft");
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("lists").count(),
        0,
        "the copy it wrote is gone"
    );
}

#[tokio::test(start_paused = true)]
async fn the_curio_tick_starts_no_poll_while_one_runs() {
    assert_eq!(
        curio_polls_until(Duration::from_millis(200)).await,
        0,
        "none while the first ran, nor at once when it ended"
    );
    assert_eq!(
        curio_polls_until(Duration::from_millis(1000)).await,
        1,
        "one on the tick after it ended"
    );
}

/// How many polls the loop started with one already running for two and a
/// half seconds, which then ended, and the loop running `after` that.
///
/// The app is in [`nowhere`], so no git or forge process is in flight to
/// race the paused clock, and the rest of start-up runs on blocking
/// threads, which hold the clock.
async fn curio_polls_until(after: Duration) -> usize {
    let mut app = idle_app();
    let (release, released) = oneshot::channel::<()>();
    app.curios.polling().start(|_| {
        tokio::spawn(async move {
            let _ = released.await;
            panic!("the poll broke");
        })
    });
    drive(&mut app, |driver| async move {
        // Held, so the input stays open until the script is done.
        let _driver = driver;
        // Off the ticks, which come on the whole second, so which of the
        // two the loop sees first is never a race.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        // The poll ends at once: the clock waits while a task can run.
        release.send(()).expect("the poll waits");
        tokio::time::sleep(after).await;
    })
    .await
    .0
    .expect("runs");
    app.curios.polls
}

#[tokio::test]
async fn quit_mid_turn_saves_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::at(dir.path());
    let mut config = config_on(Scripted::hang());
    let id = config.runtime.session.id;
    config.store = Some(store.clone());
    let mut app = App::new(config);
    drive(&mut app, |mut driver| async move {
        driver.type_text("go on");
        driver.press(KeyCode::Enter, KeyModifiers::NONE);
        driver.shows("go on").await;
        driver.ctrl('c');
        driver.shows("ctrl+c again to quit").await;
        driver.ctrl('c');
        // Kept, so the input stays open and only the quit ends the loop.
        driver
    })
    .await
    .0
    .expect("runs");
    let saved = store.load(id).await.expect("saved");
    assert!(
        saved.messages.contains(&Message::User("go on".into())),
        "{:?}",
        saved.messages
    );
}
