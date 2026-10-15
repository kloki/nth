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
        session.last_usage, None,
        "the reply after the compaction reported no usage, and the step's \
         old size describes a conversation that is gone"
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
    assert_eq!(
        session.last_usage, None,
        "the compaction cleared the size of what it summarized away"
    );
    // A reply after the compaction is what a resume has to carry.
    session.last_usage = Some(usage(3_000, 500));

    store.save(&session).await.expect("saves");
    let mut resumed = store.load(session.id).await.expect("loads");

    assert_eq!(resumed.messages, session.messages, "the marker survives");
    assert_eq!(
        resumed.last_usage, session.last_usage,
        "the usage a later reply reported survives"
    );
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

/// A turn whose last allowed step needs a compaction first: the compaction
/// does not cost the step, so the model still gets the compacted history
/// and answers.
#[tokio::test]
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

/// Refuses the exchange it just kept, so the replay overflows again. It is
/// the same conversation every time: compacting more cannot shrink it, so
/// the turn stops after one replay instead of compacting every step.
#[tokio::test]
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

/// A compaction is not once per turn: when the steps after it grow the
/// history over the window again, another reply that reaches it fires a
/// second one, and requests after that start at the newest summary.
#[tokio::test]
async fn two_auto_compactions_in_a_turn_start_at_the_newest_summary() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 100;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    let system = session.messages[0].clone();
    let provider = Scripted::new(vec![
        // The first step reaches the window; its tool result then keeps the
        // history growing.
        vec![
            StreamEvent::ToolCall(call("1", "echo", r#"{"say":"one"}"#)),
            StreamEvent::Usage(usage(7_900, 100)),
        ],
        vec![StreamEvent::TextDelta("first summary".into())],
        // The step after the first compaction reaches it again.
        vec![
            StreamEvent::ToolCall(call("2", "echo", r#"{"say":"two"}"#)),
            StreamEvent::Usage(usage(7_900, 100)),
        ],
        vec![StreamEvent::TextDelta("second summary".into())],
        vec![StreamEvent::TextDelta("done".into())],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];

    prompt(&mut session, "go", &provider, &tools)
        .await
        .expect("turn completes");

    let markers: Vec<_> = session
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Compaction { summary } => Some(summary.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        markers,
        ["first summary", "second summary"],
        "both compactions landed, the second after the first"
    );

    let sent = requests(&provider);
    assert_eq!(sent.len(), 5, "two steps, their summaries, then the answer");
    let [Message::User(second_prompt)] = &sent[3][..] else {
        panic!("one user message: {:?}", sent[3]);
    };
    assert!(
        second_prompt.contains("[Summary]: first summary"),
        "the second summary merges the first: {second_prompt}"
    );
    assert_eq!(
        sent[4][..3],
        [
            system,
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("second summary"),
        ],
        "the request after the second compaction starts at the newest summary"
    );
    assert_eq!(
        sent[4][3],
        Message::User("go".into()),
        "then the verbatim tail"
    );
}

/// A session left with its last reply already at the window compacts
/// before its first request, instead of re-sending a history that overflows
/// and only recovering through the one-replay overflow path.
#[tokio::test]
async fn a_resumed_session_over_the_window_compacts_before_its_first_request() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.set_limits(Some(10_000), Some(2_000));
    session.compaction.preserve = 40;
    session.max_steps = 1;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    // What the last reply reported before the session was left.
    session.last_usage = Some(usage(9_500, 1_500));
    let system = session.messages[0].clone();
    let provider = Scripted::new(vec![
        vec![StreamEvent::TextDelta("the summary".into())],
        vec![StreamEvent::TextDelta("done".into())],
    ]);

    prompt(&mut session, "next", &provider, &[])
        .await
        .expect("turn completes");

    let sent = requests(&provider);
    assert_eq!(sent.len(), 2, "the summary, then the one allowed step");
    let [Message::User(summary_prompt)] = &sent[0][..] else {
        panic!("one user message: {:?}", sent[0]);
    };
    assert!(
        summary_prompt.contains("[User]: old question"),
        "the head is serialized: {summary_prompt}"
    );
    assert_eq!(
        sent[1][..3],
        [
            system,
            Message::User(WHAT_DID_WE_DO.into()),
            assistant("the summary"),
        ],
        "the first request already starts at the summary exchange"
    );
    assert_eq!(session.messages.last(), Some(&assistant("done")));
}

/// `last_usage` is the last reply's usage, not the turn's sum: only it says
/// how full the window is now.
#[tokio::test]
async fn last_usage_holds_the_last_replys_usage_not_the_turn_sum() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    let provider = Scripted::new(vec![
        vec![
            StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
            StreamEvent::Usage(usage(100, 10)),
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Usage(usage(200, 20)),
        ],
    ]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];

    prompt(&mut session, "go", &provider, &tools)
        .await
        .expect("turn completes");

    assert_eq!(
        session.last_usage,
        Some(usage(200, 20)),
        "the second reply's, not the 300 + 30 the turn summed"
    );
    assert_eq!(
        session.usage.total().tokens,
        usage(300, 30),
        "the ledger still sums every request"
    );
}

/// A manual compaction summarizes the conversation away, so the size the
/// last reply reported no longer describes anything: it is cleared rather
/// than replaced by the summary request's own usage.
#[tokio::test]
async fn a_manual_compaction_clears_the_last_usage() {
    let mut session = Session::new("glm-5.3", "/repo".into());
    session.compaction.preserve = 40;
    session.messages.extend([
        Message::User("old question".into()),
        assistant(&"x".repeat(400)),
    ]);
    session.last_usage = Some(usage(9_500, 1_500));
    let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("the summary".into())]]);

    compact(&mut session, &provider).await.expect("compacts");

    assert!(
        matches!(session.messages[3], Message::Compaction { .. }),
        "the compaction landed"
    );
    assert_eq!(
        session.last_usage, None,
        "the old size describes the conversation that was summarized away"
    );
}
