//! Read-only projection of the logical next request.
//!
//! Token counts use compaction's byte-based heuristic (serialized bytes / 4,
//! plus 2,500 per image), not a tokenizer or provider accounting.

use nth_protocol::{
    Budget, Compaction, Effort, Entry, Message, Mode, Phase, Preview, Tool, ToolDefinition,
    ToolSpec, Usage,
};

use crate::{Inbox, Session, agent_loop::offered_tools, compaction};

impl Session {
    /// Project committed history and offered tools without changing session or inbox.
    /// No unsent prompt or conditional turn reminder is invented.
    pub fn context_preview(&self, tools: &[Box<dyn Tool>], inbox: &Inbox) -> Preview {
        self.context_preview_for(tools, inbox, false)
    }

    /// As `context_preview`, with whether the front-end can approve a plan.
    pub fn context_preview_for(
        &self,
        tools: &[Box<dyn Tool>],
        inbox: &Inbox,
        can_approve: bool,
    ) -> Preview {
        let specs = offered_tools(tools, self.subagent, self.primary())
            .into_iter()
            .map(|(spec, _)| spec)
            .collect::<Vec<_>>();
        let mut pending = inbox.preview(&self.id.to_string());
        if let Some(notice) = self.pending_instructions() {
            pending.insert(0, ("instructions".into(), notice));
        }
        let approver = if can_approve {
            crate::plan::Approver::User
        } else {
            crate::plan::Approver::Nobody
        };
        pending.extend(
            self.next_turn_reminders(approver)
                .into_iter()
                .map(|text| ("next turn reminder".into(), text)),
        );
        build(Projection {
            messages: &self.messages,
            specs: &specs,
            model: &self.model,
            effort: self.effort,
            mode: self.mode,
            agent: self.agent.as_deref(),
            context_window: self.context_window,
            max_output: self.max_output,
            policy: self.env.compaction,
            last_usage: self.last_usage,
            pending,
            phase: Phase::Idle,
            last_step: false,
        })
    }
}

pub(crate) struct Projection<'a> {
    pub messages: &'a [Message],
    pub specs: &'a [ToolSpec],
    pub model: &'a str,
    pub effort: Effort,
    pub mode: Mode,
    pub agent: Option<&'a str>,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    pub policy: Compaction,
    pub last_usage: Option<Usage>,
    pub pending: Vec<(String, String)>,
    pub phase: Phase,
    pub last_step: bool,
}

pub(crate) fn build(input: Projection<'_>) -> Preview {
    let effective = compaction::effective(input.messages);
    let effective = if input.last_step {
        std::borrow::Cow::Owned(crate::agent_loop::max_steps_messages(&effective))
    } else {
        effective
    };
    let checkpoint_index = compaction::last_marker(input.messages).map(|at| {
        input.messages[..at]
            .iter()
            .filter(|m| matches!(m, Message::System(_)))
            .count()
    });
    let mut budget = Budget {
        context_window: input.context_window,
        output_reserve: input.policy.reserved(input.max_output),
        usable: input.policy.usable(input.context_window, input.max_output),
        ..Budget::default()
    };
    let entries = effective
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let images = match message {
                Message::ToolResult { images, .. } => images.as_slice(),
                _ => &[],
            };
            let image_tokens = images.len() as u64 * compaction::IMAGE_TOKENS;
            let tokens = compaction::tokens(message);
            budget.images += image_tokens;
            if matches!(message, Message::System(_)) {
                budget.system += tokens - image_tokens;
            } else {
                budget.conversation += tokens - image_tokens;
            }
            Entry {
                message: message.clone(),
                estimated_tokens: tokens,
                checkpoint: checkpoint_index == Some(index),
                images_available: images.iter().map(|image| image.data.is_some()).collect(),
            }
        })
        .collect();
    let tools = input.specs.iter().map(|spec| {
            let value = serde_json::json!({"name": spec.name, "description": spec.description, "parameters": spec.parameters});
            let estimated_tokens = compaction::text_tokens(&value.to_string());
            budget.tools += estimated_tokens;
            ToolDefinition { name: spec.name.to_string(), description: spec.description.clone(), parameters: spec.parameters.clone(), estimated_tokens }
        }).collect();
    budget.input = budget.system + budget.conversation + budget.tools + budget.images;
    budget.available = budget
        .usable
        .map(|usable| usable.saturating_sub(budget.input));
    let compaction_required = input.policy.auto
        && budget.usable.is_some_and(|usable| {
            usable > 0
                && input
                    .last_usage
                    .is_some_and(|usage| usage.context() >= usable)
        });
    Preview {
        phase: input.phase,
        model: input.model.into(),
        effort: input.effort,
        mode: input.mode,
        agent: input.agent.map(str::to_owned),
        entries,
        tools,
        budget,
        last_usage: input.last_usage,
        compaction_required,
        pending: input.pending,
    }
}

/// Every snapshot the loop builds on this thread. A watch keeps only the
/// newest, and the tests count boundaries; a current-thread test runtime
/// runs the turn and its tools on the test's own thread.
#[cfg(test)]
pub(crate) mod published {
    use std::cell::RefCell;

    use super::Preview;

    thread_local! {
        static BUILT: RefCell<Vec<Preview>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn record(snapshot: &Preview) {
        BUILT.with_borrow_mut(|built| built.push(snapshot.clone()));
    }

    /// What was built since the last call.
    pub(crate) fn take() -> Vec<Preview> {
        BUILT.with_borrow_mut(std::mem::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::tests::Echo;

    #[test]
    fn checkpoint_budget_and_pending_are_read_only() {
        let mut session = Session::new("model", ".".into());
        session.messages.push(Message::User("discard me".into()));
        session.messages.push(Message::Compaction {
            summary: "summary".into(),
            recent: "".into(),
        });
        session.messages.push(Message::User("keep me".into()));
        session.set_limits(Some(100), Some(10));
        session.last_usage = Some(Usage {
            input: 90,
            ..Usage::default()
        });
        let inbox = Inbox::new();
        inbox.post_prompt("steer".into());
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let before = session.messages.clone();
        let preview = session.context_preview(&tools, &inbox);
        assert_eq!(preview, session.context_preview(&tools, &inbox));
        assert_eq!(session.messages, before);
        assert_eq!(inbox.take_prompts().as_deref(), Some("steer"));
        assert_eq!(preview.pending, [("prompt".into(), "steer".into())]);
        assert_eq!(preview.entries.iter().filter(|e| e.checkpoint).count(), 1);
        assert!(!format!("{:?}", preview.entries).contains("discard me"));
        assert!(preview.compaction_required);
        assert_eq!(preview.budget.usable, Some(90));
        assert_eq!(
            preview.budget.input,
            preview.budget.system
                + preview.budget.conversation
                + preview.budget.tools
                + preview.budget.images
        );
        assert_eq!(preview.tools[0].name, "echo");
    }

    /// A front-end that watches the turn's preview, and its receiver.
    fn watching() -> (
        crate::FrontEnd,
        tokio::sync::watch::Receiver<Option<nth_protocol::ContextSnapshot>>,
    ) {
        let front_end = crate::FrontEnd::default();
        let rx = front_end.context_preview.subscribe();
        (front_end, rx)
    }

    /// One step calling `echo`, then an answer.
    fn tool_step() -> crate::agent_loop::tests::Scripted {
        use nth_protocol::StreamEvent;

        use crate::agent_loop::tests::{Scripted, call};

        Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#))],
            vec![StreamEvent::TextDelta("finished".into())],
        ])
    }

    #[tokio::test]
    async fn nothing_builds_without_a_subscriber() {
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::FrontEnd;

        let mut session = Session::new("model", ".".into());
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let front_end = FrontEnd::default();
        // The last turn's, which this one must not leave readable.
        front_end.context_preview.publish(std::sync::Arc::new(
            session.context_preview(&tools, &Inbox::new()),
        ));
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                "go",
                &tool_step(),
                &tools,
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(front_end.context_preview.get().is_none());
        assert!(published::take().is_empty());
    }

    #[tokio::test]
    async fn a_step_publishes_each_boundary_once() {
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        let mut session = Session::new("model", ".".into());
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (front_end, mut rx) = watching();
        let (tx, _events) = mpsc::channel(64);
        session
            .prompt(
                "go",
                &tool_step(),
                &tools,
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let snapshots = published::take();
        assert_eq!(
            snapshots.iter().map(|p| p.phase).collect::<Vec<_>>(),
            [
                Phase::PendingReply,
                Phase::PendingResults,
                Phase::Ready,
                Phase::PendingReply,
                Phase::Idle
            ]
        );
        assert!(
            !snapshots[0]
                .entries
                .iter()
                .any(|entry| matches!(&entry.message, Message::Assistant(_))),
            "the request goes out before the reply is in"
        );
        assert!(snapshots[2].entries.iter().any(
            |entry| matches!(&entry.message, Message::ToolResult { content, .. } if content == "hi")
        ));
        let idle = snapshots.last().unwrap();
        assert!(idle.entries.iter().any(
            |entry| matches!(&entry.message, Message::Assistant(reply) if reply.text == "finished")
        ));
        assert!(rx.has_changed().unwrap(), "the subscriber was told");
        let newest = rx.borrow_and_update().clone().expect("a snapshot");
        assert_eq!(*newest, *idle);
    }

    /// The phases of what was built since the last call.
    fn phases() -> Vec<Phase> {
        published::take().into_iter().map(|p| p.phase).collect()
    }

    #[tokio::test]
    async fn a_pre_empted_step_goes_on_to_the_next_request_without_ready() {
        use nth_protocol::ToolContext;

        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let inbox = Inbox::new();
        let _watching = ctx.context_preview.subscribe();
        // Sent while the first reply streamed: its call never runs.
        inbox.post_prompt("stop that".into());
        let mut messages = vec![Message::User("go".into())];
        let (result, _) =
            crate::agent_loop::tests::turn_with(&tool_step(), &tools, &ctx, &inbox, &mut messages)
                .await;
        result.expect("the turn goes on with the prompt");
        assert_eq!(
            phases(),
            [
                Phase::PendingReply,
                Phase::PendingResults,
                Phase::PendingReply,
                Phase::Idle
            ]
        );
    }

    #[tokio::test]
    async fn asking_for_a_snapshot_opens_the_gate_headless() {
        use futures::FutureExt;
        use nth_protocol::{StreamEvent, ToolContext, ToolResult, ToolSpec};
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::{
            FrontEnd,
            agent_loop::tests::{Scripted, call},
        };

        /// Asks for snapshots as the `context` tool does.
        struct Want;
        impl Tool for Want {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "want",
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn call<'a>(
                &'a self,
                _: serde_json::Value,
                ctx: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, ToolResult> {
                async move {
                    ctx.context_preview.want();
                    Ok("asked".into())
                }
                .boxed()
            }
        }
        let mut session = Session::new("model", ".".into());
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "want", "{}"))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Want)];
        let front_end = FrontEnd::default();
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                "go",
                &provider,
                &tools,
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            phases(),
            [Phase::Ready, Phase::PendingReply, Phase::Idle],
            "nothing before the call asked"
        );
        assert!(
            !front_end.context_preview.builds(),
            "the ask lasts its turn"
        );
        assert!(front_end.context_preview.get().is_some());
    }

    #[tokio::test]
    async fn live_request_snapshots_match_provider_and_tool_sees_pending_results() {
        use futures::FutureExt;
        use nth_protocol::{StreamEvent, ToolContext, ToolResult, ToolSpec};
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::agent_loop::tests::{Scripted, call};

        struct Inspect;
        impl Tool for Inspect {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "inspect",
                    description: "inspect snapshot".into(),
                    parameters: serde_json::json!({"type":"object"}),
                }
            }
            fn call<'a>(
                &'a self,
                _: serde_json::Value,
                ctx: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, ToolResult> {
                async move {
                    let preview = ctx.context_preview.get().ok_or("missing snapshot")?;
                    assert_eq!(preview.phase, Phase::PendingResults);
                    assert!(
                        !preview
                            .entries
                            .iter()
                            .any(|e| matches!(e.message, Message::ToolResult { .. }))
                    );
                    Ok("seen".into())
                }
                .boxed()
            }
        }
        let mut session = Session::new("model", ".".into());
        session.env.max_steps = 2;
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("id", "inspect", "{}"))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Inspect)];
        let (front_end, _watching) = watching();
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                "go",
                &provider,
                &tools,
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let snapshots = published::take();
        let sent = provider.sent.lock().unwrap();
        let requests: Vec<_> = snapshots
            .iter()
            .filter(|p| p.phase == Phase::PendingReply)
            .collect();
        assert_eq!(requests.len(), sent.len());
        for (preview, sent) in requests.iter().zip(sent.iter()) {
            assert_eq!(
                preview
                    .entries
                    .iter()
                    .map(|e| &e.message)
                    .collect::<Vec<_>>(),
                sent.iter().collect::<Vec<_>>()
            );
            assert_eq!(preview.tools[0].name, "inspect");
        }
        assert!(snapshots.iter().any(|p| p.phase == Phase::Ready && p.entries.iter().any(|e| matches!(&e.message, Message::ToolResult { content, .. } if content == "seen"))));
    }

    #[tokio::test]
    async fn pending_instructions_and_reminders_match_next_delivery() {
        use std::sync::Arc;

        use nth_context::{Context, Instruction};
        use nth_protocol::StreamEvent;
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::{FrontEnd, agent_loop::tests::Scripted};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "old instructions").unwrap();
        let context = Context {
            instructions: vec![Instruction {
                path: path.clone(),
                content: "old instructions".into(),
            }],
            ..Context::default()
        };
        let mut session = Session::new("model", dir.path().into()).with_context(Arc::new(context));
        session.mode = Mode::Plan;
        std::fs::write(&path, "new instructions").unwrap();
        let inbox = Inbox::new();
        let preview = session.context_preview_for(&[], &inbox, false);
        assert_eq!(preview, session.context_preview_for(&[], &inbox, false));
        assert!(!dir.path().join(".nth").exists());
        let instruction = &preview
            .pending
            .iter()
            .find(|(kind, _)| kind == "instructions")
            .unwrap()
            .1;
        let reminder = &preview
            .pending
            .iter()
            .find(|(kind, _)| kind == "next turn reminder")
            .unwrap()
            .1;
        let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("done".into())]]);
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
            .unwrap();
        let sent = provider.sent.lock().unwrap();
        let text = sent[0]
            .iter()
            .find_map(|m| match m {
                Message::User(text) => Some(text),
                _ => None,
            })
            .unwrap();
        assert!(text.contains(instruction));
        assert!(text.contains(reminder));
        assert!(
            !session
                .context_preview(&[], &inbox)
                .pending
                .iter()
                .any(|(kind, _)| kind == "instructions" || kind == "next turn reminder")
        );
    }

    #[tokio::test]
    async fn a_live_instruction_change_is_in_the_ready_history_once() {
        use std::sync::Arc;

        use futures::FutureExt;
        use nth_context::{Context, Instruction};
        use nth_protocol::{StreamEvent, ToolContext, ToolResult, ToolSpec};
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::agent_loop::tests::{Scripted, call};

        struct Change(std::path::PathBuf);
        impl Tool for Change {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "change",
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn call<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a ToolContext,
            ) -> futures::future::BoxFuture<'a, ToolResult> {
                async move {
                    std::fs::write(&self.0, "changed live").map_err(|e| e.to_string())?;
                    Ok("updated".into())
                }
                .boxed()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "old instruction").unwrap();
        let context = Context {
            instructions: vec![Instruction {
                path: path.clone(),
                content: "old instruction".into(),
            }],
            ..Context::default()
        };
        let mut session = Session::new("model", dir.path().into()).with_context(Arc::new(context));
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "change", "{}"))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Change(path))];
        let (front_end, _watching) = watching();
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                "go",
                &provider,
                &tools,
                &front_end,
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let snapshots = published::take();
        let ready = snapshots
            .iter()
            .find(|p| p.phase == Phase::Ready)
            .expect("a ready snapshot");
        let delivered = |p: &Preview| {
            p.entries
                .iter()
                .filter(|entry| matches!(&entry.message, Message::User(text) if text.contains("changed live")))
                .count()
        };
        assert_eq!(delivered(ready), 1, "the edit is in the history");
        let sent = provider.sent.lock().unwrap();
        let delivered = sent[1]
            .iter()
            .filter(
                |message| matches!(message, Message::User(text) if text.contains("changed live")),
            )
            .count();
        assert_eq!(delivered, 1);
        assert!(
            !session
                .context_preview(&tools, &Inbox::default())
                .pending
                .iter()
                .any(|(kind, _)| kind == "instructions")
        );
    }
}
