//! The chat app: its state, the loop that drives it, and the fixed layout
//! of chat history, one status row and a three-row prompt. Row heights never
//! depend on content, so nothing shifts while a turn runs.

mod keys;
mod turn;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind, MouseEventKind};
use futures::StreamExt;
use nth_protocol::{Event, Provider, Tool};
use nth_session::Session;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Margin},
};
use tokio::{sync::mpsc, task::JoinError, time::MissedTickBehavior};
use turn::{Ended, Running};

use crate::{
    chat::Chat,
    command::{self, Command, Completion},
    prompt::{self, PROMPT_ROWS, Prompt},
    status,
};

const WHEEL_LINES: usize = 3;
/// How often a running turn redraws, so the live reasoning timer advances.
const TICK: Duration = Duration::from_millis(100);

pub struct App {
    pub chat: Chat,
    pub prompt: Prompt,
    pub model: String,
    pub cwd: PathBuf,
    /// The working directory as shown in the status row, `~` for home.
    pub place: String,
    /// When the running turn started; `None` while idle.
    pub busy_since: Option<Instant>,
    /// Open while the prompt starts a command; Esc closes it until the next edit.
    completion: Option<Completion>,
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
    TurnEnded(Result<Ended, JoinError>),
    Tick,
}

impl App {
    pub fn new(
        session: Session,
        provider: Arc<dyn Provider>,
        tools: Arc<Vec<Box<dyn Tool>>>,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel(256);
        let home = std::env::var("HOME").ok();
        Self {
            chat: Chat::new(session.cwd.clone()),
            prompt: Prompt::default(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            place: status::place(&session.cwd, home.as_deref()),
            busy_since: None,
            completion: None,
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
            terminal.draw(|frame| self.draw(frame))?;
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
                Step::Session(event) => self.chat.transcript.apply(&event),
                Step::TurnEnded(ended) => self.end_turn(ended.context("turn task failed")?),
                // Nothing changed but time: the redraw advances the reasoning timer.
                Step::Tick => {}
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area().inner(Margin::new(1, 0));
        let [chat, status, prompt] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(PROMPT_ROWS),
        ])
        .areas(area);

        let banner = format!("nth · {} · {}", self.model, self.place);
        self.chat.draw(frame, chat, &banner);
        status::draw(frame, status, self);
        prompt::draw(frame, prompt, &self.prompt, self.is_busy());
        // Last, so it pops over the chat and status row.
        if let Some(completion) = &self.completion {
            command::draw(frame, area, prompt, completion);
        }
    }

    fn on_terminal(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            TermEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.chat.scroll_up(WHEEL_LINES),
                MouseEventKind::ScrollDown => self.chat.scroll_down(WHEEL_LINES),
                _ => {}
            },
            TermEvent::Paste(text) => {
                self.prompt
                    .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_completion();
            }
            _ => {}
        }
    }

    fn run_command(&mut self, command: Command) {
        match command {
            // Dropping the app aborts a running turn.
            Command::Exit => self.quit = true,
            // Mid-turn the session is in the turn task, so there is nothing
            // to replace yet.
            Command::Clear if self.is_busy() => {}
            Command::Clear => {
                self.session = Some(Session::new(self.model.clone(), self.cwd.clone()));
                self.chat = Chat::new(self.cwd.clone());
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, ModelInfo, Request, StreamEvent};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    struct Idle;

    impl Provider for Idle {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    /// An idle app whose provider must never be asked for anything.
    pub(crate) fn app() -> App {
        let session = Session::new("glm", "/repo".into());
        App::new(session, Arc::new(Idle), Arc::new(Vec::new()))
    }

    fn rows(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test backend");
        terminal.draw(|frame| app.draw(frame)).expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn status_and_prompt_rows_never_move() {
        let mut app = app();
        let idle = rows(&mut app);
        assert!(idle[8].trim_end().ends_with("glm · /repo"));
        assert!(idle[9].starts_with(" ▎ Ask anything."));

        for i in 0..20 {
            app.chat.transcript.push_user(format!("message {i}"));
        }
        app.prompt.insert_str("one\ntwo\nthree\nfour");
        app.busy_since = Some(Instant::now());
        let busy = rows(&mut app);

        assert!(busy[8].contains("thinking"));
        assert!(busy[8].trim_end().ends_with("esc to interrupt"));
        assert_eq!(
            busy[9..].iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            [" ▎ two", " ▎ three", " ▎ four"]
        );
        assert_eq!(busy[7].trim_end(), " ▎ message 19");
    }

    #[test]
    fn exit_command_quits() {
        let mut app = app();
        app.prompt.insert_str("/exit");
        app.submit();
        assert!(app.quit);
    }

    #[test]
    fn clear_command_starts_a_fresh_session() {
        let mut app = app();
        let old = app.session.as_ref().expect("idle").id;
        app.session
            .as_mut()
            .expect("idle")
            .messages
            .push(nth_protocol::Message::User("hi".into()));
        app.chat.transcript.push_user("hi".into());

        app.prompt.insert_str("/clear");
        app.submit();

        let session = app.session.as_ref().expect("idle");
        assert_ne!(session.id, old);
        assert_eq!(session.messages.len(), 1, "only the system prompt");
        assert!(app.chat.transcript.is_empty());
        assert!(app.prompt.is_empty());
        assert!(!app.is_busy());
    }

    #[test]
    fn completion_pops_over_the_status_row() {
        let mut app = app();
        app.apply(keys::Action::Insert('/'));
        let rows = rows(&mut app);

        assert!(rows[7].starts_with("  /clear"));
        assert!(rows[8].starts_with("  /exit"));
        assert!(rows[9].starts_with(" ▎ /"));
    }
}
