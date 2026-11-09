//! Running a turn: moving the session into a task, interrupting it, and
//! taking the session back when it ends.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use nth_notify::Event;
use nth_protocol::{Asker, Llm, ModelSettings, Screen};
use nth_session::{FrontEnd, Session, Store, Subagents, TurnEnv, plan, store};
pub use queue::Queued;
use queue::{AfterTurn, Sent, Stop, Unqueued};
use tokio::task::JoinError;

use super::{App, TabState};
use crate::{command::Command, status};

mod queue;
pub(crate) use queue::TurnQueue;

/// What a turn task hands back: the session, how its turn ended, and
/// whether saving it afterwards, and then acknowledging the Curios
/// feedback it recorded, worked.
pub(super) struct Ended {
    session: Session,
    result: Result<(), nth_session::Error>,
    saved: Result<(), store::Error>,
    acknowledged: nth_curios::Result<()>,
    ran: Ran,
}

impl Ended {
    /// Why saving the session after the turn failed, if it did.
    pub(super) fn save_error(&self) -> Option<&store::Error> {
        self.saved.as_ref().err()
    }
}

/// What a turn task ran.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Ran {
    /// A model turn.
    Turn,
    /// A command you typed after `!`.
    Shell,
    /// `/compact`: a summary request; the divider the event pushed says
    /// how it went.
    Compact,
}

impl Ran {
    /// Whether the model answered, which only a turn has it do.
    fn answered(self) -> bool {
        self == Ran::Turn
    }
}

/// The tail of every turn task: subagents that finished while it ran
/// spent on its behalf, and the session is saved however the run ended,
/// interrupted included, since it is always valid to continue from.
async fn ended(
    mut session: Session,
    result: Result<(), nth_session::Error>,
    subagents: Subagents,
    store: Option<Arc<Store>>,
    ran: Ran,
) -> Ended {
    session.usage.extend(subagents.take_spent());
    let saved = match &store {
        Some(store) => store.save(&session).await,
        None => Ok(()),
    };
    // Only a saved session may tell the store its feedback is recorded.
    let acknowledged = match (&store, &saved) {
        (Some(_), Ok(())) => session.acknowledge_curios().await,
        _ => Ok(()),
    };
    Ended {
        session,
        result,
        saved,
        acknowledged,
        ran,
    }
}

impl App {
    pub fn is_busy(&self) -> bool {
        self.turn.is_running()
    }

    pub(super) fn submit(&mut self) {
        let shell = self.prompt.shell();
        if !shell
            && let Some((command, arg)) = Command::invocation(self.prompt.text())
            && let arg = arg.to_string()
        {
            self.prompt.clear();
            match (command, arg.is_empty()) {
                (Command::AddDir, false) => self.add_dir(&arg),
                (Command::Approve, _) => self.approve(arg),
                (command, _) => self.run_command(command),
            }
            return;
        }
        if self.prompt.text().trim().is_empty() {
            return;
        }
        // Kept in the prompt rather than sent to nobody.
        if let Some(id) = self.showing_subagent()
            && !shell
            && self.subagents.describe(id).is_none()
        {
            self.hint = Some("this subagent is gone".into());
            return;
        }
        self.history.push(self.prompt.entry());
        self.save_history();
        let text = self.prompt.take();
        self.prompt.set_shell(false);
        // On a subagent's tab the prompt is its; a command is the main
        // session's, so its output shows on the chat tab.
        if let Some(id) = self.showing_subagent() {
            match shell {
                true => self.open_content(super::Tab::Chat),
                false => return self.prompt_subagent(id, text),
            }
        }
        // Sent into the running turn at its next step, or held until it
        // ends; the chat shows it when the model gets it, so the
        // transcript keeps the order the model saw.
        let next = match shell {
            true => Queued::Shell(text),
            false => Queued::Prompt(text),
        };
        self.send(next);
    }

    /// Runs `next` as a turn, holds it until the running one ends, or, for
    /// a plain prompt while one runs, steers it: the turn picks it up from
    /// the model's inbox at its next step, pre-empting the tool calls it
    /// was about to run. `TurnQueue::send` says which.
    pub(super) fn send(&mut self, next: Queued) {
        let is_skill = matches!(&next, Queued::Prompt(text)
            if nth_context::skills::parse(text, &self.context.skills).is_some());
        let runs_as = self.runs_as();
        match self.turns.send(next, self.is_busy(), &runs_as, is_skill) {
            Sent::Steer(text) => {
                self.inbox.post_prompt(text);
            }
            Sent::Queued => {}
            Sent::Start(next) => self.start(next),
        }
    }

    pub(super) fn start(&mut self, next: Queued) {
        // Held as sent, so the status bar names an approval or plan edits
        // rather than what the model gets for them.
        if self.session_saving.is_running() {
            return self.after_save(next);
        }
        match next {
            Queued::Prompt(text) => self.start_turn(text),
            Queued::Shell(command) => self.start_shell(command),
            Queued::Approve(instructions) => self.start_turn(self.plan.approval(&instructions)),
            Queued::Compact => self.start_compact(),
            Queued::PlanEdits { original, edited } => {
                self.start_turn(plan::edits::render(self.plan.path(), &original, &edited))
            }
        }
    }

    /// Runs `command` for you, without the model; the session keeps it as
    /// a bash call so the model sees it next turn. It runs as a turn does,
    /// so Esc stops it and what you send meanwhile waits for it.
    pub(super) fn start_shell(&mut self, command: String) {
        if self.session_saving.is_running() {
            return self.after_save(Queued::Shell(command));
        }
        let Some(mut session) = self.take_session() else {
            return;
        };
        self.chat.jump_bottom();
        self.busy_since = Some(Instant::now());
        self.turns.started_without_model();
        let shell = self.shell.clone();
        let events = self.channels.events.tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        let front_end = self.front_end();
        self.turn.start(|token| {
            tokio::spawn(async move {
                let result = session
                    .shell(command, shell.as_ref(), &front_end, &events, &token)
                    .await;
                ended(session, result, subagents, store, Ran::Shell).await
            })
        });
    }

    /// `/compact`: summarizes the session now, or waits behind the running
    /// turn like a queued command, since the session is in the turn task.
    /// A compaction is always the main session's, even from a subagent's
    /// tab, which is where a command typed there goes.
    pub(super) fn compact(&mut self) {
        self.send(Queued::Compact);
    }

    /// Runs one compaction as a turn does, so Esc stops it and what you
    /// send meanwhile waits for it. The transcript shows the divider when
    /// the summary lands; nothing answers.
    pub(super) fn start_compact(&mut self) {
        if self.session_saving.is_running() {
            return self.after_save(Queued::Compact);
        }
        let Some(mut session) = self.take_session() else {
            return;
        };
        // As a turn does: the picker may have changed the model since the
        // last turn, and the summary request must run the chosen one, its
        // tail and reserved sized against the chosen model's window.
        self.adopt_llm(&mut session);
        self.chat.jump_bottom();
        self.busy_since = Some(Instant::now());
        self.turns.started_without_model();
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let events = self.channels.events.tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        let front_end = self.front_end();
        self.context_turn_starting();
        self.turn.start(|token| {
            tokio::spawn(async move {
                let result = session
                    .compact(provider.as_ref(), &tools, &front_end, &events, &token)
                    .await;
                ended(session, result, subagents, store, Ran::Compact).await
            })
        });
    }

    /// Starts a turn with `text` after what monitors said since the last
    /// turn; `text` is empty when only the monitors have something to say.
    pub(super) fn start_turn(&mut self, text: String) {
        if self.session.is_none() || (text.is_empty() && !self.inbox.wakes()) {
            return;
        }
        if self.session_saving.is_running() {
            return self.after_save(Queued::Prompt(text));
        }
        let Some(mut session) = self.take_session() else {
            return;
        };
        // `/name args` runs a skill: the chat shows it as typed, and the
        // model gets the skill filled in.
        let skill = nth_context::skills::parse(&text, &self.context.skills)
            .map(|(skill, args)| (skill.clone(), args.to_string()));
        // A skill is filled in before the model is asked, and that can
        // fail; notices taken now would be lost with the turn. They stay
        // in the inbox, where the loop hands them over between steps and
        // the end of the turn wakes the model for the rest.
        let notices = match skill {
            Some(_) => None,
            None => self.inbox.take_notices(),
        };
        // Prompts the last turn never picked up are this one's own: the
        // session treats them as it does `text`.
        let text = match skill {
            Some(_) => text,
            None => match self.inbox.take_prompts() {
                Some(prompts) if text.is_empty() => prompts,
                Some(prompts) => format!("{prompts}\n\n{text}"),
                None => text,
            },
        };
        self.adopt_llm(&mut session);
        // Added with `/add-dir`, maybe while the last turn ran: the session
        // is in its task while one does, so they reach it here.
        if session.extra_dirs != self.extra_dirs {
            session.set_extra_dirs(self.extra_dirs.clone());
        }
        let model = self.selection.current().model;
        self.llm_usage.count(&model);
        self.save_llm_usage();
        session.mode = self.selection.mode;
        if session.agent != self.selection.agent {
            session.set_agent(self.selection.agent.clone());
        }
        self.turns.started(self.runs_as());
        if let Some(notices) = &notices {
            self.chat.transcript.push_user(notices.clone());
        }
        if !text.is_empty() {
            self.chat.transcript.push_user(text.clone());
        }
        self.chat.jump_bottom();
        self.plan.view.turn_started();
        self.busy_since = Some(Instant::now());

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let events = self.channels.events.tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        let front_end = self.front_end();
        self.context_turn_starting();
        self.turn.start(|token| {
            tokio::spawn(async move {
                let text = match skill {
                    Some((skill, args)) => skill
                        .invoke(&args, &session.cwd)
                        .await
                        .map_err(nth_session::Error::Skill),
                    None => Ok(text),
                };
                let text = text.map(|text| match notices {
                    Some(notices) if text.is_empty() => notices,
                    Some(notices) => format!("{notices}\n\n{text}"),
                    None => text,
                });
                let result = match text {
                    Ok(text) if text.is_empty() => {
                        session
                            .deliver_curios(provider.as_ref(), &tools, &front_end, &events, &token)
                            .await
                    }
                    Ok(text) => {
                        session
                            .prompt(text, provider.as_ref(), &tools, &front_end, &events, &token)
                            .await
                    }
                    Err(e) => Err(e),
                };
                ended(session, result, subagents, store, Ran::Turn).await
            })
        });
    }

    /// Holds `next` until the save between turns in flight ends, when
    /// `session_saved` starts it (see `TurnQueue::hold`).
    fn after_save(&mut self, next: Queued) {
        self.turns.hold(next);
    }

    /// The session, for a turn's task to take; what it spent so far stays
    /// drawn meanwhile.
    fn take_session(&mut self) -> Option<Session> {
        let session = self.session.take()?;
        self.ledger = session.usage.clone();
        Some(session)
    }

    /// The handles the session's tools reach the app through.
    fn front_end(&self) -> FrontEnd {
        FrontEnd {
            asker: Asker::new(self.channels.asks.tx.clone()),
            screen: Screen::new(self.channels.screen.tx.clone()),
            monitors: self.monitors.clone(),
            inbox: self.inbox.clone(),
            context_preview: self.context_tab.slot.clone(),
        }
    }

    /// The model, effort and limits the next request runs with, and the
    /// rest of the session's env: the config's share, where the read tool
    /// keeps images and what its subagents and its clerk run on. The picker
    /// may have changed them since the last turn, maybe mid-turn, and the
    /// session is in its task while one runs, so a turn and a compaction
    /// both take them from the app here, as the idle context preview does.
    /// The limits follow the model in use (`Session::size_to`), so the
    /// compaction trigger sizes against it. Until the listing names the
    /// model the session keeps the limits it has — a resumed one's saved
    /// limits still fire its compaction — and a switch to a model it does
    /// not name drops them rather than size against the old one's window.
    pub(super) fn adopt_llm(&self, session: &mut Session) {
        let current = self.selection.current();
        // Picked in the model picker since the last turn, maybe mid-turn.
        let switched = session.model != current.model;
        // What the listing said of the model the session ran on, kept until
        // the listing sizes it again.
        let (mut takes_images, mut cost) = (session.env.takes_images, session.env.cost);
        if switched {
            session.set_model(current.model.clone());
            // Without a listing (`/connect` drops it) nothing sizes the new
            // model below, and the old one's price would land on its spends.
            session.set_limits(None, None);
            (takes_images, cost) = (true, None);
        }
        session.effort = current.effort;
        // Unsized until the listing is in, which sizes them with the model.
        let bare = |llm: &Llm| ModelSettings::bare(llm.clone());
        let slots = self.selection.slots();
        session.env = TurnEnv {
            takes_images,
            cost,
            // The read tool keeps images beside the save, so deleting the
            // session deletes them; without a store the read tool shows none.
            images: self
                .store
                .as_ref()
                .map(|store| store.images_dir(session.id)),
            subagent: slots.subagent.as_ref().map(bare),
            clerk: slots.clerk.as_ref().map(bare),
            ..self.env.clone()
        };
        if let Some(llms) = &self.llms {
            session.size_to(llms);
        }
    }

    /// Esc cancels the turn cooperatively so the session comes back;
    /// aborting the task would drop the session with it.
    pub(super) fn interrupt(&mut self) {
        let prompts_waiting = self.inbox.prompts_waiting().is_some();
        let stop = self.turns.interrupt(self.is_busy(), prompts_waiting);
        self.turn.cancel();
        match stop {
            Stop::Nothing => {}
            Stop::GiveBack => self.unqueue(),
            Stop::GiveBackWake => {
                self.unqueue();
                self.arm_notices();
            }
        }
    }

    pub(super) fn end_turn(
        &mut self,
        Ended {
            session,
            result,
            saved,
            acknowledged,
            ran,
        }: Ended,
    ) {
        self.drain_events();
        self.drop_asks();
        self.context_turn_ended();
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        self.running = None;
        let ending = self.turns.ended(&result);
        let transcript = &mut self.chat.transcript;
        let error = match &result {
            Err(nth_session::Error::Interrupted) | Ok(()) => None,
            Err(e) => Some(e.to_string()),
        };
        self.last_turn = match &result {
            Ok(()) if ending.went_well => TabState::Done,
            Ok(()) | Err(nth_session::Error::Interrupted) => TabState::Idle,
            Err(_) => TabState::Failed,
        };
        match result {
            Err(nth_session::Error::Interrupted) => transcript.interrupt(elapsed),
            // The bash row says how a command went, and the divider the
            // compaction pushed does for it; neither had the model answer.
            Ok(()) if !ran.answered() => {}
            result => {
                // The session's model ran the turn; the selection may have
                // been switched since.
                transcript.finish_turn(result.map_err(|e| e.to_string()), &session.model, elapsed)
            }
        }
        self.curios.saved = saved.is_ok();
        if let Err(e) = saved {
            transcript.push_error(format!("session not saved: {e}"));
        }
        if let Err(e) = acknowledged {
            transcript.push_error(format!("Curios feedback not acknowledged: {e}"));
        }
        // `Moved` has normally been followed already; this catches a move
        // whose event never arrived.
        if session.cwd != self.cwd {
            self.cwd = session.cwd.clone();
            self.follow_place();
        }
        let plan_path = session.plan_path();
        let work_dir = session.work_dir();
        self.session = Some(session);
        // Quitting took the session back only to save it: a read, a
        // notification or the next turn started now would be cut off by
        // the exit, or outlive it.
        if self.quitting {
            return;
        }
        self.refresh_context();
        self.index_files();
        self.load_git();
        self.load_pr();
        // Reload the session's plan; it stays in the shared state directory.
        match plan_path != self.plan.path() {
            true => self.plan.for_session(plan_path),
            false => self.plan.read(),
        }
        match work_dir != self.work.dir {
            true => self.work.for_session(work_dir),
            false => self.work.read(),
        }
        let steered_waiting = self.inbox.prompts_waiting().is_some();
        match self.turns.after(ending, steered_waiting) {
            AfterTurn::GiveBack => self.unqueue(),
            AfterTurn::StartSteered | AfterTurn::StartEmpty => self.start_turn(String::new()),
            AfterTurn::Start(next) => self.start(next),
        }
        // What waited through a failed turn: no new notice will come to
        // arm the timer for it.
        if !self.is_busy() && !self.turns.holds_notices() && self.inbox.wakes() {
            self.rearm_notices();
        }
        // A command of yours is not the model answering: nothing to say.
        self.notify_turn_ended(!ran.answered(), error, elapsed);
        // The turn may have called tools; the all-time totals follow.
        if matches!(self.content.active(), super::Tab::Usage) {
            self.load_tools();
        }
    }

    /// A worktree tool moved the session: the place, git state and files
    /// shown follow it while the turn goes on.
    pub(super) fn follow(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
        self.follow_place();
        self.load_git();
        self.load_pr();
        self.index_files();
    }

    fn follow_place(&mut self) {
        let home = std::env::var("HOME").ok();
        self.place = status::place(&self.cwd, home.as_deref());
    }

    /// Events sent just before the task returned may still be queued, and
    /// they belong above the footer. Through `on_session`, so a usage
    /// report at the tail counts too.
    fn drain_events(&mut self) {
        while let Ok(event) = self.channels.events.rx.try_recv() {
            self.on_session(event);
        }
    }

    /// The turn task panicked, or was aborted, and the session it held is
    /// gone with it. The app goes on in a fresh session in the same
    /// directory rather than exit: the lost one was saved after its
    /// previous turn, so `/resume` brings it back to there. A failed turn
    /// otherwise: the queue goes back into the prompt.
    pub(super) fn turn_task_failed(&mut self, error: JoinError) {
        self.drain_events();
        self.drop_asks();
        self.context_turn_ended();
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        self.running = None;
        let cx = self.notify_context(elapsed);
        self.notify(Event::Failed(format!("turn task failed: {error}")), &cx);
        self.chat.transcript.fail_turn(format!(
            "turn task failed: {error} · continuing in a new session; \
             the old one was last saved after its previous turn, /resume brings it back"
        ));
        // Before the fresh session: starting one clears the inbox, and a
        // steered prompt still in it is yours to give back.
        let unqueued = self.turns.failed(self.inbox.take_prompts());
        self.back_to_prompt(unqueued);
        self.start_fresh_session();
    }

    /// After an interrupted or failed turn, what waits goes back into the
    /// prompt ahead of what is typed (see `TurnQueue::give_back`): sent
    /// prompts were written for a turn that went well, so they wait to be
    /// looked at again. So does a turn held for a save, when you stop it or
    /// leave the session.
    pub(super) fn unqueue(&mut self) {
        let unqueued = self.turns.give_back(self.inbox.take_prompts());
        self.back_to_prompt(unqueued);
    }

    /// Puts what was given back into the prompt, and says on the status
    /// bar what was dropped.
    fn back_to_prompt(
        &mut self,
        Unqueued {
            mut prompt_parts,
            dropped,
        }: Unqueued,
    ) {
        if !dropped.is_empty() {
            self.hint = Some(dropped.join(" · ").into());
        }
        if prompt_parts.is_empty() {
            return;
        }
        if !self.prompt.is_empty() {
            prompt_parts.push(self.prompt.take());
        }
        self.prompt.set(&prompt_parts.join("\n\n"));
    }
}

#[cfg(test)]
mod tests;
