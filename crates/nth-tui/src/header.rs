//! The header above the content panel: one line with the content panel's
//! tabs on the left, each coloured by how it is doing and the one showing
//! bracketed, and the app's name and version on the right.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
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
    /// It is the one showing.
    pub active: bool,
}

impl TabLabel {
    /// `icon name`, inside `[ ]` when showing and inside spaces otherwise,
    /// so moving between tabs never shifts the strip.
    fn text(&self) -> String {
        let (open, close) = if self.active { ("[", "]") } else { (" ", " ") };
        format!("{open}{} {}{close}", self.icon, self.name)
    }
}

/// Each tab in its state's colour, one against the next.
pub fn draw(frame: &mut Frame, area: Rect, tabs: &[TabLabel]) {
    let tabs = tabs
        .iter()
        .map(|tab| {
            let style = match theme::tab_colour(tab.state) {
                Some(colour) => Style::new().fg(colour),
                None => Style::new(),
            };
            Span::styled(tab.text(), style)
        })
        .collect();
    let name = vec![
        Span::styled(
            "nth",
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(concat!(" ", env!("CARGO_PKG_VERSION")), theme::dim()),
    ];
    status::split_line(frame, area, tabs, name);
}

/// Which of `tabs`, drawn from `area.x`, is under column `x`.
pub fn tab_at(tabs: &[TabLabel], area: Rect, x: u16) -> Option<usize> {
    let mut offset = usize::from(x.checked_sub(area.x)?);
    for (index, tab) in tabs.iter().enumerate() {
        let width = tab.text().width();
        if offset < width {
            return Some(index);
        }
        offset -= width;
    }
    None
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
                active: true,
            },
            TabLabel {
                icon: "≡",
                name: "diagnostics".into(),
                state: TabState::Idle,
                active: false,
            },
        ]
    }

    #[test]
    fn a_click_lands_on_the_tab_under_it() {
        let area = Rect::new(1, 0, 40, 1);
        // `[› chat]` is 8 columns from x = 1, then ` ≡ diagnostics `.
        assert_eq!(tab_at(&labels(), area, 1), Some(0));
        assert_eq!(tab_at(&labels(), area, 8), Some(0));
        assert_eq!(tab_at(&labels(), area, 9), Some(1));
        assert_eq!(tab_at(&labels(), area, 23), Some(1));
        assert_eq!(tab_at(&labels(), area, 24), None);
        assert_eq!(tab_at(&labels(), area, 0), None);
    }
}
