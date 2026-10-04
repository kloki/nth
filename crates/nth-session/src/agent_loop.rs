use futures::StreamExt;
use nth_protocol::{
    AssistantMessage, BoxError, Effort, Event, Message, OutputSink, Provider, Request, StreamEvent,
    Tool, ToolCall, ToolContext, ToolResult,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Guards against a model that never stops calling tools.
pub const DEFAULT_MAX_STEPS: usize = 100;

/// What the model reads in place of a tool result the user cut short.
const INTERRUPTED: &str = "Error: interrupted by the user";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Provider(BoxError),
    #[error("stopped after {0} steps without a final answer")]
    TooManySteps(usize),
    #[error("interrupted")]
    Interrupted,
}

/// Where a turn's requests go: which model at what effort, on behalf of
/// which session, and how many of them the turn may make.
#[derive(Debug, Clone, Copy)]
pub struct Route<'a> {
    pub model: &'a str,
    pub effort: Effort,
    pub session_id: &'a str,
    pub max_steps: usize,
}

/// Runs one user turn: stream a reply, run its tool calls in parallel, feed
/// the results back, and repeat until the model answers without tools.
/// Everything the model and tools produce is appended to `messages`.
///
/// Cancelling `cancel` ends the turn with [`Error::Interrupted`], leaving
/// `messages` valid to continue from: partial text is kept, and every tool
/// call has a result.
pub async fn run_turn(
    provider: &dyn Provider,
    route: Route<'_>,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    messages: &mut Vec<Message>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let specs: Vec<_> = tools.iter().map(|t| t.spec()).collect();
    for _ in 0..route.max_steps {
        let request = Request {
            model: route.model,
            session_id: route.session_id,
            effort: route.effort,
            messages,
            tools: &specs,
        };
        let mut stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Interrupted),
            stream = provider.stream(request) => stream.map_err(Error::Provider)?,
        };
        let mut reply = AssistantMessage::default();
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // Calls from an unfinished reply never ran, so they are
                    // dropped rather than left without results.
                    reply.tool_calls.clear();
                    if !reply.text.is_empty() || !reply.reasoning.is_empty() {
                        messages.push(Message::Assistant(reply));
                    }
                    return Err(Error::Interrupted);
                }
                event = stream.next() => event,
            };
            let Some(event) = event else { break };
            match event.map_err(Error::Provider)? {
                StreamEvent::TextDelta(text) => {
                    reply.text.push_str(&text);
                    emit(events, Event::TextDelta(text)).await;
                }
                StreamEvent::ReasoningDelta(text) => {
                    reply.reasoning.push_str(&text);
                    emit(events, Event::ReasoningDelta(text)).await;
                }
                StreamEvent::ToolCall(call) => reply.tool_calls.push(call),
                StreamEvent::Usage(usage) => emit(events, Event::Usage(usage)).await,
            }
        }

        let calls = reply.tool_calls.clone();
        messages.push(Message::Assistant(reply));
        if calls.is_empty() {
            return Ok(());
        }

        let running =
            futures::future::join_all(calls.iter().map(|call| run_tool(tools, ctx, call, events)));
        let results = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Dropping the tool futures stops them; bash kills its process group.
                messages.extend(calls.into_iter().map(|call| Message::ToolResult {
                    call_id: call.id,
                    content: INTERRUPTED.to_string(),
                }));
                return Err(Error::Interrupted);
            }
            results = running => results,
        };
        for (call, content) in calls.into_iter().zip(results) {
            messages.push(Message::ToolResult {
                call_id: call.id,
                content: content.unwrap_or_else(|e| format!("Error: {e}")),
            });
        }
    }
    Err(Error::TooManySteps(route.max_steps))
}

async fn run_tool(
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    call: &ToolCall,
    events: &mpsc::Sender<Event>,
) -> ToolResult {
    emit(events, Event::ToolStarted(call.clone())).await;
    // Output goes straight onto the event channel from inside this future,
    // so it is dropped with the call and always lands before ToolFinished.
    let ctx = ToolContext {
        cwd: ctx.cwd.clone(),
        output: OutputSink::new(events.clone(), call.id.clone()),
        instructions: ctx.instructions.clone(),
    };
    let result = match tools.iter().find(|t| t.spec().name == call.name) {
        None => Err(format!("unknown tool: {}", call.name)),
        Some(tool) => match parse_arguments(&call.arguments) {
            Ok(args) => tool.call(args, &ctx).await,
            Err(e) => Err(e),
        },
    };
    emit(
        events,
        Event::ToolFinished {
            call: call.clone(),
            result: result.clone(),
        },
    )
    .await;
    result
}

fn parse_arguments(raw: &str) -> Result<serde_json::Value, String> {
    // Some models send an empty string for a call without arguments.
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|e| format!("arguments are not valid JSON: {e}"))
}

async fn emit(events: &mpsc::Sender<Event>, event: Event) {
    // No listener is fine: the turn still completes and lands in `messages`.
    let _ = events.send(event).await;
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use futures::{
        FutureExt,
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::{ModelInfo, ToolSpec};

    use super::*;

    /// Replays one scripted reply per request and records the model asked for.
    const ROUTE: Route = Route {
        model: "glm-5.3",
        effort: Effort::Default,
        session_id: "s1",
        max_steps: DEFAULT_MAX_STEPS,
    };

    pub(crate) struct Scripted {
        replies: Mutex<Vec<Vec<StreamEvent>>>,
        /// The model and session id of every request, in order.
        routes: Mutex<Vec<(String, String)>>,
    }

    impl Scripted {
        pub(crate) fn new(replies: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                routes: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for Scripted {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            request: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            self.routes
                .lock()
                .expect("not poisoned")
                .push((request.model.to_string(), request.session_id.to_string()));
            let reply = self.replies.lock().expect("not poisoned").remove(0);
            async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
        }
    }

    /// Streams what it is asked to say, then returns it.
    struct Echo;

    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            async move {
                let say = args["say"].as_str().unwrap_or_default().to_string();
                ctx.output.send(say.clone()).await;
                Ok(say)
            }
            .boxed()
        }
    }

    /// Cancels the turn from inside the tool, then never finishes.
    struct Stall(CancellationToken);

    impl Tool for Stall {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "stall",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            self.0.cancel();
            std::future::pending().boxed()
        }
    }

    /// Streams `events` and then stays open, like a reply cut off mid-way.
    struct Unfinished(Vec<StreamEvent>);

    impl Provider for Unfinished {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let events = self.0.clone();
            async move {
                Ok(stream::iter(events.into_iter().map(Ok))
                    .chain(stream::pending())
                    .boxed())
            }
            .boxed()
        }
    }

    pub(crate) fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[tokio::test]
    async fn runs_tools_until_the_model_answers() {
        let provider = Scripted::new(vec![
            vec![
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                StreamEvent::ToolCall(call("2", "nope", "")),
            ],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
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

        assert_eq!(
            messages[2..],
            [
                Message::ToolResult {
                    call_id: "1".into(),
                    content: "hi".into()
                },
                Message::ToolResult {
                    call_id: "2".into(),
                    content: "Error: unknown tool: nope".into()
                },
                Message::Assistant(AssistantMessage {
                    text: "done".into(),
                    ..Default::default()
                }),
            ]
        );
        drop(tx);
        let mut texts = Vec::new();
        let mut echo = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::TextDelta(t) => texts.push(t),
                Event::ToolStarted(c) if c.id == "1" => echo.push("started".to_string()),
                Event::ToolOutput { call_id, text } if call_id == "1" => echo.push(text),
                Event::ToolFinished { call: c, .. } if c.id == "1" => {
                    echo.push("finished".to_string())
                }
                _ => {}
            }
        }
        assert_eq!(texts, ["done"]);
        assert_eq!(
            echo,
            ["started", "hi", "finished"],
            "output between the two"
        );
        assert_eq!(
            *provider.routes.lock().expect("not poisoned"),
            vec![("glm-5.3".to_string(), "s1".to_string()); 2]
        );
    }

    #[tokio::test]
    async fn interrupting_tools_answers_every_call() {
        let provider = Scripted::new(vec![vec![
            StreamEvent::ToolCall(call("1", "stall", "")),
            StreamEvent::ToolCall(call("2", "echo", r#"{"say":"hi"}"#)),
        ]]);
        let cancel = CancellationToken::new();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Stall(cancel.clone())), Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(&provider, ROUTE, &tools, &ctx, &mut messages, &tx, &cancel).await;

        assert!(matches!(result, Err(Error::Interrupted)));
        assert_eq!(
            messages[2..],
            ["1", "2"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: INTERRUPTED.into(),
            })
        );
    }

    #[tokio::test]
    async fn interrupting_a_reply_keeps_its_text_but_not_its_calls() {
        let provider = Unfinished(vec![
            StreamEvent::TextDelta("partial".into()),
            StreamEvent::ToolCall(call("1", "echo", "")),
        ]);
        let cancel = CancellationToken::new();
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let turn = run_turn(&provider, ROUTE, &[], &ctx, &mut messages, &tx, &cancel);
        let interrupt = async {
            rx.recv().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(turn, interrupt);

        assert!(matches!(result, Err(Error::Interrupted)));
        assert_eq!(
            messages[1..],
            [Message::Assistant(AssistantMessage {
                text: "partial".into(),
                ..Default::default()
            })]
        );
    }
}
