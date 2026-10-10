//! What the session spent, as far as the app has heard: its saved ledger,
//! then each request's usage as it arrives, its own and its subagents'.
//! The session counts the same reports in its task, so the two agree
//! without the app waiting for the session to come back. One exception:
//! a subagent forgotten with a left session was counted here, but its
//! spend never reaches the saved ledger; the next session starts afresh.

use std::collections::HashMap;

use nth_protocol::Usage;
use nth_session::{Ledger, SubagentId};

#[derive(Debug, Default)]
pub struct Spent {
    pub ledger: Ledger,
    /// Where the running turn's spend is.
    turn: Option<usize>,
    subagents: HashMap<SubagentId, Subagent>,
}

#[derive(Debug)]
struct Subagent {
    agent: String,
    model: String,
    /// Where its running turn's spend is.
    turn: Option<usize>,
}

impl Spent {
    /// Starting from what a session saved.
    pub fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            ..Self::default()
        }
    }

    pub fn turn_started(&mut self, model: &str) {
        self.turn = Some(self.ledger.begin(model, None));
    }

    /// One request of the session's own; a report outside a turn opens one.
    pub fn usage(&mut self, model: &str, usage: Usage) {
        let at = *self
            .turn
            .get_or_insert_with(|| self.ledger.begin(model, None));
        self.ledger.add(at, usage);
    }

    pub fn subagent_started(&mut self, id: SubagentId, agent: String, model: String) {
        let subagent = Subagent {
            agent,
            model,
            turn: None,
        };
        self.subagents.insert(id, subagent);
    }

    /// One request of subagent `id`'s, counted to its running turn.
    pub fn subagent_usage(&mut self, id: SubagentId, usage: Usage) {
        let Some(subagent) = self.subagents.get_mut(&id) else {
            return;
        };
        let ledger = &mut self.ledger;
        let at = *subagent.turn.get_or_insert_with(|| {
            ledger.begin(subagent.model.clone(), Some(subagent.agent.clone()))
        });
        ledger.add(at, usage);
    }

    pub fn subagent_ended(&mut self, id: SubagentId) {
        if let Some(subagent) = self.subagents.get_mut(&id) {
            subagent.turn = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64) -> Usage {
        Usage {
            input,
            ..Usage::default()
        }
    }

    #[test]
    fn counts_turns_and_subagents_apart() {
        let mut spent = Spent::default();
        spent.turn_started("glm");
        spent.usage("glm", usage(100));
        spent.subagent_started(1, "explore".into(), "kimi".into());
        spent.subagent_usage(1, usage(30));
        spent.usage("glm", usage(200));
        spent.subagent_usage(1, usage(40));
        spent.subagent_ended(1);
        spent.subagent_usage(1, usage(5));

        let spends = spent.ledger.spends();
        assert_eq!(spends.len(), 3, "a new subagent turn after the first ended");
        assert_eq!((spends[0].steps, spends[0].tokens.input), (2, 300));
        assert_eq!(spends[1].agent.as_deref(), Some("explore"));
        assert_eq!((spends[1].steps, spends[1].tokens.input), (2, 70));
        assert_eq!(spends[2].tokens.input, 5);
    }

    #[test]
    fn a_subagent_never_announced_is_not_counted() {
        let mut spent = Spent::default();
        spent.subagent_usage(9, usage(30));
        assert!(spent.ledger.spends().is_empty());
    }
}
