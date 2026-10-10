//! What the agent loop's tests share, and the tests of a turn as a whole.

use std::sync::Mutex;

use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use nth_protocol::{
    AssistantMessage, BoxError, Effort, Event, Listing, Message, Provider, Request, StreamEvent,
    Tool, ToolCall, ToolContext, ToolResult, ToolSpec,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;

pub(super) const ROUTE: Route = Route {
    model: "glm-5.3",
    effort: Effort::Default,
    session_id: "s1",
    max_steps: DEFAULT_MAX_STEPS,
};

/// Replays one scripted reply per request and records the model asked for.
pub(crate) struct Scripted {
    pub(super) replies: Mutex<Vec<Vec<StreamEvent>>>,
    /// The model and session id of every request, in order.
    pub(super) routes: Mutex<Vec<(String, String)>>,
    /// The system prompt of every request, in order.
    pub(crate) systems: Mutex<Vec<String>>,
}

impl Scripted {
    pub(crate) fn new(replies: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            replies: Mutex::new(replies),
            routes: Mutex::new(Vec::new()),
            systems: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for Scripted {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        self.routes
            .lock()
            .expect("not poisoned")
            .push((request.model.to_string(), request.session_id.to_string()));
        if let Some(Message::System(system)) = request.messages.first() {
            self.systems
                .lock()
                .expect("not poisoned")
                .push(system.clone());
        }
        let reply = self.replies.lock().expect("not poisoned").remove(0);
        async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
    }
}

/// Streams what it is asked to say, then returns it.
pub(super) struct Echo;

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

/// A spend for a turn whose usage the test does not look at.
pub(super) fn spend() -> Spend {
    Spend::new("scripted", None)
}

pub(crate) fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

#[tokio::test]
async fn a_reply_with_nothing_in_it_fails_the_turn() {
    let provider = Scripted::new(vec![vec![]]);
    let ctx = ToolContext::new(".".into());
    let mut messages = vec![Message::User("go".into())];

    let (result, _) = turn(&provider, &[], &ctx, &mut messages).await;

    assert!(matches!(result, Err(Error::EmptyReply)));
    assert_eq!(
        messages,
        [Message::User("go".into())],
        "no empty assistant message in the history"
    );
}

#[tokio::test]
async fn a_reasoning_only_reply_is_kept_but_is_no_answer() {
    let provider = Scripted::new(vec![vec![StreamEvent::ReasoningDelta("hmm".into())]]);
    let ctx = ToolContext::new(".".into());
    let mut messages = vec![Message::User("go".into())];

    let (result, _) = turn(&provider, &[], &ctx, &mut messages).await;

    assert!(matches!(result, Err(Error::EmptyReply)));
    assert_eq!(
        messages[1],
        Message::Assistant(AssistantMessage {
            reasoning: "hmm".into(),
            ..AssistantMessage::default()
        })
    );
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

    Turn {
        provider: &provider,
        route: ROUTE,
        tools: &tools,
        ctx: &ctx,
        messages: &mut messages,
        spend: &mut spend(),
        events: &tx,
        cancel: &CancellationToken::new(),
    }
    .run()
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
            Event::ToolFinished { call: c, .. } if c.id == "1" => echo.push("finished".to_string()),
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

/// Answers with one text reply and remembers the request it was sent.
struct Recorder(Mutex<Vec<Message>>);

impl Provider for Recorder {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        *self.0.lock().expect("not poisoned") = request.messages.to_vec();
        async { Ok(stream::iter([Ok(StreamEvent::TextDelta("done".into()))]).boxed()) }.boxed()
    }
}

#[tokio::test]
async fn the_last_step_tells_the_model_to_stop() {
    let provider = Recorder(Mutex::new(Vec::new()));
    let ctx = ToolContext::new(".".into());
    let (tx, _rx) = mpsc::channel(16);
    let mut messages = vec![Message::User("go".into())];

    Turn {
        provider: &provider,
        route: Route {
            max_steps: 1,
            ..ROUTE
        },
        tools: &[],
        ctx: &ctx,
        messages: &mut messages,
        spend: &mut spend(),
        events: &tx,
        cancel: &CancellationToken::new(),
    }
    .run()
    .await
    .expect("turn completes");

    let sent = provider.0.lock().expect("not poisoned").clone();
    assert_eq!(
        sent.last(),
        Some(&Message::Assistant(AssistantMessage {
            text: MAX_STEPS_PROMPT.into(),
            ..Default::default()
        })),
        "the prompt is the last thing the model reads"
    );
    assert_eq!(
        messages[1..],
        [Message::Assistant(AssistantMessage {
            text: "done".into(),
            ..Default::default()
        })],
        "and it is never saved into the history"
    );
}

#[tokio::test]
async fn tools_called_on_the_last_step_do_not_run() {
    let provider = Scripted::new(vec![vec![StreamEvent::ToolCall(call(
        "1",
        "echo",
        r#"{"say":"hi"}"#,
    ))]]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
    let ctx = ToolContext::new(".".into());
    let (tx, mut rx) = mpsc::channel(16);
    let mut messages = vec![Message::User("go".into())];

    let result = Turn {
        provider: &provider,
        route: Route {
            max_steps: 1,
            ..ROUTE
        },
        tools: &tools,
        ctx: &ctx,
        messages: &mut messages,
        spend: &mut spend(),
        events: &tx,
        cancel: &CancellationToken::new(),
    }
    .run()
    .await;

    assert!(matches!(result, Err(Error::TooManySteps(1))));
    assert_eq!(
        messages.last(),
        Some(&Message::ToolResult {
            call_id: "1".into(),
            content: failed(MAX_STEPS_REACHED),
        })
    );
    drop(tx);
    while let Some(event) = rx.recv().await {
        assert!(
            !matches!(event, Event::ToolStarted(_)),
            "the tool never started"
        );
    }
}

/// Runs a turn on [`ROUTE`] with a fresh token, and every event it sent.
pub(super) async fn turn(
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    messages: &mut Vec<Message>,
) -> (Result<(), Error>, Vec<Event>) {
    let (tx, mut rx) = mpsc::channel(16);
    let run = async move {
        let cancel = CancellationToken::new();
        Turn {
            provider,
            route: ROUTE,
            tools,
            ctx,
            messages,
            spend: &mut spend(),
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
    };
    let sent = async {
        let mut sent = Vec::new();
        while let Some(event) = rx.recv().await {
            sent.push(event);
        }
        sent
    };
    tokio::join!(run, sent)
}
