use std::time::Duration;

use futures::StreamExt;
use nth_protocol::{
    AssistantMessage, BoxError, Effort, Event, Message, OutputSink, Provider, Question,
    QuestionOption, Reply, Request, StreamEvent, Tool, ToolCall, ToolContext, ToolResult,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Guards against a model that never stops calling tools.
pub const DEFAULT_MAX_STEPS: usize = 100;

/// The first retry waits this long; each next one doubles it.
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);
/// The longest computed backoff, for when the server asked to wait no
/// particular time.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
/// How many times one step's request may be retried before giving up.
const RETRY_MAX_ATTEMPTS: u32 = 5;
/// A server asking to wait longer than this is not retried: the error shows
/// right away rather than an unexplained stall.
const RETRY_MAX_AFTER: Duration = Duration::from_secs(60);

/// How many times the same call in a row trips the doom-loop guard.
const DOOM_LOOP_THRESHOLD: usize = 3;
/// The doom-loop question's answers.
const CONTINUE: &str = "Run it";
const STOP: &str = "Stop";

/// What the model reads in place of a tool result the user cut short.
const INTERRUPTED: &str = "Error: interrupted by the user";
/// What the model reads for calls the user stopped at the doom-loop prompt.
const STOPPED: &str = "Error: stopped by the user (the same call kept repeating)";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Provider(BoxError),
    #[error("stopped after {0} steps without a final answer")]
    TooManySteps(usize),
    #[error("interrupted")]
    Interrupted,
    #[error(
        "stopped: the model called {0} with the same input {DOOM_LOOP_THRESHOLD} times in a row"
    )]
    DoomLoop(String),
    /// A skill run as `/name` could not be filled in, so the turn never
    /// started.
    #[error("could not run the skill: {0}")]
    Skill(String),
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
    // Where this turn's messages start, so the doom-loop guard counts only
    // calls made since the user last spoke.
    let start = messages.len();
    for _ in 0..route.max_steps {
        let request = Request {
            model: route.model,
            session_id: route.session_id,
            effort: route.effort,
            messages,
            tools: &specs,
        };
        let reply = match stream_step(provider, request, events, cancel).await {
            Ok(reply) => reply,
            Err(Stop::Cancelled(partial)) => {
                if !partial.text.is_empty() || !partial.reasoning.is_empty() {
                    messages.push(Message::Assistant(partial));
                }
                return Err(Error::Interrupted);
            }
            Err(Stop::Failed(error)) => return Err(Error::Provider(error)),
        };

        let calls = reply.tool_calls.clone();
        messages.push(Message::Assistant(reply));
        if calls.is_empty() {
            return Ok(());
        }

        // The same call three times in a row is a model stuck in a loop;
        // ask before running it.
        if let Some(call) = repeating(&messages[start..]) {
            let stopped = tokio::select! {
                biased;
                _ = cancel.cancelled() => Some((INTERRUPTED, Error::Interrupted)),
                run = confirm_doom_loop(ctx, call) => {
                    (!run).then(|| (STOPPED, Error::DoomLoop(call.name.clone())))
                }
            };
            if let Some((content, error)) = stopped {
                messages.extend(calls.iter().map(|call| Message::ToolResult {
                    call_id: call.id.clone(),
                    content: content.to_string(),
                }));
                return Err(error);
            }
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
        // What monitors said while the tools ran, so a model busy on a long
        // turn hears it at its next step rather than when the turn ends.
        if let Some(notices) = ctx.monitors.take_notices() {
            messages.push(Message::User(notices.clone()));
            emit(events, Event::Notice(notices)).await;
        }
    }
    Err(Error::TooManySteps(route.max_steps))
}

/// Why a step's reply did not finish.
enum Stop {
    /// The provider failed; whether a retry can help is the provider's say.
    Failed(BoxError),
    /// Cancelled; the partial reply is handed back so it can be kept.
    Cancelled(AssistantMessage),
}

/// One step's request, retried on a transient provider error with backoff:
/// 2 s, doubling, at most `RETRY_MAX_DELAY`, or the server's `Retry-After`
/// when it gave one no longer than `RETRY_MAX_AFTER`. The retry is announced
/// as an [`Event::Retry`], and cancelling during the wait ends the step.
async fn stream_step(
    provider: &dyn Provider,
    request: Request<'_>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, Stop> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match one_attempt(provider, request, events, cancel).await {
            Err(Stop::Failed(error)) => error,
            done => return done,
        };
        let delay = provider
            .retry(&error)
            .filter(|_| attempt <= RETRY_MAX_ATTEMPTS)
            .map(|retry| retry.after.unwrap_or_else(|| backoff(attempt)))
            .filter(|delay| *delay <= RETRY_MAX_AFTER);
        let Some(delay) = delay else {
            return Err(Stop::Failed(error));
        };
        emit(events, Event::Retry { attempt, delay }).await;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Stop::Cancelled(AssistantMessage::default())),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Streams one reply, appending text, reasoning and tool calls to it as they
/// arrive.
async fn one_attempt(
    provider: &dyn Provider,
    request: Request<'_>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, Stop> {
    let mut stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Stop::Cancelled(AssistantMessage::default())),
        stream = provider.stream(request) => stream.map_err(Stop::Failed)?,
    };
    let mut reply = AssistantMessage::default();
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Calls from an unfinished reply never ran, so they are
                // dropped rather than left without results.
                reply.tool_calls.clear();
                return Err(Stop::Cancelled(reply));
            }
            event = stream.next() => event,
        };
        let Some(event) = event else { break };
        match event.map_err(Stop::Failed)? {
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
    Ok(reply)
}

/// The backoff before the `attempt`-th retry: `RETRY_INITIAL_DELAY` doubled
/// each time, capped at `RETRY_MAX_DELAY`.
fn backoff(attempt: u32) -> Duration {
    let factor = 1u32 << (attempt.saturating_sub(1)).min(16);
    (RETRY_INITIAL_DELAY * factor).min(RETRY_MAX_DELAY)
}

/// The first of the latest reply's calls that completes a run of identical
/// calls (same name and arguments) `DOOM_LOOP_THRESHOLD` long, counted over
/// `turn`. Only whole multiples of the threshold count, so after a "Run it"
/// the question comes back after another full run, not on every repeat.
fn repeating(turn: &[Message]) -> Option<&ToolCall> {
    let replies = turn.iter().filter_map(|message| match message {
        Message::Assistant(reply) => Some(reply),
        _ => None,
    });
    let latest = replies.clone().next_back()?.tool_calls.len();
    let calls: Vec<&ToolCall> = replies.flat_map(|reply| &reply.tool_calls).collect();
    let first_new = calls.len() - latest;
    let mut run = 0;
    for (i, call) in calls.iter().enumerate() {
        let same =
            i > 0 && (&calls[i - 1].name, &calls[i - 1].arguments) == (&call.name, &call.arguments);
        run = if same { run + 1 } else { 1 };
        if i >= first_new && run % DOOM_LOOP_THRESHOLD == 0 {
            return Some(call);
        }
    }
    None
}

/// Asks whether to run a call that keeps repeating. `true` means run it; a
/// front-end with nobody to ask (as in a headless run) lets it through, and
/// a decline stops the turn.
async fn confirm_doom_loop(ctx: &ToolContext, call: &ToolCall) -> bool {
    if !ctx.asker.reaches_someone() {
        return true;
    }
    let question = Question {
        question: format!(
            "The model called {} with the same input {} times in a row. Run it again?",
            call.name, DOOM_LOOP_THRESHOLD
        ),
        header: "Doom loop".into(),
        multiple: false,
        options: [CONTINUE, STOP]
            .map(|label| QuestionOption {
                label: label.into(),
                description: None,
                preview: None,
            })
            .into(),
    };
    match ctx
        .asker
        .for_call(call.id.clone())
        .ask(vec![question])
        .await
    {
        Some(Reply::Answered(answers)) => !answers
            .first()
            .is_some_and(|answer| answer.picked.iter().any(|picked| picked == STOP)),
        // Declined, or nobody answered: stop.
        _ => false,
    }
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
        context: ctx.context.clone(),
        asker: ctx.asker.for_call(call.id.clone()),
        screen: ctx.screen.clone(),
        monitors: ctx.monitors.clone(),
        writable: ctx.writable.clone(),
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
    use nth_protocol::{
        Answer, Asker, ModelInfo, MonitorEvent, Monitors, Question, Reply, Retry, Stream, ToolSpec,
    };

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
        let ctx = ToolContext {
            monitors: Monitors::new(front_end, "/logs".into()),
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

    /// A transient provider failure, retried by [`Flaky`].
    #[derive(Debug, thiserror::Error)]
    #[error("temporary failure")]
    struct Temporary;

    /// Fails the requests named in `fail` (1-based) and answers the rest.
    struct Flaky {
        fail: Vec<usize>,
        attempts: Mutex<usize>,
        reply: Vec<StreamEvent>,
        retry_after: Option<Duration>,
    }

    impl Flaky {
        fn new(fail: Vec<usize>) -> Self {
            Self {
                fail,
                attempts: Mutex::new(0),
                // A retry that waits in a test would only slow it down.
                reply: vec![StreamEvent::TextDelta("done".into())],
                retry_after: Some(Duration::ZERO),
            }
        }
    }

    impl Provider for Flaky {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let mut attempts = self.attempts.lock().expect("not poisoned");
            *attempts += 1;
            let fails = self.fail.contains(&*attempts);
            let reply = self.reply.clone();
            async move {
                match fails {
                    true => Err(Box::new(Temporary) as BoxError),
                    false => Ok(stream::iter(reply.into_iter().map(Ok)).boxed()),
                }
            }
            .boxed()
        }

        fn retry(&self, error: &BoxError) -> Option<Retry> {
            error.downcast_ref::<Temporary>().map(|_| Retry {
                after: self.retry_after,
            })
        }
    }

    #[tokio::test]
    async fn retries_a_transient_error_then_answers() {
        let provider = Flaky::new(vec![1]);
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        run_turn(
            &provider,
            ROUTE,
            &[],
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("turn completes");

        assert_eq!(
            messages[1..],
            [Message::Assistant(AssistantMessage {
                text: "done".into(),
                ..Default::default()
            })]
        );
        assert_eq!(*provider.attempts.lock().expect("not poisoned"), 2);
        drop(tx);
        let mut retries = Vec::new();
        while let Some(event) = rx.recv().await {
            if let Event::Retry { attempt, delay } = event {
                retries.push((attempt, delay));
            }
        }
        assert_eq!(retries, [(1, Duration::ZERO)]);
    }

    #[tokio::test]
    async fn gives_up_after_the_retry_budget() {
        let provider = Flaky::new((1..=20).collect());
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(
            &provider,
            ROUTE,
            &[],
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(Error::Provider(_))));
        assert_eq!(
            *provider.attempts.lock().expect("not poisoned"),
            RETRY_MAX_ATTEMPTS as usize + 1,
            "the first try plus the retries"
        );
        drop(tx);
        let mut retries = 0;
        while let Some(event) = rx.recv().await {
            retries += usize::from(matches!(event, Event::Retry { .. }));
        }
        assert_eq!(retries, RETRY_MAX_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn a_long_retry_after_fails_at_once() {
        let provider = Flaky {
            retry_after: Some(RETRY_MAX_AFTER + Duration::from_secs(1)),
            ..Flaky::new(vec![1])
        };
        let ctx = ToolContext::new(".".into());
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(
            &provider,
            ROUTE,
            &[],
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(Error::Provider(_))));
        assert_eq!(*provider.attempts.lock().expect("not poisoned"), 1);
    }

    /// `n` identical calls in one reply, then an answer.
    fn looping_reply(n: usize) -> Vec<Vec<StreamEvent>> {
        vec![
            (1..=n)
                .map(|i| StreamEvent::ToolCall(call(&i.to_string(), "echo", r#"{"say":"hi"}"#)))
                .collect(),
            vec![StreamEvent::TextDelta("done".into())],
        ]
    }

    /// Answers every question with `answer`; the task returns how many it
    /// answered once the asker is dropped.
    fn answering(answer: Reply) -> (Asker, tokio::task::JoinHandle<usize>) {
        let (asks, mut asked) = mpsc::channel::<nth_protocol::Ask>(1);
        let task = tokio::spawn(async move {
            let mut answered = 0;
            while let Some(ask) = asked.recv().await {
                ask.reply.send(answer.clone()).expect("the loop waits");
                answered += 1;
            }
            answered
        });
        (Asker::new(asks), task)
    }

    fn run_it() -> Reply {
        Reply::Answered(vec![Answer {
            picked: vec![CONTINUE.into()],
            typed: None,
        }])
    }

    #[tokio::test]
    async fn the_same_call_three_times_asks_before_running() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (asker, answering) = answering(run_it());
        let ctx = ToolContext {
            asker,
            ..ToolContext::new(".".into())
        };
        let (tx, _rx) = mpsc::channel(16);
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
        drop(ctx);

        assert_eq!(answering.await.expect("answers"), 1);
        let results: Vec<_> = messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult { .. }))
            .collect();
        assert_eq!(results.len(), DOOM_LOOP_THRESHOLD, "all three ran");
    }

    #[tokio::test]
    async fn running_a_loop_on_asks_again_only_after_another_full_run() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD + 1));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (asker, answering) = answering(run_it());
        let ctx = ToolContext {
            asker,
            ..ToolContext::new(".".into())
        };
        let (tx, _rx) = mpsc::channel(16);
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
        drop(ctx);

        assert_eq!(answering.await.expect("answers"), 1, "asked once for four");
    }

    #[tokio::test]
    async fn declining_the_doom_loop_stops_the_turn() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let (asker, answering) = answering(Reply::Declined);
        let ctx = ToolContext {
            asker,
            ..ToolContext::new(".".into())
        };
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(
            &provider,
            ROUTE,
            &tools,
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await;
        drop(ctx);
        answering.await.expect("answers");

        assert!(matches!(result, Err(Error::DoomLoop(name)) if name == "echo"));
        assert_eq!(
            messages[2..],
            ["1", "2", "3"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: STOPPED.into(),
            }),
            "every call is answered, so the session stays valid"
        );
    }

    #[tokio::test]
    async fn cancelling_at_the_doom_loop_question_interrupts() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let cancel = CancellationToken::new();
        // Cancels once asked, and never answers.
        let (asks, mut asked) = mpsc::channel::<nth_protocol::Ask>(1);
        let canceller = cancel.clone();
        let unanswered = tokio::spawn(async move {
            let ask = asked.recv().await.expect("asks");
            canceller.cancel();
            ask
        });
        let ctx = ToolContext {
            asker: Asker::new(asks),
            ..ToolContext::new(".".into())
        };
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(&provider, ROUTE, &tools, &ctx, &mut messages, &tx, &cancel).await;
        let _ask = unanswered.await.expect("asked");

        assert!(matches!(result, Err(Error::Interrupted)));
        assert_eq!(
            messages[2..],
            ["1", "2", "3"].map(|id| Message::ToolResult {
                call_id: id.into(),
                content: INTERRUPTED.into(),
            }),
            "every call is answered, so the session stays valid"
        );
    }

    #[tokio::test]
    async fn a_headless_run_never_asks_about_a_loop() {
        let provider = Scripted::new(looping_reply(DOOM_LOOP_THRESHOLD));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let (tx, _rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        // Nobody to ask, so it must not block; the model decides.
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
    }

    #[test]
    fn a_new_turn_starts_the_count_over() {
        let reply = |id: &str| {
            Message::Assistant(AssistantMessage {
                tool_calls: vec![call(id, "echo", "{}")],
                ..Default::default()
            })
        };
        let earlier = [reply("1"), reply("2")];
        let turn = [reply("3")];

        assert!(repeating(&earlier).is_none());
        assert!(
            repeating(&turn).is_none(),
            "the earlier turn is not counted"
        );
        assert_eq!(
            repeating(&[reply("1"), reply("2"), reply("3")]).map(|c| c.id.as_str()),
            Some("3")
        );
    }
}
