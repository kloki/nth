//! Draws the connect panel in the prompt's place: a header with its keys,
//! then a row per provider with whether it is connected; or, while a login
//! runs, the provider, its URL and what it waits on.

use nth_icons::icons;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{ConnectPanel, Connectable, Waiting};
use crate::theme::{BAR, BAR_WIDTH, dim, panel_row, panel_title, pick};

const TITLE: &str = "connect";
const PICK_KEYS: &str = "↑↓ provider · enter sign in · esc";
const WAITING_KEYS: &str = "c copy url · esc cancel";

/// The panel's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;

/// Draws `panel` into `area`. Where the URL ended up, for a click to open
/// it; `None` while there is none.
pub fn draw(frame: &mut Frame, area: Rect, panel: &ConnectPanel) -> Option<Rect> {
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(panel_title(TITLE, ACCENT)), header);
    let keys = match panel.waiting() {
        Some(_) => WAITING_KEYS,
        None => PICK_KEYS,
    };
    // Dropped rather than drawn over the title when the row is too narrow.
    let room = usize::from(header.width);
    if room > BAR.chars().count() + TITLE.len() + keys.chars().count() + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(keys, dim())).alignment(Alignment::Right),
            header,
        );
    }
    match panel.waiting() {
        Some(waiting) => draw_waiting(frame, body, waiting),
        None => {
            draw_pick(frame, body, panel);
            None
        }
    }
}

fn draw_pick(frame: &mut Frame, area: Rect, panel: &ConnectPanel) {
    let width = Connectable::ALL
        .map(|c| c.name().len())
        .into_iter()
        .max()
        .unwrap_or(0);
    let lines: Vec<_> = Connectable::ALL
        .into_iter()
        .map(|to| {
            let (arrow, name) = if to == panel.selected() {
                (format!("{} ", icons().pick), pick())
            } else {
                ("  ".to_string(), Style::new())
            };
            let mark = if panel.is_connected(to) {
                icons().ok
            } else {
                " "
            };
            panel_row(
                ACCENT,
                [
                    Span::styled(arrow, pick()),
                    Span::styled(format!("{:width$} ", to.name()), name),
                    Span::styled(mark, Style::new().fg(Color::Green)),
                    Span::styled(format!("  {}", to.about()), dim()),
                ],
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// The URL is cut to one row; `c` copies it whole and a click opens it.
fn draw_waiting(frame: &mut Frame, area: Rect, waiting: &Waiting) -> Option<Rect> {
    let room = usize::from(area.width.saturating_sub(BAR_WIDTH));
    let url = waiting.url.as_deref().map(|url| cut(url, room));
    let link = match &url {
        Some(url) => Span::styled(
            url.clone(),
            Style::new()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED),
        ),
        None => Span::styled("starting the login…", dim()),
    };
    let lines: Vec<Line> = vec![
        panel_row(
            ACCENT,
            [Span::styled(
                waiting.to.name(),
                Style::new().add_modifier(Modifier::BOLD),
            )],
        ),
        panel_row(ACCENT, [link]),
        panel_row(
            ACCENT,
            [Span::styled(
                "Waiting for authorization in your browser…",
                dim(),
            )],
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
    let url = url?;
    let shown = Rect {
        x: area.x + BAR_WIDTH,
        y: area.y + 1,
        width: u16::try_from(url.chars().count()).unwrap_or(u16::MAX),
        height: 1,
    };
    Some(shown.intersection(area))
}

/// `url` in at most `room` columns, ending in `…` when cut. URLs are ASCII,
/// a column a character.
fn cut(url: &str, room: usize) -> String {
    if url.chars().count() <= room {
        return url.to_string();
    }
    let kept: String = url.chars().take(room.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn drawn(panel: &ConnectPanel, width: u16) -> (Vec<String>, Option<Rect>) {
        let height = panel.rows();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        let mut link = None;
        terminal
            .draw(|frame| link = draw(frame, frame.area(), panel))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (rows, link)
    }

    #[test]
    fn marks_a_connected_provider() {
        assert_eq!(
            drawn(&ConnectPanel::new([false]), 60).0,
            [
                "▎ connect                  ↑↓ provider · enter sign in · esc",
                "▎ → ChatGPT    a Plus or Pro subscription",
            ]
        );
        assert_eq!(
            drawn(&ConnectPanel::new([true]), 60).0[1],
            "▎ → ChatGPT ✓  a Plus or Pro subscription"
        );
    }

    #[test]
    fn waits_with_the_url_cut_to_one_row() {
        let mut panel = ConnectPanel::new([false]);
        panel.start();
        let (rows, link) = drawn(&panel, 40);
        assert_eq!(rows[2], "▎ starting the login…");
        assert_eq!(link, None);

        panel.set_url("https://auth.openai.com/oauth/authorize?client_id=app".into());
        let (rows, link) = drawn(&panel, 40);
        assert_eq!(
            rows,
            [
                "▎ connect        c copy url · esc cancel",
                "▎ ChatGPT",
                "▎ https://auth.openai.com/oauth/authori…",
                "▎ Waiting for authorization in your brow",
            ]
        );
        assert_eq!(link, Some(Rect::new(2, 2, 38, 1)));
    }
}
