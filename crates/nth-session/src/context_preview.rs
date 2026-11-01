//! Read-only projection of the logical next request.
//!
//! Token counts use compaction's byte-based heuristic (serialized bytes / 4,
//! plus 2,500 per image), not a tokenizer or provider accounting.

use nth_protocol::{Compaction, Effort, Inbox, Message, Mode, Tool, ToolSpec, Usage};
use serde::{Deserialize, Serialize};

use crate::{Session, agent_loop::offered_tools, compaction};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub message: Message,
    pub estimated_tokens: u64,
    pub checkpoint: bool,
    /// Availability reflects whether bytes are loaded, not whether the path exists.
    pub images_available: Vec<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub system: u64,
    pub conversation: u64,
    pub tools: u64,
    pub images: u64,
    pub input: u64,
    pub context_window: Option<u64>,
    pub output_reserve: u64,
    pub usable: Option<u64>,
    pub available: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    /// Idle, pending_reply, or pending_results. Partial work is not committed.
    pub phase: String,
    pub model: String,
    pub effort: Effort,
    pub mode: Mode,
    pub agent: Option<String>,
    pub entries: Vec<Entry>,
    pub tools: Vec<ToolDefinition>,
    pub budget: Budget,
    pub last_usage: Option<Usage>,
    pub compaction_required: bool,
    /// Submitted but not yet delivered items, in inbox order.
    pub pending: Vec<(String, String)>,
}

impl Preview {
    /// Plain-text document shared by terminal consumers. Images are described
    /// through metadata only; their encoded bytes never appear in the text.
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Next-request preview · {} · {}",
            self.model,
            self.agent.as_deref().unwrap_or(match self.mode {
                Mode::Plan => "plan",
                Mode::Act => "act",
            })
        );
        let _ = writeln!(
            out,
            "Estimated input ~{} / {} context",
            self.budget.input,
            self.budget
                .context_window
                .map_or_else(|| "unknown".into(), |n| n.to_string())
        );
        let _ = writeln!(
            out,
            "System ~{} · Conversation ~{} · Tools ~{} · Images ~{}",
            self.budget.system, self.budget.conversation, self.budget.tools, self.budget.images
        );
        let _ = writeln!(
            out,
            "Output reserve {} · usable {} · available {}",
            self.budget.output_reserve,
            self.budget
                .usable
                .map_or_else(|| "unknown".into(), |n| n.to_string()),
            self.budget
                .available
                .map_or_else(|| "unknown".into(), |n| n.to_string())
        );
        let _ = writeln!(out, "State: {}", self.phase);
        if self.compaction_required {
            let _ = writeln!(out, "Compaction required before next request");
        }
        if let Some(usage) = self.last_usage {
            let _ = writeln!(
                out,
                "Last reported input: {} · cached: {}",
                usage.input,
                usage
                    .cache_read
                    .map_or_else(|| "unknown".into(), |n| n.to_string())
            );
        }
        for (kind, text) in &self.pending {
            let _ = writeln!(out, "\nPending {kind}\n{text}");
        }
        for entry in &self.entries {
            let label = if entry.checkpoint {
                "Checkpoint"
            } else {
                match &entry.message {
                    Message::System(_) => "System prompt",
                    Message::User(_) => "User",
                    Message::Assistant(_) => "Assistant",
                    Message::ToolResult { .. } => "Tool result",
                    Message::Compaction { .. } => "Compaction",
                }
            };
            let _ = writeln!(out, "\n{label} · ~{} tokens", entry.estimated_tokens);
            match &entry.message {
                Message::System(text) | Message::User(text) => {
                    let _ = writeln!(out, "{text}");
                }
                Message::Assistant(reply) => {
                    if !reply.reasoning.is_empty() {
                        let _ = writeln!(out, "Reasoning:\n{}", reply.reasoning);
                    }
                    let _ = writeln!(out, "{}", reply.text);
                    for call in &reply.tool_calls {
                        let _ = writeln!(
                            out,
                            "Tool call {} · {}\n{}",
                            call.name, call.id, call.arguments
                        );
                    }
                }
                Message::ToolResult {
                    call_id,
                    content,
                    images,
                } => {
                    let _ = writeln!(out, "Call {call_id}\n{content}");
                    for (image, available) in images.iter().zip(&entry.images_available) {
                        let _ = writeln!(
                            out,
                            "Image {} · {} · {} · ~{} tokens",
                            image.path.display(),
                            image.mime,
                            if *available {
                                "available"
                            } else {
                                "unavailable"
                            },
                            compaction::IMAGE_TOKENS
                        );
                    }
                }
                Message::Compaction { summary, recent } => {
                    let _ = writeln!(out, "{summary}\n{recent}");
                }
            }
        }
        let _ = writeln!(out, "\nTool definitions · {} tools", self.tools.len());
        for tool in &self.tools {
            let _ = writeln!(
                out,
                "\n{} · ~{} tokens\n{}\n{}",
                tool.name, tool.estimated_tokens, tool.description, tool.parameters
            );
        }
        out
    }
}

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
            policy: self.compaction,
            last_usage: self.last_usage,
            pending,
            phase: "idle",
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
    pub phase: &'a str,
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
        phase: input.phase.into(),
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

    #[tokio::test]
    async fn live_snapshot_tracks_committed_reply() {
        use nth_protocol::{Event, FrontEnd, StreamEvent};
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::agent_loop::tests::Scripted;

        let mut session = Session::new("model", ".".into());
        let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("finished".into())]]);
        let (tx, mut rx) = mpsc::channel(64);
        session
            .prompt(
                "hello",
                &provider,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut snapshots = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::ContextUpdated(value) = event {
                snapshots.push(serde_json::from_value::<Preview>((*value).clone()).unwrap());
            }
        }
        assert_eq!(snapshots.first().unwrap().phase, "pending_reply");
        assert!(
            !snapshots
                .first()
                .unwrap()
                .entries
                .iter()
                .any(|entry| matches!(&entry.message, Message::Assistant(_)))
        );
        let last = snapshots.last().unwrap();
        assert_eq!(last.phase, "idle");
        assert!(last.entries.iter().any(
            |entry| matches!(&entry.message, Message::Assistant(reply) if reply.text == "finished")
        ));
    }

    #[tokio::test]
    async fn live_request_snapshots_match_provider_and_tool_sees_pending_results() {
        use futures::FutureExt;
        use nth_protocol::{Event, FrontEnd, StreamEvent, ToolContext, ToolResult, ToolSpec};
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
                    let snapshot = ctx.context_preview.get().ok_or("missing snapshot")?;
                    let preview: Preview =
                        serde_json::from_value((*snapshot).clone()).map_err(|e| e.to_string())?;
                    assert_eq!(preview.phase, "pending_results");
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
        session.max_steps = 2;
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("id", "inspect", "{}"))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Inspect)];
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
            .unwrap();
        let mut snapshots = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::ContextUpdated(value) = event {
                snapshots.push(serde_json::from_value::<Preview>((*value).clone()).unwrap());
            }
        }
        let sent = provider.sent.lock().unwrap();
        let requests: Vec<_> = snapshots
            .iter()
            .filter(|p| p.phase == "pending_reply")
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
        assert!(snapshots.iter().any(|p| p.phase == "ready" && p.entries.iter().any(|e| matches!(&e.message, Message::ToolResult { content, .. } if content == "seen"))));
    }

    #[tokio::test]
    async fn pending_instructions_and_reminders_match_next_delivery() {
        use std::sync::Arc;

        use nth_context::{Context, Instruction};
        use nth_protocol::{FrontEnd, StreamEvent};
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::agent_loop::tests::Scripted;

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
    async fn live_instruction_change_is_pending_then_delivered_once() {
        use std::sync::Arc;

        use futures::FutureExt;
        use nth_context::{Context, Instruction};
        use nth_protocol::{Event, FrontEnd, StreamEvent, ToolContext, ToolResult, ToolSpec};
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
            .unwrap();
        let mut snapshots = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let Event::ContextUpdated(value) = event {
                snapshots.push(serde_json::from_value::<Preview>((*value).clone()).unwrap());
            }
        }
        let pending = snapshots
            .iter()
            .find(|p| {
                p.pending
                    .iter()
                    .any(|(kind, text)| kind == "instructions" && text.contains("changed live"))
            })
            .expect("edit is pending before handover");
        assert!(pending.entries.iter().any(|entry| matches!(&entry.message, Message::ToolResult { content, .. } if content == "updated")));
        let last = snapshots.last().unwrap();
        assert!(!last.pending.iter().any(|(kind, _)| kind == "instructions"));
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
