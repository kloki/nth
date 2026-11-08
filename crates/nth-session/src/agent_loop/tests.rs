//! What the agent loop's tests share, and the tests of a turn as a whole.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use nth_protocol::{
    AssistantMessage, BoxError, ContextOverflow, Effort, Event, Listing, Message, Provider,
    Request, Retry, StreamEvent, Tool, ToolCall, ToolContext, ToolResult, ToolSpec, failed,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;

pub(crate) const ROUTE: Route = Route {
    model: "glm-5.3",
    effort: Effort::Default,
    session_id: "s1",
    parent_session_id: None,
    max_steps: DEFAULT_MAX_STEPS,
    // The window is the front-ends' to fill; tests that compact set it.
    context_window: None,
    max_output: None,
    compaction: crate::Compaction::DEFAULT,
};

/// How a scripted request fails.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    /// Refused as too long, as a wire reports its provider's overflow.
    Overflow,
    /// A transient failure, retried after [`Scripted::retry_after`].
    Temporary,
    /// A failure no retry helps, as from a provider that is down.
    Down,
}

impl Failure {
    fn error(self) -> BoxError {
        match self {
            Failure::Overflow => Box::new(ContextOverflow),
            Failure::Temporary => Box::new(Temporary),
            Failure::Down => "connection refused".into(),
        }
    }
}

/// A retryable provider failure that is not an overflow.
#[derive(Debug, thiserror::Error)]
#[error("temporary failure")]
pub(crate) struct Temporary;

/// What a [`Scripted`] provider answers one request with.
#[derive(Debug, Clone)]
pub(crate) enum Reply {
    /// Streams these events, then ends.
    Events(Vec<StreamEvent>),
    /// Fails the request before anything streams.
    Refused(Failure),
    /// Streams these events, then fails as a dropped connection does.
    Breaks(Vec<StreamEvent>),
    /// Streams these events, then stays open: a reply that never ends.
    Hangs(Vec<StreamEvent>),
}

/// Answers each request with the next scripted reply and records what it
/// was asked.
pub(crate) struct Scripted {
    pub(crate) replies: Mutex<Vec<Reply>>,
    /// What every request after the script gets; without it, a request
    /// past the script is a test bug and panics.
    pub(crate) then: Option<Reply>,
    /// How long a [`Failure::Temporary`] asks to wait before the retry.
    pub(crate) retry_after: Option<Duration>,
    /// The model and session id of every request, in order.
    pub(crate) routes: Mutex<Vec<(String, String)>>,
    /// The system prompt of every request, in order.
    pub(crate) systems: Mutex<Vec<String>>,
    /// Every request's messages, in order.
    pub(crate) sent: Mutex<Vec<Vec<Message>>>,
    /// The tools every request offered, in order, so a request that must
    /// carry none (a summary) can prove it.
    pub(crate) tools: Mutex<Vec<Vec<String>>>,
}

impl Scripted {
    /// Streams `replies`, one per request.
    pub(crate) fn new(replies: Vec<Vec<StreamEvent>>) -> Self {
        Self::script(replies.into_iter().map(Reply::Events).collect())
    }

    pub(crate) fn script(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies),
            then: None,
            // A retry that waits in a test would only slow it down.
            retry_after: Some(Duration::ZERO),
            routes: Mutex::new(Vec::new()),
            systems: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
        }
    }

    /// Answers every request with `reply`.
    pub(crate) fn always(reply: Reply) -> Self {
        Self {
            then: Some(reply),
            ..Self::script(Vec::new())
        }
    }

    /// A model whose reply never arrives.
    pub(crate) fn stalled() -> Self {
        Self::always(Reply::Hangs(Vec::new()))
    }

    /// How many requests it was sent.
    pub(crate) fn requests(&self) -> usize {
        self.sent.lock().expect("not poisoned").len()
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
        self.tools.lock().expect("not poisoned").push(
            request
                .tools
                .iter()
                .map(|tool| tool.name.to_string())
                .collect(),
        );
        self.sent
            .lock()
            .expect("not poisoned")
            .push(request.messages.to_vec());
        let reply = {
            let mut replies = self.replies.lock().expect("not poisoned");
            match (replies.is_empty(), &self.then) {
                (true, Some(then)) => then.clone(),
                _ => replies.remove(0),
            }
        };
        let ok = |events: Vec<StreamEvent>| events.into_iter().map(Ok);
        async move {
            Ok(match reply {
                Reply::Events(events) => stream::iter(ok(events)).boxed(),
                Reply::Refused(failure) => return Err(failure.error()),
                Reply::Breaks(events) => stream::iter(ok(events))
                    .chain(stream::iter([Err(Failure::Temporary.error())]))
                    .boxed(),
                Reply::Hangs(events) => stream::iter(ok(events)).chain(stream::pending()).boxed(),
            })
        }
        .boxed()
    }

    fn retry(&self, error: &BoxError) -> Option<Retry> {
        error.downcast_ref::<Temporary>().map(|_| Retry {
            after: self.retry_after,
        })
    }
}

/// Streams what it is asked to say, then returns it.
pub(crate) struct Echo;

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

/// A tool that is only its name, for tests of which tools are offered.
pub(crate) struct Named(pub &'static str);

impl Tool for Named {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0,
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    fn call<'a>(
        &'a self,
        _args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async { Ok(String::new()) }.boxed()
    }
}

/// A spend for a turn whose usage the test does not look at.
pub(crate) fn spend() -> Spend {
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

    let mut last_usage = None;
    Turn {
        provider: &provider,
        route: ROUTE,
        tools: &tools,
        ctx: &ctx,
        inbox: &crate::Inbox::default(),
        messages: &mut messages,
        spend: &mut spend(),
        last_usage: &mut last_usage,
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
                content: "hi".into(),
                images: Vec::new(),
            },
            Message::ToolResult {
                call_id: "2".into(),
                content: "Error: unknown tool: nope".into(),
                images: Vec::new(),
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

#[tokio::test]
async fn zero_steps_fails_without_a_provider_request() {
    let provider = Scripted::new(vec![]);
    let ctx = ToolContext::new(".".into());
    let (tx, _rx) = mpsc::channel(16);
    let mut messages = vec![Message::User("go".into())];
    let mut last_usage = None;

    let result = Turn {
        provider: &provider,
        route: Route {
            max_steps: 0,
            ..ROUTE
        },
        tools: &[],
        ctx: &ctx,
        inbox: &crate::Inbox::default(),
        messages: &mut messages,
        spend: &mut spend(),
        last_usage: &mut last_usage,
        events: &tx,
        cancel: &CancellationToken::new(),
    }
    .run()
    .await;

    assert!(matches!(result, Err(Error::TooManySteps(0))));
    assert_eq!(provider.requests(), 0);
    assert_eq!(messages, [Message::User("go".into())]);
}

#[tokio::test]
async fn the_last_step_tells_the_model_to_stop() {
    let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("done".into())]]);
    let ctx = ToolContext::new(".".into());
    let (tx, _rx) = mpsc::channel(16);
    let mut messages = vec![Message::User("go".into())];

    let mut last_usage = None;
    Turn {
        provider: &provider,
        route: Route {
            max_steps: 1,
            ..ROUTE
        },
        tools: &[],
        ctx: &ctx,
        inbox: &crate::Inbox::default(),
        messages: &mut messages,
        spend: &mut spend(),
        last_usage: &mut last_usage,
        events: &tx,
        cancel: &CancellationToken::new(),
    }
    .run()
    .await
    .expect("turn completes");

    let sent = provider.sent.lock().expect("not poisoned")[0].clone();
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

    let mut last_usage = None;
    let result = Turn {
        provider: &provider,
        route: Route {
            max_steps: 1,
            ..ROUTE
        },
        tools: &tools,
        ctx: &ctx,
        inbox: &crate::Inbox::default(),
        messages: &mut messages,
        spend: &mut spend(),
        last_usage: &mut last_usage,
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
            images: Vec::new(),
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

/// Streams its deltas, counting each one the turn takes, then never ends.
struct Counted {
    deltas: [&'static str; 3],
    taken: Arc<AtomicUsize>,
}

impl Provider for Counted {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        _request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        let taken = self.taken.clone();
        let deltas = self
            .deltas
            .map(|text| Ok(StreamEvent::TextDelta(text.into())));
        async move {
            Ok(stream::iter(deltas)
                .inspect(move |_| {
                    taken.fetch_add(1, Ordering::SeqCst);
                })
                .chain(stream::pending())
                .boxed())
        }
        .boxed()
    }
}

#[tokio::test]
async fn a_turn_blocked_on_a_full_event_channel_still_answers_its_cancel() {
    let taken = Arc::new(AtomicUsize::new(0));
    let provider = Counted {
        deltas: ["a", "b", "c"],
        taken: taken.clone(),
    };
    let ctx = ToolContext::new(".".into());
    let mut messages = vec![Message::User("go".into())];
    // Room for one event and nobody reading: a front-end that stopped
    // draining.
    let (tx, _rx) = mpsc::channel(1);
    let cancel = CancellationToken::new();
    let (mut spend, mut last_usage) = (spend(), None);
    let inbox = crate::Inbox::default();
    let run = Turn {
        provider: &provider,
        route: ROUTE,
        tools: &[],
        ctx: &ctx,
        inbox: &inbox,
        messages: &mut messages,
        spend: &mut spend,
        last_usage: &mut last_usage,
        events: &tx,
        cancel: &cancel,
    }
    .run();
    // Both run on this one task, so `stop` only looks while the turn is
    // parked. One slot cannot hold three deltas, so once the turn has taken
    // a delta and the slot is full, it is parked sending one, not opening
    // the stream or waiting for the next event: the cancel lands on the
    // blocked send, however long the provider takes to answer.
    let stop = async {
        while taken.load(Ordering::SeqCst) == 0 || tx.capacity() > 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
    };

    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, stop) })
            .await
            .expect("the turn answered its cancel");

    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
}

/// Runs a turn on [`ROUTE`] with a fresh token and no inbox, and every
/// event it sent.
pub(crate) async fn turn(
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    messages: &mut Vec<Message>,
) -> (Result<(), Error>, Vec<Event>) {
    turn_with(provider, tools, ctx, &crate::Inbox::default(), messages).await
}

/// [`turn`] handing the model what waits in `inbox`.
pub(crate) async fn turn_with(
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    ctx: &ToolContext,
    inbox: &crate::Inbox,
    messages: &mut Vec<Message>,
) -> (Result<(), Error>, Vec<Event>) {
    let (tx, mut rx) = mpsc::channel(16);
    let run = async move {
        let cancel = CancellationToken::new();
        let mut last_usage = None;
        Turn {
            provider,
            route: ROUTE,
            tools,
            ctx,
            inbox,
            messages,
            spend: &mut spend(),
            last_usage: &mut last_usage,
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

#[test]
fn settling_a_dropped_turn_answers_only_the_calls_left_open() {
    let interrupted = |id: &str| Message::ToolResult {
        call_id: id.into(),
        content: failed(nth_protocol::INTERRUPTED),
        images: Vec::new(),
    };
    let answered = Message::ToolResult {
        call_id: "2".into(),
        content: "done".into(),
        images: Vec::new(),
    };
    let reply = Message::Assistant(AssistantMessage {
        tool_calls: ["1", "2", "3"].map(|id| call(id, "echo", "{}")).to_vec(),
        ..AssistantMessage::default()
    });
    let mut messages = vec![Message::User("go".into()), reply.clone(), answered.clone()];

    settle_unanswered(&mut messages);
    assert_eq!(
        messages[1..],
        [reply, answered, interrupted("1"), interrupted("3")]
    );
    let settled = messages.clone();
    settle_unanswered(&mut messages);
    assert_eq!(messages, settled, "nothing is left to answer");

    let mut answer = vec![
        Message::User("hi".into()),
        Message::Assistant(AssistantMessage::default()),
    ];
    settle_unanswered(&mut answer);
    assert_eq!(answer.len(), 2, "an answer has no calls");
}
