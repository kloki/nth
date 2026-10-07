//! The header above the content panel: one line with the content panel's
//! tabs on the left, each coloured by how it is doing and the one showing
//! bracketed, and the app's name and version on the right.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};

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

/// Each tab is `icon name` in its state's colour, the showing one inside
/// `[ ]` and the others inside spaces, so moving between them never shifts
/// the strip.
pub fn draw(frame: &mut Frame, area: Rect, tabs: &[TabLabel]) {
    let tabs = tabs
        .iter()
        .map(|tab| {
            let (open, close) = if tab.active { ("[", "]") } else { (" ", " ") };
            let style = match theme::tab_colour(tab.state) {
                Some(colour) => Style::new().fg(colour),
                None => Style::new(),
            };
            Span::styled(format!("{open}{} {}{close}", tab.icon, tab.name), style)
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
