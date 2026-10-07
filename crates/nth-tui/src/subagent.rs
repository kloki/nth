//! A subagent's tab: the agent the model delegated to, what it was asked,
//! how its turn is doing, and its own chat under that. While the tab shows,
//! the prompt talks to it.

use std::time::{Duration, Instant};

use nth_protocol::{Event, TaskOutcome};
use nth_session::subagent::State;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::TabState, chat::Chat, spinner, theme};

/// The header row above the chat.
const HEADER_ROWS: u16 = 2;
/// The most characters of what it was asked the tab's label shows.
const TITLE_CHARS: usize = 20;

pub struct SubagentView {
    pub agent: String,
    pub description: String,
    pub chat: Chat,
    /// Where it is between prompts; `None` until its first one.
    state: Option<State>,
    /// When the running turn began.
    running_since: Option<Instant>,
    /// How long the last turn took.
    took: Option<Duration>,
    /// Tool calls of the running or last turn.
    tool_calls: usize,
    /// Prompts waiting in its inbox, as last heard.
    pub queued: usize,
    /// Its session was left: the tab closes once its turn has ended.
    pub closing: bool,
}

impl SubagentView {
    pub fn new(agent: String, description: String, cwd: std::path::PathBuf) -> Self {
        Self {
            agent,
            description,
            chat: Chat::new(cwd),
            state: None,
            running_since: None,
            took: None,
            tool_calls: 0,
            queued: 0,
            closing: false,
        }
    }

    /// A turn starts on `text`: the task tool's prompt, or one you typed.
    pub fn prompted(&mut self, text: String) {
        self.chat.transcript.push_user(text);
        self.chat.jump_bottom();
        self.state = Some(State::Running);
        self.running_since = Some(Instant::now());
        self.took = None;
        self.tool_calls = 0;
    }

    pub fn apply(&mut self, event: &Event) {
        if matches!(event, Event::ToolStarted(_)) {
            self.tool_calls += 1;
        }
        self.chat.apply(event);
    }

    pub fn ended(&mut self, outcome: &TaskOutcome, elapsed: Duration, model: &str) {
        self.running_since = None;
        self.took = Some(elapsed);
        let transcript = &mut self.chat.transcript;
        self.state = Some(match outcome {
            TaskOutcome::Completed(_) => {
                transcript.finish_turn(Ok(()), model, elapsed);
                State::Idle
            }
            TaskOutcome::Failed(error) => {
                transcript.finish_turn(Err(error.clone()), model, elapsed);
                State::Failed
            }
            TaskOutcome::Interrupted => {
                transcript.interrupt(elapsed);
                State::Interrupted
            }
        });
    }

    pub fn is_running(&self) -> bool {
        self.running_since.is_some()
    }

    /// When the running turn began, for the prompt's spinner.
    pub fn running_since(&self) -> Option<Instant> {
        self.running_since
    }

    /// The tab's name in the header: the agent and what it was asked, cut
    /// short.
    pub fn label(&self) -> String {
        let mut title: String = self.description.chars().take(TITLE_CHARS).collect();
        if self.description.chars().count() > TITLE_CHARS {
            title.push('…');
        }
        format!("{} · {title}", self.agent)
    }

    /// In front of the label: the spinner while its turn runs.
    pub fn icon(&self) -> Option<&'static str> {
        self.running_since
            .map(|since| spinner::frame(since.elapsed()))
    }

    pub fn state(&self) -> TabState {
        match self.state {
            None | Some(State::Running) => TabState::Working,
            Some(State::Idle) => TabState::Done,
            Some(State::Interrupted) | Some(State::Failed) => TabState::Failed,
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.chat.scroll_up(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.chat.scroll_down(lines);
    }

    pub fn page_up(&mut self) {
        self.chat.page_up();
    }

    pub fn page_down(&mut self) {
        self.chat.page_down();
    }

    pub fn jump_top(&mut self) {
        self.chat.jump_top();
    }

    pub fn jump_bottom(&mut self) {
        self.chat.jump_bottom();
    }

    /// Draws the header and the chat, and returns where the chat went, for
    /// its scrollbar.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let [header, chat] =
            Layout::vertical([Constraint::Length(HEADER_ROWS), Constraint::Min(0)]).areas(area);
        frame.render_widget(Paragraph::new(self.header()), header);
        self.chat.draw(frame, chat, "");
        chat
    }

    /// `@explore · find tabs · running · 12s · 3 tool calls · 1 queued`.
    fn header(&self) -> Line<'static> {
        let dim = theme::dim();
        let (state, colour, elapsed) = match (self.state, self.running_since, self.took) {
            (_, Some(since), _) => ("running", Color::Yellow, since.elapsed()),
            (Some(State::Idle), _, Some(took)) => ("done", Color::Green, took),
            (Some(State::Interrupted), _, Some(took)) => ("interrupted", Color::Yellow, took),
            (Some(State::Failed), _, Some(took)) => ("failed", Color::Red, took),
            _ => ("starting", Color::Yellow, Duration::ZERO),
        };
        let calls = match self.tool_calls {
            1 => "1 tool call".to_string(),
            n => format!("{n} tool calls"),
        };
        let mut rest = format!(" · {}s · {calls}", elapsed.as_secs());
        if self.queued > 0 {
            rest.push_str(&format!(" · {} queued", self.queued));
        }
        Line::from(vec![
            Span::styled(format!("@{}", self.agent), Style::new().fg(Color::Cyan)),
            Span::styled(format!(" · {} · ", self.description), dim),
            Span::styled(state, Style::new().fg(colour)),
            Span::styled(rest, dim),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> SubagentView {
        SubagentView::new("explore".into(), "find tabs".into(), "/repo".into())
    }

    #[test]
    fn the_state_follows_the_turn() {
        let mut view = view();
        assert_eq!(view.label(), "explore · find tabs");
        assert_eq!(view.state(), TabState::Working);
        assert_eq!(view.icon(), None, "no turn to spin for yet");
        view.prompted("go".into());
        assert_eq!(view.state(), TabState::Working);
        assert!(view.icon().is_some(), "spins while running");
        assert!(view.is_running());
        view.ended(
            &TaskOutcome::Completed("ok".into()),
            Duration::from_secs(2),
            "glm",
        );
        assert_eq!(view.state(), TabState::Done);
        assert_eq!(view.icon(), None);
        assert!(!view.is_running());
        view.prompted("again".into());
        view.ended(&TaskOutcome::Interrupted, Duration::from_secs(1), "glm");
        assert_eq!(view.state(), TabState::Failed);
        view.prompted("once more".into());
        view.ended(
            &TaskOutcome::Failed("boom".into()),
            Duration::from_secs(1),
            "glm",
        );
        assert_eq!(view.state(), TabState::Failed);
    }

    #[test]
    fn the_header_counts_the_turns_tool_calls() {
        let mut view = view();
        view.prompted("go".into());
        view.apply(&Event::ToolStarted(nth_protocol::ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        }));
        view.queued = 1;
        let header: String = view
            .header()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(
            header,
            "@explore · find tabs · running · 0s · 1 tool call · 1 queued"
        );

        view.ended(
            &TaskOutcome::Completed("ok".into()),
            Duration::from_secs(3),
            "glm",
        );
        view.queued = 0;
        let header: String = view
            .header()
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(header, "@explore · find tabs · done · 3s · 1 tool call");
    }
}
