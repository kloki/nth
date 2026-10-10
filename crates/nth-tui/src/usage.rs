//! The usage tab: what the session spent, its subagents included. The
//! totals, then per model and per turn: requests, tokens in, the share of
//! them read from the provider's cache, tokens out and the price at the
//! catalogue's rates.

use nth_protocol::Cost;
use nth_session::{
    Ledger, Total,
    usage::{self, short, steps},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, ScrollbarState},
};

use crate::theme;

/// Where the tab is scrolled, as of the last draw.
#[derive(Debug, Default)]
pub struct UsageView {
    top: usize,
    max_top: usize,
    height: usize,
}

impl UsageView {
    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = (self.top + lines).min(self.max_top);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.max(1));
    }

    pub fn jump_top(&mut self) {
        self.top = 0;
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
    }

    /// Where the view sits in the tab, while it doesn't all fit.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        (self.max_top > 0).then(|| {
            ScrollbarState::new(self.max_top + 1)
                .position(self.top)
                .viewport_content_length(self.height)
        })
    }

    /// `cost` is what each model costs, when the catalogue says.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ledger: &Ledger,
        cost: &dyn Fn(&str) -> Option<Cost>,
    ) {
        let lines = lines(ledger, cost);
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = self.top.min(self.max_top);
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }
}

fn lines(ledger: &Ledger, cost: &dyn Fn(&str) -> Option<Cost>) -> Vec<Line<'static>> {
    let total = ledger.total();
    if total.steps == 0 {
        return vec![
            title("usage"),
            note("nothing spent yet in this session".into()),
        ];
    }
    let price = |model: &str, total: Total| usage::price([(model, total)], cost);
    let mut lines = vec![title("usage")];
    let all = ledger
        .price(cost)
        .map(|p| p.to_string())
        .unwrap_or_default();
    lines.extend(join(&[cells("all", total, all)]).into_iter().map(plain));

    lines.push(Line::default());
    lines.push(title("per model"));
    let models: Vec<_> = ledger
        .by_model()
        .into_iter()
        .map(|(model, total)| {
            let price = price(model, total)
                .map(|p| p.to_string())
                .unwrap_or_default();
            cells(model, total, price)
        })
        .collect();
    lines.extend(join(&models).into_iter().map(plain));

    lines.push(Line::default());
    lines.push(title("per turn"));
    let turns: Vec<_> = ledger
        .spends()
        .iter()
        .filter(|spend| spend.steps > 0)
        .enumerate()
        .map(|(n, spend)| {
            let who = match &spend.agent {
                Some(agent) => format!("#{} @{agent} {}", n + 1, spend.model),
                None => format!("#{} {}", n + 1, spend.model),
            };
            let total = Total {
                steps: spend.steps,
                tokens: spend.tokens,
            };
            let price = price(&spend.model, total)
                .map(|p| p.to_string())
                .unwrap_or_default();
            cells(&who, total, price)
        })
        .collect();
    lines.extend(join(&turns).into_iter().map(plain));

    lines.push(Line::default());
    lines.push(note(
        "prices are list-price estimates from models.dev, whatever your plan bills".into(),
    ));
    lines
}

/// One row: who spent it, then its counts, then its price.
fn cells(who: &str, total: Total, price: String) -> [String; 6] {
    let tokens = total.tokens;
    let cached = match tokens.cached_share() {
        Some(share) => format!("{:.0}% cached", share * 100.0),
        None => "cache ?".into(),
    };
    [
        who.to_string(),
        steps(total.steps),
        format!("{} in", short(tokens.input)),
        cached,
        format!("{} out", short(tokens.output)),
        price,
    ]
}

/// Rows as text in columns: the first left-aligned, the counts right.
fn join(rows: &[[String; 6]]) -> Vec<String> {
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..6).map(width).collect();
    rows.iter()
        .map(|row| {
            let mut line = format!("{:<w$}", row[0], w = widths[0]);
            for (cell, w) in row.iter().zip(&widths).skip(1) {
                line.push_str(&format!("  {cell:>w$}"));
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn title(text: &'static str) -> Line<'static> {
    Line::styled(text, Style::new().add_modifier(Modifier::BOLD))
}

fn plain(text: String) -> Line<'static> {
    Line::from(vec![Span::raw(theme::INDENT), Span::raw(text)])
}

fn note(text: String) -> Line<'static> {
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(text, theme::dim()),
    ])
}

#[cfg(test)]
mod tests {
    use nth_protocol::Usage;
    use nth_session::Spend;

    use super::*;

    fn text(ledger: &Ledger, cost: &dyn Fn(&str) -> Option<Cost>) -> Vec<String> {
        lines(ledger, cost)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn an_empty_session_says_so() {
        assert_eq!(
            text(&Ledger::default(), &|_| None),
            ["usage", "  nothing spent yet in this session"]
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
            text(&ledger, &cost),
            [
                "usage",
                "  all  4 steps  1.0M in  77% cached  20k out  ≈$0.38+",
                "",
                "per model",
                "  glm    1 step  1.0M in  80% cached  20k out  ≈$0.38",
                "  kimi  3 steps   40k in     cache ?  900 out",
                "",
                "per turn",
                "  #1 glm             1 step  1.0M in  80% cached  20k out  ≈$0.38",
                "  #2 @explore kimi  3 steps   40k in     cache ?  900 out",
                "",
                "  prices are list-price estimates from models.dev, whatever your plan bills",
            ]
        );
    }
}
