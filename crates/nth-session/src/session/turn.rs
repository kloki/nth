use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use nth_protocol::{
    ChildLlm, Event, Llm, Message, Mode, ModelSettings, Phase, Provider, Tool, ToolContext, Workdir,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::Session;
use crate::{
    Error, FrontEnd, Inbox, Route, Spend, Turn,
    agent_loop::emit,
    curios::Receipts,
    inbox::instructions_notice,
    memory,
    plan::{self, Approver},
    primary, subagent,
};

impl Session {
    /// Adds a user message and runs the turn it starts, until it ends or
    /// `cancel` interrupts it. Tools reach you through `front_end`.
    pub async fn prompt(
        &mut self,
        text: impl Into<String>,
        provider: &dyn Provider,
        tools: &[Arc<dyn Tool>],
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let mut text = text.into();
        // Plan mode names the source folder the model may write review
        // documents in, so it is made ready; otherwise Curios waits for its
        // tools.
        if !self.subagent && self.tools_mode() == Mode::Plan {
            self.ready_curios().await;
        }
        // `@explore …` tells the model to delegate to that agent; a subagent
        // has no task tool to do so with.
        if !self.subagent
            && let Some(mention) = subagent::resolve(&text, &self.context.agents, &self.cwd)
        {
            text.push_str(&mention);
        }
        self.prepared_turn(
            TurnInput::Prompt(text),
            provider,
            tools,
            front_end,
            events,
            cancel,
        )
        .await
    }

    /// Shared turn preparation for ordinary prompts and typed review feedback.
    /// Feedback reaches this directly so document text is never parsed as input.
    pub(crate) async fn prepared_turn(
        &mut self,
        input: TurnInput,
        provider: &dyn Provider,
        tools: &[Arc<dyn Tool>],
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        // Feedback arrives already taken, by `deliver_curios`, which readied
        // the state before taking it.
        if matches!(input, TurnInput::Prompt(_)) {
            self.ready_state().await?;
        }
        // Notices lead the message, as `split_notices` reads them; feedback
        // is notices alone, with only the reminders after it.
        let (curios, mut text) = match input {
            TurnInput::Prompt(text) => (self.take_curios(front_end, events, cancel).await, text),
            TurnInput::Feedback(notices) => (Some(notices), String::new()),
        };
        let agent = self.primary().map(|agent| agent.name.clone());
        let approver = match front_end.asker.reaches_someone() {
            true => Approver::User,
            false => Approver::Nobody,
        };
        for reminder in self.next_turn_reminders(approver) {
            append(&mut text, &reminder);
        }
        // An instruction file edited since the last turn, in front of the
        // prompt as the front-end's notices are.
        let changes = self.watch.changes(self.loaded()).await;
        let changed = instructions_notice(&changes);
        if let Some(changed) = &changed {
            emit(events, cancel, Event::Notice(changed.clone())).await;
        }
        let notices = [changed, curios].into_iter().flatten().collect::<Vec<_>>();
        self.last_turn_mode = match agent {
            Some(_) => None,
            None => Some(self.mode),
        };
        self.last_turn_agent = agent;
        self.refresh_system_prompt();
        self.push_user(notices, text);
        self.updated_at = SystemTime::now();
        let held = self.held(front_end);
        let result = self
            .turn(&held, provider, tools, events, cancel)
            .run()
            .await;
        self.usage.close(held.spend);
        let Held { ctx, .. } = held;
        if !self.subagent {
            self.find_curio_root().await;
        }
        // Prompts you sent mid-turn joined it.
        self.updated_at = SystemTime::now();
        self.loaded_instructions = ctx
            .instructions
            .lock()
            .expect("loaded instructions lock poisoned")
            .clone();
        if let Some(cwd) = ctx.workdir.moved_to() {
            self.set_cwd(cwd, ctx.workdir.origin());
        }
        result
    }

    /// Adds what you said, its notices in front, as the next user message.
    /// A turn that failed before the model said anything left the last
    /// prompt unanswered; this joins it rather than following it, its
    /// notices still in front, since some endpoints reject two user
    /// messages in a row.
    pub(super) fn push_user(&mut self, notices: Vec<String>, text: String) {
        match self.messages.last_mut() {
            Some(Message::User(previous)) => {
                *previous = notices
                    .into_iter()
                    .chain([std::mem::take(previous)])
                    .collect::<Vec<_>>()
                    .join("\n");
                append(previous, &text);
            }
            _ => {
                let message = notices.into_iter().chain([text]);
                let message = message.filter(|part| !part.is_empty());
                let message = message.collect::<Vec<_>>().join("\n");
                self.messages.push(Message::User(message));
            }
        }
    }

    /// What a turn of this session needs held for as long as it runs.
    /// Its spend is in the ledger from the start, so a turn dropped part
    /// way keeps what it spent; whoever runs the turn closes it.
    fn held(&mut self, front_end: &FrontEnd) -> Held {
        let mut spend = Spend::new(&self.model, None);
        spend.cost = self.env.cost;
        Held {
            spend: self.usage.open(spend),
            ctx: self.tool_context(front_end),
            inbox: front_end.inbox.clone(),
            session_id: self.id.to_string(),
            parent_session_id: self.parent.map(|p| p.to_string()),
        }
    }

    /// A turn on this session's history and route, with what `held` holds.
    fn turn<'a>(
        &'a mut self,
        held: &'a Held,
        provider: &'a dyn Provider,
        tools: &'a [Arc<dyn Tool>],
        events: &'a mpsc::Sender<Event>,
        cancel: &'a CancellationToken,
    ) -> Turn<'a> {
        Turn {
            provider,
            route: Route {
                model: &self.model,
                effort: self.effort,
                session_id: &held.session_id,
                parent_session_id: held.parent_session_id.as_deref(),
                max_steps: self.env.max_steps,
                // The window and the output limit are the front-end's to
                // fill; without them nothing compacts on its own.
                context_window: self.context_window,
                max_output: self.max_output,
                compaction: self.env.compaction,
            },
            tools,
            ctx: &held.ctx,
            inbox: &held.inbox,
            receipts: (!self.subagent)
                .then(|| Receipts::new(&held.session_id, &mut self.curio_receipts)),
            messages: &mut self.messages,
            spend: self.usage.spend_mut(held.spend),
            last_usage: &mut self.last_usage,
            events,
            cancel,
        }
    }

    /// What the model's tools and the front-end reach this session through.
    pub(super) fn tool_context(&self, front_end: &FrontEnd) -> ToolContext {
        let mut writable = plan::writable(self.tools_mode(), self.plan_path(), self.memory_dir());
        if let nth_protocol::Writable::Plan { source, .. } = &mut writable {
            *source = self.plan_source_dir();
        }
        ToolContext {
            is_subagent: self.subagent,
            extra_dirs: self.extra_dirs.clone(),
            instructions: Arc::new(Mutex::new(self.loaded())),
            watch: self.watch.clone(),
            context: self.context.clone(),
            asker: front_end.asker.clone(),
            screen: front_end.screen.clone(),
            monitors: front_end.monitors.clone(),
            context_preview: front_end.context_preview.clone(),
            writable,
            agent: self.primary().cloned(),
            child: ChildLlm {
                own: ModelSettings {
                    llm: Llm {
                        model: self.model.clone(),
                        effort: self.effort,
                    },
                    context_window: self.context_window,
                    max_output: self.max_output,
                    takes_images: self.env.takes_images,
                    cost: self.env.cost,
                },
                subagent: self.env.subagent.clone(),
                clerk: self.env.clerk.clone(),
                compaction: self.env.compaction,
            },
            workdir: Workdir::new(self.origin.clone()),
            state_root: Some(self.state_root()),
            session: self.id.to_string(),
            takes_images: self.env.takes_images,
            images: self.env.images.clone(),
            tool_timeout: self.env.tool_timeout,
            ..ToolContext::new(self.cwd.clone())
        }
    }

    /// Makes the folders the model may write this turn: the plan's in plan
    /// mode, and the memory folder with the `.gitignore` that keeps what it
    /// saves out of git, so a turn that cannot have it never starts. Called
    /// before Curios feedback is taken, so a failure cannot record a receipt
    /// without adding the notice to the conversation.
    pub(crate) async fn ready_state(&self) -> Result<(), Error> {
        if self.tools_mode() == Mode::Plan {
            plan::ensure_dir(&self.state_root())
                .await
                .map_err(Error::State)?;
        }
        if let Some(dir) = self.memory_dir() {
            memory::ensure_dir(&dir, &self.cwd)
                .await
                .map_err(Error::State)?;
        }
        Ok(())
    }

    /// The memory folder this session keeps, when it keeps one: a subagent
    /// works for another model, which keeps it.
    fn memory_dir(&self) -> Option<PathBuf> {
        let memory = self.context.memory.as_ref().filter(|_| !self.subagent)?;
        Some(memory.dir.clone())
    }

    /// Conditional additions to the next submitted message. Reads the plan's
    /// existence without preparing directories or changing the session.
    pub(crate) fn next_turn_reminders(&self, approver: Approver) -> Vec<String> {
        let agent = self.primary().map(|agent| agent.name.as_str());
        let first = !self.messages.iter().any(|m| matches!(m, Message::User(_)));
        let mut reminders = Vec::new();
        if let Some(reminder) = primary::reminder(agent, self.last_turn_agent.as_deref(), first) {
            reminders.push(reminder);
        }
        if agent.is_none()
            && let Some(reminder) = plan::reminder(
                self.mode,
                self.last_turn_mode,
                &self.plan_path(),
                self.memory_dir().as_deref(),
                approver,
            )
        {
            reminders.push(reminder);
        }
        if self.tools_mode() == Mode::Plan
            && let Some(source) = self.plan_source_dir()
        {
            reminders.push(format!(
                "<system-reminder>In plan mode you may also author review documents only in {} and use curio_publish and curio_reply. This grants no other project writes; private Curios state is changed only through domain tools.</system-reminder>",
                source.display()
            ));
        }
        reminders
    }

    pub(crate) fn pending_instructions(&self) -> Option<String> {
        instructions_notice(&self.watch.preview(self.loaded()))
    }

    /// The instruction files in the conversation: the system prompt's, so
    /// read never repeats them, and those tools attached.
    fn loaded(&self) -> BTreeSet<PathBuf> {
        let mut loaded = self.loaded_instructions.clone();
        loaded.extend(self.context.instructions.iter().map(|i| i.path.clone()));
        loaded
    }

    /// Runs one compaction outside a turn: everything the requests would
    /// not keep verbatim becomes a summary they start from. The `/compact`
    /// command and the loop's overflow recovery share `Turn::compact`;
    /// this is the manual entry.
    pub async fn compact(
        &mut self,
        provider: &dyn Provider,
        tools: &[Arc<dyn Tool>],
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let held = self.held(front_end);
        // Only `Turn::run` writes the trigger's usage: a compaction's is the
        // summary request's, not the conversation's size.
        // As a turn does: what the slot holds from here on is this one's.
        front_end.context_preview.reset();
        let mut turn = self.turn(&held, provider, tools, events, cancel);
        let result = turn.compact().await;
        // In a turn the next request shows the summary; here nothing follows.
        let usage = match result {
            Ok(true) => None,
            _ => *turn.last_usage,
        };
        turn.publish_preview(Phase::Idle, false, usage);
        if matches!(result, Ok(true)) {
            // The conversation the old usage described is gone; keeping it
            // would fire the trigger again at once.
            self.last_usage = None;
        }
        self.usage.close(held.spend);
        self.updated_at = SystemTime::now();
        result.map(|_| ())
    }
}

/// Typed feedback keeps its own history entry after an unanswered prompt.
pub(crate) enum TurnInput {
    Prompt(String),
    /// Curio notices, already recorded.
    Feedback(String),
}

/// Puts `part` after `text` as a paragraph of its own.
fn append(text: &mut String, part: &str) {
    if !part.is_empty() && !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(part);
}

/// What a turn borrows that the session does not hold.
struct Held {
    ctx: ToolContext,
    inbox: Inbox,
    /// Where the turn's spend is in the ledger.
    spend: usize,
    session_id: String,
    parent_session_id: Option<String>,
}

#[cfg(test)]
mod tests {

    use nth_context::Context;
    use nth_protocol::{AssistantMessage, StreamEvent, Usage};

    use super::*;
    use crate::{session::tests::send, system_prompt};

    /// Marks `/repo/sub/AGENTS.md` loaded, as read does when it attaches it.
    struct Claim;

    impl Tool for Claim {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "claim",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            let mut loaded = ctx.instructions.lock().expect("not poisoned");
            let fresh = loaded.insert("/repo/sub/AGENTS.md".into());
            Box::pin(async move { Ok(fresh.to_string()) })
        }
    }

    #[tokio::test]
    async fn remembers_the_instruction_files_tools_attached() {
        use crate::agent_loop::tests::{Scripted, call};

        let context = Arc::new(Context {
            instructions: vec![nth_context::Instruction {
                path: "/repo/AGENTS.md".into(),
                content: "root".into(),
            }],
            ..Context::default()
        });
        let mut session = Session::new("glm-5.3", "/repo".into()).with_context(context);
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "claim", ""))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Claim)];
        let (tx, _rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        assert_eq!(
            session.loaded_instructions,
            BTreeSet::from(["/repo/AGENTS.md".into(), "/repo/sub/AGENTS.md".into()])
        );
        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.loaded_instructions, session.loaded_instructions);
    }

    /// Moves the session into `/repo/.nth/worktrees/x`, as enter_worktree
    /// does, or says where it runs when asked with `"pwd"`.
    struct Move;

    impl Tool for Move {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "move",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            if args != "pwd" {
                ctx.workdir.enter(&ctx.cwd, "/repo/.nth/worktrees/x".into());
            }
            let cwd = ctx.cwd.display().to_string();
            Box::pin(async move { Ok(cwd) })
        }
    }

    #[tokio::test]
    async fn a_tool_moves_the_next_step_and_the_session() {
        use crate::agent_loop::tests::{Scripted, call};

        let mut session = Session::new("glm-5.3", "/repo".into());
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "move", "{}"))],
            vec![StreamEvent::ToolCall(call("2", "move", r#""pwd""#))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Move)];
        let (tx, mut rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        let results: Vec<&str> = session
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ["/repo", "/repo/.nth/worktrees/x"]);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Moved(_) | Event::ToolFinished { .. } => events.push(event),
                _ => {}
            }
        }
        // Only the call that moved says so, before it finishes.
        assert!(matches!(
            &events[..],
            [Event::Moved(cwd), Event::ToolFinished { .. }, Event::ToolFinished { .. }]
                if cwd == std::path::Path::new("/repo/.nth/worktrees/x")
        ));
        assert_eq!(session.cwd, PathBuf::from("/repo/.nth/worktrees/x"));
        assert_eq!(session.origin, Some("/repo".into()));
        let Some(Message::System(prompt)) = session.messages.first() else {
            panic!("no system prompt");
        };
        assert!(prompt.contains("/repo/.nth/worktrees/x"), "{prompt}");
        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.origin, session.origin);
    }

    #[tokio::test]
    async fn an_unwritable_memory_dir_fails_the_turn_with_why() {
        use crate::agent_loop::tests::Scripted;

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join(".git")).expect("repository");
        // A file where the folder goes fails as a read-only disk would, and
        // still does for a test run as root.
        std::fs::write(tmp.path().join(".nth"), "").expect("blocks the folder");
        let dir = tmp.path().join(".nth/memory");
        let context = Arc::new(Context {
            memory: Some(nth_context::Memory {
                dir: dir.clone(),
                index: String::new(),
                truncated: false,
            }),
            ..Context::default()
        });
        let mut session =
            Session::new("glm-5.3", tmp.path().to_path_buf()).with_context(context.clone());
        let front_end = FrontEnd {
            inbox: crate::Inbox::new(),
            ..FrontEnd::default()
        };
        let notice = crate::curios::CurioNotice {
            batch: nth_curios::Batch {
                id: "waiting".into(),
                session_id: session.id.to_string(),
                curio_id: "review".into(),
                revision: 1,
                messages: vec![],
                recorded: false,
            },
        };
        assert!(front_end.inbox.post_curio(notice.clone()));
        let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
        let (tx, _rx) = mpsc::channel(64);

        let error = session
            .prompt(
                "go",
                &provider,
                &[],
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect_err("the turn cannot keep memory out of git");

        assert!(matches!(error, Error::State(_)), "{error:?}");
        let why = error.to_string();
        assert!(why.contains(&dir.display().to_string()), "{why}");
        assert!(why.contains("[memory]"), "{why}");
        assert!(provider.sent.lock().unwrap().is_empty(), "nothing was sent");
        assert!(
            !session
                .messages
                .iter()
                .any(|m| matches!(m, Message::User(_))),
            "the prompt is not in the history"
        );
        // The feedback waits for a turn that can take it.
        assert!(session.curio_receipts.is_empty());
        assert_eq!(
            front_end.inbox.take_curios(&session.id.to_string()).len(),
            1
        );

        assert!(front_end.inbox.post_curio(notice));
        let error = session
            .deliver_curios(&provider, &[], &front_end, &tx, &CancellationToken::new())
            .await
            .expect_err("feedback fails the same way");
        assert!(matches!(error, Error::State(_)), "{error:?}");
        assert!(session.curio_receipts.is_empty());

        // A subagent never writes the memory, so it is not held up by it.
        let mut child = Session::new("glm-5.3", tmp.path().to_path_buf()).with_context(context);
        child.subagent = true;
        assert!(child.ready_state().await.is_ok());
    }

    fn with_agents_md(dir: &std::path::Path, text: &str) -> Session {
        std::fs::write(dir.join("AGENTS.md"), text).expect("writes");
        let context = Context::discover(dir, &nth_context::Paths::default());
        Session::new("glm-5.3", dir.to_path_buf()).with_context(Arc::new(context))
    }

    /// Delivers typed feedback and returns the messages captured on the wire.
    async fn deliver_feedback(session: &mut Session, id: &str) -> Vec<Message> {
        use crate::agent_loop::tests::{Echo, Named, Scripted};

        let front_end = FrontEnd {
            inbox: crate::Inbox::new(),
            ..FrontEnd::default()
        };
        assert!(front_end.inbox.post_curio(crate::curios::CurioNotice {
            batch: nth_curios::Batch {
                id: id.into(),
                session_id: session.id.to_string(),
                curio_id: "review".into(),
                revision: 1,
                messages: vec![],
                recorded: false,
            },
        }));
        let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Echo), Arc::new(Named("bash"))];
        let (tx, _rx) = mpsc::channel(64);
        session
            .deliver_curios(
                &provider,
                &tools,
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("feedback completes");
        if session.primary().is_some() {
            assert_eq!(
                *provider.tools.lock().unwrap(),
                [vec!["echo".to_string()]],
                "primary agent tool permissions apply to feedback"
            );
        }
        provider.sent.into_inner().unwrap().pop().unwrap()
    }

    fn last_user(messages: &[Message]) -> &str {
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(text) => Some(text.as_str()),
                _ => None,
            })
            .expect("user message")
    }

    #[tokio::test]
    async fn curio_feedback_prepares_mode_transitions_and_refreshes_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = with_agents_md(dir.path(), "old instructions");
        send(&mut session, "first act prompt").await;
        session.mode = Mode::Plan;
        std::fs::write(dir.path().join("AGENTS.md"), "updated instructions").unwrap();

        let captured = deliver_feedback(&mut session, "plan-feedback").await;
        let text = last_user(&captured);
        assert!(text.starts_with("<instructions "), "{text}");
        assert!(text.contains("updated instructions"), "{text}");
        assert!(text.contains("<curio delivery="), "{text}");
        assert!(text.contains("Plan mode is active"), "{text}");
        assert!(dir.path().join(".nth/.gitignore").exists());
        assert!(
            session
                .tool_context(&FrontEnd::default())
                .writable
                .planning()
        );
        assert_eq!(session.last_turn_mode, Some(Mode::Plan));

        let next = send(&mut session, "continue planning").await;
        assert!(
            !next.contains("<instructions "),
            "instructions delivered once"
        );
        session.mode = Mode::Act;
        let captured = deliver_feedback(&mut session, "act-feedback").await;
        let text = last_user(&captured);
        assert!(
            text.contains("operational mode has changed from plan to act"),
            "{text}"
        );
        assert_eq!(session.last_turn_mode, Some(Mode::Act));
    }

    #[tokio::test]
    async fn curio_feedback_prepares_primary_entry_and_exit() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            Session::new("fake", dir.path().to_path_buf()).with_context(with_reviewer(dir.path()));
        send(&mut session, "ordinary act prompt").await;
        session.mode = Mode::Plan;
        session.set_agent(Some("review".into()));

        let captured = deliver_feedback(&mut session, "primary-feedback").await;
        let text = last_user(&captured);
        assert!(text.contains("now running as the review agent"), "{text}");
        assert!(!text.contains("Plan mode"), "{text}");
        assert!(matches!(&captured[0], Message::System(text) if text.starts_with("You review.\n")));
        assert!(
            !session
                .tool_context(&FrontEnd::default())
                .writable
                .planning()
        );
        assert_eq!(session.last_turn_mode, None);
        assert_eq!(session.last_turn_agent.as_deref(), Some("review"));
        assert!(
            !send(&mut session, "continue review")
                .await
                .contains("now running")
        );

        session.set_agent(None);
        let captured = deliver_feedback(&mut session, "exit-feedback").await;
        let text = last_user(&captured);
        assert!(
            text.contains("no longer running as the review agent"),
            "{text}"
        );
        assert!(text.contains("Plan mode is active"), "{text}");
        assert_eq!(session.last_turn_agent, None);
        assert_eq!(session.last_turn_mode, Some(Mode::Plan));
    }

    #[tokio::test]
    async fn an_instruction_file_edited_between_turns_comes_before_the_next_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = with_agents_md(dir.path(), "old");
        assert_eq!(send(&mut session, "first").await, "first");

        std::fs::write(dir.path().join("AGENTS.md"), "new").expect("writes");
        let path = dir.path().join("AGENTS.md").display().to_string();
        assert_eq!(
            send(&mut session, "second").await,
            format!(
                "<instructions path=\"{path}\" change=\"edited\" content=\"whole\">\nnew\n</instructions>\nsecond"
            )
        );
        assert_eq!(send(&mut session, "third").await, "third", "told once");
    }

    /// Rewrites the working directory's `AGENTS.md`, as the model would.
    struct Rewrite;

    impl Tool for Rewrite {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "rewrite",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            let path = ctx.cwd.join("AGENTS.md");
            Box::pin(async move {
                tokio::fs::write(path, "rewritten")
                    .await
                    .map_err(|e| e.to_string())?;
                Ok("written".into())
            })
        }
    }

    #[tokio::test]
    async fn an_instruction_file_a_tool_edits_reaches_the_next_step() {
        use crate::agent_loop::tests::{Scripted, call};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = with_agents_md(dir.path(), "original");
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "rewrite", ""))],
            vec![StreamEvent::TextDelta("noted".into())],
        ]);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Rewrite)];
        let (tx, mut rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        let Some(Message::User(after)) = session.messages.get(4) else {
            panic!("a message after the result: {:?}", session.messages);
        };
        assert!(
            after.starts_with("<instructions ") && after.contains("change=\"edited\""),
            "{after}"
        );
        assert!(after.contains("rewritten"), "{after}");
        drop(tx);
        let mut shown = false;
        while let Some(event) = rx.recv().await {
            shown |= matches!(event, Event::Notice(text) if text == *after);
        }
        assert!(shown, "the front-end hears it");
    }

    #[tokio::test]
    async fn a_session_keeps_its_memory_out_of_git_and_writable_while_planning() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("dir");
        let paths = nth_context::Paths {
            memory: true,
            ..nth_context::Paths::default()
        };
        let context = Arc::new(Context::discover(dir.path(), &paths));
        let mut session =
            Session::new("glm-5.3", dir.path().to_path_buf()).with_context(context.clone());
        session.mode = Mode::Plan;

        send(&mut session, "plan it").await;

        let memory = dir.path().join(".nth/memory");
        assert!(memory.join(".gitignore").is_file());
        let ctx = session.tool_context(&FrontEnd::default());
        assert_eq!(ctx.writable.check(&memory.join("terse.md")), Ok(()));
        assert!(ctx.writable.check(&dir.path().join("src/main.rs")).is_err());

        let subagent = Session::new("glm-5.3", dir.path().to_path_buf())
            .with_context(context)
            .as_subagent(None);
        assert_eq!(subagent.memory_dir(), None, "the parent keeps it");
    }

    /// A project with the primary agent `review`, which may only read and
    /// echo.
    fn with_reviewer(dir: &std::path::Path) -> Arc<Context> {
        std::fs::create_dir(dir.join(".git")).expect("dir");
        let agents = dir.join(".nth/agents");
        std::fs::create_dir_all(&agents).expect("dir");
        std::fs::write(
            agents.join("review.md"),
            "---\ndescription: Reviews\nmode: primary\ntools: read, echo\n---\nYou review.\n",
        )
        .expect("writes");
        Arc::new(Context::discover(dir, &nth_context::Paths::default()))
    }

    #[tokio::test]
    async fn a_primary_agent_runs_in_place_of_the_mode() {
        use crate::agent_loop::tests::{Echo, Named, Scripted};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new("glm-5.3", dir.path().to_path_buf())
            .with_context(with_reviewer(dir.path()));
        session.mode = Mode::Plan;
        session.set_agent(Some("review".into()));

        let Message::System(system) = &session.messages[0] else {
            panic!("a system prompt first");
        };
        assert!(system.starts_with("You review.\n"), "{system}");
        let ctx = session.tool_context(&FrontEnd::default());
        assert!(!ctx.writable.planning(), "the agent acts");
        assert_eq!(ctx.writable.check(&dir.path().join("src/main.rs")), Ok(()));
        assert_eq!(ctx.agent.as_ref().map(|a| a.name.as_str()), Some("review"));

        let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Echo), Arc::new(Named("bash"))];
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                "look",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");
        assert_eq!(
            *provider.tools.lock().expect("not poisoned"),
            [vec!["echo".to_string()]],
            "only the tools the agent allows"
        );
        assert_eq!(
            session.messages[1],
            Message::User("look".into()),
            "no reminder on the first turn, and no plan mode"
        );

        session.set_agent(None);
        let back = send(&mut session, "plan now").await;
        assert!(
            back.contains("no longer running as the review agent"),
            "{back}"
        );
        assert!(back.contains("Plan mode is active"), "{back}");

        session.set_agent(Some("review".into()));
        let again = send(&mut session, "review again").await;
        assert!(again.contains("now running as the review agent"), "{again}");
        assert!(!again.contains("Plan mode"), "{again}");
    }

    #[tokio::test]
    async fn a_prompt_after_a_failed_turn_joins_the_unanswered_one() {
        use crate::agent_loop::tests::{Failure, Reply, Scripted};

        let mut session = Session::new("glm-5.3", "/repo".into());
        let (tx, _rx) = mpsc::channel(64);

        let failed = session
            .prompt(
                "first",
                &Scripted::always(Reply::Refused(Failure::Down)),
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await;

        assert!(matches!(failed, Err(Error::Provider(_))));
        assert_eq!(session.messages[1..], [Message::User("first".into())]);

        // One user message, as endpoints that reject two in a row expect.
        assert_eq!(send(&mut session, "second").await, "first\n\nsecond");
        assert!(matches!(
            session.messages[1..],
            [Message::User(_), Message::Assistant(_)]
        ));
        assert_eq!(session.title().as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn a_prompt_rewrites_a_system_prompt_from_another_day() {
        let mut session = Session::new("glm-5.3", "/repo".into());
        let current = session.messages[0].clone();
        let stale = system_prompt(
            None,
            session.id,
            "glm-5.3",
            "/repo".as_ref(),
            &[],
            &Context::default(),
        )
        .replace("Today's date: ", "Today's date: Mon Jan 01 2001, not ");
        session.messages[0] = Message::System(stale);

        send(&mut session, "go").await;

        assert_eq!(session.messages[0], current);
    }

    #[tokio::test]
    async fn a_turn_records_the_usage_it_reported() {
        use crate::agent_loop::tests::Scripted;

        let mut session = Session::new("glm-5.3", "/repo".into());
        let provider = Scripted::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Usage(Usage {
                input: 100,
                output: 10,
                ..Usage::default()
            }),
        ]]);
        let (tx, _rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        assert_eq!(
            session.last_usage,
            Some(Usage {
                input: 100,
                output: 10,
                ..Usage::default()
            })
        );
    }

    #[tokio::test]
    async fn a_manual_compaction_summarizes_and_records_its_spend() {
        use crate::agent_loop::tests::Scripted;

        let mut session = Session::new("glm-5.3", "/repo".into());
        session.messages.push(Message::User("go".into()));
        session.messages.push(Message::Assistant(AssistantMessage {
            text: "done".into(),
            ..AssistantMessage::default()
        }));
        let provider = Scripted::new(vec![vec![
            StreamEvent::TextDelta("the summary".into()),
            StreamEvent::Usage(Usage {
                input: 50,
                output: 5,
                ..Usage::default()
            }),
        ]]);
        let (tx, _rx) = mpsc::channel(64);

        session
            .compact(
                &provider,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("compacts");

        assert_eq!(
            session.messages.last(),
            Some(&Message::Compaction {
                summary: "the summary".into(),
                recent: "[Assistant]: done".into(),
            })
        );
        assert_eq!(
            session.last_usage, None,
            "the summary request's own size is not the conversation's, and \
             what was compacted is gone"
        );
        assert_eq!(session.usage.total().steps, 1, "the spend is still counted");
    }
}
