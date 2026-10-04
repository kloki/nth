//! Draws the question panel in the prompt's place: a title or tab row with
//! its keys, then the question shown with its options and open field, or
//! on the Submit tab every answer.

use nth_protocol::{Answer, Question};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use super::{Pane, QuestionPanel};
use crate::{
    prompt::Prompt,
    theme::{BAR_WIDTH, dim, panel_row, panel_title, pick},
};

const TITLE: &str = "question";
const PLACEHOLDER: &str = "Type your own answer…";
/// The most rows a long question wraps to; it is cut after that.
const QUESTION_ROWS: usize = 3;
/// Between the options and the preview: the only divider line in nth, since
/// two sides of free text, one of them ASCII art, need keeping apart.
const RULE: &str = " │ ";
/// After a one-choice question's chosen label.
const CHOSEN: &str = " ✓";

/// The model talks to you here, so the panel has the model answer's colour.
const ACCENT: Color = Color::Cyan;

/// Set once from the tallest question or preview, so moving between them
/// never makes the layout jump, and kept to half of `screen`; the options
/// scroll past that and previews are cut.
pub fn rows(panel: &QuestionPanel, screen: Rect) -> u16 {
    let width = usize::from(screen.width.saturating_sub(BAR_WIDTH));
    let tallest = panel
        .questions()
        .iter()
        .map(|q| {
            let side = split(q, width).map_or(0, |(_, side_width)| {
                (0..q.options.len())
                    .map(|highlight| {
                        let pane = Pane {
                            highlight,
                            ..Pane::default()
                        };
                        side(q, &pane, side_width).len()
                    })
                    .max()
                    .unwrap_or(0)
            });
            question_rows(q, width).len() + side.max(q.options.len() + 1)
        })
        .chain([panel.questions().len()])
        .max()
        .unwrap_or(0);
    let rows = u16::try_from(1 + tallest).unwrap_or(u16::MAX);
    rows.min((screen.height / 2).max(3))
}

pub fn draw(frame: &mut Frame, area: Rect, panel: &QuestionPanel) {
    let header = Rect { height: 1, ..area };
    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    let title = title(panel);
    let used: usize = title.spans.iter().map(|s| s.content.width()).sum();
    frame.render_widget(Paragraph::new(title), header);
    // Dropped rather than drawn over the title when the row is too narrow.
    let keys = keys(panel);
    if usize::from(header.width) > used + keys.width() + 2 {
        frame.render_widget(
            Paragraph::new(Line::styled(keys, dim()).right_aligned()),
            header,
        );
    }

    if panel.on_submit() {
        frame.render_widget(Paragraph::new(review(panel)), body);
        return;
    }
    let question = &panel.questions()[panel.tab];
    let pane = &panel.panes[panel.tab];
    let width = usize::from(area.width.saturating_sub(BAR_WIDTH));
    let asked: Vec<Line> = question_rows(question, width)
        .into_iter()
        .map(|row| panel_row(ACCENT, [Span::raw(row)]))
        .collect();
    let split = split(question, width);
    let (choices, cursor) = choices(question, pane, width, split);

    // The options scroll to keep the highlighted one in view when the panel
    // is short; the side stays put and is cut at the bottom.
    let room = usize::from(body.height).saturating_sub(asked.len());
    let top = (pane.highlight + 1).saturating_sub(room);
    let left = choices.into_iter().skip(top);
    let mut lines = asked;
    let first_choice = lines.len();
    match split {
        None => lines.extend(left.map(|spans| panel_row(ACCENT, spans))),
        Some((left_width, side_width)) => {
            let mut left: Vec<_> = left.collect();
            let side = side(question, pane, side_width);
            let height = left.len().max(side.len()).min(room);
            left.resize(height, Vec::new());
            for (i, mut spans) in left.into_iter().enumerate() {
                let used: usize = spans.iter().map(|s| s.content.width()).sum();
                spans.push(Span::raw(" ".repeat(left_width.saturating_sub(used))));
                spans.push(Span::styled(RULE, dim()));
                spans.extend(side.get(i).cloned());
                lines.push(panel_row(ACCENT, spans));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), body);

    if let Some(col) = cursor
        && let Ok(row) = u16::try_from(first_choice + pane.highlight - top)
    {
        let col = u16::try_from(col).unwrap_or(0);
        frame.set_cursor_position(Position::new(body.x + BAR_WIDTH + col, body.y + row));
    }
}

/// When any option has a preview, the widths of the options' side and of
/// the preview's side, which the rule between them takes the rest of.
fn split(question: &Question, width: usize) -> Option<(usize, usize)> {
    if question.options.iter().all(|o| o.preview.is_none()) {
        return None;
    }
    let lead = lead(false, question.multiple.then_some(false), 0)
        .iter()
        .map(|s| s.content.width())
        .sum::<usize>();
    let label = question
        .options
        .iter()
        .map(|o| o.label.width())
        .max()
        .unwrap_or(0);
    let left = (lead + label + CHOSEN.width()).min(width * 2 / 5);
    Some((left, width.saturating_sub(left + RULE.width())))
}

/// One row per option and the open field last, without the bar; and when
/// the open field is highlighted, the cursor's column in it. Split, the
/// rows keep to the left side and the descriptions move to the right.
fn choices<'a>(
    question: &'a Question,
    pane: &'a Pane,
    width: usize,
    split: Option<(usize, usize)>,
) -> (Vec<Vec<Span<'a>>>, Option<usize>) {
    let width = split.map_or(width, |(left, _)| left);
    let label_width = question
        .options
        .iter()
        .map(|o| o.label.width())
        .max()
        .unwrap_or(0);
    let mut rows = Vec::new();
    for (i, option) in question.options.iter().enumerate() {
        let here = i == pane.highlight;
        let mut spans = lead(here, question.multiple.then_some(pane.checked[i]), i);
        let label_style = if here {
            pick()
        } else {
            Style::new().fg(Color::Blue)
        };
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let label = cut(&option.label, width.saturating_sub(used + CHOSEN.width()));
        let gap = label_width.saturating_sub(label.width());
        spans.push(Span::styled(label, label_style));
        let chosen = !question.multiple && pane.checked[i];
        spans.push(Span::styled(
            if chosen { CHOSEN } else { "  " },
            Style::new().fg(Color::Green),
        ));
        if let (None, Some(description)) = (split, &option.description) {
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            let room = width.saturating_sub(used + gap + 1);
            spans.push(Span::raw(" ".repeat(gap + 1)));
            spans.push(Span::styled(cut(description, room), dim()));
        }
        rows.push(spans);
    }

    let open = question.options.len();
    let here = pane.highlight == open;
    let ticked = !pane.open.text().trim().is_empty();
    let mut spans = lead(here, question.multiple.then_some(ticked), open);
    let prefix: usize = spans.iter().map(|s| s.content.width()).sum();
    let room = width.saturating_sub(prefix + 1);
    let (shown, cursor) = open_text(&pane.open, room);
    if pane.open.is_empty() {
        spans.push(Span::styled(cut(PLACEHOLDER, room), dim()));
    } else {
        spans.push(Span::raw(shown));
    }
    rows.push(spans);
    (rows, here.then_some(prefix + cursor))
}

/// The highlighted option's preview as written, cut rather than wrapped so
/// a mockup keeps its shape, then its description, dim and wrapped.
fn side<'a>(question: &'a Question, pane: &Pane, width: usize) -> Vec<Span<'a>> {
    let Some(option) = question.options.get(pane.highlight) else {
        return Vec::new();
    };
    let mut rows: Vec<Span> = option
        .preview
        .iter()
        .flat_map(|p| p.trim_end().lines())
        .map(|line| Span::raw(cut(line, width)))
        .collect();
    if let Some(description) = &option.description {
        rows.extend(
            wrap(description, width)
                .into_iter()
                .map(|row| Span::styled(row, dim())),
        );
    }
    rows
}

/// `question` alone, or with several questions a tab per question's header
/// and a Submit tab, the one shown highlighted.
fn title(panel: &QuestionPanel) -> Line<'_> {
    if !panel.has_tabs() {
        return panel_title(TITLE, ACCENT);
    }
    let style = |here: bool| if here { pick() } else { Style::new() };
    let mut spans = Vec::new();
    for (i, question) in panel.questions().iter().enumerate() {
        let mark = if panel.answer(i).is_some() {
            "☒"
        } else {
            "☐"
        };
        spans.push(Span::styled(
            format!("{mark} {}", question.header),
            style(i == panel.tab),
        ));
        spans.push(Span::raw("   "));
    }
    spans.push(Span::styled("✓ Submit", style(panel.on_submit())));
    panel_row(ACCENT, spans)
}

fn keys(panel: &QuestionPanel) -> String {
    if panel.on_submit() {
        return "enter send · esc".into();
    }
    let tabs = if panel.has_tabs() {
        "tab question · "
    } else {
        ""
    };
    let question = &panel.questions()[panel.tab];
    let pick = if question.multiple {
        "space toggle".to_string()
    } else {
        format!("1-{}", question.options.len())
    };
    format!("{tabs}↑↓ · {pick} · enter · esc")
}

/// One row per question: its header, then the answer, or "unanswered" in
/// yellow.
fn review(panel: &QuestionPanel) -> Vec<Line<'_>> {
    let width = panel
        .questions()
        .iter()
        .map(|q| q.header.width())
        .max()
        .unwrap_or(0);
    panel
        .questions()
        .iter()
        .enumerate()
        .map(|(i, question)| {
            let answer = match panel.answer(i) {
                Some(answer) => Span::raw(summary(&answer)),
                None => Span::styled("unanswered", Style::new().fg(Color::Yellow)),
            };
            let header = format!("{:<width$}  ", question.header);
            panel_row(ACCENT, [Span::styled(header, dim()), answer])
        })
        .collect()
}

/// The picked labels, then what was typed, in quotes.
fn summary(answer: &Answer) -> String {
    let typed = answer.typed.iter().map(|t| format!("\"{t}\""));
    let parts: Vec<String> = answer.picked.iter().cloned().chain(typed).collect();
    parts.join(", ")
}

/// The arrow on the highlighted row, a box when any number may be picked,
/// and the row's number.
fn lead(here: bool, ticked: Option<bool>, i: usize) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(if here { "→ " } else { "  " }, pick())];
    if let Some(ticked) = ticked {
        spans.push(Span::raw(if ticked { "[x] " } else { "[ ] " }));
    }
    spans.push(Span::styled(format!("{}. ", i + 1), dim()));
    spans
}

fn question_rows(question: &Question, width: usize) -> Vec<String> {
    let mut rows = wrap(&question.question, width);
    rows.truncate(QUESTION_ROWS);
    rows
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut prompt = Prompt::default();
    prompt.set(text.trim());
    prompt.wrap(width).rows
}

/// What fits of the open field in `room` cells, scrolled to keep the
/// cursor in view, and the cursor's column in it.
fn open_text(open: &Prompt, room: usize) -> (String, usize) {
    let wrapped = open.wrap(usize::MAX);
    let line = wrapped.rows.concat();
    let skip = wrapped.cursor_col.saturating_sub(room);
    let shown: String = line.chars().skip(skip).take(room.max(1)).collect();
    (shown, wrapped.cursor_col - skip)
}

/// `text` cut to `room` cells, with `…` where it was cut.
fn cut(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + c.to_string().width() + 1 > room {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use nth_protocol::QuestionOption;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::question::tests::{panel, question};

    fn drawn(p: &QuestionPanel, width: u16) -> Vec<String> {
        let screen = Rect::new(0, 0, width, 40);
        let height = rows(p, screen);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, frame.area(), p))
            .expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn one_question_lists_its_options_then_the_open_field() {
        let mut auth = question("auth", false, &["OAuth", "API key"]);
        auth.options[0].description = Some("works with SSO".into());
        let (p, _) = panel(vec![auth]);

        assert_eq!(
            drawn(&p, 60),
            [
                "▎ question                            ↑↓ · 1-2 · enter · esc",
                "▎ Which auth?",
                "▎ → 1. OAuth     works with SSO",
                "▎   2. API key",
                "▎   3. Type your own answer…",
            ]
        );
    }

    #[test]
    fn any_number_shows_boxes() {
        let (mut p, _) = panel(vec![question("checks", true, &["fmt", "clippy"])]);
        p.insert(' ');
        p.insert('3');
        p.insert('x');

        assert_eq!(
            drawn(&p, 60)[2..],
            ["▎   [x] 1. fmt", "▎   [ ] 2. clippy", "▎ → [x] 3. x",]
        );
    }

    #[test]
    fn several_questions_get_tabs_and_a_review() {
        let (mut p, _) = panel(vec![
            question("auth", false, &["oauth", "key"]),
            question("checks", true, &["fmt", "clippy"]),
        ]);
        p.enter();

        let rows = drawn(&p, 80);
        assert!(
            rows[0].starts_with("▎ ☒ auth   ☐ checks   ✓ Submit"),
            "{rows:?}"
        );
        assert_eq!(rows.len(), 5, "the tallest question");

        p.next_tab();
        assert_eq!(
            drawn(&p, 80)[1..3],
            ["▎ auth    oauth", "▎ checks  unanswered"]
        );
    }

    #[test]
    fn long_descriptions_are_cut() {
        let mut q = question("auth", false, &["a", "b"]);
        q.options[0] = QuestionOption {
            label: "a".into(),
            description: Some("a description far too long for the row".into()),
            preview: None,
        };
        let (p, _) = panel(vec![q]);

        assert_eq!(drawn(&p, 24)[2], "▎ → 1. a   a descriptio…");
    }

    fn with_previews() -> Question {
        let mut q = question("layout", false, &["Two lines", "One line"]);
        q.options[0].preview =
            Some("┌──────────────┐\n│ glm · ~/nth  │\n│ git · main   │\n└──────────────┘".into());
        q.options[0].description = Some("Room for git".into());
        q.options[1].preview = Some("glm · ~/nth · git · main".into());
        q
    }

    #[test]
    fn previews_show_beside_the_options_for_the_highlighted_one() {
        let (mut p, _) = panel(vec![with_previews()]);

        assert_eq!(
            drawn(&p, 60)[1..],
            [
                "▎ Which layout?",
                "▎ → 1. Two lines   │ ┌──────────────┐",
                "▎   2. One line    │ │ glm · ~/nth  │",
                "▎   3. Type your…  │ │ git · main   │",
                "▎                  │ └──────────────┘",
                "▎                  │ Room for git",
            ]
        );

        p.next();
        assert_eq!(
            drawn(&p, 60)[2..5],
            [
                "▎   1. Two lines   │ glm · ~/nth · git · main",
                "▎ → 2. One line    │",
                "▎   3. Type your…  │",
            ],
            "the side follows the highlight, the height stays"
        );
    }

    #[test]
    fn the_panel_never_takes_more_than_half_the_screen() {
        let (p, _) = panel(vec![question("auth", false, &["a", "b", "c", "d"])]);

        assert_eq!(rows(&p, Rect::new(0, 0, 60, 40)), 7);
        assert_eq!(rows(&p, Rect::new(0, 0, 60, 10)), 5);
    }
}
