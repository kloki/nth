use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures::StreamExt;
use nth_protocol::{Event, Provider, Tool};
use nth_session::{CancellationToken, Session};
use ratatui::DefaultTerminal;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
    time::MissedTickBehavior,
};

use crate::{prompt::Prompt, scroll::Scroll, transcript::Transcript, view};

const WHEEL_LINES: usize = 3;
/// How often a running turn redraws, so the live reasoning timer advances.
const TICK: Duration = Duration::from_millis(100);

type Turn = JoinHandle<(Session, Result<(), nth_session::Error>)>;

/// A turn in flight. Esc cancels it cooperatively so the session comes back;
/// aborting the task would drop the session with it.
struct Running {
    handle: Turn,
    cancel: CancellationToken,
}

pub struct App {
    pub transcript: Transcript,
    pub prompt: Prompt,
    pub scroll: Scroll,
    pub model: String,
    pub cwd: PathBuf,
    /// The working directory as shown in the status row, `~` for home.
    pub place: String,
    /// When the running turn started; `None` while idle.
    pub busy_since: Option<Instant>,
    /// Chat viewport size from the last draw, for page-sized scrolling.
    pub chat_height: usize,
    pub max_top: usize,
    /// Held here between turns; moved into the turn task while one runs.
    session: Option<Session>,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    turn: Option<Running>,
    quit: bool,
}

enum Step {
    Terminal(Option<std::io::Result<TermEvent>>),
    Session(Event),
    TurnEnded(Result<(Session, Result<(), nth_session::Error>), JoinError>),
    Tick,
}

impl App {
    pub fn new(
        session: Session,
        provider: Arc<dyn Provider>,
        tools: Arc<Vec<Box<dyn Tool>>>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(256);
        let place = match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => match session.cwd.strip_prefix(&home) {
                Ok(rest) => format!("~/{}", rest.display())
                    .trim_end_matches('/')
                    .to_string(),
                Err(_) => session.cwd.display().to_string(),
            },
            _ => session.cwd.display().to_string(),
        };
        Self {
            transcript: Transcript::new(session.cwd.clone()),
            prompt: Prompt::default(),
            scroll: Scroll::default(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            place,
            busy_since: None,
            chat_height: 0,
            max_top: 0,
            session: Some(session),
            provider,
            tools,
            events_tx,
            events_rx,
            turn: None,
            quit: false,
        }
    }

    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut input = EventStream::new();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        while !self.quit {
            terminal.draw(|frame| view::draw(frame, self))?;
            let busy = self.is_busy();
            let turn = self.turn.as_mut().map(|running| &mut running.handle);
            let step = tokio::select! {
                event = input.next() => Step::Terminal(event),
                Some(event) = self.events_rx.recv() => Step::Session(event),
                ended = async {
                    match turn {
                        Some(turn) => turn.await,
                        None => std::future::pending().await,
                    }
                }, if busy => Step::TurnEnded(ended),
                _ = tick.tick(), if busy => Step::Tick,
            };
            match step {
                Step::Terminal(None) => break,
                Step::Terminal(Some(event)) => {
                    self.on_terminal(event.context("reading terminal input")?)
                }
                Step::Session(event) => self.transcript.apply(&event),
                Step::TurnEnded(ended) => self.end_turn(ended.context("turn task failed")?),
                // Nothing changed but time: the redraw advances the reasoning timer.
                Step::Tick => {}
            }
        }
        Ok(())
    }

    pub fn is_busy(&self) -> bool {
        self.turn.is_some()
    }

    fn on_terminal(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            TermEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll.up(WHEEL_LINES, self.max_top),
                MouseEventKind::ScrollDown => self.scroll.down(WHEEL_LINES, self.max_top),
                _ => {}
            },
            TermEvent::Paste(text) => self
                .prompt
                .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.chat_height / 2).max(1);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.prompt.is_empty() {
                    self.quit = true;
                } else {
                    self.prompt.clear();
                }
            }
            KeyCode::Esc => self.interrupt(),
            KeyCode::Char('j') if ctrl => self.prompt.insert('\n'),
            KeyCode::Char('u') if ctrl => self.scroll.up(half, self.max_top),
            KeyCode::Char('d') if ctrl => self.scroll.down(half, self.max_top),
            KeyCode::Char(c) if !ctrl => self.prompt.insert(c),
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.prompt.insert('\n')
            }
            KeyCode::Enter => self.submit(),
            KeyCode::PageUp => self.scroll.up(half, self.max_top),
            KeyCode::PageDown => self.scroll.down(half, self.max_top),
            KeyCode::Home if ctrl => self.scroll.jump_top(self.max_top),
            KeyCode::End if ctrl => self.scroll.jump_bottom(),
            KeyCode::Home => self.prompt.home(),
            KeyCode::End => self.prompt.end(),
            KeyCode::Left => self.prompt.left(),
            KeyCode::Right => self.prompt.right(),
            KeyCode::Backspace => self.prompt.backspace(),
            KeyCode::Delete => self.prompt.delete(),
            _ => {}
        }
    }

    fn submit(&mut self) {
        if self.is_busy() || self.prompt.text().trim().is_empty() {
            return;
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        let text = self.prompt.take();
        self.transcript.push_user(text.clone());
        self.scroll.jump_bottom();
        self.busy_since = Some(Instant::now());

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let events = self.events_tx.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            let result = session
                .prompt(text, provider.as_ref(), &tools, &events, &token)
                .await;
            (session, result)
        });
        self.turn = Some(Running { handle, cancel });
    }

    fn interrupt(&mut self) {
        if let Some(running) = &self.turn {
            running.cancel.cancel();
        }
    }

    fn end_turn(&mut self, (session, result): (Session, Result<(), nth_session::Error>)) {
        // Events sent just before the task returned may still be queued, and
        // they belong above the footer.
        while let Ok(event) = self.events_rx.try_recv() {
            self.transcript.apply(&event);
        }
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        match result {
            Err(nth_session::Error::Interrupted) => self.transcript.interrupt(elapsed),
            result => {
                self.transcript
                    .finish_turn(result.map_err(|e| e.to_string()), &self.model, elapsed)
            }
        }
        self.session = Some(session);
        self.turn = None;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Quitting mid-turn must not leave the agent running tools.
        if let Some(running) = &self.turn {
            running.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, Message, ModelInfo, Request, StreamEvent};

    use super::*;
    use crate::transcript::Entry;

    /// A provider that never answers, and records when its request is dropped.
    struct Hang(Arc<AtomicBool>);

    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl Provider for Hang {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let guard = SetOnDrop(self.0.clone());
            async move {
                let _guard = guard;
                std::future::pending().await
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn esc_interrupts_the_turn_and_keeps_the_session() {
        let dropped = Arc::new(AtomicBool::new(false));
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(
            session,
            Arc::new(Hang(dropped.clone())),
            Arc::new(Vec::new()),
        );
        app.prompt.insert_str("go");
        app.submit();
        tokio::task::yield_now().await;

        app.on_key(KeyEvent::from(KeyCode::Esc));
        let running = app.turn.take().expect("turn was running");
        let ended = running.handle.await.expect("turn task finished");
        app.end_turn(ended);

        assert!(!app.is_busy());
        assert!(dropped.load(Ordering::SeqCst), "request kept running");
        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.messages.last(), Some(&Message::User("go".into())));
        assert!(matches!(
            app.transcript.entries().last(),
            Some(Entry::Interrupted { .. })
        ));
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_running_turn() {
        let dropped = Arc::new(AtomicBool::new(false));
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(
            session,
            Arc::new(Hang(dropped.clone())),
            Arc::new(Vec::new()),
        );
        app.prompt.insert_str("go");
        app.submit();
        tokio::task::yield_now().await;
        assert!(app.is_busy());

        drop(app);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            dropped.load(Ordering::SeqCst),
            "turn kept running after quit"
        );
    }
}
