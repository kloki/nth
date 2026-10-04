//! The plan tab: the plan file of the session as rendered markdown, with
//! what its latest change did marked in colour: added lines green, removed
//! ones red.

mod diff;

use hoodrich::Change;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, ScrollbarState},
};

use crate::{rich, theme};

/// The plan as last read, and the version its diff is against.
#[derive(Debug, Default)]
pub struct PlanView {
    /// `None` while there is no plan file.
    current: Option<String>,
    /// What the diff is against: the plan before the first change of the
    /// latest turn that changed it, or the one last approved.
    baseline: String,
    /// The plan changed since the running turn started, so later changes
    /// in that turn add to the same diff.
    changed_this_turn: bool,
    /// The running turn wrote the first plan. Everything in it would be
    /// marked new, which says nothing, so it is shown unmarked.
    created_this_turn: bool,
    /// The first line in view, and how far it can go, as of the last draw.
    top: usize,
    max_top: usize,
    height: usize,
    /// Lines added and removed since the baseline.
    counts: (usize, usize),
    /// The plan as drawn last, and the width it was wrapped for, so a frame
    /// renders it only after it or the width changed.
    rendered: Option<(usize, Vec<Line<'static>>)>,
}

impl PlanView {
    pub fn exists(&self) -> bool {
        self.current.is_some()
    }

    /// The plan as last read.
    pub fn text(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// The plan now reads `text`, or is gone with `None`. Returns whether
    /// that changed it.
    pub fn update(&mut self, text: Option<String>) -> bool {
        if text == self.current {
            return false;
        }
        if !self.changed_this_turn {
            self.created_this_turn = self.current.is_none();
            self.baseline = self.current.take().unwrap_or_default();
            self.changed_this_turn = true;
        }
        self.current = text;
        if self.created_this_turn {
            self.baseline = self.current.clone().unwrap_or_default();
        }
        self.changed();
        true
    }

    /// Takes `text` as the plan with nothing marked, as when a session is
    /// opened.
    pub fn settle(&mut self, text: Option<String>) {
        self.current = text;
        self.accept();
    }

    /// Clears the marks: what the plan reads now is what later changes are
    /// shown against.
    pub fn accept(&mut self) {
        self.baseline = self.current.clone().unwrap_or_default();
        self.changed_this_turn = false;
        self.changed();
    }

    /// The plan or its baseline changed: count the diff again, and render
    /// it again when next drawn.
    fn changed(&mut self) {
        self.counts = diff::counts(&self.baseline, self.current.as_deref().unwrap_or_default());
        self.rendered = None;
    }

    /// A turn starts, so the next change to the plan starts a new diff.
    pub fn turn_started(&mut self) {
        self.changed_this_turn = false;
    }

    /// Lines added and removed since the baseline.
    pub fn counts(&self) -> (usize, usize) {
        self.counts
    }

    /// `plan`, then what changed while anything did: `plan +3 -1`.
    pub fn label(&self) -> String {
        match self.counts() {
            (0, 0) => "plan".into(),
            (added, removed) => format!("plan +{added} -{removed}"),
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = (self.top + lines).min(self.max_top);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.max(1));
    }

    pub fn jump_top(&mut self) {
        self.top = 0;
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
    }

    /// Where the view sits in the plan, while the plan is longer than the
    /// tab; `None` while it all fits.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        (self.max_top > 0).then(|| {
            ScrollbarState::new(self.max_top + 1)
                .position(self.top)
                .viewport_content_length(self.height)
        })
    }

    /// `path` is the plan file as the header names it.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, path: &str) {
        let width = usize::from(area.width);
        let lines = match self.rendered.take() {
            Some((drawn, lines)) if drawn == width => lines,
            _ => self.lines(path, width),
        };
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = self.top.min(self.max_top);
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines.clone()).scroll((top, 0)), area);
        self.rendered = Some((width, lines));
    }

    /// A header naming the file and what changed, then the plan wrapped to
    /// `width`, each line behind a gutter that marks it added or removed.
    fn lines(&self, path: &str, width: usize) -> Vec<Line<'static>> {
        let mut header = vec![path.to_string()];
        let (added, removed) = self.counts();
        if added + removed > 0 {
            header.push(format!("+{added} -{removed}"));
        }
        header.push("ctrl+g edit".into());
        header.push("/approve".into());
        let mut lines = vec![
            Line::styled(header.join(" · "), theme::dim()),
            Line::default(),
        ];
        if self.current.is_none() {
            lines.push(Line::styled(
                "No plan yet. In plan mode the model writes it here.",
                theme::dim(),
            ));
            return lines;
        }
        let room = width.saturating_sub(GUTTER).max(1);
        let current = self.current.as_deref().unwrap_or_default();
        for (change, line) in rich::markdown_diff(&self.baseline, current, room) {
            let (mark, style) = match change {
                Change::Same => ("  ", Style::new()),
                Change::Added => ("+ ", Style::new().fg(Color::Green)),
                Change::Removed => ("- ", Style::new().fg(Color::Red)),
            };
            let line = line.patch_style(style);
            for (i, row) in rich::wrap(line, room).into_iter().enumerate() {
                let gutter = if i == 0 { mark } else { "  " };
                let mut spans = vec![Span::styled(gutter, style)];
                spans.extend(row.spans);
                lines.push(Line::from(spans));
            }
        }
        lines
    }
}

/// The `+ ` or `- ` in front of every line.
const GUTTER: usize = 2;

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn shows_the_latest_turns_changes_until_accepted() {
        let mut view = PlanView::default();
        view.settle(None);
        assert!(!view.exists());

        view.turn_started();
        assert!(view.update(Some("# Plan\nstep one\n".into())));
        assert_eq!(view.label(), "plan", "a first plan is shown unmarked");
        view.update(Some("# Plan\nstep one\nstep two\n".into()));
        assert_eq!(view.label(), "plan", "and so is the rest of its turn");

        view.turn_started();
        view.update(Some("# Plan\nstep 1\nstep two\n".into()));
        assert_eq!(
            view.label(),
            "plan +1 -1",
            "against the plan before this turn"
        );
        view.update(Some("# Plan\nstep 1\nstep two\nstep three\n".into()));
        assert_eq!(view.label(), "plan +2 -1", "one turn, one diff");

        view.turn_started();
        assert!(!view.update(Some("# Plan\nstep 1\nstep two\nstep three\n".into())));
        assert_eq!(
            view.label(),
            "plan +2 -1",
            "a turn that only talks keeps it"
        );

        view.accept();
        assert_eq!(view.label(), "plan");
    }

    #[test]
    fn the_scrollbar_shows_only_when_the_plan_overflows() {
        let mut view = PlanView::default();
        view.settle(Some("one\ntwo\n".into()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 10)).expect("backend");
        terminal
            .draw(|frame| view.draw(frame, frame.area(), "p.md"))
            .expect("draws");
        assert!(view.scrollbar().is_none());

        view.settle(Some("line\n".repeat(30)));
        terminal
            .draw(|frame| view.draw(frame, frame.area(), "p.md"))
            .expect("draws");
        assert!(view.scrollbar().is_some());
    }

    #[test]
    fn lines_carry_a_gutter_and_wrap_under_it() {
        let mut view = PlanView::default();
        view.settle(Some("keep\ngone\n".into()));
        view.update(Some("keep\na long new line here\n".into()));

        let lines = view.lines(".nth/plans/1.md", 12);

        assert_eq!(
            text(&lines),
            [
                ".nth/plans/1.md · +1 -1 · ctrl+g edit · /approve",
                "",
                "  keep",
                "- gone",
                "+ a long new",
                "  line here",
            ]
        );
        assert_eq!(lines[3].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[4].spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn the_plan_renders_as_markdown_and_removed_lines_as_written() {
        let mut view = PlanView::default();
        view.settle(Some("# Plan\n## Old\n".into()));
        view.update(Some("# Plan\n- step\n".into()));

        let lines = view.lines("p.md", 40);

        assert_eq!(text(&lines[2..]), ["  Plan", "- ## Old", "+ - step"]);
        let heading = &lines[2].spans[1].style;
        assert!(heading.add_modifier.contains(Modifier::BOLD), "{heading:?}");
        assert!(
            lines[3]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(Color::Red)),
            "a removed line is all red"
        );
    }
}
