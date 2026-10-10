//! Draws the prompt: the mode's bar down its left, its label and the model
//! it runs with on the top row, and three rows of text under it, scrolled to
//! the cursor.

use nth_protocol::{Effort, Llm, Mode};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::Prompt;
use crate::theme::{
    BAR, BAR_WIDTH, PROVIDER_COLOUR, SHELL_COLOUR, SUBAGENT_COLOUR, dim, mode_colour, panel_row,
    panel_title,
};

const PLACEHOLDER: &str = "Ask anything.";
const SHELL_PLACEHOLDER: &str = "Run a command.";
/// The label while the prompt holds a command, in place of the mode's.
const SHELL_LABEL: &str = "cmd";
/// Right-aligned on the label row while a turn runs.
const CANCEL_HINT: &str = "esc to cancel";
/// The label row on top, then the text.
pub const ROWS: u16 = 1 + TEXT_ROWS;
const TEXT_ROWS: u16 = 3;
/// The mode field's width: `plan` and the spinner fill it, `act` is padded
/// to it, so the model after the mode never shifts when either changes.
const MODE_WIDTH: usize = 4;

/// `spinner` replaces the mode's label while a turn runs, and the label
/// row says how to cancel. The text stays as it is: Enter queues it. A
/// command to run shows as `cmd` in yellow instead of the mode, and on a
/// subagent's tab the label names the subagent, `target`, in cyan. After
/// the mode, a model with its provider and effort: ` · opencode/glm-5.3`.
pub fn draw(
    frame: &mut Frame,
    area: Rect,
    prompt: &Prompt,
    mode: Mode,
    spinner: Option<&str>,
    target: Option<&str>,
    llm: &Llm,
) {
    let (label, colour, placeholder) = match (prompt.shell(), target) {
        (true, _) => (SHELL_LABEL, SHELL_COLOUR, SHELL_PLACEHOLDER),
        (false, Some(agent)) => (agent, SUBAGENT_COLOUR, PLACEHOLDER),
        (false, None) => (mode.label(), mode_colour(mode), PLACEHOLDER),
    };
    // A command and a subagent run without this session's model.
    let running_with = !prompt.shell() && target.is_none();
    let label = spinner.unwrap_or(label);
    let wrapped = prompt.wrap(room(area));
    let top = scroll(wrapped.cursor_row);
    let text: Vec<Span> = if prompt.is_empty() {
        vec![Span::styled(placeholder, dim())]
    } else {
        wrapped.rows[top..]
            .iter()
            .take(usize::from(TEXT_ROWS))
            .map(|row| Span::raw(row.as_str()))
            .collect()
    };

    let mut lines = vec![match running_with {
        true => label_row(label, colour, llm),
        false => panel_title(label, colour),
    }];
    lines.extend((0..usize::from(TEXT_ROWS)).map(|i| panel_row(colour, text.get(i).cloned())));
    frame.render_widget(Paragraph::new(lines), area);
    if spinner.is_some() {
        let label_row = Rect { height: 1, ..area };
        frame.render_widget(
            Paragraph::new(Line::styled(CANCEL_HINT, dim()).right_aligned()),
            label_row,
        );
    }

    frame.set_cursor_position(position(area, prompt, prompt.cursor()));
}

/// The top row for plan and act: the bar and mode label in the mode's
/// colour, the label padded to [`MODE_WIDTH`], then the model.
fn label_row<'a>(label: &'a str, colour: Color, llm: &'a Llm) -> Line<'a> {
    let style = Style::new().fg(colour);
    let mut spans = vec![Span::styled(BAR, style), Span::styled(label, style)];
    if let Some(pad) = MODE_WIDTH.checked_sub(label.chars().count()) {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.extend(model_spans(&llm.model, llm.effort));
    Line::from(spans)
}

/// The model the next turn runs with, after the mode: ` · `, its
/// `provider/` prefix in bright white, the model id in blue, and the
/// effort after another ` · ` when it takes one.
fn model_spans<'a>(model: &'a str, effort: Effort) -> Vec<Span<'a>> {
    let mut spans = vec![Span::styled(" · ", dim())];
    match model
        .split_once('/')
        .filter(|(provider, _)| !provider.is_empty())
    {
        Some((provider, id)) => {
            spans.push(Span::styled(
                format!("{provider}/"),
                Style::new().fg(PROVIDER_COLOUR),
            ));
            spans.push(Span::styled(id, Style::new().fg(Color::Blue)));
        }
        None => spans.push(Span::styled(model, Style::new().fg(Color::Blue))),
    }
    if let Some(effort) = effort.wire() {
        spans.push(Span::styled(
            format!(" · {effort}"),
            Style::new().fg(Color::Gray),
        ));
    }
    spans
}

/// Where byte `at` falls on the cursor's row on screen; a position on an
/// earlier row clamps to the row's start.
pub fn position(area: Rect, prompt: &Prompt, at: usize) -> Position {
    let wrapped = prompt.wrap(room(area));
    let back = prompt.text()[at..prompt.cursor()].chars().count();
    let col = wrapped.cursor_col.saturating_sub(back);
    let row = wrapped.cursor_row - scroll(wrapped.cursor_row);
    Position::new(
        area.x + BAR_WIDTH + u16::try_from(col).unwrap_or(0),
        area.y + 1 + u16::try_from(row).unwrap_or(0),
    )
}

fn room(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(BAR_WIDTH))
}

/// The first text row shown: the top, until the cursor runs past the last
/// row, then whatever keeps the cursor on the bottom one.
fn scroll(cursor_row: usize) -> usize {
    cursor_row.saturating_sub(usize::from(TEXT_ROWS) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn llm(model: &str, effort: Effort) -> Llm {
        Llm {
            model: model.into(),
            effort,
        }
    }

    #[test]
    fn the_model_follows_the_provider_in_the_modes_colour() {
        let spans = model_spans("opencode/glm-5.3", Effort::High);

        assert_eq!(text(&spans), " · opencode/glm-5.3 · high");
        assert_eq!(spans[1].content, "opencode/");
        assert_eq!(spans[1].style.fg, Some(PROVIDER_COLOUR));
        assert_eq!(spans[2].style.fg, Some(Color::Blue));
        assert_eq!(spans[3].style.fg, Some(Color::Gray));
    }

    #[test]
    fn a_model_without_a_provider_shows_whole() {
        let spans = model_spans("glm", Effort::Default);

        assert_eq!(text(&spans), " · glm");
        assert_eq!(spans[1].style.fg, Some(Color::Blue));
    }

    #[test]
    fn only_the_first_slash_splits_the_provider() {
        let spans = model_spans("openrouter/z-ai/glm-5.2", Effort::Default);

        assert_eq!(spans[1].content, "openrouter/");
        assert_eq!(spans[2].content, "z-ai/glm-5.2");
        assert_eq!(text(&spans), " · openrouter/z-ai/glm-5.2");
    }

    #[test]
    fn the_mode_field_keeps_the_model_in_place() {
        let model = llm("opencode/glm-5.3", Effort::Default);
        let at = |label: &str| {
            let line = label_row(label, Color::Blue, &model);
            let row = text(&line.spans);
            let byte = row.find(" · ").expect("separator");
            row[..byte].chars().count()
        };

        assert_eq!(at("act"), at("plan"), "act is padded to plan's width");
        assert_eq!(at("plan"), at("⠖⠉⠉⠑"), "the spinner is 4 wide too");
        let plain = llm("glm", Effort::Default);
        let act = label_row("act", Color::Blue, &plain);
        assert!(
            text(&act.spans).starts_with("▎ act  · "),
            "{:?}",
            text(&act.spans)
        );
    }
}
