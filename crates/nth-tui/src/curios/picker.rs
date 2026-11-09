//! The `/curios` picker: the curios the last poll listed, filtered as you
//! type, with those whose feedback is queued marked.

use std::collections::BTreeSet;

use nth_curios::Curio;
use ratatui::{
    Frame,
    layout::Rect,
    style::Color,
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    fuzzy::Filter,
    theme::{dim, panel_row, panel_title},
};

#[derive(Debug)]
pub(crate) struct Picker {
    items: Vec<Curio>,
    pending: Vec<bool>,
    filter: Filter,
    shown: Vec<usize>,
    selected: usize,
}

impl Picker {
    /// Lists `items`, newest first as the poll sorted them, marking those
    /// `queued` names.
    pub(super) fn new(items: Vec<Curio>, queued: &BTreeSet<(String, String)>) -> Self {
        let mut picker = Self {
            shown: (0..items.len()).collect(),
            items,
            pending: Vec::new(),
            filter: Filter::default(),
            selected: 0,
        };
        picker.mark(queued);
        picker
    }

    fn refilter(&mut self) {
        let names: Vec<_> = self
            .items
            .iter()
            .map(|c| format!("{} {}", c.title, c.session_id))
            .collect();
        self.shown = self.filter.rank(names.iter().map(String::as_str));
        self.selected = 0;
    }

    /// Marks the curios `queued` names, from the latest poll.
    pub(crate) fn mark(&mut self, queued: &BTreeSet<(String, String)>) {
        self.pending = self
            .items
            .iter()
            .map(|c| queued.contains(&(c.session_id.clone(), c.id.clone())))
            .collect();
    }

    pub(crate) fn select_next(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + 1) % self.shown.len();
        }
    }

    pub(crate) fn select_prev(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + self.shown.len() - 1) % self.shown.len();
        }
    }

    pub(crate) fn push(&mut self, c: char) {
        self.filter.push(c);
        self.refilter();
    }

    pub(crate) fn pop(&mut self) {
        self.filter.pop();
        self.refilter();
    }

    /// The curio the selection is on, if the filter left any.
    pub(crate) fn selected(&self) -> Option<&Curio> {
        self.shown
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> &[bool] {
        &self.pending
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect, owner: uuid::Uuid) {
        let mut lines = vec![
            panel_title("curios · enter open · ctrl+y copy link · esc", Color::Cyan),
            panel_row(
                Color::Cyan,
                [Span::styled(self.filter.query().to_string(), dim())],
            ),
        ];
        if self.items.is_empty() {
            lines.push(Line::from("  no published curios in this repository"));
        }
        let height = usize::from(area.height.saturating_sub(2));
        let first = self.selected.saturating_sub(height.saturating_sub(1));
        for (row, &i) in self.shown.iter().enumerate().skip(first).take(height) {
            let c = &self.items[i];
            let mark = if row == self.selected {
                nth_icons::icons().pick
            } else {
                " "
            };
            let own = if c.session_id == owner.to_string() {
                "this session"
            } else {
                &c.session_id
            };
            lines.push(panel_row(
                Color::Cyan,
                [Span::raw(format!(
                    "{mark} {} · r{} · {}{}",
                    c.title,
                    c.current_revision,
                    own,
                    if self.pending[i] { " · queued" } else { "" }
                ))],
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::curios::CuriosTab;

    fn curio(id: &str, title: &str, session: &str, revision: u64) -> Curio {
        Curio {
            id: id.into(),
            title: title.into(),
            session_id: session.into(),
            current_revision: revision,
            adopted_from: None,
            updated_at: None,
        }
    }

    fn rows(picker: &Picker, owner: uuid::Uuid) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        terminal
            .draw(|frame| picker.draw(frame, frame.area(), owner))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn the_picker_draws_from_the_tab() {
        let owner = uuid::Uuid::new_v4();
        let mut tab = CuriosTab::new("/nowhere".into());
        tab.items = vec![
            curio("plan", "Plan", &owner.to_string(), 3),
            curio("notes", "Notes", "other-session", 1),
        ];
        tab.queued = BTreeSet::from([("other-session".to_string(), "notes".to_string())]);
        let mut picker = tab.picker();
        let shown = rows(&picker, owner);
        assert!(shown[0].contains("curios · enter open"));
        let pick = nth_icons::icons().pick;
        assert!(shown[2].contains(&format!("{pick} Plan · r3 · this session")));
        assert!(!shown[2].contains("queued"));
        assert!(shown[3].contains("Notes · r1 · other-session · queued"));

        picker.push('n');
        picker.push('o');
        let shown = rows(&picker, owner);
        assert!(shown[1].contains("no"));
        assert!(shown[2].contains("Notes"), "{shown:?}");
        assert!(!shown.iter().any(|row| row.contains("Plan")));
        assert_eq!(picker.selected().map(|c| c.id.as_str()), Some("notes"));
    }

    #[test]
    fn an_empty_repository_says_so() {
        let picker = CuriosTab::new("/nowhere".into()).picker();
        let shown = rows(&picker, uuid::Uuid::new_v4());
        assert_eq!(shown[2].trim(), "no published curios in this repository");
        assert!(picker.selected().is_none());
    }

    #[test]
    fn the_selection_wraps_both_ways() {
        let mut tab = CuriosTab::new("/nowhere".into());
        tab.items = vec![curio("a", "A", "s", 1), curio("b", "B", "s", 1)];
        let mut picker = tab.picker();
        picker.select_prev();
        assert_eq!(picker.selected().map(|c| c.id.as_str()), Some("b"));
        picker.select_next();
        assert_eq!(picker.selected().map(|c| c.id.as_str()), Some("a"));
    }
}
