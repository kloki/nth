//! What one turn spent, as the session's ledger keeps it and as a running
//! turn reports it to the front-ends.

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{Cost, Message, Usage, failure_reason};

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

    /// One request's usage, as the provider reported it.
    pub fn count(&mut self, usage: Usage) {
        self.steps += 1;
        self.tokens += usage;
    }

    /// Tallies one finished call of `name`: whether it failed, the time it
    /// took and the size of what it returned.
    pub fn tool(&mut self, name: &str, result: &Message, took: Duration) {
        let use_of = self.tools.entry(name.to_string()).or_default();
        use_of.calls += 1;
        use_of.duration += took;
        if let Message::ToolResult { content, .. } = result {
            use_of.output += content.len() as u64;
            if failure_reason(content).is_some() {
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
