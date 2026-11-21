//! A side request beside a turn: one bounded, tool-free request on the
//! clerk's model that answers a small question about the turn, such as what
//! to name the session, while the turn itself runs. What it spends is
//! tagged with its `AsideKind`, so the ledger keeps it apart from the
//! turn's own requests. A new one-shot request of this kind goes through
//! here rather than growing a race of its own in a front-end.

use std::{future::Future, time::Duration};

use futures::StreamExt;
use nth_protocol::{AsideKind, Message, ModelSettings, Provider, Request, Spend, StreamEvent};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::Session;

/// How much of the input the model is shown: enough to say what it is
/// about, never a whole pasted file.
const INPUT_CHARS: usize = 4_000;
/// What the reply may run to, its reasoning included, before it is given
/// up on: a short answer that rambles is not the answer asked for.
const OUTPUT_BYTES: usize = 16_384;
/// How long it may take at most, so a slow provider never holds the turn's
/// end.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long an aside may outlive the turn it runs beside, so a quick turn
/// still gets its answer without waiting out `TIMEOUT`.
pub const GRACE: Duration = Duration::from_secs(1);

/// A bounded, tool-free request beside a turn, on the clerk's model.
#[derive(Debug, Clone)]
pub struct Aside {
    pub(crate) kind: AsideKind,
    pub(crate) model: ModelSettings,
    instruction: &'static str,
    pub(crate) input: String,
    parent: Uuid,
}

/// What an aside said, `None` when it failed, was cut short or ran out of
/// time, and what it spent either way.
#[derive(Debug)]
pub struct Answer {
    pub text: Option<String>,
    pub spend: Spend,
}

impl Aside {
    /// An aside of `session` that tells the model `instruction` and hands
    /// it `input`, cut to `INPUT_CHARS`. It runs on what the clerk runs
    /// on, else the subagents' model, else the session's own, since it is
    /// the same kind of small one-off task.
    pub fn new(session: &Session, kind: AsideKind, instruction: &'static str, input: &str) -> Self {
        Self {
            kind,
            model: session.child_llm().for_agent(true).clone(),
            instruction,
            input: input.chars().take(INPUT_CHARS).collect(),
            parent: session.id,
        }
    }

    /// Runs the request to its end, `TIMEOUT` or `cancel`. Await it after
    /// cancelling rather than dropping it, so the usage the provider
    /// already reported is kept.
    pub async fn run(self, provider: &dyn Provider, cancel: &CancellationToken) -> Answer {
        let mut spend = Spend::new(&self.model.llm.model, None);
        spend.aside = Some(self.kind);
        spend.cost = self.model.cost;
        let mut text = String::new();
        let mut collected = 0usize;
        // A request of its own, so it never shares a prompt cache key with
        // the turn, but tied to the session as a subagent's are.
        let id = Uuid::new_v4().to_string();
        let parent = self.parent.to_string();
        let messages = [
            Message::System(self.instruction.into()),
            Message::User(self.input),
        ];
        let request = Request {
            model: &self.model.llm.model,
            session_id: &id,
            parent_session_id: Some(&parent),
            effort: self.model.llm.effort,
            messages: &messages,
            tools: &[],
        };
        let work = async {
            let mut stream = provider.stream(request).await.map_err(|_| ())?;
            while let Some(event) = stream.next().await {
                let delta = match event.map_err(|_| ())? {
                    StreamEvent::TextDelta(delta) => {
                        text.push_str(&delta);
                        delta.len()
                    }
                    StreamEvent::ReasoningDelta(delta) => delta.len(),
                    StreamEvent::Usage(usage) => {
                        spend.count(usage);
                        0
                    }
                    _ => 0,
                };
                collected = collected.saturating_add(delta);
                if collected > OUTPUT_BYTES {
                    return Err(());
                }
            }
            Ok::<(), ()>(())
        };
        let answered = tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = tokio::time::timeout(TIMEOUT, work) => matches!(result, Ok(Ok(()))),
        };
        Answer {
            text: answered.then_some(text),
            spend,
        }
    }
}

/// Runs `work` with `aside` beside it and returns what both gave. Once
/// `work` ends the aside has `GRACE` left before `stop` cancels it; it
/// should run on `stop`, a child of the turn's own token, so cancelling the
/// turn cancels it at once. Its answer, with what it spent, always comes
/// back, even when it was cut short.
pub async fn beside<T, A>(
    work: impl Future<Output = T>,
    aside: Option<A>,
    stop: &CancellationToken,
) -> (T, Option<Answer>)
where
    A: Future<Output = Answer>,
{
    let Some(aside) = aside else {
        return (work.await, None);
    };
    tokio::pin!(work);
    tokio::pin!(aside);
    let (out, answer) = tokio::select! {
        out = &mut work => (out, None),
        answer = &mut aside => (work.await, Some(answer)),
    };
    let answer = match answer {
        Some(answer) => answer,
        None => tokio::select! {
            answer = &mut aside => answer,
            _ = tokio::time::sleep(GRACE) => {
                stop.cancel();
                aside.await
            }
        },
    };
    (out, Some(answer))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::{
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::{BoxError, Listing, Llm, Usage};

    use super::*;

    const INSTRUCTION: &str = "Name it.";

    /// Reports usage, then answers, fails or hangs.
    struct Fake {
        seen: Arc<Mutex<Vec<String>>>,
        hang: bool,
        fail: bool,
    }

    impl Provider for Fake {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            Box::pin(async { Ok(Listing::default()) })
        }
        fn stream<'a>(
            &'a self,
            request: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let mut seen = self.seen.lock().unwrap();
            seen.push(request.model.to_owned());
            seen.push(request.session_id.to_owned());
            seen.push(request.parent_session_id.unwrap().to_owned());
            assert!(request.tools.is_empty());
            assert_eq!(request.messages.len(), 2);
            assert!(matches!(&request.messages[0], Message::System(s) if s == INSTRUCTION));
            let Message::User(prompt) = &request.messages[1] else {
                panic!("user prompt")
            };
            seen.push(prompt.clone());
            let hang = self.hang;
            let fail = self.fail;
            Box::pin(async move {
                let events: Vec<Result<StreamEvent, BoxError>> = vec![
                    Ok(StreamEvent::Usage(Usage {
                        input: 42,
                        output: 4,
                        ..Usage::default()
                    })),
                    if fail {
                        Err(std::io::Error::other("failed").into())
                    } else {
                        Ok(StreamEvent::TextDelta("an answer".into()))
                    },
                ];
                let stream = stream::iter(events);
                if hang {
                    Ok(stream.chain(stream::pending()).boxed())
                } else {
                    Ok(stream.boxed())
                }
            })
        }
    }

    fn fake(hang: bool, fail: bool) -> Fake {
        Fake {
            seen: Arc::default(),
            hang,
            fail,
        }
    }

    fn llm(model: &str) -> Option<ModelSettings> {
        Some(ModelSettings::bare(Llm {
            model: model.into(),
            ..Llm::default()
        }))
    }

    fn aside(session: &Session, input: &str) -> Aside {
        Aside::new(session, AsideKind::Title, INSTRUCTION, input)
    }

    #[test]
    fn runs_on_the_clerk_else_the_subagents_else_the_sessions_model() {
        let mut session = Session::new("own", ".".into());
        assert_eq!(aside(&session, "x").model.llm.model, "own");
        session.env.subagent = llm("sub");
        assert_eq!(aside(&session, "x").model.llm.model, "sub");
        session.env.clerk = llm("clerk");
        assert_eq!(aside(&session, "x").model.llm.model, "clerk");
    }

    #[tokio::test]
    async fn a_request_of_its_own_on_the_cut_input_counts_its_usage() {
        let mut session = Session::new("parent", ".".into());
        session.env.clerk = llm("other/clerk");
        let provider = fake(false, false);
        let answer = aside(&session, &"é".repeat(4_100))
            .run(&provider, &CancellationToken::new())
            .await;
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0], "other/clerk");
        assert_ne!(seen[1], seen[2], "a session id of its own");
        assert_eq!(seen[2], session.id.to_string(), "tied to the session");
        assert_eq!(seen[3].chars().count(), INPUT_CHARS);
        assert_eq!(answer.text.as_deref(), Some("an answer"));
        assert_eq!(answer.spend.aside, Some(AsideKind::Title));
        assert_eq!(answer.spend.model, "other/clerk");
        assert_eq!(answer.spend.tokens.input, 42);
        session.usage.record(answer.spend);
        let back: crate::Ledger =
            serde_json::from_str(&serde_json::to_string(&session.usage).unwrap()).unwrap();
        assert_eq!(back.total().tokens.input, 42);
        let (who, _) = crate::usage::turns(&back).next().unwrap();
        assert_eq!(who, "#1 title other/clerk");
    }

    #[tokio::test]
    async fn failed_and_cancelled_requests_keep_reported_usage() {
        for (hang, fail) in [(false, true), (true, false)] {
            let session = Session::new("model", ".".into());
            let provider = fake(hang, fail);
            let cancel = CancellationToken::new();
            let future = aside(&session, "prompt").run(&provider, &cancel);
            tokio::pin!(future);
            let answer = if hang {
                tokio::select! {
                    _ = &mut future => panic!("must wait"),
                    _ = tokio::task::yield_now() => {}
                }
                cancel.cancel();
                future.await
            } else {
                future.await
            };
            assert!(answer.text.is_none());
            assert_eq!(answer.spend.tokens.input, 42);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_keeps_usage_without_returning_partial_text() {
        let session = Session::new("model", ".".into());
        let provider = fake(true, false);
        let answer = aside(&session, "prompt")
            .run(&provider, &CancellationToken::new())
            .await;
        assert!(answer.text.is_none());
        assert_eq!(answer.spend.tokens.input, 42);
    }

    /// An aside still running when the work ends gets `GRACE`, then is
    /// cancelled; what it spent comes back all the same.
    #[tokio::test(start_paused = true)]
    async fn beside_cancels_after_the_grace_and_keeps_the_spend() {
        let session = Session::new("model", ".".into());
        let provider = fake(true, false);
        let stop = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let running = aside(&session, "prompt").run(&provider, &stop);
        let (out, answer) = beside(async { 7 }, Some(running), &stop).await;
        assert_eq!(out, 7);
        assert_eq!(started.elapsed(), GRACE);
        assert!(stop.is_cancelled());
        let answer = answer.expect("an answer");
        assert!(answer.text.is_none());
        assert_eq!(answer.spend.tokens.input, 42);
    }

    /// One that answers in time is waited for, never cancelled.
    #[tokio::test(start_paused = true)]
    async fn beside_waits_for_an_aside_that_answers_in_the_grace() {
        let session = Session::new("model", ".".into());
        let provider = fake(false, false);
        let stop = CancellationToken::new();
        let running = aside(&session, "prompt").run(&provider, &stop);
        let (_, answer) = beside(async {}, Some(running), &stop).await;
        assert!(!stop.is_cancelled());
        assert_eq!(answer.unwrap().text.as_deref(), Some("an answer"));
    }

    /// The turn's own token, `stop`'s parent, cuts it short at once.
    #[tokio::test(start_paused = true)]
    async fn cancelling_the_turn_cancels_the_aside_at_once() {
        let session = Session::new("model", ".".into());
        let provider = fake(true, false);
        let cancel = CancellationToken::new();
        let stop = cancel.child_token();
        let started = tokio::time::Instant::now();
        let running = aside(&session, "prompt").run(&provider, &stop);
        let work = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        };
        let (_, answer) = beside(work, Some(running), &stop).await;
        assert_eq!(started.elapsed(), Duration::from_millis(100));
        assert_eq!(answer.unwrap().spend.tokens.input, 42);
    }
}
