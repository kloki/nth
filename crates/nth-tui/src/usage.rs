//! The usage tab: what the session spent, its subagents included, then the
//! turns run on each model across runs and every tool call's totals. The
//! spend is the totals, then per model and per turn: requests, tokens in,
//! the share of them read from and written to the provider's cache, tokens
//! out, the share spent thinking, and the price at the catalogue's rates;
//! the `all time` bars graph the picker's turn counts.

use nth_protocol::Cost;
use nth_session::{
    Ledger, ToolUse,
    usage::{self, Row, row, short_duration},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{pager::Pager, theme};

/// Where the tab is scrolled, as of the last draw.
#[derive(Debug, Default)]
pub struct UsageView {
    pub pager: Pager,
}

impl UsageView {
    /// `cost` is what each model costs, when the catalogue says; `turns`
    /// is how many ran on each model, most first, and `tools` every tool's
    /// all-time totals, most called first.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ledger: &Ledger,
        turns: &[(&str, u64)],
        tools: &[(String, ToolUse)],
        cost: &dyn Fn(&str) -> Option<Cost>,
    ) {
        let lines = lines(ledger, turns, tools, cost);
        self.pager.render(frame, area, lines);
    }
}

fn lines(
    ledger: &Ledger,
    turns: &[(&str, u64)],
    tool_totals: &[(String, ToolUse)],
    cost: &dyn Fn(&str) -> Option<Cost>,
) -> Vec<Line<'static>> {
    let mut session = vec![title("current session")];
    let mut sections = Vec::new();
    let total = ledger.total();
    if total.steps == 0 {
        session.push(note("nothing spent yet in this session".into()));
        sections.push(session);
    } else {
        let all = row("all", total, ledger.price(cost));
        session.extend(styled(&[all]));
        if let Some(saved) = ledger.saved(cost) {
            session.push(savings(saved));
        }
        sections.push(session);

        let mut per_model = vec![title("per model")];
        let models: Vec<_> = ledger
            .by_model()
            .into_iter()
            .map(|(model, total)| row(model, total, ledger.price_of(model, cost)))
            .collect();
        per_model.extend(styled(&models));
        sections.push(per_model);

        let mut per_turn = vec![title("per turn")];
        let turns: Vec<_> = usage::turns(ledger)
            .map(|(who, spend)| row(&who, spend.into(), spend.price(cost)))
            .collect();
        per_turn.extend(styled(&turns));
        // The caveat stays with the prices it is about.
        per_turn.push(Line::default());
        per_turn.push(note(
            "prices are list-price estimates from models.dev, whatever your plan bills".into(),
        ));
        sections.push(per_turn);
    }

    let mut all_time = vec![title("all time")];
    model_usage(&mut all_time, turns);
    sections.push(all_time);

    let mut tools = vec![title("all time tools")];
    tool_usage(&mut tools, tool_totals);
    sections.push(tools);
    theme::sections(sections)
}

/// `$1.23 saved by caching`, green as the price it is against.
fn savings(dollars: f64) -> Line<'static> {
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(format!("${dollars:.2}"), Style::new().fg(Color::Green)),
        Span::styled(" saved by caching", theme::dim()),
    ])
}

/// One line per tool: its calls, failures, the mean time and the output it
/// returned, all time; the busiest first.
fn tool_usage(lines: &mut Vec<Line<'static>>, tools: &[(String, ToolUse)]) {
    if tools.is_empty() {
        lines.push(note("no tool calls yet".into()));
        return;
    }
    let width = tools
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    let digits = tools
        .iter()
        .map(|(_, use_of)| use_of.calls.to_string().len())
        .max()
        .unwrap_or(0);
    for (name, use_of) in tools {
        let mean = use_of
            .duration
            .checked_div(use_of.calls)
            .unwrap_or_default();
        let failed = match use_of.failed {
            0 => theme::dim(),
            _ => Style::new().fg(Color::Yellow),
        };
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::styled(format!("{name:width$}  "), Style::new().fg(Color::Blue)),
            Span::styled(format!("{:>digits$} calls  ", use_of.calls), theme::dim()),
            Span::styled(format!("{} failed  ", use_of.failed), failed),
            Span::styled(format!("{} avg  ", short_duration(mean)), theme::dim()),
            Span::styled(format!("{} out", usage::short(use_of.output)), theme::dim()),
        ]));
    }
}

/// Cells the busiest model's bar fills.
const BAR_WIDTH: u64 = 30;

/// A bar per model that ran a turn, scaled to the busiest one, then its
/// turns.
fn model_usage(lines: &mut Vec<Line<'static>>, turns: &[(&str, u64)]) {
    let Some(most) = turns.iter().map(|(_, turns)| *turns).max() else {
        lines.push(note("no turns yet".into()));
        return;
    };
    let width = turns.iter().map(|(m, _)| m.len()).max().unwrap_or(0);
    let digits = most.to_string().len();
    for (model, turns) in turns {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::raw(format!("{model:width$}  ")),
            Span::styled(
                format!("{:bar$}", bar(*turns, most), bar = BAR_WIDTH as usize),
                Style::new().fg(Color::Blue),
            ),
            Span::styled(format!("  {turns:>digits$}"), theme::dim()),
        ]));
    }
}

/// `turns` of `most` in eighths of a cell, at least one so every used
/// model shows.
fn bar(turns: u64, most: u64) -> String {
    const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (turns * BAR_WIDTH * 8 / most.max(1)).max(1);
    let mut bar = "█".repeat((eighths / 8) as usize);
    let rest = (eighths % 8) as usize;
    if rest > 0 {
        bar.push(EIGHTHS[rest]);
    }
    bar
}

/// Rows in columns, who left-aligned and the counts right, each cell in
/// its colour: models blue, agents cyan as a subagent's prompt is, turn
/// numbers and steps dim, the cache share green from half on and yellow
/// under it, the write and think shares plain, prices magenta. The totals
/// row is bold.
fn styled(rows: &[Row]) -> Vec<Line<'static>> {
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..usage::COLUMNS).map(width).collect();
    rows.iter()
        .map(|row| {
            let mut spans = vec![Span::raw(theme::INDENT)];
            spans.extend(who(&row[0], widths[0]));
            for (i, (cell, w)) in row.iter().zip(&widths).enumerate().skip(1) {
                // An unknown price leaves no gap at the end of the row.
                if cell.is_empty() && i == row.len() - 1 {
                    break;
                }
                spans.push(Span::raw("  "));
                spans.push(Span::styled(format!("{cell:>w$}"), count(i, cell)));
            }
            Line::from(spans)
        })
        .collect()
}

/// The who column, padded to `width`: `all`, a model, or a turn as
/// `#2 @explore kimi-k3`.
fn who(text: &str, width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let style = match word {
            "all" => Style::new().add_modifier(Modifier::BOLD),
            _ if word.starts_with('#') => theme::dim(),
            _ if word.starts_with('@') => Style::new().fg(Color::Cyan),
            _ => Style::new().fg(Color::Blue),
        };
        spans.push(Span::styled(word.to_string(), style));
    }
    let pad = width.saturating_sub(text.chars().count());
    spans.push(Span::raw(" ".repeat(pad)));
    spans
}

/// The style of count column `i` holding `cell`.
fn count(i: usize, cell: &str) -> Style {
    match i {
        1 => theme::dim(),
        3 => cached(cell),
        7 => Style::new().fg(Color::Magenta),
        _ => Style::new(),
    }
}

/// Green when at least half came from the cache, yellow under it, dim
/// when the provider never said.
fn cached(cell: &str) -> Style {
    let share = cell
        .trim_start()
        .split('%')
        .next()
        .and_then(|n| n.parse::<u32>().ok());
    match share {
        Some(share) if share >= 50 => Style::new().fg(Color::Green),
        Some(_) => Style::new().fg(Color::Yellow),
        None => theme::dim(),
    }
}

fn title(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::new().add_modifier(Modifier::BOLD),
    ))
}

fn note(text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(text, theme::dim()),
    ])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nth_protocol::Usage;
    use nth_session::Spend;

    use super::*;

    fn text(
        ledger: &Ledger,
        turns: &[(&str, u64)],
        tools: &[(String, ToolUse)],
        cost: &dyn Fn(&str) -> Option<Cost>,
    ) -> Vec<String> {
        lines(ledger, turns, tools, cost)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn cells_are_coloured_by_what_they_say() {
        let rows: [Row; 2] = [
            [
                "#2 @explore kimi".to_string(),
                "3 steps".into(),
                "40k in".into(),
                "80% cached".into(),
                "40% written".into(),
                "900 out".into(),
                "60% think".into(),
                "$0.38".into(),
            ],
            [
                "all".to_string(),
                "1 step".into(),
                "1k in".into(),
                "cache ?".into(),
                "write ?".into(),
                "9 out".into(),
                "think ?".into(),
                String::new(),
            ],
        ];
        let lines = styled(&rows);
        let colour = |line: &Line, text: &str| {
            let span = line
                .spans
                .iter()
                .find(|s| s.content.trim() == text)
                .expect(text);
            span.style
        };
        assert_eq!(colour(&lines[0], "#2"), theme::dim());
        assert_eq!(colour(&lines[0], "@explore").fg, Some(Color::Cyan));
        assert_eq!(colour(&lines[0], "kimi").fg, Some(Color::Blue));
        assert_eq!(colour(&lines[0], "80% cached").fg, Some(Color::Green));
        assert_eq!(colour(&lines[0], "$0.38").fg, Some(Color::Magenta));
        assert_eq!(colour(&lines[1], "cache ?"), theme::dim());
        assert_eq!(cached("12% cached").fg, Some(Color::Yellow));
    }

    #[test]
    fn sections_carry_the_grey_bar() {
        let lines = lines(&Ledger::default(), &[], &[], &|_| None);
        let bar = &lines[0].spans[0];
        assert_eq!(bar.content, theme::BAR);
        assert_eq!(bar.style.fg, Some(Color::DarkGray));
        assert!(lines[2].spans.is_empty(), "no bar between sections");
    }

    #[test]
    fn an_empty_session_says_so() {
        assert_eq!(
            text(&Ledger::default(), &[], &[], &|_| None),
            [
                "▎ current session",
                "▎   nothing spent yet in this session",
                "",
                "▎ all time",
                "▎   no turns yet",
                "",
                "▎ all time tools",
                "▎   no tool calls yet",
            ]
        );
    }

    #[test]
    fn totals_then_per_model_then_per_turn() {
        let mut ledger = Ledger::default();
        let mine = ledger.begin("glm", None);
        ledger.add(
            mine,
            Usage {
                input: 1_000_000,
                output: 20_000,
                cache_read: Some(800_000),
                cache_write: None,
                reasoning: None,
            },
        );
        let mut theirs = Spend::new("kimi", Some("explore".into()));
        theirs.steps = 3;
        theirs.tokens = Usage {
            input: 40_000,
            output: 900,
            ..Usage::default()
        };
        ledger.record(theirs);
        let cost = |model: &str| {
            (model == "glm").then_some(Cost {
                input: 1.0,
                output: 5.0,
                cache_read: Some(0.1),
                cache_write: None,
            })
        };

        assert_eq!(
            text(&ledger, &[], &[], &cost),
            [
                "▎ current session",
                "▎   all  4 steps  1.0M in  77% cached  write ?  20k out  think ?  $0.38+",
                "▎   $0.72 saved by caching",
                "",
                "▎ per model",
                "▎   glm    1 step  1.0M in  80% cached  write ?  20k out  think ?  $0.38",
                "▎   kimi  3 steps   40k in     cache ?  write ?  900 out  think ?",
                "",
                "▎ per turn",
                "▎   #1 glm             1 step  1.0M in  80% cached  write ?  20k out  think ?  $0.38",
                "▎   #2 @explore kimi  3 steps   40k in     cache ?  write ?  900 out  think ?",
                "▎ ",
                "▎   prices are list-price estimates from models.dev, whatever your plan bills",
                "",
                "▎ all time",
                "▎   no turns yet",
                "",
                "▎ all time tools",
                "▎   no tool calls yet",
            ]
        );
    }

    #[test]
    fn graphs_the_turns_on_each_model() {
        let turns = [("glm", 120), ("kimi", 30), ("qwen", 1)];
        let text = text(&Ledger::default(), &turns, &[], &|_| None);
        assert_eq!(text[3], "▎ all time", "after the empty session's line");
        let row = |model: &str, bar: &str, turns: &str| format!("▎   {model}  {bar:30}  {turns}");
        assert_eq!(text[4], row("glm ", &"█".repeat(30), "120"));
        assert_eq!(text[5], row("kimi", "███████▌", " 30"));
        assert_eq!(text[6], row("qwen", "▎", "  1"), "every used model shows");
    }

    #[test]
    fn tool_totals_are_graphed_all_time() {
        let tools = [
            (
                "read".to_string(),
                ToolUse {
                    calls: 12,
                    failed: 1,
                    duration: Duration::from_secs(6),
                    output: 40_000,
                },
            ),
            (
                "bash".to_string(),
                ToolUse {
                    calls: 3,
                    failed: 0,
                    duration: Duration::from_secs(3),
                    output: 900,
                },
            ),
        ];
        let text = text(&Ledger::default(), &[], &tools, &|_| None);
        let at = text
            .iter()
            .position(|line| line.contains("all time tools"))
            .expect("a tools section");
        let read = &text[at + 1];
        assert!(read.contains("read"), "{read:?}");
        assert!(read.contains("12 calls"), "{read:?}");
        assert!(read.contains("1 failed"), "{read:?}");
        assert!(read.contains("500ms avg"), "{read:?}");
        assert!(read.contains("40k out"), "{read:?}");
    }
}
