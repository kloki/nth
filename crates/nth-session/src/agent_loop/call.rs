//! Running one tool call: its arguments parsed, its output streamed, and
//! its result or why it failed.

use nth_protocol::{Event, OutputSink, Tool, ToolCall, ToolContext, ToolResult};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::emit;

/// Why a tool the user cut short failed; the model reads it after `Error: `.
pub(crate) const INTERRUPTED: &str = "interrupted by the user";

/// A tool's arguments as the struct it expects, or what was wrong with
/// them for the model.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Value,
) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

/// A tool result's content when the tool failed for `reason`: the shape
/// front-ends read the reason back out of.
pub(crate) fn failed(reason: &str) -> String {
    format!("Error: {reason}")
}

/// Runs `call` on `tool`, announcing it with [`Event::ToolStarted`] and
/// [`Event::ToolFinished`] and streaming its output in between. No `tool`
/// means the model named one that does not exist. Cancelling `cancel` stops
/// the tool and fails the call with [`INTERRUPTED`], finished event
/// included, so a front-end sees every started call end.
pub(crate) async fn run_call(
    tool: Option<&dyn Tool>,
    ctx: &ToolContext,
    call: &ToolCall,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> ToolResult {
    emit(events, Event::ToolStarted(call.clone())).await;
    // Output goes straight onto the event channel from inside this future,
    // so it is dropped with the call and always lands before ToolFinished.
    // A tool in an earlier step may have moved the session.
    let ctx = ToolContext {
        cwd: ctx.workdir.moved_to().unwrap_or_else(|| ctx.cwd.clone()),
        workdir: ctx.workdir.clone(),
        extra_dirs: ctx.extra_dirs.clone(),
        output: OutputSink::new(events.clone(), call.id.clone()),
        instructions: ctx.instructions.clone(),
        context: ctx.context.clone(),
        asker: ctx.asker.for_call(call.id.clone()),
        screen: ctx.screen.clone(),
        monitors: ctx.monitors.clone(),
        inbox: ctx.inbox.clone(),
        writable: ctx.writable.clone(),
        llm: ctx.llm.clone(),
    };
    let run = async {
        match tool {
            None => Err(format!("unknown tool: {}", call.name)),
            Some(tool) => match parse_arguments(&call.arguments) {
                Ok(args) => tool.call(args, &ctx).await,
                Err(e) => Err(e),
            },
        }
    };
    let result = tokio::select! {
        biased;
        // Dropping the tool's future stops it; bash kills its process group.
        _ = cancel.cancelled() => Err(INTERRUPTED.to_string()),
        result = run => result,
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

#[cfg(test)]
mod tests {

    use futures::{FutureExt, future::BoxFuture};
    use nth_protocol::{
        Answer, Asker, Event, Message, Question, Reply, StreamEvent, Tool, ToolContext, ToolResult,
        ToolSpec,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{Error, run_turn, tests::*};

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

    /// Asks one question and returns the first label you picked.
    struct Pick;

    impl Tool for Pick {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "pick",
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
                let question = Question {
                    question: "Which?".into(),
                    header: "Which".into(),
                    multiple: false,
                    options: Vec::new(),
                };
                match ctx.asker.ask(vec![question]).await {
                    Some(Reply::Answered(answers)) => Ok(answers[0].picked[0].clone()),
                    _ => Err("no answer".into()),
                }
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn tools_ask_under_their_own_call_id() {
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("7", "pick", ""))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Pick)];
        let (asks, mut asked) = mpsc::channel(1);
        let ctx = ToolContext {
            asker: Asker::new(asks),
            ..ToolContext::new(".".into())
        };
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];
        let answering = tokio::spawn(async move {
            let ask = asked.recv().await.expect("asks");
            let answer = Answer {
                picked: vec![format!("for {}", ask.call_id)],
                typed: None,
            };
            ask.reply
                .send(Reply::Answered(vec![answer]))
                .expect("tool waits");
        });

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
        answering.await.expect("answers");

        assert_eq!(
            messages[2],
            Message::ToolResult {
                call_id: "7".into(),
                content: "for 7".into()
            }
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
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(&provider, ROUTE, &tools, &ctx, &mut messages, &tx, &cancel).await;

        assert!(matches!(result, Err(Error::Interrupted)));
        assert_eq!(
            messages[2..],
            ["1", "2"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: failed(INTERRUPTED),
            })
        );
        // A front-end that only sees events, like `nth run`, sees each call
        // end too, with the same result the history has.
        drop(tx);
        let mut seen = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::ToolStarted(call) => seen.push(format!("started {}", call.id)),
                Event::ToolFinished { call, result } => {
                    seen.push(format!("finished {} {result:?}", call.id))
                }
                _ => {}
            }
        }
        // The calls run in parallel, so only the set of endings is fixed.
        seen.sort();
        let interrupted = Err::<String, _>(INTERRUPTED.to_string());
        assert_eq!(
            seen,
            [
                format!("finished 1 {interrupted:?}"),
                format!("finished 2 {interrupted:?}"),
                "started 1".to_string(),
                "started 2".to_string(),
            ]
        );
    }
}
