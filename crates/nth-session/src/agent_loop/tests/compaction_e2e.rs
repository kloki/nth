//! Compaction through a whole [`Session`]: what the provider is sent, what
//! the saved history keeps, and how a resumed session carries on. The policy
//! itself is tested in `compaction.rs`; these go through the agent loop.

use std::sync::Mutex;

use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use nth_context::{Context, Paths};
use nth_protocol::{
    ContextOverflow, FrontEnd, Listing, Llm, Message, Provider, Request, StreamEvent, Tool,
    ToolContext, Usage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    Compaction, DEFAULT_MAX_STEPS, Error, Session, Store, compaction::WHAT_DID_WE_DO,
    subagent::child_session,
};

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input,
        output,
        ..Usage::default()
    }
}

fn assistant(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        text: text.into(),
        ..AssistantMessage::default()
    })
}

/// Sends one prompt through `session` on a fresh channel and token.
async fn prompt(
    session: &mut Session,
    text: &str,
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
) -> Result<(), Error> {
    let (tx, _rx) = mpsc::channel(64);
    session
        .prompt(
            text,
            provider,
            tools,
            &FrontEnd::default(),
            &tx,
            &CancellationToken::new(),
        )
        .await
}

/// Runs one manual compaction on `session`.
async fn compact(session: &mut Session, provider: &dyn Provider) -> Result<(), Error> {
    let (tx, _rx) = mpsc::channel(64);
    session
        .compact(
            provider,
            &[],
            &FrontEnd::default(),
            &tx,
            &CancellationToken::new(),
        )
        .await
}

/// Every request a [`Scripted`] provider received, in order.
fn requests(provider: &Scripted) -> Vec<Vec<Message>> {
    provider.sent.lock().expect("not poisoned").clone()
}

/// The policy's own estimate, serialized chars over four, used here only to
/// pick a budget that forces a split, never to assert one.
fn estimate(message: &Message) -> u64 {
    serde_json::to_string(message).map_or(0, |json| json.len() as u64) / 4
}

#[tokio::test]
async fn usage_at_the_window_compacts_and_the_next_request_starts_at_the_summary() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    // A window of 10k less 2k reserved: exactly 8k fills its usable part,
    // which must fire.
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 100;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let system = session.messages[0].clone();
    let provider = Scripted::new(vec![
        vec![
            StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
            StreamEvent::Usage(usage(7_900, 100)),
        ],
        vec![StreamEvent::TextDelta("the summary".into())],
        vec![StreamEvent::TextDelta("done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];

    prompt(&mut session, "go", &provider, &tools)
        .await
        .expect("turn completes");

    assert_eq!(
        session.last_usage,
        Some(usage(7_900, 100)),
        "what the turn's requests reported"
    );
    let sent = requests(&provider);
    assert_eq!(sent.len(), 3, "the step, the summary, then the step after");
    let [Message::User(summary_prompt)] = &sent[1][..] else {
        panic!("one user message: {:?}", sent[1]);
    };
    assert!(
        summary_prompt.contains("[User]: old question"),
        "the head is serialized: {summary_prompt}"
    );
    assert!(
        !summary_prompt.contains("[User]: go"),
        "the verbatim tail is not serialized: {summary_prompt}"
    );
    assert_eq!(
        sent[2][..3],
        [
            system.clone(),
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("the summary"),
        ],
        "the request starts at the summary exchange"
    );
    assert_eq!(
        sent[2][3..],
        [
            Message::User("go".into()),
            Message::Assistant(AssistantMessage {
                tool_calls: vec![call("1", "echo", r#"{"say":"hi"}"#)],
                ..AssistantMessage::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "hi".into()
            },
        ],
        "then the verbatim tail"
    );
    assert_eq!(
        session.messages,
        [
            system,
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::Compaction {
                summary: "the summary".into()
            },
            Message::User("go".into()),
            Message::Assistant(AssistantMessage {
                tool_calls: vec![call("1", "echo", r#"{"say":"hi"}"#)],
                ..AssistantMessage::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "hi".into()
            },
            assistant("done"),
        ],
        "the history keeps every original message, with the marker in order"
    );
}

/// Fails its first request as a context overflow, as a wire does once it
/// recognizes its provider's too-long error, and answers the rest.
struct OverflowThen {
    replies: Mutex<Vec<Vec<StreamEvent>>>,
    sent: Mutex<Vec<Vec<Message>>>,
}

impl Provider for OverflowThen {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        let mut sent = self.sent.lock().expect("not poisoned");
        sent.push(request.messages.to_vec());
        if sent.len() == 1 {
            return async { Err(Box::new(ContextOverflow) as BoxError) }.boxed();
        }
        let reply = self.replies.lock().expect("not poisoned").remove(0);
        async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
    }
}

#[tokio::test]
async fn an_overflowing_request_compacts_and_replays_the_refused_prompt() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 40;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let system = session.messages[0].clone();
    let provider = OverflowThen {
        replies: Mutex::new(vec![
            vec![StreamEvent::TextDelta("the summary".into())],
            vec![StreamEvent::TextDelta("done".into())],
        ]),
        sent: Mutex::new(Vec::new()),
    };

    prompt(&mut session, "the failed prompt", &provider, &[])
        .await
        .expect("the turn recovers");

    let sent = provider.sent.lock().expect("not poisoned").clone();
    assert_eq!(
        sent.len(),
        3,
        "the refused request, the summary, the replay"
    );
    assert_eq!(
        sent[0].last(),
        Some(&Message::User("the failed prompt".into())),
        "the request that was refused"
    );
    let [Message::User(summary_prompt)] = &sent[1][..] else {
        panic!("one user message: {:?}", sent[1]);
    };
    assert!(
        summary_prompt.contains("[User]: old question"),
        "the older turn is summarized: {summary_prompt}"
    );
    assert!(
        !summary_prompt.contains("the failed prompt"),
        "the prompt to replay is not summarized: {summary_prompt}"
    );
    assert_eq!(
        sent[2],
        [
            system.clone(),
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("the summary"),
            Message::User("the failed prompt".into()),
        ],
        "the prompt runs again under the summary"
    );
    assert_eq!(
        session.messages,
        [
            system,
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::Compaction {
                summary: "the summary".into()
            },
            Message::User("the failed prompt".into()),
            assistant("done"),
        ]
    );
}

#[tokio::test]
async fn a_fresh_session_that_overflows_recovers_through_a_summary_of_the_prompt() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    let system = session.messages[0].clone();
    let provider = OverflowThen {
        replies: Mutex::new(vec![
            vec![StreamEvent::TextDelta("the summary".into())],
            vec![StreamEvent::TextDelta("done".into())],
        ]),
        sent: Mutex::new(Vec::new()),
    };

    prompt(&mut session, "doomed", &provider, &[])
        .await
        .expect("the turn recovers");

    let sent = provider.sent.lock().expect("not poisoned").clone();
    assert_eq!(sent.len(), 3, "the refused request, the summary, the reply");
    let [Message::User(summary_prompt)] = &sent[1][..] else {
        panic!("one user message: {:?}", sent[1]);
    };
    assert!(
        summary_prompt.contains("[User]: doomed"),
        "with nothing else to summarize, the prompt itself is: {summary_prompt}"
    );
    assert_eq!(
        sent[2],
        [
            system,
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("the summary"),
        ],
        "nothing is left to replay verbatim"
    );
    assert_eq!(
        session.messages[2],
        Message::Compaction {
            summary: "the summary".into()
        },
        "the marker still lands"
    );
    assert_eq!(session.messages[3], assistant("done"));
}

#[tokio::test]
async fn a_compacted_session_resumes_at_its_marker_with_the_limits_and_usage_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::at(dir.path().join("sessions"));
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 40;
    session.messages.extend([
        Message::User("first question".into()),
        assistant(&"x".repeat(400)),
        Message::User("recent".into()),
        assistant("recent answer"),
    ]);
    session.last_usage = Some(usage(9_000, 1_500));
    let summarizer = Scripted::new(vec![vec![StreamEvent::TextDelta("a summary".into())]]);

    compact(&mut session, &summarizer).await.expect("compacts");
    assert_eq!(
        session.messages[3],
        Message::Compaction {
            summary: "a summary".into()
        }
    );

    store.save(&session).await.expect("saves");
    let mut resumed = store.load(session.id).await.expect("loads");

    assert_eq!(resumed.messages, session.messages, "the marker survives");
    assert_eq!(resumed.last_usage, session.last_usage, "the usage survives");
    assert_eq!(
        resumed.compaction, session.compaction,
        "the policy survives"
    );
    assert_eq!(
        (resumed.context_window, resumed.max_output),
        (Some(10_000), Some(2_000)),
        "the limits survive"
    );

    let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("next answer".into())]]);
    prompt(&mut resumed, "next", &provider, &[])
        .await
        .expect("turn completes");

    let sent = requests(&provider);
    assert_eq!(
        sent[0][..3],
        [
            session.messages[0].clone(),
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("a summary"),
        ],
        "a resumed session still starts its requests at the marker"
    );
    assert_eq!(
        sent[0][3..],
        [
            Message::User("recent".into()),
            assistant("recent answer"),
            Message::User("next".into()),
        ],
        "the tail after the marker is verbatim"
    );
}

#[tokio::test]
async fn a_save_from_before_compaction_still_loads_and_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::at(dir.path().join("sessions"));
    let mut old = Session::new("glm-5.3", "/repo".into());
    old.messages.extend([
        Message::User("an old prompt".into()),
        assistant("an old answer"),
    ]);
    store.save(&old).await.expect("saves");

    // Strip the fields compaction added, as a save from before it has none.
    let path = dir.path().join("sessions").join(format!("{}.json", old.id));
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("reads")).expect("parses");
    let fields = json.as_object_mut().expect("an object");
    fields.remove("last_usage");
    fields.remove("compaction");
    fields.remove("context_window");
    fields.remove("max_output");
    std::fs::write(&path, serde_json::to_vec(&json).expect("serializes")).expect("writes");

    let mut session = store.load(old.id).await.expect("loads");
    assert_eq!(
        session.compaction,
        Compaction::default(),
        "the defaults, as before"
    );
    let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);

    prompt(&mut session, "new", &provider, &[])
        .await
        .expect("turn completes");

    assert_eq!(session.messages.last(), Some(&assistant("ok")));
    assert!(
        !session
            .messages
            .iter()
            .any(|m| matches!(m, Message::Compaction { .. })),
        "nothing compacted without limits"
    );
    let sent = requests(&provider);
    assert_eq!(
        sent[0],
        [
            session.messages[0].clone(),
            Message::User("an old prompt".into()),
            assistant("an old answer"),
            Message::User("new".into()),
        ],
        "the whole history goes to the provider"
    );
}

#[tokio::test]
async fn a_split_turn_keeps_every_tool_call_with_its_result() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    let calls = Message::Assistant(AssistantMessage {
        tool_calls: vec![
            call("1", "echo", r#"{"say":"one"}"#),
            call("2", "echo", r#"{"say":"two"}"#),
            call("3", "echo", r#"{"say":"three"}"#),
        ],
        ..AssistantMessage::default()
    });
    session.messages.extend([
        Message::User("older question".into()),
        assistant(&"x".repeat(40_000)),
        Message::User("work".into()),
        calls.clone(),
        Message::ToolResult {
            call_id: "1".into(),
            content: "one".repeat(200),
        },
        Message::ToolResult {
            call_id: "2".into(),
            content: "two".repeat(200),
        },
        Message::ToolResult {
            call_id: "3".into(),
            content: "three".repeat(200),
        },
        assistant("done"),
    ]);
    // A budget that fits the exchange but not the prompt that opened it, so
    // the tail starts mid-turn at the calls.
    let from_calls: u64 = session.messages[4..].iter().map(estimate).sum();
    let whole: u64 = session.messages[3..].iter().map(estimate).sum();
    session.compaction.preserve = from_calls + (whole - from_calls) / 2;
    let summarizer = Scripted::new(vec![vec![StreamEvent::TextDelta("split summary".into())]]);

    compact(&mut session, &summarizer).await.expect("compacts");

    assert_eq!(
        session.messages[4],
        Message::Compaction {
            summary: "split summary".into()
        }
    );
    assert_eq!(
        session.messages[5], calls,
        "the tail starts inside the turn, at the calls"
    );
    assert_eq!(
        session.messages.len(),
        10,
        "every original message plus the marker"
    );

    let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
    prompt(&mut session, "after", &provider, &[])
        .await
        .expect("turn completes");

    let sent = requests(&provider);
    let request = &sent[0];
    for (at, message) in request.iter().enumerate() {
        match message {
            Message::Assistant(reply) => {
                for call in &reply.tool_calls {
                    let answered = request[at + 1..].iter().any(
                        |m| matches!(m, Message::ToolResult { call_id, .. } if call_id == &call.id),
                    );
                    assert!(answered, "call {} lost its result: {request:?}", call.id);
                }
            }
            Message::ToolResult { call_id, .. } => {
                let called = request[..at].iter().any(|m| {
                    matches!(m, Message::Assistant(reply)
                        if reply.tool_calls.iter().any(|c| &c.id == call_id))
                });
                assert!(called, "result {call_id} lost its call: {request:?}");
            }
            _ => {}
        }
    }
    assert_eq!(
        request[..3],
        [
            session.messages[0].clone(),
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("split summary"),
        ],
        "the request starts at the summary, then the split tail"
    );
    assert!(
        matches!(&request[3], Message::Assistant(reply) if !reply.tool_calls.is_empty()),
        "the tail starts at the calls: {request:?}"
    );
}

#[tokio::test]
async fn a_child_session_inherits_the_limits_and_compacts_on_its_own() {
    let ctx = ToolContext {
        session: uuid::Uuid::new_v4().to_string(),
        context_window: Some(10_000),
        max_output: Some(2_000),
        llm: Llm {
            model: "glm-5.3".into(),
            ..Llm::default()
        },
        ..ToolContext::new("/repo".into())
    };
    let explore = Context::discover(std::path::Path::new("/nowhere"), &Paths::default())
        .agents
        .get("explore")
        .expect("built in")
        .clone();
    let mut child = child_session(&explore, &ctx, DEFAULT_MAX_STEPS);
    assert_eq!(
        (child.context_window, child.max_output),
        (Some(10_000), Some(2_000)),
        "the parent's limits"
    );
    child.compaction.preserve = 100;
    child.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let system = child.messages[0].clone();
    let provider = Scripted::new(vec![
        vec![
            StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
            StreamEvent::Usage(usage(7_900, 100)),
        ],
        vec![StreamEvent::TextDelta("child summary".into())],
        vec![StreamEvent::TextDelta("child done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];

    prompt(&mut child, "go", &provider, &tools)
        .await
        .expect("the child's turn completes");

    assert_eq!(
        child.messages[3],
        Message::Compaction {
            summary: "child summary".into()
        },
        "the child compacts on its own"
    );
    let sent = requests(&provider);
    assert_eq!(sent.len(), 3, "the step, the child's summary, then after");
    assert_eq!(
        sent[2][..3],
        [
            system,
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("child summary"),
        ],
        "the child's request starts at its own marker"
    );
    assert_eq!(sent[2][3], Message::User("go".into()));
}

#[tokio::test]
async fn a_manual_compaction_of_only_the_system_prompt_does_nothing() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    let before = session.messages.clone();
    let provider = Scripted::new(Vec::new());

    compact(&mut session, &provider)
        .await
        .expect("nothing to compact");

    assert_eq!(session.messages, before);
    assert_eq!(requests(&provider).len(), 0, "no summary was asked for");
}

#[tokio::test]
async fn two_compactions_in_a_row_leave_both_markers_and_start_at_the_newest() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.compaction.preserve = 40;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
        Message::User("recent".into()),
        assistant("recent answer"),
    ]);
    let provider = Scripted::new(vec![
        vec![StreamEvent::TextDelta("first summary".into())],
        vec![StreamEvent::TextDelta("second summary".into())],
        vec![StreamEvent::TextDelta("ok".into())],
    ]);

    compact(&mut session, &provider)
        .await
        .expect("first compacts");
    compact(&mut session, &provider)
        .await
        .expect("second compacts");

    assert_eq!(
        session.messages[3],
        Message::Compaction {
            summary: "first summary".into()
        }
    );
    assert_eq!(
        session.messages[4],
        Message::Compaction {
            summary: "second summary".into()
        },
        "the new marker follows the old one"
    );
    let sent = requests(&provider);
    let [Message::User(second_prompt)] = &sent[1][..] else {
        panic!("one user message: {:?}", sent[1]);
    };
    assert!(
        second_prompt.contains("[Summary]: first summary"),
        "the first summary is merged into the second: {second_prompt}"
    );

    prompt(&mut session, "next", &provider, &[])
        .await
        .expect("turn completes");

    let sent = requests(&provider);
    assert_eq!(
        sent[2][..3],
        [
            session.messages[0].clone(),
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("second summary"),
        ],
        "requests start at the newest marker"
    );
    assert_eq!(
        sent[2][3..],
        [
            Message::User("recent".into()),
            assistant("recent answer"),
            Message::User("next".into()),
        ]
    );
}

#[tokio::test]
async fn automatic_compaction_off_nothing_fires_even_past_the_window() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.auto = false;
    session.compaction.preserve = 40;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let provider = Scripted::new(vec![
        vec![
            StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
            // Past the usable window, which would fire with auto on.
            StreamEvent::Usage(usage(9_500, 1_500)),
        ],
        vec![StreamEvent::TextDelta("done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];

    prompt(&mut session, "go", &provider, &tools)
        .await
        .expect("turn completes");

    assert!(
        !session
            .messages
            .iter()
            .any(|m| matches!(m, Message::Compaction { .. })),
        "no marker: {:?}",
        session.messages
    );
    let sent = requests(&provider);
    assert_eq!(sent.len(), 2, "no summary request");
    assert_eq!(
        sent[1],
        [
            session.messages[0].clone(),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("go".into()),
            Message::Assistant(AssistantMessage {
                tool_calls: vec![call("1", "echo", r#"{"say":"hi"}"#)],
                ..AssistantMessage::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "hi".into()
            },
        ],
        "the whole history goes to the provider untouched"
    );
}

/// Refuses every request as too long, recording how many it refused.
struct AlwaysOverflow {
    requests: Mutex<usize>,
}

impl Provider for AlwaysOverflow {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        _: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        *self.requests.lock().expect("not poisoned") += 1;
        async { Err(Box::new(ContextOverflow) as BoxError) }.boxed()
    }
}

#[tokio::test]
async fn a_summary_request_that_overflows_stops_as_too_big_to_compact() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 40;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let provider = AlwaysOverflow {
        requests: Mutex::new(0),
    };
    let mut before = session.messages.clone();
    before.push(Message::User("doomed".into()));

    let result = prompt(&mut session, "doomed", &provider, &[]).await;

    assert!(matches!(&result, Err(Error::TooBigToCompact)), "{result:?}");
    assert_eq!(
        *provider.requests.lock().expect("not poisoned"),
        2,
        "the refused step and the refused summary, no retry loop"
    );
    assert_eq!(session.messages, before, "nothing was summarized away");
}

#[tokio::test]
async fn an_overflow_with_nothing_but_the_prompt_to_summarize_gives_up_too() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    let provider = AlwaysOverflow {
        requests: Mutex::new(0),
    };

    let result = prompt(&mut session, "doomed", &provider, &[]).await;

    assert!(matches!(&result, Err(Error::TooBigToCompact)), "{result:?}");
    assert_eq!(*provider.requests.lock().expect("not poisoned"), 2);
    assert!(
        !session
            .messages
            .iter()
            .any(|m| matches!(m, Message::Compaction { .. })),
        "no marker from a compaction that never finished"
    );
}

/// A turn whose last allowed step falls to the automatic compaction: the
/// compaction spends the step, the loop ends, and the model never gets the
/// compacted history. Expected: the compaction does not cost the step it
/// was preparing.
#[tokio::test]
#[ignore = "bug: auto-compaction on the last allowed step spends it, so the turn fails with TooManySteps without the model seeing the summary"]
async fn the_last_allowed_step_still_asks_the_model_after_compacting() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 40;
    session.max_steps = 2;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let provider = Scripted::new(vec![
        vec![
            StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
            StreamEvent::Usage(usage(9_500, 1_500)),
        ],
        vec![StreamEvent::TextDelta("the summary".into())],
        vec![StreamEvent::TextDelta("done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];

    let result = prompt(&mut session, "go", &provider, &tools).await;

    assert!(result.is_ok(), "{result:?}");
    let sent = requests(&provider);
    assert_eq!(sent.len(), 3, "the step, the summary, then the last step");
    assert_eq!(sent[2][1], Message::User(WHAT_DID_WE_DO.into()));
    assert_eq!(session.messages.last(), Some(&assistant("done")));
}

/// Answers the first request with the tool call whose result is over the
/// window; refuses any request carrying it; answers summaries. Auto is off,
/// so the only compactions are the overflow recoveries.
struct ReplayStaysTooBig {
    requests: Mutex<usize>,
    summaries: Mutex<usize>,
}

impl Provider for ReplayStaysTooBig {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        let n = {
            let mut requests = self.requests.lock().expect("not poisoned");
            *requests += 1;
            *requests
        };
        if request.messages.len() == 1 {
            *self.summaries.lock().expect("not poisoned") += 1;
            let reply = vec![StreamEvent::TextDelta("a summary".into())];
            return async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed();
        }
        let huge = request
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult { content, .. } if content.len() > 1_000));
        if huge {
            return async { Err(Box::new(ContextOverflow) as BoxError) }.boxed();
        }
        let reply = match n {
            1 => vec![StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#))],
            _ => vec![StreamEvent::TextDelta("done".into())],
        };
        async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
    }
}

/// Refuses the exchange it just kept, so every replay overflows again. The
/// replay is identical every time: compacting once more cannot shrink it,
/// so the turn must stop instead of compacting every step.
#[tokio::test]
#[ignore = "bug: a replay that overflows again compacts every step until TooManySteps instead of stopping"]
async fn a_replay_that_overflows_again_does_not_compact_forever() {
    struct EchoHuge;

    impl Tool for EchoHuge {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "echo",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, nth_protocol::ToolResult> {
            async move { Ok("r".repeat(4_000)) }.boxed()
        }
    }

    let mut session = Session::new("glm-5.3", "/repo".into());
    session.compaction.auto = false;
    session.compaction.preserve = 20_000;
    session.max_steps = 6;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(100_000)),
    ]);
    let provider = ReplayStaysTooBig {
        requests: Mutex::new(0),
        summaries: Mutex::new(0),
    };
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoHuge)];

    let _ = prompt(&mut session, "go", &provider, &tools).await;

    let summaries = *provider.summaries.lock().expect("not poisoned");
    assert!(
        summaries <= 2,
        "compacted {summaries} times for one exchange that will not shrink"
    );
}
