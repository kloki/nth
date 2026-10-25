//! The header above the content panel: one line with the content panel's
//! tabs on the left, each coloured by how it is doing and the ones showing
//! bracketed, and the app's name and version on the right. When the panel
//! splits, the chat's tab sits over the left pane and the others start
//! over the right one, with the focused tab in bold.

use std::ops::Range;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::{app::TabState, status, theme};

/// Always this tall.
pub const ROWS: u16 = 1;

/// One tab as the header shows it.
pub struct TabLabel {
    pub icon: &'static str,
    pub name: String,
    pub state: TabState,
    /// A pane shows it.
    pub shown: bool,
    /// It has the focus, which only needs saying when two tabs show.
    pub focused: bool,
}

impl TabLabel {
    /// `icon name`, inside `[ ]` when showing and inside spaces otherwise,
    /// so moving between tabs never shifts the strip.
    fn text(&self) -> String {
        let (open, close) = if self.shown { ("[", "]") } else { (" ", " ") };
        format!("{open}{} {}{close}", self.icon, self.name)
    }

    fn span(&self) -> Span<'static> {
        let mut style = match theme::tab_colour(self.state) {
            Some(colour) => Style::new().fg(colour),
            None => Style::new(),
        };
        if self.focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        Span::styled(self.text(), style)
    }
}

/// Where each run of tabs is drawn: all of them across `area`, or, with
/// the right pane starting at column `split`, the chat over the left pane
/// and the rest over the right.
fn runs(tabs: &[TabLabel], area: Rect, split: Option<u16>) -> Vec<(Rect, Range<usize>)> {
    match split {
        Some(x) if x > area.x && x < area.right() => {
            let left = Rect {
                width: x - area.x,
                ..area
            };
            let right = Rect {
                x,
                width: area.right() - x,
                ..area
            };
            vec![
                (left, 0..tabs.len().min(1)),
                (right, tabs.len().min(1)..tabs.len()),
            ]
        }
        _ => vec![(area, 0..tabs.len())],
    }
}

/// Each tab in its state's colour, one against the next; `split` is the
/// column the right pane starts at, if the panel is split.
pub fn draw(frame: &mut Frame, area: Rect, tabs: &[TabLabel], split: Option<u16>) {
    let name = vec![
        Span::styled(
            "nth",
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(concat!(" ", env!("CARGO_PKG_VERSION")), theme::dim()),
    ];
    let mut runs = runs(tabs, area, split);
    // The name goes after the last run of tabs.
    let (last, range) = runs.pop().expect("one run at least");
    for (run, range) in runs {
        let spans: Vec<Span> = tabs[range].iter().map(TabLabel::span).collect();
        frame.render_widget(Paragraph::new(Line::from(spans)), run);
    }
    let spans = tabs[range].iter().map(TabLabel::span).collect();
    status::split_line(frame, last, spans, name);
}

/// Where each of `tabs`, drawn as [`draw`] does, sits on screen, in
/// order; a tab cut off at the end of its run is cut short here too, and
/// one past it has no width.
pub fn tab_areas(tabs: &[TabLabel], area: Rect, split: Option<u16>) -> Vec<Rect> {
    let mut areas = Vec::with_capacity(tabs.len());
    for (run, range) in runs(tabs, area, split) {
        let mut x = run.x;
        for tab in &tabs[range] {
            let width = u16::try_from(tab.text().width()).unwrap_or(u16::MAX);
            let width = width.min(run.right().saturating_sub(x));
            areas.push(Rect { x, width, ..run });
            x = x.saturating_add(width);
        }
    }
    areas
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Vec<TabLabel> {
        vec![
            TabLabel {
                icon: "›",
                name: "chat".into(),
                state: TabState::Idle,
                shown: true,
                focused: false,
            },
            TabLabel {
                icon: "≡",
                name: "diagnostics".into(),
                state: TabState::Idle,
                shown: false,
                focused: false,
            },
        ]
    }

    #[test]
    fn each_tab_has_the_columns_it_is_drawn_over() {
        let area = Rect::new(1, 0, 40, 1);
        // `[› chat]` is 8 columns from x = 1, then ` ≡ diagnostics `.
        assert_eq!(
            tab_areas(&labels(), area, None),
            [Rect::new(1, 0, 8, 1), Rect::new(9, 0, 15, 1)]
        );
    }

    #[test]
    fn split_the_other_tabs_start_over_the_right_pane() {
        let area = Rect::new(1, 0, 40, 1);
        // `[› chat]` from x = 1, then ` ≡ diagnostics ` from x = 21.
        assert_eq!(
            tab_areas(&labels(), area, Some(21)),
            [Rect::new(1, 0, 8, 1), Rect::new(21, 0, 15, 1)]
        );
    }

    #[test]
    fn a_tab_past_the_edge_is_cut_short() {
        let area = Rect::new(0, 0, 12, 1);
        assert_eq!(
            tab_areas(&labels(), area, None),
            [Rect::new(0, 0, 8, 1), Rect::new(8, 0, 4, 1)]
        );
    }
}
