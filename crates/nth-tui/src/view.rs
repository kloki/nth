//! Draws the fixed layout: chat history, one status row, a three-row prompt.
//! Row heights never depend on content, so nothing shifts while a turn runs.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::App, transcript::Activity};

pub const PROMPT_ROWS: u16 = 3;
const BAR: &str = "▎ ";
const PLACEHOLDER: &str = "Ask anything.";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area().inner(Margin::new(1, 0));
    let [chat, status, prompt] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(PROMPT_ROWS),
    ])
    .areas(area);

    draw_chat(frame, chat, app);
    draw_status(frame, status, app);
    draw_prompt(frame, prompt, app);
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn draw_chat(frame: &mut Frame, area: Rect, app: &mut App) {
    let total = app.transcript.layout(area.width);
    let height = usize::from(area.height);
    app.chat_height = height;
    app.max_top = total.saturating_sub(height);

    if app.transcript.is_empty() {
        let middle = Rect {
            y: area.y + area.height / 2,
            height: area.height.min(1),
            ..area
        };
        let line = format!("nth · {} · {}", app.model, app.place);
        frame.render_widget(
            Paragraph::new(line)
                .style(dim())
                .alignment(Alignment::Center),
            middle,
        );
        return;
    }

    let top = app.scroll.top(app.max_top);
    frame.render_widget(Paragraph::new(app.transcript.visible(top, height)), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    if app.busy_since.is_some() {
        let mut spans = Vec::new();
        match app.transcript.activity() {
            Activity::Thinking => spans.push(Span::styled("thinking", dim())),
            Activity::Writing => spans.push(Span::styled("writing", dim())),
            Activity::Tool(call) => spans.extend([
                Span::styled(call.name.clone(), Style::new().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(call.summary(&app.cwd), dim()),
            ]),
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    let below = app.max_top - app.scroll.top(app.max_top);
    let right = if !app.scroll.is_following() && below > 0 {
        Span::styled(
            format!("↓ {below} more · ctrl+End"),
            Style::new().fg(Color::Yellow),
        )
    } else if app.busy_since.is_none() {
        Span::styled(format!("{} · {}", app.model, app.place), dim())
    } else {
        Span::styled("esc to interrupt", dim())
    };
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

fn draw_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let room = usize::from(area.width.saturating_sub(2));
    let wrapped = app.prompt.wrap(room);
    let rows = usize::from(PROMPT_ROWS);
    // Scroll inside the box just enough to keep the cursor row visible.
    let first = wrapped.cursor_row.saturating_sub(rows - 1);

    let bar = if app.prompt.is_empty() || app.is_busy() {
        dim()
    } else {
        Style::new().fg(Color::Blue)
    };
    let lines: Vec<Line> = (first..first + rows)
        .map(|i| {
            let text = match wrapped.rows.get(i) {
                _ if app.prompt.is_empty() && i == 0 => Span::styled(PLACEHOLDER, dim()),
                Some(row) => Span::raw(row.as_str()),
                None => Span::raw(""),
            };
            Line::from(vec![Span::styled(BAR, bar), text])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);

    let row = u16::try_from(wrapped.cursor_row - first).unwrap_or(0);
    let col = u16::try_from(wrapped.cursor_col).unwrap_or(0);
    frame.set_cursor_position(Position::new(area.x + 2 + col, area.y + row));
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};

    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, ModelInfo, Provider, Request, StreamEvent};
    use nth_session::Session;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    struct Idle;

    impl Provider for Idle {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    fn app() -> App {
        let session = Session::new("glm", "/repo".into());
        App::new(session, Arc::new(Idle), Arc::new(Vec::new()))
    }

    fn rows(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).expect("test backend");
        terminal.draw(|frame| draw(frame, app)).expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn status_and_prompt_rows_never_move() {
        let mut app = app();
        let idle = rows(&mut app);
        assert!(idle[8].trim_end().ends_with("glm · /repo"));
        assert!(idle[9].starts_with(" ▎ Ask anything."));

        for i in 0..20 {
            app.transcript.push_user(format!("message {i}"));
        }
        app.prompt.insert_str("one\ntwo\nthree\nfour");
        app.busy_since = Some(Instant::now());
        let busy = rows(&mut app);

        assert!(busy[8].contains("thinking"));
        assert!(busy[8].trim_end().ends_with("esc to interrupt"));
        assert_eq!(
            busy[9..].iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            [" ▎ two", " ▎ three", " ▎ four"]
        );
        assert_eq!(busy[7].trim_end(), " ▎ message 19");
    }
}
