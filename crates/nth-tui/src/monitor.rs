//! A monitor's tab: the command the model left running, how it is doing,
//! and the lines it printed, following the newest unless scrolled up.

use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{MonitorEnd, Stream};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::TabState, theme};

/// The most lines a tab keeps; the log has all of them.
const KEPT_LINES: usize = 2000;
/// The most characters of the description a tab's label shows.
const LABEL_CHARS: usize = 20;

#[derive(Debug)]
pub struct MonitorView {
    pub description: String,
    pub command: String,
    pub log: PathBuf,
    started: Instant,
    /// How it ended, and after how long; `None` while its process runs.
    ended: Option<(MonitorEnd, Duration)>,
    /// Stdout lines so far, which are the model's events.
    events: usize,
    lines: VecDeque<(Stream, String)>,
    /// Lines dropped from the front to keep at most [`KEPT_LINES`].
    dropped: usize,
    /// Its session was left: the tab closes once the process has stopped.
    pub closing: bool,
    top: usize,
    max_top: usize,
    height: usize,
    /// Keeps the newest line in view as lines come.
    follow: bool,
}

impl MonitorView {
    pub fn new(description: String, command: String, log: PathBuf) -> Self {
        Self {
            description,
            command,
            log,
            started: Instant::now(),
            ended: None,
            events: 0,
            lines: VecDeque::new(),
            dropped: 0,
            closing: false,
            top: 0,
            max_top: 0,
            height: 0,
            follow: true,
        }
    }

    pub fn push(&mut self, stream: Stream, line: String) {
        if stream == Stream::Stdout {
            self.events += 1;
        }
        if self.lines.len() == KEPT_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back((stream, line));
    }

    pub fn end(&mut self, end: MonitorEnd) {
        self.ended = Some((end, self.started.elapsed()));
    }

    /// Whether its process still runs, as far as the tab has heard.
    pub fn is_running(&self) -> bool {
        self.ended.is_none()
    }

    /// The tab's name in the header: its description, cut short.
    pub fn label(&self) -> String {
        let mut description: String = self.description.chars().take(LABEL_CHARS).collect();
        if self.description.chars().count() > LABEL_CHARS {
            description.push('…');
        }
        description
    }

    pub fn state(&self) -> TabState {
        match self.ended {
            None => TabState::Working,
            Some((end, _)) if end.is_success() => TabState::Done,
            Some(_) => TabState::Failed,
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
        self.follow = false;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = (self.top + lines).min(self.max_top);
        self.follow = self.top == self.max_top;
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.max(1));
    }

    pub fn jump_top(&mut self) {
        self.scroll_up(self.top);
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
        self.follow = true;
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, home: Option<&str>) {
        let lines = self.render(home);
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = if self.follow {
            self.max_top
        } else {
            self.top.min(self.max_top)
        };
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    fn render(&self, home: Option<&str>) -> Vec<Line<'static>> {
        let dim = theme::dim();
        let (state, colour, elapsed) = match self.ended {
            None => ("running".to_string(), Color::Yellow, self.started.elapsed()),
            Some((end, took)) if end.is_success() => (end.to_string(), Color::Green, took),
            Some((end, took)) => (end.to_string(), Color::Red, took),
        };
        let events = match self.events {
            1 => "1 event".to_string(),
            n => format!("{n} events"),
        };
        let mut log = self.log.display().to_string();
        if let Some(rest) = home.and_then(|home| log.strip_prefix(home)) {
            log = format!("~{rest}");
        }
        let mut lines = vec![
            Line::from(vec![
                Span::styled("$ ", Style::new().fg(Color::Cyan)),
                Span::raw(self.command.clone()),
            ]),
            Line::from(vec![
                Span::styled(state, Style::new().fg(colour)),
                Span::styled(format!(" · {}s · {events} · {log}", elapsed.as_secs()), dim),
            ]),
            Line::default(),
        ];
        if self.dropped > 0 {
            lines.push(Line::styled(
                format!("… {} earlier lines in the log", self.dropped),
                dim,
            ));
        }
        lines.extend(self.lines.iter().map(|(stream, text)| match stream {
            Stream::Stdout => Line::raw(text.clone()),
            Stream::Stderr => Line::styled(text.clone(), dim),
        }));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> MonitorView {
        MonitorView::new(
            "errors in the deploy log".into(),
            "tail -f".into(),
            "/l/1.log".into(),
        )
    }

    #[test]
    fn the_state_follows_the_process() {
        let mut view = view();
        assert_eq!(view.label(), "errors in the deploy…");
        assert_eq!(view.state(), TabState::Working);
        view.end(MonitorEnd::Exited(Some(0)));
        assert_eq!(view.state(), TabState::Done);
        view.end(MonitorEnd::TimedOut { after_ms: 1000 });
        assert_eq!(view.state(), TabState::Failed);
        assert!(!view.is_running());
    }

    #[test]
    fn keeps_the_newest_lines() {
        let mut view = view();
        for i in 0..KEPT_LINES + 2 {
            view.push(Stream::Stdout, i.to_string());
        }
        view.push(Stream::Stderr, "oops".into());
        assert_eq!(view.events, KEPT_LINES + 2);
        assert_eq!(view.dropped, 3);
        assert_eq!(view.lines.front().map(|(_, l)| l.as_str()), Some("3"));
    }
}
