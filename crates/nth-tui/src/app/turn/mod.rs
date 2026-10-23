//! Running a turn: moving the session into a task, interrupting it, and
//! taking the session back when it ends.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use nth_notify::Event;
use nth_protocol::{Asker, FrontEnd, Llm, Screen, SubagentLlm};
use nth_session::{Session, Store, Subagents, plan, store};
use tokio::task::JoinError;

use super::{App, TabState};
use crate::{command::Command, status};

/// What a turn task hands back: the session, how its turn ended, and
/// whether saving it afterwards worked.
pub(super) struct Ended {
    session: Session,
    result: Result<(), nth_session::Error>,
    saved: Result<(), store::Error>,
    ran: Ran,
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
    Ended {
        session,
        result,
        saved,
        ran,
    }
}

/// What is sent while a turn runs. A plain prompt is not held: it steers
/// into the running turn through the model's inbox (see [`App::send`]);
/// the rest waits for the turn to end. What the model gets for the plan
/// ones is rendered only then: it is written for the model, so it never
/// goes back into the prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum Queued {
    Prompt(String),
    /// A command to run, typed after `!`.
    Shell(String),
    /// `/approve`: the plan's approval.
    Approve,
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
            Queued::Approve => "/approve".into(),
            Queued::Compact => "/compact".into(),
            Queued::PlanEdits { .. } => "plan edits".into(),
        }
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
                // `/add-dir <dir>` is the one command with an argument,
                // and it runs without a turn.
                (Command::AddDir, false) => self.add_dir(&arg),
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
            false => {
                self.hold_notices = false;
                Queued::Prompt(text)
            }
        };
        self.send(next);
    }

    /// Runs `next` as a turn, holds it until the running one ends, or, for
    /// a plain prompt while one runs, steers it: the turn picks it up from
    /// the model's inbox at its next step, pre-empting the tool calls it
    /// was about to run. A skill is filled in when its own turn starts,
    /// what already waits keeps its place ahead, and a prompt for another
    /// mode, agent or model than the running turn's runs on its own, so
    /// those queue.
    pub(super) fn send(&mut self, next: Queued) {
        if let Queued::Prompt(text) = &next
            && self.is_busy()
            && self.queue.is_empty()
            && self.steerable == Some(self.runs_as())
            && nth_context::skills::parse(text, &self.context.skills).is_none()
        {
            self.inbox.post_prompt(text.clone());
            return;
        }
        if self.is_busy() {
            self.queue.push_back(next);
        } else {
            self.start(next);
        }
    }

    fn start(&mut self, next: Queued) {
        match next {
            Queued::Prompt(text) => self.start_turn(text),
            Queued::Shell(command) => self.start_shell(command),
            Queued::Approve => self.start_turn(self.plan.approval()),
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
        let Some(shell) = self.shell.clone() else {
            self.chat
                .transcript
                .push_error("no shell to run commands with".into());
            return;
        };
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.chat.jump_bottom();
        self.busy_since = Some(Instant::now());
        // A command never asks the model, so nothing would pick a prompt up.
        self.steerable = None;
        let events = self.events_tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        self.turn.start(|token| {
            tokio::spawn(async move {
                let result = session
                    .shell(command, shell.as_ref(), &events, &token)
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
        match self.is_busy() {
            true => self.queue.push_back(Queued::Compact),
            false => self.start_compact(),
        }
    }

    /// Runs one compaction as a turn does, so Esc stops it and what you
    /// send meanwhile waits for it. The transcript shows the divider when
    /// the summary lands; nothing answers.
    pub(super) fn start_compact(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        // As a turn does: the picker may have changed the model since the
        // last turn, and the summary request must run the chosen one, its
        // tail and reserved sized against the chosen model's window.
        self.adopt_llm(&mut session);
        self.chat.jump_bottom();
        self.busy_since = Some(Instant::now());
        // A compaction never picks a prompt up, so nothing steers into it.
        self.steerable = None;
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let events = self.events_tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        let front_end = self.front_end();
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
        let Some(mut session) = self.session.take() else {
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
        self.notices_due = None;
        self.adopt_llm(&mut session);
        // The read tool keeps images beside the save, so deleting the
        // session deletes them; without a store the read tool shows none.
        if let Some(store) = &self.store {
            session.images = Some(store.images_dir(session.id));
        }
        // Added with `/add-dir`, maybe while the last turn ran: the session
        // is in its task while one does, so they reach it here.
        if session.extra_dirs != self.extra_dirs {
            session.set_extra_dirs(self.extra_dirs.clone());
        }
        self.llm_usage.count(&self.model);
        self.save_llm_usage();
        self.spent.turn_started(&self.model);
        session.mode = self.mode;
        if session.agent != self.agent {
            session.set_agent(self.agent.clone());
        }
        self.steerable = Some(self.runs_as());
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
        let events = self.events_tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        let front_end = self.front_end();
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

    /// The handles the session's tools reach the app through.
    fn front_end(&self) -> FrontEnd {
        FrontEnd {
            asker: Asker::new(self.asks_tx.clone()),
            screen: Screen::new(self.screen_tx.clone()),
            monitors: self.monitors.clone(),
            inbox: self.inbox.clone(),
        }
    }

    /// The model, effort and limits the next request runs with, and what
    /// its subagents and its clerk run on: the picker
    /// may have changed them since the last turn, maybe mid-turn, and the
    /// session is in its task while one runs, so a turn and a compaction
    /// both take them from the app here. The limits follow the model in
    /// use, so the compaction trigger sizes against it. Until the listing
    /// names the model the session keeps the limits it has — a resumed
    /// one's saved limits still fire its compaction — and a switch to a
    /// model it does not name drops them rather than size against the old
    /// one's window.
    fn adopt_llm(&self, session: &mut Session) {
        // Picked in the model picker since the last turn, maybe mid-turn.
        if session.model != self.model {
            session.set_model(self.model.clone());
            session.set_limits(None, None);
        }
        if let Some((context_window, max_output)) = self.limits_of(&self.model) {
            session.set_limits(context_window, max_output);
        }
        session.takes_images = self.takes_images_of(&self.model).unwrap_or(true);
        session.cost = self.cost_of(&self.model);
        session.effort = self.effort;
        session.subagent_llm = self.mode_llms.subagent.clone().map(|llm| self.picked(llm));
        session.clerk_llm = self.mode_llms.clerk.clone().map(|llm| self.picked(llm));
    }

    /// What subagents or the clerk run on, sized against its own model's
    /// window when the listing names it.
    fn picked(&self, llm: Llm) -> SubagentLlm {
        let (context_window, max_output) = self.limits_of(&llm.model).unwrap_or_default();
        SubagentLlm {
            takes_images: self.takes_images_of(&llm.model),
            cost: self.cost_of(&llm.model),
            llm,
            context_window,
            max_output,
        }
    }

    /// Esc cancels the turn cooperatively so the session comes back;
    /// aborting the task would drop the session with it.
    pub(super) fn interrupt(&mut self) {
        if self.is_busy() {
            self.interrupted = true;
        }
        self.turn.cancel();
    }

    pub(super) fn end_turn(
        &mut self,
        Ended {
            session,
            result,
            saved,
            ran,
        }: Ended,
    ) {
        self.drain_events();
        self.drop_asks();
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        let transcript = &mut self.chat.transcript;
        // Esc after the reply ended still comes back `Ok`, but it still
        // means stop.
        let interrupted = std::mem::take(&mut self.interrupted)
            || matches!(result, Err(nth_session::Error::Interrupted));
        let send_next = result.is_ok() && !interrupted;
        let error = match &result {
            Err(nth_session::Error::Interrupted) | Ok(()) => None,
            Err(e) => Some(e.to_string()),
        };
        self.last_turn = match &result {
            Ok(()) if send_next => TabState::Done,
            Ok(()) | Err(nth_session::Error::Interrupted) => TabState::Idle,
            Err(_) => TabState::Failed,
        };
        match result {
            Err(nth_session::Error::Interrupted) => transcript.interrupt(elapsed),
            // The bash row says how a command went, and the divider the
            // compaction pushed does for it; neither had the model answer.
            Ok(()) if !ran.answered() => {}
            result => {
                // The session's model ran the turn; `self.model` may have
                // been switched since.
                transcript.finish_turn(result.map_err(|e| e.to_string()), &session.model, elapsed)
            }
        }
        if let Err(e) = saved {
            transcript.push_error(format!("session not saved: {e}"));
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
        self.index_files();
        self.load_git();
        self.load_pr();
        // The plan lives under the working directory, so a move moves it.
        match plan_path != self.plan.path() {
            true => self.plan.for_session(plan_path),
            false => self.plan.read(),
        }
        match work_dir != self.work.dir {
            true => self.work.for_session(work_dir),
            false => self.work.read(),
        }
        // Notices that came after the model's last step go with the next
        // prompt once you stopped it, else wake it on their own: a turn
        // that failed is no reason to keep a subagent's answer from it.
        self.hold_notices = interrupted;
        self.steerable = None;
        if !send_next {
            // What the turn never picked up goes back, steered or queued:
            // sent prompts were written for a turn that went well. The
            // notices stay and wake the model on their own.
            self.unqueue();
        } else if self.inbox.prompts_waiting().is_some() {
            // Steered prompts the turn never picked up were sent before
            // anything that queued behind them, so they run first.
            self.start_turn(String::new());
        } else {
            match self.queue.pop_front() {
                Some(next) => self.start(next),
                None => self.start_turn(String::new()),
            }
        }
        // What waited through a failed turn: no new notice will come to
        // arm the timer for it.
        if !self.is_busy() && !self.hold_notices && self.inbox.wakes() {
            self.notices_due = None;
            self.arm_notices();
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
        while let Ok(event) = self.events_rx.try_recv() {
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
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        self.interrupted = false;
        let cx = self.notify_context(elapsed);
        self.notify(Event::Failed(format!("turn task failed: {error}")), &cx);
        self.chat.transcript.fail_turn(format!(
            "turn task failed: {error} · continuing in a new session; \
             the old one was last saved after its previous turn, /resume brings it back"
        ));
        self.steerable = None;
        // Before the fresh session: starting one clears the inbox, and a
        // steered prompt still in it is yours to give back.
        self.unqueue();
        self.start_fresh_session();
    }

    /// After an interrupted or failed turn, the queued prompts go back into
    /// the prompt ahead of what is typed: sent prompts were written for a
    /// turn that went well, so they wait to be looked at again. An
    /// approval, a compaction or plan edits have no text of yours to give
    /// back, so they are dropped, and the status bar says so.
    fn unqueue(&mut self) {
        let mut parts = Vec::new();
        let mut dropped = Vec::new();
        // Prompts on their way into the turn were typed before what queued.
        if let Some(prompts) = self.inbox.take_prompts() {
            parts.push(prompts);
        }
        for next in self.queue.drain(..) {
            match next {
                Queued::Prompt(_) | Queued::Shell(_) => parts.push(next.label()),
                Queued::Approve => dropped.push("plan approval discarded"),
                Queued::Compact => dropped.push("compaction discarded"),
                Queued::PlanEdits { .. } => dropped.push("plan edits discarded"),
            }
        }
        dropped.dedup();
        if !dropped.is_empty() {
            self.hint = Some(dropped.join(" · ").into());
        }
        if parts.is_empty() {
            return;
        }
        if !self.prompt.is_empty() {
            parts.push(self.prompt.take());
        }
        self.prompt.set(&parts.join("\n\n"));
    }
}

#[cfg(test)]
mod tests;
