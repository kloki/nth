//! The subagents the model delegated to: a tab each, which the prompt talks
//! to while it shows, and which closes only once its turn has ended, so
//! nothing keeps running out of sight; by itself once it answered. A task's answer waits in the model's
//! inbox and wakes it as a monitor's notice does.

use std::time::Duration;

use nth_session::{
    CancellationToken,
    subagent::{Done, Job, SubagentEvent, SubagentId},
};

use super::{App, NOTICE_DELAY, Tab, TabState};
use crate::subagent::SubagentView;

/// How long quitting waits for the stopped subagents to report back.
const EXIT_WAIT: Duration = Duration::from_secs(2);

impl App {
    pub(super) fn on_subagent(&mut self, event: SubagentEvent) {
        match event {
            SubagentEvent::Started {
                id,
                agent,
                description,
                ..
            } => {
                // Forgotten before it got here: its session was left.
                if self.subagents.forgotten(id) {
                    return;
                }
                self.subagent_views
                    .insert(id, SubagentView::new(agent, description, self.cwd.clone()));
                // Opened, not shown: you keep looking at the chat while the
                // model works.
                self.content.add(Tab::Subagent(id));
            }
            SubagentEvent::Prompted { id, text } => {
                if !self.subagent_views.contains_key(&id)
                    && let Some((agent, description)) = self.subagents.describe(id)
                {
                    self.subagent_views
                        .insert(id, SubagentView::new(agent, description, self.cwd.clone()));
                }
                // Its tab was closed and the model continued it: it opens
                // again, with its earlier turns, so nothing runs out of
                // sight.
                if self.subagent_views.contains_key(&id) {
                    self.content.add(Tab::Subagent(id));
                }
                if let Some(view) = self.subagent_views.get_mut(&id) {
                    view.prompted(text);
                    view.queued = self.subagents.queued(id);
                }
            }
            SubagentEvent::Session { id, event } => {
                if let Some(view) = self.subagent_views.get_mut(&id) {
                    view.apply(&event);
                }
            }
            SubagentEvent::TurnEnded {
                id,
                outcome,
                elapsed,
                model,
            } => {
                if let Some(view) = self.subagent_views.get_mut(&id) {
                    view.ended(&outcome, elapsed, &model);
                    if view.closing {
                        self.subagent_views.remove(&id);
                        self.content.remove(Tab::Subagent(id));
                    }
                }
                self.take_subagent_spend();
            }
        }
        // A task's answer waits in the inbox with the monitors' notices.
        if self.notices_due.is_none() && self.inbox.has_notices() {
            self.notices_due = Some(tokio::time::Instant::now() + NOTICE_DELAY);
        }
    }

    /// What the subagents' finished turns spent goes into the session
    /// when it is at hand, saved; mid-turn the turn's task takes it in.
    pub(super) fn take_subagent_spend(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };
        let spent = self.subagents.take_spent();
        if !spent.is_empty() {
            session.usage.extend(spent);
            self.save_session();
        }
    }

    /// Subagents whose turn runs, as far as their tabs have heard.
    pub(crate) fn running_subagents(&self) -> usize {
        self.subagent_views
            .values()
            .filter(|v| v.is_running())
            .count()
    }

    /// Closes the tabs of subagents that answered, once you are not looking
    /// at them. Their views stay, so a continued one opens again with its
    /// earlier turns; a failed or stopped one stays open to say why.
    pub(super) fn close_answered_subagents(&mut self) {
        let answered: Vec<Tab> = self
            .content
            .tabs()
            .iter()
            .copied()
            .filter(|&tab| tab != self.content.active())
            .filter(|tab| match tab {
                Tab::Subagent(id) => self
                    .subagent_views
                    .get(id)
                    .is_some_and(|view| view.state() == TabState::Done),
                _ => false,
            })
            .collect();
        for tab in answered {
            self.content.remove(tab);
        }
    }

    /// The subagent whose tab shows, if one does.
    pub(super) fn showing_subagent(&self) -> Option<SubagentId> {
        match self.content.active() {
            Tab::Subagent(id) => Some(id),
            _ => None,
        }
    }

    /// Sends what you typed on a subagent's tab to it. It waits in the
    /// inbox behind whatever the subagent is doing; the tab shows it once
    /// its turn starts.
    pub(super) fn prompt_subagent(&mut self, id: SubagentId, text: String) {
        let job = Job {
            text,
            cancel: CancellationToken::new(),
            done: Done::Nothing,
            timeout: None,
        };
        if !self.subagents.prompt(id, job) {
            self.hint = Some("this subagent is gone".into());
            return;
        }
        if let Some(view) = self.subagent_views.get_mut(&id) {
            view.queued = self.subagents.queued(id);
        }
    }

    /// Esc or ctrl+w on a subagent's tab: stops its running turn and what
    /// you queued for it, never the parent's.
    pub(super) fn interrupt_subagent(&mut self, id: SubagentId) {
        self.subagents.cancel(id);
        if let Some(view) = self.subagent_views.get_mut(&id) {
            view.queued = self.subagents.queued(id);
        }
    }

    /// Ends the subagents of a session that was left, after `/clear` or a
    /// resume: they worked for that conversation. Their tabs close as each
    /// turn ends, and their views go with them, closed tab or not.
    pub(super) fn left_subagents(&mut self) {
        self.subagents.forget_all();
        let mut ended = Vec::new();
        for (&id, view) in &mut self.subagent_views {
            if view.is_running() {
                view.closing = true;
            } else {
                ended.push(id);
            }
        }
        for id in ended {
            self.subagent_views.remove(&id);
            self.content.remove(Tab::Subagent(id));
        }
    }

    /// Stops every running subagent as nth quits, and waits briefly for
    /// their interrupted notices, which the caller saves in the session.
    pub(super) async fn stop_subagents_for_exit(&mut self) {
        if self.subagents.running() == 0 {
            return;
        }
        for id in self.subagents.ids() {
            self.subagents.cancel(id);
        }
        let subagents = self.subagents.clone();
        let rx = &mut self.subagent_rx;
        let ended = async { while subagents.running() > 0 && rx.recv().await.is_some() {} };
        let _ = tokio::time::timeout(EXIT_WAIT, ended).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_protocol::{Event, Message, TaskOutcome};
    use nth_session::{Session, Subagents};
    use ratatui::style::Color;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        app::{
            keys::Action,
            tests::{Idle, app, rows, tab_colour},
        },
        chat::Entry,
    };

    /// An app with a front-end for subagents, as the chat has.
    fn fronted() -> App {
        let mut app = app();
        let (tx, rx) = mpsc::channel(64);
        app.subagents = Subagents::new(tx);
        app.subagent_rx = rx;
        app
    }

    fn started(app: &mut App, id: SubagentId, agent: &str) {
        app.on_subagent(SubagentEvent::Started {
            id,
            agent: agent.into(),
            description: "find tabs".into(),
            model: "glm".into(),
        });
    }

    fn prompted(app: &mut App, id: SubagentId, text: &str) {
        app.on_subagent(SubagentEvent::Prompted {
            id,
            text: text.into(),
        });
    }

    fn ended(app: &mut App, id: SubagentId, outcome: TaskOutcome) {
        app.on_subagent(SubagentEvent::TurnEnded {
            id,
            outcome,
            elapsed: Duration::from_secs(1),
            model: "glm".into(),
        });
    }

    fn hear(app: &mut App) {
        while let Ok(event) = app.subagent_rx.try_recv() {
            app.on_subagent(event);
        }
    }

    /// A real subagent in the registry, whose provider answers nothing.
    fn spawn(app: &mut App) -> SubagentId {
        let agents = nth_context::Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        )
        .agents;
        let explore = agents.get("explore").expect("built in");
        let session = Session::new("glm", "/repo".into()).as_subagent(None);
        app.subagents
            .spawn(explore, "find tabs", session, Arc::new(Idle), Vec::new())
    }

    #[test]
    fn a_subagent_opens_a_tab_without_showing_it() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "where do tabs open?");

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Subagent(1)]);
        assert_eq!(app.content.active(), Tab::Chat);
        let rows = rows(&mut app);
        assert!(rows[0].starts_with(" [› chat] "), "{}", rows[0]);
        assert!(
            rows[0].contains(" find tabs ")
                && !rows[0].contains('@')
                && !rows[0].contains("explore"),
            "the spinner stands in for @ while it runs: {}",
            rows[0]
        );
        assert!(app.chat.transcript.is_empty(), "nothing in the main chat");
    }

    #[test]
    fn the_tab_shows_the_header_and_its_chat_and_the_prompt_names_it() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "where do tabs open?");
        app.on_subagent(SubagentEvent::Session {
            id: 1,
            event: Event::TextDelta("In content.rs.".into()),
        });
        app.apply(Action::Content(1));

        let before = rows(&mut app);
        assert!(
            before[2].starts_with(" @explore · find tabs · running · 0s"),
            "{}",
            before[2]
        );
        assert!(
            before[4].starts_with(" ▎ where do tabs open?"),
            "{}",
            before[4]
        );
        assert!(before[6].starts_with(" ▎ In content.rs."), "{}", before[6]);
        assert!(
            before[9].starts_with(" ▎ ⠖⠉⠉⠑") || before[9].contains("esc to cancel"),
            "{}",
            before[9]
        );

        ended(&mut app, 1, TaskOutcome::Completed("In content.rs.".into()));
        let after = rows(&mut app);
        assert!(after[0].contains("[@ find tabs]"), "{}", after[0]);
        assert_eq!(
            after[0].find(']').map(|i| after[0][..i].chars().count()),
            before[0].find(']').map(|i| before[0][..i].chars().count()),
            "running and done take the same width: {} / {}",
            before[0],
            after[0]
        );
        assert_eq!(
            tab_colour(&mut app, "@ find tabs"),
            Color::Reset,
            "done fades on the tab showing"
        );
        assert!(
            after[9].starts_with(" ▎ explore "),
            "the label names it: {}",
            after[9]
        );
        app.apply(Action::NextTab);
        assert_eq!(
            app.mode,
            nth_protocol::Mode::Act,
            "Tab does not switch the mode here"
        );
    }

    #[tokio::test]
    async fn enter_on_its_tab_prompts_the_subagent_not_the_chat() {
        let mut app = fronted();
        let id = spawn(&mut app);
        tokio::task::yield_now().await;
        hear(&mut app);
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Subagent(id)]);
        app.apply(Action::Content(1));
        for c in "look again".chars() {
            app.apply(Action::Insert(c));
        }

        app.apply(Action::Submit);

        assert!(app.prompt.is_empty());
        assert!(!app.is_busy(), "the main session did not start a turn");
        assert!(app.chat.transcript.is_empty());
        // The actor takes the job and says so.
        for _ in 0..20 {
            tokio::task::yield_now().await;
            hear(&mut app);
            if !app.subagent_views[&id].chat.transcript.is_empty() {
                break;
            }
        }
        let rows = rows(&mut app);
        assert!(rows[4].starts_with(" ▎ look again"), "{}", rows[4]);
        assert_eq!(
            app.history.prev(&app.prompt.entry()).as_deref(),
            Some("look again")
        );
    }

    #[test]
    fn a_task_answer_wakes_the_model_through_the_inbox() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "go");
        assert!(app.notices_due.is_none());

        app.inbox.post_task(nth_protocol::TaskNotice {
            id: 1,
            agent: "explore".into(),
            description: "find tabs".into(),
            outcome: TaskOutcome::Completed("found".into()),
        });
        ended(&mut app, 1, TaskOutcome::Completed("found".into()));

        assert!(app.notices_due.is_some(), "the inbox wakes the model");
    }

    #[test]
    fn a_running_subagent_tab_does_not_close_and_esc_stops_it() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "go");
        app.apply(Action::Content(1));

        app.apply(Action::CloseContent);
        assert_eq!(app.content.active(), Tab::Subagent(1));
        assert!(rows(&mut app)[15].starts_with(" still running · ctrl+w stops it first"));

        app.apply(Action::Interrupt);
        assert!(!app.interrupted, "the parent's turn is left alone");
        ended(&mut app, 1, TaskOutcome::Interrupted);
        app.apply(Action::Content(0));
        assert_eq!(
            tab_colour(&mut app, "@ find tabs"),
            Color::Red,
            "stays open to say why"
        );
        app.apply(Action::Content(1));
        app.apply(Action::CloseContent);
        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(
            app.subagent_views.contains_key(&1),
            "kept for when it is continued"
        );
    }

    #[test]
    fn an_answered_tab_closes_by_itself() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "go");
        ended(&mut app, 1, TaskOutcome::Completed("found".into()));

        rows(&mut app);
        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(app.subagent_views.contains_key(&1));
    }

    #[test]
    fn an_answered_tab_showing_closes_once_left() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "go");
        app.apply(Action::Content(1));
        ended(&mut app, 1, TaskOutcome::Completed("found".into()));

        rows(&mut app);
        assert_eq!(app.content.active(), Tab::Subagent(1), "you are reading it");
        app.apply(Action::Content(0));
        rows(&mut app);
        assert_eq!(app.content.tabs(), [Tab::Chat]);
    }

    #[test]
    fn a_continued_subagent_opens_again_with_its_turns() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "first question");
        ended(&mut app, 1, TaskOutcome::Completed("found".into()));
        rows(&mut app);
        assert_eq!(app.content.tabs(), [Tab::Chat]);

        prompted(&mut app, 1, "second question");
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Subagent(1)]);
        let asked: Vec<&Entry> = app.subagent_views[&1]
            .chat
            .transcript
            .entries()
            .filter(|entry| matches!(entry, Entry::User(_)))
            .collect();
        assert_eq!(
            asked,
            [
                &Entry::User("first question".into()),
                &Entry::User("second question".into())
            ]
        );
    }

    #[tokio::test]
    async fn clear_closes_the_tabs_as_their_turns_end() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "go");
        started(&mut app, 2, "general");
        prompted(&mut app, 2, "go");
        ended(&mut app, 2, TaskOutcome::Completed("done".into()));

        app.run_command(crate::command::Command::Clear);
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Subagent(1)]);

        ended(&mut app, 1, TaskOutcome::Interrupted);
        assert_eq!(app.content.tabs(), [Tab::Chat]);
    }

    #[tokio::test]
    async fn a_closed_tab_opens_again_when_its_subagent_is_prompted() {
        let mut app = fronted();
        let id = spawn(&mut app);
        tokio::task::yield_now().await;
        hear(&mut app);
        app.apply(Action::Content(1));
        app.apply(Action::CloseContent);
        assert_eq!(app.content.tabs(), [Tab::Chat]);

        prompted(&mut app, id, "and now?");

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Subagent(id)]);
        assert!(app.subagent_views[&id].is_running());
    }

    #[tokio::test]
    async fn a_subagent_started_before_clear_gets_no_tab() {
        let mut app = fronted();
        let id = spawn(&mut app);

        app.run_command(crate::command::Command::Clear);
        tokio::task::yield_now().await;
        hear(&mut app);
        started(&mut app, id, "explore");

        assert_eq!(app.content.tabs(), [Tab::Chat]);
    }

    #[test]
    fn quitting_with_a_subagent_running_asks_twice() {
        let mut app = app();
        started(&mut app, 1, "explore");
        prompted(&mut app, 1, "go");

        app.apply(Action::ClearOrQuit);
        assert!(!app.quit);
        assert!(
            rows(&mut app)[15].starts_with(" 1 subagent running · ctrl+c again"),
            "{}",
            rows(&mut app)[15]
        );
        app.apply(Action::ClearOrQuit);
        assert!(app.quit);
    }

    /// Answers every request with a word and what it used.
    struct Spends;

    impl nth_protocol::Provider for Spends {
        fn models(
            &self,
        ) -> futures::future::BoxFuture<'_, Result<nth_protocol::Listing, nth_protocol::BoxError>>
        {
            Box::pin(async { Ok(nth_protocol::Listing::default()) })
        }

        fn stream<'a>(
            &'a self,
            _: nth_protocol::Request<'a>,
        ) -> futures::future::BoxFuture<
            'a,
            Result<
                futures::stream::BoxStream<
                    'static,
                    Result<nth_protocol::StreamEvent, nth_protocol::BoxError>,
                >,
                nth_protocol::BoxError,
            >,
        > {
            let events = [
                nth_protocol::StreamEvent::TextDelta("found".into()),
                nth_protocol::StreamEvent::Usage(nth_protocol::Usage {
                    input: 120,
                    output: 4,
                    ..nth_protocol::Usage::default()
                }),
            ];
            Box::pin(async move { Ok(Box::pin(futures::stream::iter(events.map(Ok))) as _) })
        }
    }

    #[tokio::test]
    async fn a_finished_subagent_turn_is_counted_in_the_session_and_saved() {
        let mut app = fronted();
        let agents = nth_context::Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        )
        .agents;
        let explore = agents.get("explore").expect("built in");
        let session = Session::new("kimi", "/repo".into()).as_subagent(None);
        let id = app
            .subagents
            .spawn(explore, "find tabs", session, Arc::new(Spends), Vec::new());
        app.prompt_subagent(id, "go".into());
        for _ in 0..50 {
            tokio::task::yield_now().await;
            hear(&mut app);
            if !app
                .session
                .as_ref()
                .expect("idle")
                .usage
                .spends()
                .is_empty()
            {
                break;
            }
        }

        let spends = app.session.as_ref().expect("idle").usage.spends();
        assert_eq!(spends.len(), 1);
        assert_eq!(spends[0].agent.as_deref(), Some("explore"));
        assert_eq!(
            (spends[0].model.as_str(), spends[0].tokens.input),
            ("kimi", 120)
        );
    }

    #[tokio::test]
    async fn quitting_stops_the_subagents_and_tells_the_session() {
        let mut app = fronted();
        let id = spawn(&mut app);
        // The provider errs at once, so the task ends failed rather than
        // interrupted; either way its notice reaches the session.
        app.subagents.prompt(
            id,
            Job {
                text: "go".into(),
                cancel: CancellationToken::new(),
                done: Done::Notify(app.inbox.clone()),
                timeout: None,
            },
        );
        for _ in 0..20 {
            tokio::task::yield_now().await;
            hear(&mut app);
            if app.inbox.has_notices() {
                break;
            }
        }

        app.stop_background_for_exit().await;

        let session = app.session.as_ref().expect("idle");
        let Some(Message::User(notice)) = session.messages.last() else {
            panic!("no notice saved");
        };
        assert!(
            notice.contains("<task id=\"1\" agent=\"explore\""),
            "{notice}"
        );
    }
}
