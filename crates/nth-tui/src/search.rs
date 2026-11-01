//! Fuzzy search over the rows a content view renders.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    app::Tab,
    fuzzy::{self, Filter},
    theme,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The row's line in the view as drawn, counted from 0.
    pub index: usize,
    pub text: String,
}

/// The nonblank rows among `lines`, whose first is line `first` of the view.
pub fn rows<'a>(lines: impl Iterator<Item = &'a Line<'static>>, first: usize) -> Vec<Row> {
    lines
        .enumerate()
        .filter_map(|(i, line)| {
            let text = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            (!text.trim().is_empty()).then_some(Row {
                index: i + first,
                text,
            })
        })
        .collect()
}

#[derive(Debug)]
pub struct Search {
    pub source: Tab,
    pub width: u16,
    pub revision: Option<u64>,
    rows: Vec<Row>,
    filter: Filter,
    shown: Vec<usize>,
    selected: usize,
}

impl Search {
    pub fn new(source: Tab, width: u16) -> Self {
        Self {
            source,
            width,
            revision: None,
            rows: Vec::new(),
            filter: Filter::default(),
            shown: Vec::new(),
            selected: 0,
        }
    }
    pub fn update(&mut self, rows: Vec<Row>, width: u16) {
        self.width = width;
        if self.rows == rows {
            return;
        }
        let selected = self.chosen().cloned();
        self.rows = rows;
        self.rank();
        if let Some(row) = selected {
            self.selected = self
                .shown
                .iter()
                .position(|&i| self.rows[i] == row)
                .unwrap_or(0);
        }
    }
    fn rank(&mut self) {
        self.shown = self.filter.rank(self.rows.iter().map(|r| r.text.as_str()));
        self.selected = 0;
    }
    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.filter.push(c);
        }
        self.rank();
    }
    pub fn insert(&mut self, c: char) {
        self.filter.push(c);
        self.rank();
    }
    pub fn backspace(&mut self) {
        self.filter.pop();
        self.rank();
    }
    pub fn clear(&mut self) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        self.filter.clear();
        self.rank();
        true
    }
    pub fn next(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + 1) % self.shown.len();
        }
    }
    pub fn prev(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + self.shown.len() - 1) % self.shown.len();
        }
    }
    pub fn chosen(&self) -> Option<&Row> {
        self.shown.get(self.selected).map(|&i| &self.rows[i])
    }
    pub fn query(&self) -> &str {
        self.filter.query()
    }
    /// The chosen result's place among the matches.
    pub fn selected(&self) -> usize {
        self.selected
    }
    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let [title, query, list] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);
        let name = if self.source == Tab::Chat {
            "search chat · ↑↓ enter esc"
        } else {
            "search plan · ↑↓ enter esc"
        };
        frame.render_widget(
            Paragraph::new(theme::panel_title(name, Color::Magenta)),
            title,
        );
        fuzzy::query_row(
            frame,
            query,
            Color::Magenta,
            self.filter.query(),
            self.shown.len(),
            self.rows.len(),
        );
        let height = usize::from(list.height);
        let first = self.selected.saturating_sub(height.saturating_sub(1));
        let lines: Vec<Line> = if self.shown.is_empty() {
            let empty = if self.rows.is_empty() {
                "nothing here to search"
            } else {
                "no matches"
            };
            vec![theme::panel_row(
                Color::Magenta,
                [Span::styled(format!("  {empty}"), theme::dim())],
            )]
        } else {
            self.shown
                .iter()
                .enumerate()
                .skip(first)
                .take(height)
                .map(|(n, &i)| {
                    let row = &self.rows[i];
                    let base = if n == self.selected {
                        theme::pick()
                    } else {
                        Style::new()
                    };
                    let indices = self.filter.indices(&row.text);
                    // A long row is cut around its first match, so the
                    // match is never the part the pane clips away.
                    let digits = (row.index + 1).to_string().len();
                    let room = usize::from(area.width)
                        .saturating_sub(2 + 2 + digits + 2)
                        .max(8);
                    let at = indices.first().map_or(0, |&i| i as usize);
                    let start = at.saturating_sub(room / 2);
                    let text: String = row.text.chars().skip(start).take(room).collect();
                    let mut spans = vec![
                        Span::styled(if n == self.selected { "→ " } else { "  " }, theme::pick()),
                        Span::styled(format!("{} ", row.index + 1), theme::dim()),
                    ];
                    if start > 0 {
                        spans.push(Span::styled("…", theme::dim()));
                    }
                    spans.extend(fuzzy::highlight(
                        &text,
                        &indices,
                        start,
                        base,
                        base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    ));
                    theme::panel_row(Color::Magenta, spans)
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), list);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(texts: &[&str]) -> Vec<Line<'static>> {
        texts
            .iter()
            .map(|text| Line::from(text.to_string()))
            .collect()
    }

    #[test]
    fn blank_rows_are_skipped_but_their_lines_still_count() {
        let lines = lines(&["one", "", "  ", "two"]);
        let rows = rows(lines.iter(), 3);
        assert_eq!(
            rows.iter()
                .map(|r| (r.index, r.text.as_str()))
                .collect::<Vec<_>>(),
            [(3, "one"), (6, "two")]
        );
    }

    #[test]
    fn a_query_ranks_rows_and_survives_a_source_update() {
        let mut search = Search::new(Tab::Chat, 20);
        search.update(rows(lines(&["alpha", "beta"]).iter(), 0), 20);
        search.insert_str("bt");
        assert_eq!(search.chosen().map(|r| r.text.as_str()), Some("beta"));

        search.update(rows(lines(&["alpha", "new", "beta"]).iter(), 0), 20);
        assert_eq!(
            search.chosen().map(|r| r.text.as_str()),
            Some("beta"),
            "the chosen row is kept when the source grows"
        );
    }

    #[test]
    fn unicode_queries_match_and_highlight() {
        let mut search = Search::new(Tab::Chat, 20);
        search.update(rows(lines(&["café noir", "tea"]).iter(), 0), 20);
        search.insert_str("fé");
        assert_eq!(search.chosen().map(|r| r.text.as_str()), Some("café noir"));
        assert_eq!(search.chosen().map(|r| r.index), Some(0));
    }
}
