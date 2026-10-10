//! What a session spent: one [`Spend`] per turn, with the tokens its
//! requests used as the provider reported them, kept in the session's
//! [`Ledger`] and saved with it.

use std::{collections::BTreeMap, time::SystemTime};

use nth_protocol::Usage;
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
        self.steps += spend.steps;
        self.tokens += spend.tokens;
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
        let mut models = BTreeMap::<&str, Total>::new();
        for spend in &self.0 {
            models.entry(&spend.model).or_default().add(spend);
        }
        models
    }
}

impl Extend<Spend> for Ledger {
    fn extend<I: IntoIterator<Item = Spend>>(&mut self, spends: I) {
        spends.into_iter().for_each(|spend| self.record(spend));
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
    fn short_counts() {
        assert_eq!(short(812), "812");
        assert_eq!(short(4_250), "4.2k");
        assert_eq!(short(40_000), "40k");
        assert_eq!(short(10_527_843), "10.5M");
    }
}
