//! What a session spent: one [`Spend`] per turn, with the tokens its
//! requests used as the provider reported them, kept in the session's
//! [`Ledger`] and saved with it.

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use nth_protocol::{Cost, Message, Usage};
use serde::{Deserialize, Serialize};

/// One turn's requests: on which model, for whom, how many, and the tokens
/// they used added up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub model: String,
    /// The subagent that spent it on the session's behalf; `None` for the
    /// session's own turns.
    pub agent: Option<String>,
    /// Requests the provider reported usage for.
    pub steps: u32,
    pub tokens: Usage,
    /// When the turn started.
    pub at: SystemTime,
    /// What the model's tokens cost, when the listing said; `None` prices
    /// them by the front-end's live listing at display time.
    #[serde(default)]
    pub cost: Option<Cost>,
    /// Every tool call the turn ran, by name, so a spend carries what it
    /// did, not only what it cost.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolUse>,
}

/// What the calls of one tool used: how many ran, how many failed, how long
/// they took added up, and how much output they returned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUse {
    pub calls: u32,
    pub failed: u32,
    pub duration: Duration,
    pub output: u64,
}

impl std::ops::AddAssign for ToolUse {
    fn add_assign(&mut self, other: Self) {
        self.calls += other.calls;
        self.failed += other.failed;
        self.duration += other.duration;
        self.output += other.output;
    }
}

/// `SystemTime` has none, so the epoch stands in for a spend with no turn.
impl Default for Spend {
    fn default() -> Self {
        Self {
            model: String::new(),
            agent: None,
            steps: 0,
            tokens: Usage::default(),
            at: SystemTime::UNIX_EPOCH,
            cost: None,
            tools: BTreeMap::new(),
        }
    }
}

impl Spend {
    pub fn new(model: impl Into<String>, agent: Option<String>) -> Self {
        Self {
            model: model.into(),
            agent,
            at: SystemTime::now(),
            ..Self::default()
        }
    }

    /// Tallies one finished call of `name`: whether it failed, the time it
    /// took and the size of what it returned.
    pub fn tool(&mut self, name: &str, result: &Message, took: Duration) {
        let use_of = self.tools.entry(name.to_string()).or_default();
        use_of.calls += 1;
        use_of.duration += took;
        if let Message::ToolResult { content, .. } = result {
            use_of.output += content.len() as u64;
            if content.starts_with("Error:") {
                use_of.failed += 1;
            }
        }
    }

    /// What this spend cost at its stored rates, else at what `cost` knows
    /// for its model; `None` when neither prices it.
    pub fn price(&self, cost: &dyn Fn(&str) -> Option<Cost>) -> Option<Price> {
        let rates = self.cost.or_else(|| cost(&self.model))?;
        Some(Price {
            dollars: rates.price(self.tokens),
            partial: false,
        })
    }

    /// What the cache saved this spend, at its stored rates else `cost`'s
    /// for its model.
    pub fn saved(&self, cost: &dyn Fn(&str) -> Option<Cost>) -> Option<f64> {
        self.cost
            .or_else(|| cost(&self.model))
            .map(|rates| rates.saved(self.tokens))
    }
}

/// Steps and tokens added up over several spends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Total {
    pub steps: u32,
    pub tokens: Usage,
}

impl Total {
    pub fn add(&mut self, spend: &Spend) {
        *self += Total::from(spend);
    }
}

impl std::ops::AddAssign for Total {
    fn add_assign(&mut self, other: Self) {
        self.steps += other.steps;
        self.tokens += other.tokens;
    }
}

impl<'a> std::iter::Sum<&'a Total> for Total {
    fn sum<I: Iterator<Item = &'a Total>>(totals: I) -> Self {
        let mut sum = Total::default();
        totals.for_each(|total| sum += *total);
        sum
    }
}

/// Every turn's spend, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ledger(Vec<Spend>);

impl Ledger {
    pub fn spends(&self) -> &[Spend] {
        &self.0
    }

    /// The spends from `at` on, as [`Ledger::spends`]`().len()` was then;
    /// a ledger only grows, so they are what was spent since.
    pub fn since(&self, at: usize) -> &[Spend] {
        self.0.get(at..).unwrap_or_default()
    }

    /// Starts a turn's spend and returns where it is, for [`Ledger::add`].
    pub fn begin(&mut self, model: impl Into<String>, agent: Option<String>) -> usize {
        self.0.push(Spend::new(model, agent));
        self.0.len() - 1
    }

    /// One request's usage, counted to the spend at `at`.
    pub fn add(&mut self, at: usize, usage: Usage) {
        if let Some(spend) = self.0.get_mut(at) {
            spend.steps += 1;
            spend.tokens += usage;
        }
    }

    /// A spend made elsewhere, such as a subagent's turn. One that made no
    /// request is left out.
    pub fn record(&mut self, spend: Spend) {
        if spend.steps > 0 {
            self.0.push(spend);
        }
    }

    pub fn total(&self) -> Total {
        let mut total = Total::default();
        self.0.iter().for_each(|spend| total.add(spend));
        total
    }

    /// Totals per model, since each is priced on its own.
    pub fn by_model(&self) -> BTreeMap<&str, Total> {
        by_model(&self.0)
    }
}

impl Ledger {
    /// What it all cost at its spends' stored rates, else what `cost` knows
    /// per model; `None` when none of it can be priced.
    pub fn price(&self, cost: impl Fn(&str) -> Option<Cost>) -> Option<Price> {
        price_spends(self.spends(), &cost)
    }

    /// What the spends on `model` cost, by the same rules.
    pub fn price_of(&self, model: &str, cost: impl Fn(&str) -> Option<Cost>) -> Option<Price> {
        price_spends(
            self.spends().iter().filter(|spend| spend.model == model),
            &cost,
        )
    }

    /// What the cache saved it all, at the spends' stored rates else
    /// `cost`'s; `None` when nothing could be priced.
    pub fn saved(&self, cost: impl Fn(&str) -> Option<Cost>) -> Option<f64> {
        let mut sum: Option<f64> = None;
        for spend in self.spends().iter().filter(|spend| spend.steps > 0) {
            if let Some(saved) = spend.saved(&cost) {
                *sum.get_or_insert(0.0) += saved;
            }
        }
        sum
    }
}

/// Totals of `spends` per model, since each is priced on its own.
pub fn by_model(spends: &[Spend]) -> BTreeMap<&str, Total> {
    let mut models = BTreeMap::<&str, Total>::new();
    for spend in spends {
        models.entry(&spend.model).or_default().add(spend);
    }
    models
}

/// Steps and tokens of `spends` added up.
pub fn total<'a>(spends: impl IntoIterator<Item = &'a Spend>) -> Total {
    let mut total = Total::default();
    spends.into_iter().for_each(|spend| total.add(spend));
    total
}

/// The tools every spend called, added up across `spends`, by name.
pub fn tools<'a>(spends: impl IntoIterator<Item = &'a Spend>) -> BTreeMap<&'a str, ToolUse> {
    let mut tools = BTreeMap::<&str, ToolUse>::new();
    for spend in spends {
        for (name, use_of) in &spend.tools {
            *tools.entry(name).or_default() += *use_of;
        }
    }
    tools
}

/// What `spends` cost at their stored rates, else what `cost` knows per
/// model; `None` when none of them can be priced.
pub fn price_spends<'a>(
    spends: impl IntoIterator<Item = &'a Spend>,
    cost: &dyn Fn(&str) -> Option<Cost>,
) -> Option<Price> {
    let mut sum: Option<Price> = None;
    let mut unpriced = false;
    for spend in spends {
        if spend.steps == 0 {
            continue;
        }
        match spend.price(cost) {
            Some(price) => sum.get_or_insert_default().dollars += price.dollars,
            None => unpriced = true,
        }
    }
    sum.map(|price| Price {
        partial: unpriced,
        ..price
    })
}

/// A list-price estimate in dollars: what the tokens would cost at the
/// catalogue's prices, whatever the plan they ran on bills.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Price {
    pub dollars: f64,
    /// Some of it ran on a model without a known price, left out.
    pub partial: bool,
}

/// `$3.10`, with a `+` when some of it could not be priced.
impl std::fmt::Display for Price {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let more = if self.partial { "+" } else { "" };
        write!(f, "${:.2}{more}", self.dollars)
    }
}

impl Extend<Spend> for Ledger {
    fn extend<I: IntoIterator<Item = Spend>>(&mut self, spends: I) {
        spends.into_iter().for_each(|spend| self.record(spend));
    }
}

/// One row of a usage table: who spent it, its requests, tokens in, the
/// share read from the cache, the share written to it, tokens out, the
/// share spent thinking, and its price when known.
pub fn row(who: &str, total: Total, price: Option<Price>) -> Row {
    let tokens = total.tokens;
    let cached = match tokens.cached_share() {
        Some(share) => format!("{:.0}% cached", share * 100.0),
        None => "cache ?".into(),
    };
    let written = match tokens.cache_write {
        Some(write) if tokens.input > 0 => {
            format!(
                "{:.0}% written",
                (write as f64 / tokens.input as f64).min(1.0) * 100.0
            )
        }
        Some(_) => "0% written".into(),
        None => "write ?".into(),
    };
    let think = match tokens.reasoning_share() {
        Some(share) => format!("{:.0}% think", share * 100.0),
        None => "think ?".into(),
    };
    [
        who.to_string(),
        steps(total.steps),
        format!("{} in", short(tokens.input)),
        cached,
        written,
        format!("{} out", short(tokens.output)),
        think,
        price.map(|price| price.to_string()).unwrap_or_default(),
    ]
}

/// Cells a usage row has, matching [`row`].
pub const COLUMNS: usize = 8;

/// One cell of a usage row.
pub type Row = [String; COLUMNS];

/// Rows as text in columns: who left-aligned, the counts right.
pub fn columns(rows: &[Row]) -> Vec<String> {
    padded(rows)
        .iter()
        .map(|row| row.join("  ").trim_end().to_string())
        .collect()
}

/// Each row's cells padded to their column's width, who left-aligned and
/// the counts right, for a front-end that styles them one by one.
pub fn padded(rows: &[Row]) -> Vec<Row> {
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let widths: [usize; COLUMNS] = std::array::from_fn(width);
    rows.iter()
        .map(|row| {
            std::array::from_fn(|i| match i {
                0 => format!("{:<w$}", row[0], w = widths[0]),
                _ => format!("{:>w$}", row[i], w = widths[i]),
            })
        })
        .collect()
}

/// Each turn that made a request, numbered from 1 in that order, with who
/// spent it: `#2 @explore kimi-k3`.
pub fn turns(ledger: &Ledger) -> impl Iterator<Item = (String, &Spend)> {
    ledger
        .spends()
        .iter()
        .filter(|spend| spend.steps > 0)
        .enumerate()
        .map(|(n, spend)| {
            let who = match &spend.agent {
                Some(agent) => format!("#{} @{agent} {}", n + 1, spend.model),
                None => format!("#{} {}", n + 1, spend.model),
            };
            (who, spend)
        })
}

impl From<&Spend> for Total {
    fn from(spend: &Spend) -> Self {
        Self {
            steps: spend.steps,
            tokens: spend.tokens,
        }
    }
}

/// `1 step`, `12 steps`.
pub fn steps(count: u32) -> String {
    match count {
        1 => "1 step".into(),
        n => format!("{n} steps"),
    }
}

/// A token count short enough for a status line: `812`, `40k`, `10.5M`.
pub fn short(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..10_000 => format!("{:.1}k", tokens as f64 / 1e3),
        10_000..1_000_000 => format!("{}k", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f64 / 1e6),
    }
}

/// A duration short enough for a table cell: `340ms`, `1.2s`, `2m4s`.
pub fn short_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    match secs {
        s if s < 1.0 => format!("{}ms", duration.as_millis()),
        s if s < 60.0 => format!("{s:.1}s"),
        _ => format!("{}m{}s", duration.as_secs() / 60, duration.as_secs() % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: Option<u64>) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write: None,
            reasoning: None,
        }
    }

    #[test]
    fn a_turn_adds_up_its_requests() {
        let mut ledger = Ledger::default();
        let turn = ledger.begin("glm", None);
        ledger.add(turn, usage(100, 10, Some(80)));
        ledger.add(turn, usage(200, 20, None));

        let total = ledger.total();
        assert_eq!(total.steps, 2);
        assert_eq!(total.tokens, usage(300, 30, Some(80)));
    }

    #[test]
    fn a_cache_count_stays_unknown_until_one_is_reported() {
        let mut sum = usage(1, 1, None);
        sum += usage(1, 1, None);
        assert_eq!(sum.cache_read, None);
        sum += usage(1, 1, Some(1));
        assert_eq!(sum.cache_read, Some(1));
    }

    #[test]
    fn totals_per_model_and_spends_without_requests_left_out() {
        let mut ledger = Ledger::default();
        let mine = ledger.begin("glm", None);
        ledger.add(mine, usage(100, 10, None));
        let mut theirs = Spend::new("kimi", Some("explore".into()));
        theirs.steps = 3;
        theirs.tokens = usage(50, 5, None);
        ledger.extend([theirs, Spend::new("kimi", Some("general".into()))]);

        assert_eq!(ledger.spends().len(), 2);
        let models = ledger.by_model();
        assert_eq!(models["glm"].tokens.input, 100);
        assert_eq!(models["kimi"].steps, 3);
    }

    #[test]
    fn saved_and_loaded_unchanged() {
        let mut ledger = Ledger::default();
        let turn = ledger.begin("glm", Some("explore".into()));
        ledger.add(turn, usage(100, 10, Some(5)));
        let mut spend = Spend::new("kimi", None);
        spend.steps = 1;
        spend.tokens = usage(50, 5, None);
        spend.cost = Some(Cost {
            input: 1.0,
            ..Cost::default()
        });
        spend.tools.insert(
            "read".into(),
            ToolUse {
                calls: 2,
                failed: 1,
                duration: Duration::from_millis(120),
                output: 30,
            },
        );
        ledger.record(spend);

        let json = serde_json::to_string(&ledger).expect("serializes");
        assert_eq!(
            serde_json::from_str::<Ledger>(&json).expect("loads"),
            ledger
        );
    }

    #[test]
    fn an_old_spend_loads_without_cost_tools_or_reasoning() {
        let json = r#"[{"model":"glm","agent":null,"steps":1,
            "tokens":{"input":10,"output":2},
            "at":{"secs_since_epoch":0,"nanos_since_epoch":0}}]"#;
        let ledger: Ledger = serde_json::from_str(json).expect("loads");
        let spend = &ledger.spends()[0];
        assert!(spend.tools.is_empty());
        assert_eq!(spend.cost, None);
        assert_eq!(spend.tokens.reasoning, None);
    }

    #[test]
    fn priced_per_model_and_unpriced_models_marked() {
        let mut ledger = Ledger::default();
        let glm = ledger.begin("glm", None);
        ledger.add(glm, usage(1_000_000, 100_000, Some(500_000)));
        let cost = |model: &str| {
            (model == "glm").then_some(Cost {
                input: 1.0,
                output: 4.0,
                cache_read: Some(0.2),
                cache_write: None,
            })
        };
        let price = ledger.price(cost).expect("glm is priced");
        assert!((price.dollars - (0.5 + 0.1 + 0.4)).abs() < 1e-9);
        assert_eq!(price.to_string(), "$1.00");

        let kimi = ledger.begin("kimi", None);
        ledger.add(kimi, usage(10, 1, None));
        assert_eq!(ledger.price(cost).expect("partly").to_string(), "$1.00+");
        assert_eq!(ledger.price(|_| None), None);
    }

    #[test]
    fn short_counts() {
        assert_eq!(short(812), "812");
        assert_eq!(short(4_250), "4.2k");
        assert_eq!(short(40_000), "40k");
        assert_eq!(short(10_527_843), "10.5M");
    }

    #[test]
    fn short_durations() {
        assert_eq!(short_duration(Duration::from_millis(340)), "340ms");
        assert_eq!(short_duration(Duration::from_millis(1_250)), "1.2s");
        assert_eq!(short_duration(Duration::from_secs(124)), "2m4s");
    }

    #[test]
    fn a_stored_price_wins_over_the_listing() {
        let mut spend = Spend::new("glm", None);
        spend.steps = 1;
        spend.tokens = usage(1_000_000, 0, None);
        spend.cost = Some(Cost {
            input: 1.0,
            ..Cost::default()
        });
        let mut ledger = Ledger::default();
        ledger.record(spend);

        let live = |_: &str| {
            Some(Cost {
                input: 9.0,
                ..Cost::default()
            })
        };
        assert_eq!(ledger.price(live).expect("priced").dollars, 1.0);
        assert_eq!(
            ledger.price(|_| None).expect("stored prices it").dollars,
            1.0,
            "no listing needed"
        );
    }

    #[test]
    fn saved_sums_the_cache_discount() {
        let mut spend = Spend::new("glm", None);
        spend.steps = 1;
        spend.tokens = usage(1_000_000, 0, Some(500_000));
        spend.cost = Some(Cost {
            input: 2.0,
            cache_read: Some(0.5),
            ..Cost::default()
        });
        let mut ledger = Ledger::default();
        ledger.record(spend);

        let saved = ledger.saved(|_| None).expect("priced");
        assert!((saved - 0.75).abs() < 1e-9, "{saved}");
    }

    #[test]
    fn tools_add_up_across_spends() {
        let result = |id: &str, content: &str| Message::ToolResult {
            call_id: id.into(),
            content: content.into(),
            images: Vec::new(),
        };
        let mut a = Spend::new("glm", None);
        a.steps = 1;
        a.tool("read", &result("1", "hi"), Duration::from_millis(100));
        a.tool(
            "bash",
            &result("2", "Error: nope"),
            Duration::from_millis(300),
        );
        let mut b = Spend::new("glm", None);
        b.steps = 1;
        b.tool("read", &result("3", "yo"), Duration::from_millis(200));

        let totals = tools([&a, &b]);
        let read = totals["read"];
        assert_eq!((read.calls, read.failed, read.output), (2, 0, 4));
        assert_eq!(read.duration, Duration::from_millis(300));
        let bash = totals["bash"];
        assert_eq!((bash.calls, bash.failed), (1, 1));
    }

    #[test]
    fn a_row_shows_the_written_and_think_shares() {
        let total = Total {
            steps: 1,
            tokens: Usage {
                input: 1_000,
                output: 100,
                cache_read: Some(800),
                cache_write: Some(100),
                reasoning: Some(60),
            },
        };
        let row = row("all", total, None);
        assert_eq!(row[3], "80% cached");
        assert_eq!(row[4], "10% written");
        assert_eq!(row[6], "60% think");
        assert_eq!(row[7], "", "no price, no cell");
    }
}
