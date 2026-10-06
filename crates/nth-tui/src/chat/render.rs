//! Wraps transcript entries into styled lines for the current width, and
//! caches them so only what changed is wrapped again.

use hoodrich::Change;
use nth_protocol::ToolCall;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::transcript::{Entry, OUTPUT_LINES, ToolState, Transcript};
use crate::{
    rich,
    theme::{BAR, BAR_WIDTH, INDENT, dim},
};

impl Transcript {
    /// Wraps whatever changed for `width` and returns the total line count.
    pub fn layout(&mut self, width: u16) -> usize {
        if width != self.width {
            self.width = width;
            self.items.iter_mut().for_each(|item| item.lines = None);
        }
        let mut previous: Option<&Entry> = None;
        let mut total = 0;
        for item in &mut self.items {
            if item.lines.is_none() || is_live(&item.entry) {
                let mut lines = Vec::new();
                // Every entry stands apart from the one before it.
                if previous.is_some() {
                    lines.push(Line::default());
                }
                lines.extend(render(&item.entry, &self.cwd, width));
                item.lines = Some(lines);
            }
            total += item.lines.as_ref().map_or(0, Vec::len);
            previous = Some(&item.entry);
        }
        total
    }

    /// The lines in `top..top + height`, as wrapped by the last `layout`.
    pub fn visible(&self, top: usize, height: usize) -> Vec<Line<'static>> {
        self.items
            .iter()
            .flat_map(|item| item.lines.iter().flatten())
            .skip(top)
            .take(height)
            .cloned()
            .collect()
    }
}

/// An entry whose line changes without an event, so it is redrawn each frame.
fn is_live(entry: &Entry) -> bool {
    matches!(entry, Entry::Reasoning { took: None, .. })
}

fn render(entry: &Entry, cwd: &std::path::Path, width: u16) -> Vec<Line<'static>> {
    let dim = dim();
    match entry {
        Entry::User(text) => barred_markdown(text, width, Style::new().fg(Color::Green)),
        Entry::PlanEdits(edits) => vec![Line::from(vec![
            Span::raw(INDENT),
            Span::styled("✎ ", Style::new().fg(Color::Magenta)),
            Span::styled(
                format!("plan edits · +{} -{}", edits.added, edits.removed),
                dim,
            ),
        ])],
        Entry::Notice(notice) => {
            let said = match (&notice.ended, notice.lines) {
                (Some(ended), _) => ended.clone(),
                (None, 1) => "1 line".to_string(),
                (None, n) => format!("{n} lines"),
            };
            vec![Line::from(vec![
                Span::raw(INDENT),
                Span::styled("» ", Style::new().fg(Color::Magenta)),
                Span::styled(
                    format!("monitor {} · {} · {said}", notice.id, notice.description),
                    dim,
                ),
            ])]
        }
        Entry::Answer(text) => barred_markdown(text, width, Style::new().fg(Color::Blue)),
        Entry::Retry { attempt, delay } => vec![Line::from(vec![
            Span::raw(INDENT),
            Span::styled("⟳ ", Style::new().fg(Color::Yellow)),
            Span::styled(nth_protocol::retry_label(*attempt, *delay), dim),
        ])],
        Entry::TurnError(e) => {
            let red = Style::new().fg(Color::Red);
            barred(&format!("✗ {e}"), width, red, red)
        }
        Entry::Reasoning {
            started,
            took,
            text,
        } => {
            let timing = match took {
                None => format!("thinking · {:.1}s", started.elapsed().as_secs_f64()),
                // A resumed session's reasoning; how long it took isn't saved.
                Some(took) if took.is_zero() => "thought".to_string(),
                Some(took) => format!("thought · {:.1}s", took.as_secs_f64()),
            };
            let mut lines = vec![Line::styled(format!("{INDENT}∴ {timing}"), dim)];
            // Plain text rather than markdown: reasoning is raw prose, often
            // with half-written markup.
            let body = dim.add_modifier(Modifier::ITALIC);
            lines.extend(prefixed(text, width, Span::raw(INDENT), body));
            lines
        }
        Entry::Tool {
            call,
            state,
            output,
            notes,
        } => {
            // A call is told apart by its tool's icon, not by a success
            // mark: only a failure stands out, in red.
            let name_style = match state {
                ToolState::Failed(_) => Style::new().fg(Color::Red),
                ToolState::Running | ToolState::Done => Style::new().fg(Color::Cyan),
            };
            let icon_style = match state {
                ToolState::Running => dim,
                ToolState::Done | ToolState::Failed(_) => name_style,
            };
            // One bar down the call and its output, so they read as one block.
            let bar = Style::new().fg(Color::Cyan);
            let mut spans = vec![
                Span::styled(BAR, bar),
                Span::styled(icon(&call.name), icon_style),
                Span::raw(" "),
                Span::styled(format!("{:<6} ", call.name), name_style),
                Span::styled(call.summary(cwd), dim),
            ];
            if let ToolState::Failed(e) = state {
                spans.push(Span::styled(format!("  {e}"), Style::new().fg(Color::Red)));
            }
            let mut lines = vec![Line::from(spans)];
            let room = usize::from(width.saturating_sub(BAR_WIDTH)).saturating_sub(INDENT.len());
            lines.extend(output_lines(call, output, room).into_iter().map(|line| {
                let mut spans = vec![Span::styled(BAR, bar), Span::raw(INDENT)];
                spans.extend(line.spans);
                Line::from(spans)
            }));
            lines.extend(notes.iter().map(|note| {
                Line::from(vec![
                    Span::styled(BAR, bar),
                    Span::raw(INDENT),
                    note.span(cwd),
                ])
            }));
            lines
        }
        Entry::TurnDone {
            model,
            tool_calls,
            elapsed,
        } => {
            let calls = match tool_calls {
                0 => String::new(),
                1 => " · 1 tool call".to_string(),
                n => format!(" · {n} tool calls"),
            };
            vec![Line::from(vec![
                Span::raw(INDENT),
                // Closes the turn as `∴` opens its thinking.
                Span::styled(
                    format!("∎ {model}{calls} · {:.1}s", elapsed.as_secs_f64()),
                    dim,
                ),
            ])]
        }
        Entry::Interrupted { elapsed } => vec![Line::from(vec![
            Span::raw(INDENT),
            Span::styled("⏹ ", Style::new().fg(Color::Yellow)),
            Span::styled(format!("interrupted · {:.1}s", elapsed.as_secs_f64()), dim),
        ])],
    }
}

/// The mark a tool's row starts with, so calls can be told apart at a glance.
fn icon(tool: &str) -> &'static str {
    match tool {
        "read" => "≡",
        "write" => ">",
        "edit" => "±",
        "apply_patch" => "Δ",
        "bash" => "$",
        "glob" => "*",
        "grep" => "/",
        "webfetch" => "↓",
        "websearch" => "?",
        "skill" => "✦",
        "question" => "¿",
        "panel" => "▣",
        "monitor" | "monitor_stop" => "»",
        _ => "•",
    }
}

/// What a call produced, as fits its tool: files highlighted by their
/// language, an edit as a diff, a fetched page as markdown, anything else
/// as it came. Lines are not wrapped, except a page's prose: a row shows
/// the start of each.
fn output_lines(call: &ToolCall, output: &[String], room: usize) -> Vec<Line<'static>> {
    let args = serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap_or_default();
    let arg = |key: &str| args[key].as_str().unwrap_or_default();
    match call.name.as_str() {
        "read" => read_lines(output, arg("filePath"), room),
        "write" => rich::code(&output.join("\n"), arg("filePath"), room),
        "edit" => edit_lines(arg("oldString"), arg("newString"), arg("filePath"), room),
        "webfetch" if matches!(arg("format"), "" | "markdown") => {
            let mut lines = rich::markdown(&output.join("\n"), room);
            lines.truncate(OUTPUT_LINES);
            lines
        }
        _ => plain(output),
    }
}

fn plain(output: &[String]) -> Vec<Line<'static>> {
    output
        .iter()
        .map(|text| Line::styled(text.clone(), Style::new().fg(Color::Gray)))
        .collect()
}

/// The file's numbered lines highlighted behind a dim gutter of their
/// numbers. What is not a numbered line, a directory listing or the note
/// on where to read on, shows as it came.
fn read_lines(output: &[String], path: &str, room: usize) -> Vec<Line<'static>> {
    let numbered: Vec<(&str, &str)> = output
        .iter()
        .map_while(|line| {
            let (number, text) = line.split_once(": ").unwrap_or((line, ""));
            let numbered = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
            numbered.then_some((number, text))
        })
        .collect();
    let digits = numbered.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let source: Vec<&str> = numbered.iter().map(|(_, text)| *text).collect();
    let code = rich::code(&source.join("\n"), path, room.saturating_sub(digits + 1));
    let mut lines: Vec<Line<'static>> = numbered
        .iter()
        .zip(code)
        .map(|((number, _), line)| {
            let mut spans = vec![Span::styled(format!("{number:>digits$} "), dim())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();
    lines.extend(plain(&output[numbered.len()..]));
    lines
}

/// The edit as a diff of what it replaced, behind a `+` or `-` gutter.
fn edit_lines(old: &str, new: &str, path: &str, room: usize) -> Vec<Line<'static>> {
    rich::code_diff(old, new, path, room.saturating_sub(GUTTER.len()))
        .into_iter()
        .take(OUTPUT_LINES)
        .map(|(change, line)| {
            let (mark, style) = match change {
                Change::Same => (GUTTER, Style::new()),
                Change::Added => ("+ ", Style::new().fg(Color::Green)),
                Change::Removed => ("- ", Style::new().fg(Color::Red)),
            };
            let mut spans = vec![Span::styled(mark, style)];
            spans.extend(line.spans.into_iter().map(|span| span.patch_style(style)));
            Line::from(spans)
        })
        .collect()
}

/// The gutter in front of an unchanged line of a diff.
const GUTTER: &str = "  ";

/// Markdown beside the message bar, wrapped to fit, the bar repeated on
/// every row, blank ones too, so a block reads as one.
fn barred_markdown(text: &str, width: u16, bar: Style) -> Vec<Line<'static>> {
    let room = usize::from(width.saturating_sub(BAR_WIDTH).max(1));
    rich::markdown(text.trim_matches('\n'), room)
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled(BAR, bar)];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Wraps `text` to fit beside the message bar, repeating the bar on every
/// line. Blank lines inside the text keep the bar so a block reads as one.
fn barred(text: &str, width: u16, bar: Style, body: Style) -> Vec<Line<'static>> {
    prefixed(text, width, Span::styled(BAR, bar), body)
}

/// Wraps `text` to fit after `prefix`, repeating it on every line, blank
/// ones too.
fn prefixed(text: &str, width: u16, prefix: Span<'static>, body: Style) -> Vec<Line<'static>> {
    let room = usize::from(width).saturating_sub(prefix.width()).max(1);
    let text = text.trim_matches('\n').replace('\t', "    ");
    let mut lines = Vec::new();
    if text.is_empty() {
        return lines;
    }
    for raw in text.split('\n') {
        let raw = raw.trim_end();
        if raw.is_empty() {
            lines.push(Line::from(prefix.clone()));
            continue;
        }
        // Indented lines (code, nested lists) wrap under their own indent.
        let content = raw.trim_start();
        let indent = &raw[..raw.len() - content.len()];
        let indent = if indent.len() < room { indent } else { "" };
        let options = textwrap::Options::new(room)
            .initial_indent(indent)
            .subsequent_indent(indent);
        for piece in textwrap::wrap(content, options) {
            lines.push(Line::from(vec![
                prefix.clone(),
                Span::styled(piece.into_owned(), body),
            ]));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use nth_protocol::Event;
    use ratatui::style::{Color, Modifier};

    use crate::chat::transcript::tests::{call, text, transcript};

    #[test]
    fn wraps_under_the_bar_and_separates_blocks() {
        let mut t = transcript();
        t.push_user("one two three".into());
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolStarted(call("2")));
        t.apply(&Event::TextDelta("ok\n\n*leaning* tower".into()));

        let total = t.layout(10);

        assert_eq!(
            text(&t.visible(0, total)),
            [
                "▎ one two",
                "▎ three",
                "",
                "▎ ≡ read   src/a.rs",
                "",
                "▎ ≡ read   src/a.rs",
                "",
                "▎ ok",
                "▎ ",
                "▎ leaning",
                "▎ tower",
            ]
        );
        let lines = t.visible(0, total);
        assert!(
            lines[9].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(
            text(&t.visible(3, 3)),
            ["▎ ≡ read   src/a.rs", "", "▎ ≡ read   src/a.rs"]
        );
    }

    #[test]
    fn reasoning_wraps_in_italics_under_its_timing() {
        let mut t = transcript();
        t.apply(&Event::ReasoningDelta("look at the".into()));
        t.apply(&Event::ReasoningDelta(" file\n\nthen".into()));
        t.apply(&Event::TextDelta("ok".into()));

        let total = t.layout(12);

        let lines = t.visible(0, total);
        assert!(text(&lines)[0].starts_with("  ∴ thought · "));
        assert_eq!(
            text(&lines)[1..],
            ["  look at", "  the file", "  ", "  then", "", "▎ ok"]
        );
        assert!(
            lines[1].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn the_turn_footer_sits_apart_from_the_turn() {
        let mut t = transcript();
        t.push_user("go".into());
        t.apply(&Event::ToolStarted(call("1")));
        t.finish_turn(Ok(()), "glm", std::time::Duration::from_secs(2));

        let total = t.layout(40);

        assert_eq!(
            text(&t.visible(0, total)),
            [
                "▎ go",
                "",
                "▎ ≡ read   src/a.rs",
                "",
                "  ∎ glm · 1 tool call · 2.0s",
            ]
        );
    }

    #[test]
    fn tool_output_sits_under_its_row() {
        let mut t = transcript();
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolOutput {
            call_id: "1".into(),
            text: "fn main() {}\n".into(),
        });
        t.apply(&Event::ToolStarted(call("2")));

        let total = t.layout(40);

        assert_eq!(
            text(&t.visible(0, total)),
            [
                "▎ ≡ read   src/a.rs",
                "▎   fn main() {}",
                "",
                "▎ ≡ read   src/a.rs",
            ]
        );
    }

    #[test]
    fn a_loaded_skill_is_one_row() {
        let mut t = transcript();
        let skill = nth_protocol::ToolCall {
            id: "1".into(),
            name: "skill".into(),
            arguments: r#"{"name":"research-opencode"}"#.into(),
        };
        t.apply(&Event::ToolStarted(skill.clone()));
        t.apply(&Event::ToolFinished {
            call: skill,
            result: Ok("<skill_content name=\"research-opencode\">\nlots of body\n".into()),
        });

        let total = t.layout(40);

        assert_eq!(text(&t.visible(0, total)), ["▎ ✦ skill  research-opencode"]);
    }

    #[test]
    fn a_write_shows_its_format_note_and_errors() {
        let mut t = transcript();
        let write = nth_protocol::ToolCall {
            id: "1".into(),
            name: "write".into(),
            arguments: r#"{"filePath":"/repo/src/a.rs","content":"fn main() {\n    let x: u8 = \"no\";\n}"}"#
                .into(),
        };
        t.apply(&Event::ToolStarted(write.clone()));
        t.apply(&Event::ToolFinished {
            call: write,
            result: Ok("Wrote file: /repo/src/a.rs\n\n\
                Formatted with rustfmt.\n\n\
                LSP errors detected in this file, please fix:\n\
                <diagnostics file=\"/repo/src/a.rs\">\n\
                ERROR [2:18] mismatched types\n\
                </diagnostics>"
                .into()),
        });

        let total = t.layout(60);
        let lines = t.visible(0, total);

        assert_eq!(
            trimmed(&lines),
            [
                "▎ > write  src/a.rs",
                "▎   fn main() {",
                "▎       let x: u8 = \"no\";",
                "▎   }",
                "▎   Formatted with rustfmt.",
                "▎   src/a.rs",
                "▎     ERROR [2:18] mismatched types",
            ]
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.content == "fn" && matches!(s.style.fg, Some(Color::Rgb(..)))),
            "highlighted"
        );
        let note = lines[4].spans.last().expect("note");
        assert!(note.style.add_modifier.contains(Modifier::DIM), "dim");
        for line in &lines[5..] {
            let fg = line.spans.last().expect("span").style.fg;
            assert_eq!(fg, Some(Color::Red), "{line}");
        }
    }

    #[test]
    fn an_answered_question_is_one_row() {
        let mut t = transcript();
        let question = nth_protocol::ToolCall {
            id: "1".into(),
            name: "question".into(),
            arguments: r#"{"questions":[{"header":"Auth"}]}"#.into(),
        };
        t.apply(&Event::ToolStarted(question.clone()));
        t.apply(&Event::ToolFinished {
            call: question,
            result: Ok("The user answered:\n\"Which auth?\" = OAuth".into()),
        });

        let total = t.layout(40);

        assert_eq!(text(&t.visible(0, total)), ["▎ ¿ question Auth"]);
    }

    fn trimmed(lines: &[ratatui::text::Line]) -> Vec<String> {
        text(lines)
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    #[test]
    fn a_message_renders_its_markdown() {
        let mut t = transcript();
        t.push_user("make it **bold**".into());

        let total = t.layout(40);
        let lines = t.visible(0, total);

        assert_eq!(text(&lines), ["▎ make it bold"]);
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "bold")
            .expect("bold");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_read_is_highlighted_behind_its_line_numbers() {
        let mut t = transcript();
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolOutput {
            call_id: "1".into(),
            text: "9: fn a() {}\n10: fn b() {}\n\n(Showing lines 9-10 of 20.)".into(),
        });

        let total = t.layout(40);
        let lines = t.visible(0, total);

        assert_eq!(
            trimmed(&lines),
            [
                "▎ ≡ read   src/a.rs",
                "▎    9 fn a() {}",
                "▎   10 fn b() {}",
                "▎",
                "▎   (Showing lines 9-10 of 20.)",
            ]
        );
        assert!(lines[1].spans[2].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn an_edit_shows_what_it_changed() {
        let mut t = transcript();
        let edit = nth_protocol::ToolCall {
            id: "1".into(),
            name: "edit".into(),
            arguments: r#"{"filePath":"/repo/src/a.rs","oldString":"let a = 1;\nlet b = 2;","newString":"let a = 1;\nlet b = 3;"}"#
                .into(),
        };
        t.apply(&Event::ToolStarted(edit));

        let total = t.layout(40);
        let lines = t.visible(0, total);

        assert_eq!(
            trimmed(&lines),
            [
                "▎ ± edit   src/a.rs",
                "▎     let a = 1;",
                "▎   - let b = 2;",
                "▎   + let b = 3;",
            ]
        );
        assert_eq!(lines[2].spans[2].style.fg, Some(Color::Red));
        assert_eq!(lines[3].spans[3].style.fg, Some(Color::Green));
    }

    #[test]
    fn a_fetched_page_shows_as_markdown() {
        let mut t = transcript();
        let fetch = nth_protocol::ToolCall {
            id: "1".into(),
            name: "webfetch".into(),
            arguments: r#"{"url":"https://example.com"}"#.into(),
        };
        t.apply(&Event::ToolStarted(fetch.clone()));
        t.apply(&Event::ToolFinished {
            call: fetch,
            result: Ok("# Example\n\nSome text.".into()),
        });

        let total = t.layout(40);

        assert_eq!(
            trimmed(&t.visible(0, total)),
            [
                "▎ ↓ webfetch https://example.com",
                "▎   Example",
                "▎",
                "▎   Some text.",
            ]
        );
    }

    #[test]
    fn rewraps_when_the_width_changes() {
        let mut t = transcript();
        t.push_user("one two three".into());

        assert_eq!(t.layout(10), 2);
        assert_eq!(t.layout(40), 1);
    }
}
