//! The plan tab: the plan file of the session, with what its latest change
//! did marked in colour: added lines green, removed ones red.

mod diff;

use diff::Kind;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::theme;

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
}

impl PlanView {
    pub fn exists(&self) -> bool {
        self.current.is_some()
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
    }

    /// A turn starts, so the next change to the plan starts a new diff.
    pub fn turn_started(&mut self) {
        self.changed_this_turn = false;
    }

    /// Lines added and removed since the baseline.
    pub fn counts(&self) -> (usize, usize) {
        diff::counts(&self.diff())
    }

    /// `plan`, then what changed while anything did: `plan +3 -1`.
    pub fn label(&self) -> String {
        match self.counts() {
            (0, 0) => "plan".into(),
            (added, removed) => format!("plan +{added} -{removed}"),
        }
    }

    fn diff(&self) -> Vec<diff::Line> {
        diff::lines(&self.baseline, self.current.as_deref().unwrap_or_default())
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

    /// `path` is the plan file as the header names it.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, path: &str) {
        let lines = self.lines(path, usize::from(area.width));
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = self.top.min(self.max_top);
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    /// A header naming the file and what changed, then the plan wrapped to
    /// `width`, each line behind a gutter that marks it added or removed.
    fn lines(&self, path: &str, width: usize) -> Vec<Line<'static>> {
        let mut header = vec![path.to_string()];
        let (added, removed) = self.counts();
        if added + removed > 0 {
            header.push(format!("+{added} -{removed}"));
        }
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
        for line in self.diff() {
            let (mark, style) = match line.kind {
                Kind::Same => ("  ", Style::new()),
                Kind::Added => ("+ ", Style::new().fg(Color::Green)),
                Kind::Removed => ("- ", Style::new().fg(Color::Red)),
            };
            let rows = textwrap::wrap(&line.text, room);
            if rows.is_empty() {
                lines.push(Line::styled(mark, style));
            }
            for (i, row) in rows.into_iter().enumerate() {
                let gutter = if i == 0 { mark } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(gutter, style),
                    Span::styled(row.into_owned(), style),
                ]));
            }
        }
        lines
    }
}

/// The `+ ` or `- ` in front of every line.
const GUTTER: usize = 2;

#[cfg(test)]
mod tests {
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
    fn lines_carry_a_gutter_and_wrap_under_it() {
        let mut view = PlanView::default();
        view.settle(Some("keep\ngone\n".into()));
        view.update(Some("keep\na long new line here\n".into()));

        let lines = view.lines(".nth/plans/1.md", 12);

        assert_eq!(
            text(&lines),
            [
                ".nth/plans/1.md · +1 -1 · /approve",
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
}
