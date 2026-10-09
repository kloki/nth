//! What you send while a turn works reaches the model at its next step:
//! a prompt sent while the reply streamed pre-empts the reply's tool
//! calls, and what monitors said and you sent while the tools ran
//! follows their results.

use nth_protocol::{Event, Message, ToolCall, ToolContext};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{INTERRUPTED, emit, failed};

/// Appended to a prompt that pre-empted a reply's tool calls, so the model
/// knows why they did not run and may run them itself. Appended, never
/// alone, so the transcript shows only what you typed.
const INTERRUPTED_REMINDER: &str = "\n\n<system-reminder>\nThe user interrupted before the tool calls above ran; they were skipped and their results are placeholders. Run any you still need yourself.\n</system-reminder>";

/// When a prompt waits, answers every one of `calls` as interrupted
/// without running it, and gives the model the prompt: `true` means the
/// turn goes on with it rather than with the calls.
pub(super) async fn preempt(
    calls: &[ToolCall],
    ctx: &ToolContext,
    messages: &mut Vec<Message>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    let Some(prompts) = ctx.inbox.take_prompts() else {
        return false;
    };
    for call in calls {
        emit(events, Event::ToolStarted(call.clone())).await;
        emit(
            events,
            Event::ToolFinished {
                call: call.clone(),
                result: Err(INTERRUPTED.to_string()),
            },
        )
        .await;
    }
    messages.extend(calls.iter().map(|call| Message::ToolResult {
        call_id: call.id.clone(),
        content: failed(INTERRUPTED),
    }));
    // The reminder hidden from the transcript, as the mention is.
    let text = steered(&prompts, ctx) + INTERRUPTED_REMINDER;
    messages.push(Message::User(text));
    emit(events, Event::Notice(prompts)).await;
    true
}

/// What monitors said and you sent while the tools ran, so a model busy on
/// a long turn hears it at its next step rather than when the turn ends:
/// the notices first, so the transcript reads them back, then the prompts,
/// as a turn's own prompt follows its notices.
pub(super) async fn hand_over(
    ctx: &ToolContext,
    messages: &mut Vec<Message>,
    events: &mpsc::Sender<Event>,
) {
    let notices = ctx.inbox.take_notices();
    let prompts = ctx.inbox.take_prompts();
    let shown = [notices.clone(), prompts.clone()].into_iter().flatten();
    let shown = shown.collect::<Vec<_>>().join("\n");
    let sent = [notices, prompts.map(|prompts| steered(&prompts, ctx))];
    let sent = sent.into_iter().flatten().collect::<Vec<_>>().join("\n");
    if !sent.is_empty() {
        messages.push(Message::User(sent));
        emit(events, Event::Notice(shown)).await;
    }
}

/// Prompts sent mid-turn as `Session::prompt` would send them: the agents
/// they mention told of. What the transcript shows is `prompts` alone.
fn steered(prompts: &str, ctx: &ToolContext) -> String {
    let mention = crate::subagent::resolve(prompts, &ctx.context.agents, &ctx.cwd);
    format!("{prompts}{}", mention.unwrap_or_default())
}

#[cfg(test)]
mod tests {

    use futures::{FutureExt, future::BoxFuture};
    use nth_protocol::{
        AssistantMessage, Event, Inbox, Message, MonitorEvent, Monitors, Stream, StreamEvent, Tool,
        ToolContext, ToolResult, ToolSpec,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{run_turn, tests::*};

    /// Starts a monitor that says one line straight away.
    struct Watch;

    impl Tool for Watch {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "watch",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            async move {
                let m = ctx
                    .monitors
                    .register("ci", "watch")
                    .await
                    .ok_or("headless")?;
                let line = MonitorEvent::Output {
                    id: m.id,
                    line: "build failed".into(),
                    stream: Stream::Stdout,
                };
                ctx.monitors.event(line).await;
                Ok("started".into())
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn monitor_notices_reach_the_model_at_its_next_step() {
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "watch", ""))],
            vec![StreamEvent::TextDelta("on it".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Watch)];
        let (front_end, _monitor_events) = mpsc::channel(16);
        let inbox = Inbox::new();
        let ctx = ToolContext {
            monitors: Monitors::new(front_end, "/logs".into(), inbox.clone()),
            inbox,
            ..ToolContext::new(".".into())
        };
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        run_turn(
            &provider,
            ROUTE,
            &tools,
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("turn completes");

        let notice =
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild failed\n</monitor>";
        assert_eq!(
            messages[3],
            Message::User(notice.into()),
            "after the result"
        );
        assert!(matches!(messages[4], Message::Assistant(_)));
        drop(tx);
        let mut shown = false;
        while let Some(event) = rx.recv().await {
            shown |= event == Event::Notice(notice.into());
        }
        assert!(shown);
    }

    #[tokio::test]
    async fn a_prompt_preempts_the_replys_tool_calls() {
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#))],
            vec![StreamEvent::TextDelta("changed plans".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let inbox = Inbox::new();
        let ctx = ToolContext {
            inbox: inbox.clone(),
            ..ToolContext::new(".".into())
        };
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];
        assert!(inbox.post_prompt("never mind, just wave".into()));

        run_turn(
            &provider,
            ROUTE,
            &tools,
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("turn completes");

        assert_eq!(
            messages[1..],
            [
                Message::Assistant(AssistantMessage {
                    tool_calls: vec![call("1", "echo", r#"{"say":"hi"}"#)],
                    ..AssistantMessage::default()
                }),
                Message::ToolResult {
                    call_id: "1".into(),
                    content: failed(INTERRUPTED),
                },
                Message::User(format!("never mind, just wave{INTERRUPTED_REMINDER}")),
                Message::Assistant(AssistantMessage {
                    text: "changed plans".into(),
                    ..AssistantMessage::default()
                }),
            ],
            "the call is answered as interrupted and the model hears the prompt"
        );
        assert_eq!(
            *provider.routes.lock().expect("not poisoned"),
            vec![("glm-5.3".to_string(), "s1".to_string()); 2]
        );
        // What a front-end sees live: the call start and fail as an Esc'd
        // one does, then the prompt as it reached the model.
        drop(tx);
        let interrupted = Err::<String, String>(INTERRUPTED.to_string());
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::ToolStarted(c) => seen.push(format!("started {}", c.id)),
                Event::ToolFinished { call, result } => {
                    seen.push(format!("finished {} {result:?}", call.id))
                }
                Event::Notice(text) => seen.push(format!("notice {text}")),
                _ => {}
            }
        }
        assert_eq!(
            seen,
            [
                "started 1".to_string(),
                format!("finished 1 {interrupted:?}"),
                "notice never mind, just wave".to_string(),
            ]
        );
    }

    /// Posts a prompt from inside the tool, as you would while it runs.
    struct Shout(&'static str);

    impl Tool for Shout {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "shout",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            async move {
                ctx.inbox.post_prompt(self.0.into());
                Ok("done".into())
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn a_prompt_sent_while_tools_run_follows_their_results() {
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "shout", ""))],
            vec![StreamEvent::TextDelta("ok".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Shout("meanwhile"))];
        let ctx = ToolContext {
            inbox: Inbox::new(),
            ..ToolContext::new(".".into())
        };
        let mut messages = vec![Message::User("go".into())];

        turn(&provider, &tools, &ctx, &mut messages)
            .await
            .0
            .expect("turn completes");

        assert_eq!(
            messages[3],
            Message::User("meanwhile".into()),
            "plain, through the notices: the calls above it ran"
        );
    }

    #[tokio::test]
    async fn a_prompt_sent_while_tools_run_resolves_the_agents_it_mentions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = nth_context::Context::discover(dir.path(), &nth_context::Paths::default());
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "shout", ""))],
            vec![StreamEvent::TextDelta("ok".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Shout("@explore find the tabs"))];
        let ctx = ToolContext {
            context: std::sync::Arc::new(context),
            inbox: Inbox::new(),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        run_turn(
            &provider,
            ROUTE,
            &tools,
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("turn completes");

        let Message::User(sent) = &messages[3] else {
            panic!("the prompt followed the results: {:?}", messages[3]);
        };
        assert!(sent.starts_with("@explore find the tabs"), "{sent}");
        assert!(
            sent.contains("call the task tool with subagent: explore"),
            "{sent}"
        );
        drop(tx);
        let mut shown = None;
        while let Some(event) = rx.recv().await {
            if let Event::Notice(text) = event {
                shown = Some(text);
            }
        }
        assert_eq!(
            shown.as_deref(),
            Some("@explore find the tabs"),
            "the chat shows what you typed"
        );
    }

    #[tokio::test]
    async fn a_prompt_preempting_calls_resolves_the_agents_it_mentions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = nth_context::Context::discover(dir.path(), &nth_context::Paths::default());
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let inbox = Inbox::new();
        assert!(inbox.post_prompt("@explore find the tabs".into()));
        let ctx = ToolContext {
            context: std::sync::Arc::new(context),
            inbox,
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let mut messages = vec![Message::User("go".into())];

        turn(&provider, &tools, &ctx, &mut messages)
            .await
            .0
            .expect("turn completes");

        let Message::User(steered) = &messages[3] else {
            panic!("the prompt was steered in: {:?}", messages[3]);
        };
        assert!(
            steered.starts_with("@explore find the tabs\n\n<system-reminder>\n"),
            "{steered}"
        );
        assert!(
            steered.contains("call the task tool with subagent: explore"),
            "{steered}"
        );
    }
}
