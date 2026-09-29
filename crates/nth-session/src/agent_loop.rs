use futures::StreamExt;
use nth_protocol::{
    AssistantMessage, BoxError, Event, Message, Provider, Request, StreamEvent, Tool, ToolCall,
    ToolContext, ToolResult,
};
use tokio::sync::mpsc;

/// Guards against a model that never stops calling tools.
pub const MAX_STEPS: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Provider(BoxError),
    #[error("stopped after {MAX_STEPS} steps without a final answer")]
    TooManySteps,
}

/// Runs one user turn: stream a reply, run its tool calls in parallel, feed
/// the results back, and repeat until the model answers without tools.
/// Everything the model and tools produce is appended to `messages`.
pub async fn run_turn(
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    messages: &mut Vec<Message>,
    events: &mpsc::Sender<Event>,
) -> Result<(), Error> {
    let specs: Vec<_> = tools.iter().map(|t| t.spec()).collect();
    for _ in 0..MAX_STEPS {
        let request = Request {
            messages,
            tools: &specs,
        };
        let mut stream = provider.stream(request).await.map_err(Error::Provider)?;
        let mut reply = AssistantMessage::default();
        while let Some(event) = stream.next().await {
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
            }
        }

        let calls = reply.tool_calls.clone();
        messages.push(Message::Assistant(reply));
        if calls.is_empty() {
            return Ok(());
        }

        let results =
            futures::future::join_all(calls.iter().map(|call| run_tool(tools, ctx, call, events)))
                .await;
        for (call, content) in calls.into_iter().zip(results) {
            messages.push(Message::ToolResult {
                call_id: call.id,
                content: content.unwrap_or_else(|e| format!("Error: {e}")),
            });
        }
    }
    Err(Error::TooManySteps)
}

async fn run_tool(
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    call: &ToolCall,
    events: &mpsc::Sender<Event>,
) -> ToolResult {
    emit(events, Event::ToolStarted(call.clone())).await;
    let result = match tools.iter().find(|t| t.spec().name == call.name) {
        None => Err(format!("unknown tool: {}", call.name)),
        Some(tool) => match parse_arguments(&call.arguments) {
            Ok(args) => tool.call(args, ctx).await,
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
mod tests {
    use std::sync::Mutex;

    use futures::{
        FutureExt,
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::ToolSpec;

    use super::*;

    /// Replays one scripted reply per request.
    struct Scripted(Mutex<Vec<Vec<StreamEvent>>>);

    impl Provider for Scripted {
        fn model(&self) -> &str {
            "scripted"
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let reply = self.0.lock().expect("not poisoned").remove(0);
            async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
        }
    }

    struct Echo;

    impl Tool for Echo {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo",
                description: "",
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            async move { Ok(args["say"].as_str().unwrap_or_default().to_string()) }.boxed()
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[tokio::test]
    async fn runs_tools_until_the_model_answers() {
        let provider = Scripted(Mutex::new(vec![
            vec![
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                StreamEvent::ToolCall(call("2", "nope", "")),
            ],
            vec![StreamEvent::TextDelta("done".into())],
        ]));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext { cwd: ".".into() };
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        run_turn(&provider, &tools, &ctx, &mut messages, &tx)
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
        while let Some(event) = rx.recv().await {
            if let Event::TextDelta(t) = event {
                texts.push(t);
            }
        }
        assert_eq!(texts, ["done"]);
    }
}
