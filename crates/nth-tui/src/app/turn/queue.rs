//! What waits for the running turn, and the rules of when it runs: a
//! prompt steers into the turn only while that turn is still yours and
//! nothing queued before it, notices wait for your next prompt after you
//! stopped a turn, and a turn that did not go well gives back what you
//! sent. `TurnQueue` decides; `App` applies the decision, since the inbox,
//! the prompt and starting a turn are its own.

use std::collections::VecDeque;

use tokio::time::Instant;

use crate::app::mode::Steerable;

/// What is sent while a turn runs. A plain prompt is not held: it steers
/// into the running turn through the model's inbox (see
/// [`TurnQueue::send`]); the rest waits for the turn to end. What the model
/// gets for the plan ones is rendered only then: it is written for the
/// model, so it never goes back into the prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum Queued {
    Prompt(String),
    /// A command to run, typed after `!`.
    Shell(String),
    /// `/approve`: the plan's approval.
    Approve(String),
    /// `/compact`: one compaction of the session.
    Compact,
    /// The plan as ctrl+g found it and as you left it.
    PlanEdits {
        original: String,
        edited: String,
    },
}

impl Queued {
    /// As the status bar names it: a prompt as typed, a command with its
    /// `!`, and a word for what goes to the model as a reminder or a diff.
    pub fn label(&self) -> String {
        match self {
            Queued::Prompt(text) => text.clone(),
            Queued::Shell(command) => format!("!{command}"),
            Queued::Approve(instructions) if instructions.is_empty() => "/approve".into(),
            Queued::Approve(instructions) => format!("/approve {instructions}"),
            Queued::Compact => "/compact".into(),
            Queued::PlanEdits { .. } => "plan edits".into(),
        }
    }

    /// A turn for the notices alone, which nobody sent.
    pub(crate) fn is_wake(&self) -> bool {
        matches!(self, Queued::Prompt(text) if text.is_empty())
    }
}

/// What to do with something sent.
#[derive(Debug, PartialEq)]
pub(crate) enum Sent {
    /// Post it to the model's inbox: the running turn picks it up at its
    /// next step.
    Steer(String),
    /// It waits for the running turn to end.
    Queued,
    /// Nothing runs: start it.
    Start(Queued),
}

/// How the turn that ended went, as far as what waits is concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ending {
    /// You stopped it, maybe after it had finished.
    pub interrupted: bool,
    /// It went well and you let it: what waits runs.
    pub went_well: bool,
}

/// What runs after a turn ended.
#[derive(Debug, PartialEq)]
pub(crate) enum AfterTurn {
    /// Nothing: what waits goes back to the prompt (see
    /// [`TurnQueue::give_back`]).
    GiveBack,
    /// A turn for the prompts steered into the last one too late for it,
    /// sent before anything that queued.
    StartSteered,
    /// The oldest thing that queued.
    Start(Queued),
    /// A turn for the notices alone, which starts only if any wake.
    StartEmpty,
}

/// What Esc gives back when no turn runs.
#[derive(Debug, PartialEq)]
pub(crate) enum Stop {
    /// Nothing was held: a running turn, if any, is cancelled.
    Nothing,
    /// The turn held for a save goes back; the notices wait for your next
    /// prompt.
    GiveBack,
    /// What was held was a wake for the notices alone: it goes, and they
    /// still wake the model.
    GiveBackWake,
}

/// What goes back to the prompt when the queue is given up: the prompts,
/// commands and an approval's instructions, oldest first, and what was
/// dropped, for the status bar.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Unqueued {
    pub prompt_parts: Vec<String>,
    pub dropped: Vec<&'static str>,
}

#[derive(Debug, Default)]
pub(crate) struct TurnQueue {
    /// Sent while a turn runs, oldest first; each runs as its own turn
    /// once the one before ends well. Empty while idle, unless a turn is
    /// `held`, which they queue behind.
    queue: VecDeque<Queued>,
    /// Started while the session was being saved between turns, held until
    /// the save ends: that save is of an older clone of the session, and
    /// landing after the turn's own it would roll the file back.
    held: Option<Queued>,
    /// What the running model turn runs as and on, which a prompt steers
    /// into only while it is still yours; `None` while idle or running a
    /// command or a compaction, which pick no prompt up.
    steerable: Option<Steerable>,
    /// Esc was pressed during the running turn, which may have finished
    /// before it saw the cancel.
    interrupted: bool,
    /// Notices wait for your next prompt rather than start a turn, after
    /// you stopped one.
    hold_notices: bool,
    /// When the notices waiting start a turn, if the app is idle by then.
    notices_due: Option<Instant>,
}

impl TurnQueue {
    /// What to do with `next`, sent while a turn is `busy` or not. A plain
    /// prompt steers into the running turn, unless something already
    /// waits, which keeps its place ahead, the turn runs as another mode,
    /// agent or model than `runs_as` (a prompt for those runs on its own),
    /// or it is a skill, filled in only when its own turn starts. A prompt
    /// or plan edits are yours, so notices held after you stopped a turn
    /// go with them.
    pub fn send(&mut self, next: Queued, busy: bool, runs_as: &Steerable, is_skill: bool) -> Sent {
        if matches!(next, Queued::Prompt(_) | Queued::PlanEdits { .. }) {
            self.hold_notices = false;
        }
        match next {
            Queued::Prompt(text)
                if busy
                    && self.queue.is_empty()
                    && self.steerable.as_ref() == Some(runs_as)
                    && !is_skill =>
            {
                Sent::Steer(text)
            }
            next if busy => {
                self.queue.push_back(next);
                Sent::Queued
            }
            next => Sent::Start(next),
        }
    }

    /// Holds `next` until the save in flight ends. What starts meanwhile
    /// queues behind it, as behind a running turn. A wake for notices
    /// alone gives way to anything else: a turn hands the notices over,
    /// and the end of any run wakes the model with what is left.
    pub fn hold(&mut self, next: Queued) {
        match &self.held {
            Some(_) if next.is_wake() => {}
            Some(held) if !held.is_wake() => self.queue.push_back(next),
            _ => self.held = Some(next),
        }
    }

    /// The turn held for the save, which can start now that it ended.
    pub fn take_held(&mut self) -> Option<Queued> {
        self.held.take()
    }

    /// A model turn started, running as `runs_as`: prompts sent now steer
    /// into it, and it takes the notices, so no timer waits for them.
    pub fn started(&mut self, runs_as: Steerable) {
        self.steerable = Some(runs_as);
        self.notices_due = None;
    }

    /// A command or a compaction started: neither asks the model anything,
    /// so nothing would pick a prompt up.
    pub fn started_without_model(&mut self) {
        self.steerable = None;
    }

    /// Esc, with a turn `busy` or not, and whether prompts sent earlier
    /// still wait in the inbox. A turn held for a save goes back as a
    /// running one would: nothing runs yet, but Esc still means stop.
    pub fn interrupt(&mut self, busy: bool, prompts_waiting: bool) -> Stop {
        if busy {
            self.interrupted = true;
            return Stop::Nothing;
        }
        match &self.held {
            None => Stop::Nothing,
            // A wake for notices alone was nobody's to stop, so they still
            // wake the model on their own.
            Some(held) if held.is_wake() && !prompts_waiting => Stop::GiveBackWake,
            Some(_) => {
                self.hold_notices = true;
                Stop::GiveBack
            }
        }
    }

    /// The running turn ended with `result`; Esc after the reply ended
    /// still comes back `Ok`, but it still means stop.
    pub fn ended(&mut self, result: &Result<(), nth_session::Error>) -> Ending {
        let interrupted = std::mem::take(&mut self.interrupted)
            || matches!(result, Err(nth_session::Error::Interrupted));
        self.steerable = None;
        Ending {
            interrupted,
            went_well: result.is_ok() && !interrupted,
        }
    }

    /// What runs after a turn that ended as `ending` says, with prompts
    /// steered into it still `steered_waiting` in the inbox. Notices that
    /// came after the model's last step go with the next prompt once you
    /// stopped it, else wake it on their own: a turn that failed is no
    /// reason to keep a subagent's answer from it.
    pub fn after(&mut self, ending: Ending, steered_waiting: bool) -> AfterTurn {
        self.hold_notices = ending.interrupted;
        if !ending.went_well {
            // Sent prompts were written for a turn that went well.
            return AfterTurn::GiveBack;
        }
        if steered_waiting {
            // Sent before anything that queued behind them.
            return AfterTurn::StartSteered;
        }
        match self.queue.pop_front() {
            Some(next) => AfterTurn::Start(next),
            None => AfterTurn::StartEmpty,
        }
    }

    /// The turn task panicked or was aborted: everything waiting goes back,
    /// `steered` (the prompts still in the inbox) first.
    pub fn failed(&mut self, steered: Option<String>) -> Unqueued {
        self.interrupted = false;
        self.steerable = None;
        self.give_back(steered)
    }

    /// Gives up what waits: `steered`, the prompts on their way into the
    /// turn, were typed before what queued, and a turn held for a save
    /// before what queued behind it. Prompts and commands go back to the
    /// prompt, and so do an approval's instructions; an approval, a
    /// compaction and plan edits are dropped.
    pub fn give_back(&mut self, steered: Option<String>) -> Unqueued {
        let mut unqueued = Unqueued::default();
        unqueued.prompt_parts.extend(steered);
        let held = self.held.take().filter(|held| !held.is_wake());
        for next in held.into_iter().chain(self.queue.drain(..)) {
            let dropped = match next {
                Queued::Prompt(_) | Queued::Shell(_) => {
                    unqueued.prompt_parts.push(next.label());
                    continue;
                }
                Queued::Approve(instructions) => {
                    if !instructions.is_empty() {
                        unqueued.prompt_parts.push(instructions);
                    }
                    "plan approval discarded"
                }
                Queued::Compact => "compaction discarded",
                Queued::PlanEdits { .. } => "plan edits discarded",
            };
            // The status bar names each kind once, however often it queued.
            if !unqueued.dropped.contains(&dropped) {
                unqueued.dropped.push(dropped);
            }
        }
        unqueued
    }

    /// Starts the wait before notices wake the model, until `due`, unless
    /// one runs already: a burst keeps its first deadline rather than
    /// putting it off line by line.
    pub fn arm_notices(&mut self, due: Instant) {
        self.notices_due.get_or_insert(due);
    }

    /// Starts the wait afresh: for what waited through a turn, which no
    /// new notice will come to arm.
    pub fn rearm_notices(&mut self, due: Instant) {
        self.notices_due = Some(due);
    }

    /// The wait ran out: whether the notices may start a turn, which they
    /// may not while one runs, which hands them over itself, or after you
    /// stopped the last one.
    pub fn notices_came_due(&mut self, busy: bool) -> bool {
        self.notices_due = None;
        !busy && !self.hold_notices
    }

    /// The session was left: its notices went with it.
    pub fn forget_notices(&mut self) {
        self.notices_due = None;
        self.hold_notices = false;
    }

    /// When the notices waiting start a turn, if one is set to.
    pub fn due(&self) -> Option<Instant> {
        self.notices_due
    }

    /// Whether notices wait for your next prompt.
    pub fn holds_notices(&self) -> bool {
        self.hold_notices
    }

    /// Whether Esc was pressed during the running turn.
    #[cfg(test)]
    pub fn interrupted(&self) -> bool {
        self.interrupted
    }

    #[cfg(test)]
    pub fn queued(&self) -> &VecDeque<Queued> {
        &self.queue
    }

    #[cfg(test)]
    pub fn held(&self) -> Option<&Queued> {
        self.held.as_ref()
    }

    /// The status bar's `2 queued · <first line of the next one>`, counting
    /// `steered`, the prompts on their way into the running turn, which are
    /// the next thing the model hears and so name the line; a turn held
    /// for a save counts too, a wake for notices not, since nobody sent it.
    pub fn label(&self, steered: Option<(usize, String)>) -> Option<String> {
        let held = self.held.as_ref().filter(|held| !held.is_wake());
        let count = steered.as_ref().map_or(0, |(count, _)| *count)
            + usize::from(held.is_some())
            + self.queue.len();
        let first = match steered {
            Some((_, first)) => first,
            None => {
                let next = held.or(self.queue.front())?.label();
                let first = next.lines().find(|line| !line.trim().is_empty());
                first.unwrap_or_default().to_string()
            }
        };
        Some(format!("{count} queued · {first}"))
    }

    #[cfg(test)]
    pub fn push(&mut self, next: Queued) {
        self.queue.push_back(next);
    }

    #[cfg(test)]
    pub fn set_queued(&mut self, queued: impl IntoIterator<Item = Queued>) {
        self.queue = queued.into_iter().collect();
    }

    #[cfg(test)]
    pub fn set_hold_notices(&mut self, hold: bool) {
        self.hold_notices = hold;
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Llm, Mode};

    use super::*;

    fn runs_as(mode: Mode) -> Steerable {
        let llm = Llm {
            model: "glm".into(),
            ..Llm::default()
        };
        (mode, None, llm)
    }

    fn prompt(text: &str) -> Queued {
        Queued::Prompt(text.into())
    }

    /// A queue whose model turn runs in act on glm.
    fn running() -> TurnQueue {
        let mut queue = TurnQueue::default();
        queue.started(runs_as(Mode::Act));
        queue
    }

    #[test]
    fn a_prompt_steers_only_an_unchanged_turn_with_an_empty_queue() {
        let act = runs_as(Mode::Act);
        assert_eq!(
            running().send(prompt("hi"), true, &act, false),
            Sent::Steer("hi".into())
        );
        // Idle: nothing to steer into.
        assert_eq!(
            running().send(prompt("hi"), false, &act, false),
            Sent::Start(prompt("hi"))
        );
        // Something waits already, and keeps its place ahead.
        let mut queue = running();
        queue.push(Queued::Compact);
        assert_eq!(queue.send(prompt("hi"), true, &act, false), Sent::Queued);
        assert_eq!(*queue.queued(), [Queued::Compact, prompt("hi")]);
        // You switched to plan since the turn started.
        let mut queue = running();
        let plan = runs_as(Mode::Plan);
        assert_eq!(queue.send(prompt("hi"), true, &plan, false), Sent::Queued);
        // A skill is filled in when its own turn starts.
        let mut queue = running();
        assert_eq!(queue.send(prompt("/fix"), true, &act, true), Sent::Queued);
        // A command or compaction runs: nothing picks a prompt up.
        let mut queue = running();
        queue.started_without_model();
        assert_eq!(queue.send(prompt("hi"), true, &act, false), Sent::Queued);
        // Only a prompt steers.
        let mut queue = running();
        let shell = Queued::Shell("make".into());
        assert_eq!(queue.send(shell, true, &act, false), Sent::Queued);
    }

    #[test]
    fn a_failed_turn_gives_prompts_back_and_drops_the_rest() {
        let act = runs_as(Mode::Act);
        let mut queue = running();
        queue.interrupt(true, false);
        for next in [
            Queued::Approve("skip the migration".into()),
            Queued::Compact,
            Queued::PlanEdits {
                original: "a".into(),
                edited: "b".into(),
            },
            Queued::Shell("make".into()),
            Queued::Approve(String::new()),
        ] {
            assert_eq!(queue.send(next, true, &act, false), Sent::Queued);
        }
        let unqueued = queue.failed(Some("steered".into()));
        assert_eq!(
            unqueued,
            Unqueued {
                prompt_parts: vec![
                    "steered".into(),
                    "skip the migration".into(),
                    "!make".into()
                ],
                dropped: vec![
                    "plan approval discarded",
                    "compaction discarded",
                    "plan edits discarded",
                ],
            }
        );
        assert!(queue.queued().is_empty());
        assert!(!queue.interrupted(), "the next turn starts clean");
    }

    #[test]
    fn notices_wait_after_an_interrupt_until_the_next_prompt() {
        let act = runs_as(Mode::Act);
        let mut queue = running();
        queue.interrupt(true, false);
        // Esc after the reply had ended: the turn still comes back `Ok`.
        let ending = queue.ended(&Ok(()));
        assert_eq!(
            ending,
            Ending {
                interrupted: true,
                went_well: false
            }
        );
        assert_eq!(queue.after(ending, false), AfterTurn::GiveBack);
        assert!(queue.holds_notices());
        queue.arm_notices(Instant::now());
        assert!(!queue.notices_came_due(false), "they wait for you");

        let shell = Queued::Shell("ls".into());
        assert_eq!(
            queue.send(shell, false, &act, false),
            Sent::Start(Queued::Shell("ls".into()))
        );
        assert!(
            queue.holds_notices(),
            "a command is not a prompt to the model"
        );
        queue.send(prompt("go on"), false, &act, false);
        assert!(!queue.holds_notices());

        // A turn that failed on its own keeps nothing from the model.
        let mut queue = running();
        let ending = queue.ended(&Err(nth_session::Error::Skill("no".into())));
        assert_eq!(queue.after(ending, false), AfterTurn::GiveBack);
        assert!(!queue.holds_notices());
        assert!(queue.notices_came_due(false));
        assert!(
            !queue.notices_came_due(true),
            "a running turn hands them over"
        );
    }

    #[test]
    fn a_steered_prompt_left_behind_runs_before_the_queue() {
        let act = runs_as(Mode::Act);
        let mut queue = running();
        queue.send(Queued::Compact, true, &act, false);
        let ending = queue.ended(&Ok(()));
        assert_eq!(queue.after(ending, true), AfterTurn::StartSteered);
        assert_eq!(*queue.queued(), [Queued::Compact], "still waits its turn");

        let ending = queue.ended(&Ok(()));
        assert_eq!(
            queue.after(ending, false),
            AfterTurn::Start(Queued::Compact)
        );
        let ending = queue.ended(&Ok(()));
        assert_eq!(queue.after(ending, false), AfterTurn::StartEmpty);
    }

    #[test]
    fn a_wake_held_for_a_save_gives_way_to_anything_sent() {
        let mut queue = TurnQueue::default();
        queue.hold(prompt(""));
        queue.hold(prompt("go"));
        assert_eq!(queue.held(), Some(&prompt("go")));
        queue.hold(prompt(""));
        queue.hold(Queued::Shell("ls".into()));
        assert_eq!(queue.held(), Some(&prompt("go")));
        assert_eq!(*queue.queued(), [Queued::Shell("ls".into())]);
        assert_eq!(
            queue.label(None).as_deref(),
            Some("2 queued · go"),
            "the held turn names the line"
        );
        assert_eq!(
            queue.label(Some((1, "steered".into()))).as_deref(),
            Some("3 queued · steered")
        );

        // Esc on a held wake alone leaves the notices free to wake the model.
        let mut queue = TurnQueue::default();
        queue.hold(prompt(""));
        assert_eq!(queue.interrupt(false, false), Stop::GiveBackWake);
        assert!(!queue.holds_notices());
        assert_eq!(queue.give_back(None), Unqueued::default());
        // ... and never frees ones an earlier Esc held for your prompt.
        queue.set_hold_notices(true);
        queue.hold(prompt(""));
        assert_eq!(queue.interrupt(false, false), Stop::GiveBackWake);
        assert!(queue.holds_notices());
        queue.give_back(None);
        // A held turn of yours is stopped like a running one.
        queue.set_hold_notices(false);
        queue.hold(prompt(""));
        assert_eq!(queue.interrupt(false, true), Stop::GiveBack, "prompts wait");
        assert!(queue.holds_notices());
        assert_eq!(queue.label(None), None);
    }
}
