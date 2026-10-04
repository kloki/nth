//! The header above the content panel: one line with the content panel's
//! tabs on the left, the one showing highlighted, and the app's name and
//! version on the right.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};

use crate::{app::Content, status, theme};

/// Always this tall.
pub const ROWS: u16 = 1;

pub fn draw(frame: &mut Frame, area: Rect, content: &Content) {
    let mut tabs = Vec::new();
    for (i, &tab) in content.tabs().iter().enumerate() {
        if i > 0 {
            tabs.push(Span::raw("  "));
        }
        let style = if tab == content.active() {
            theme::pick()
        } else {
            theme::dim()
        };
        tabs.push(Span::styled(format!("{} {}", i + 1, tab.name()), style));
    }
    let name = vec![
        Span::styled(
            "nth",
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(concat!(" ", env!("CARGO_PKG_VERSION")), theme::dim()),
    ];
    status::split_line(frame, area, tabs, name);
}
