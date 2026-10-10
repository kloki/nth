//! What a session spent: one [`Spend`] per turn, with the tokens its
//! requests used as the provider reported them, kept in the session's
//! [`Ledger`] and saved with it.

use std::{collections::BTreeMap, time::SystemTime};

use nth_protocol::{Cost, Usage};
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
}

impl Spend {
    pub fn new(model: impl Into<String>, agent: Option<String>) -> Self {
        Self {
            model: model.into(),
            agent,
            steps: 0,
            tokens: Usage::default(),
            at: SystemTime::now(),
        }
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
    /// What it all cost at the prices `cost` knows per model; `None` when
    /// it knows none of the models spent on.
    pub fn price(&self, cost: impl Fn(&str) -> Option<Cost>) -> Option<Price> {
        price(self.by_model(), cost)
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

/// What spends cost at the prices `cost` knows per model; `None` when it
/// knows none of them.
pub fn price<'a>(
    totals: impl IntoIterator<Item = (&'a str, Total)>,
    cost: impl Fn(&str) -> Option<Cost>,
) -> Option<Price> {
    let mut sum: Option<Price> = None;
    let mut unpriced = false;
    for (model, total) in totals {
        if total.steps == 0 {
            continue;
        }
        match cost(model) {
            Some(cost) => sum.get_or_insert_default().dollars += cost.price(total.tokens),
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
/// share read from the cache, tokens out and its price when known.
pub fn row(who: &str, total: Total, price: Option<Price>) -> [String; 6] {
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
        price.map(|price| price.to_string()).unwrap_or_default(),
    ]
}

/// Rows as text in columns: who left-aligned, the counts right.
pub fn columns(rows: &[[String; 6]]) -> Vec<String> {
    padded(rows)
        .iter()
        .map(|row| row.join("  ").trim_end().to_string())
        .collect()
}

/// Each row's cells padded to their column's width, who left-aligned and
/// the counts right, for a front-end that styles them one by one.
pub fn padded(rows: &[[String; 6]]) -> Vec<[String; 6]> {
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let widths: [usize; 6] = std::array::from_fn(width);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: Option<u64>) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write: None,
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
        let json = serde_json::to_string(&ledger).expect("serializes");
        assert_eq!(
            serde_json::from_str::<Ledger>(&json).expect("loads"),
            ledger
        );
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
}
