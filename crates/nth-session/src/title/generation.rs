//! A bounded, tool-free request, owned and settled by the front-end.

use std::time::Duration;

use futures::StreamExt;
use nth_protocol::{
    Llm, Message, ModelSettings, Provider, Request, Spend, SpendSource, StreamEvent,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Error, Generation, validate};
use crate::Session;

const INPUT_CHARS: usize = 4_000;
const OUTPUT_BYTES: usize = 16_384;
const TIMEOUT: Duration = Duration::from_secs(10);
const INSTRUCTION: &str = include_str!("instruction.txt");

/// A small snapshot, never a copy of the conversation. Create it only after
/// the human prompt is admitted, using its text before any expansion.
#[derive(Debug, Clone)]
pub struct Job {
    pub session_id: Uuid,
    pub expected_revision: u64,
    pub model: ModelSettings,
    excerpt: String,
}

/// The owner records `spend` even when `candidate` is absent or stale.
#[derive(Debug)]
pub struct Outcome {
    pub session_id: Uuid,
    pub expected_revision: u64,
    pub candidate: Option<String>,
    pub spend: Spend,
}

impl Job {
    /// Consume the first eligible opportunity, including when disabled. Legacy
    /// sessions, subagents, empty prompts and already-attempted sessions do not
    /// start a job. The owner must persist the updated metadata with its turn.
    pub fn admit(session: &mut Session, prompt: &str) -> Result<Option<Self>, Error> {
        if session.subagent || prompt.trim().is_empty() {
            return Ok(None);
        }
        let Some(metadata) = session.title_metadata.as_mut() else {
            return Ok(None);
        };
        if metadata.generation != Generation::Pending {
            return Ok(None);
        }
        metadata.attempted()?;
        if !session.env.auto_title {
            return Ok(None);
        }
        let model = session
            .env
            .clerk
            .clone()
            .or_else(|| session.env.subagent.clone())
            .unwrap_or_else(|| ModelSettings {
                llm: Llm {
                    model: session.model.clone(),
                    effort: session.effort,
                },
                context_window: session.context_window,
                max_output: session.max_output,
                takes_images: session.env.takes_images,
                cost: session.env.cost,
            });
        Ok(Some(Self {
            session_id: session.id,
            expected_revision: metadata.revision,
            model,
            excerpt: prompt.chars().take(INPUT_CHARS).collect(),
        }))
    }

    /// Run to completion, timeout, or cooperative cancellation. Await the
    /// returned outcome after cancelling instead of aborting this future, so
    /// usage already reported by the provider is retained.
    pub async fn run(self, provider: &dyn Provider, cancel: &CancellationToken) -> Outcome {
        let mut spend = Spend::new(&self.model.llm.model, None);
        spend.source = Some(SpendSource::Title);
        spend.cost = self.model.cost;
        let mut text = String::new();
        let mut collected = 0usize;
        let id = Uuid::new_v4().to_string();
        let parent = self.session_id.to_string();
        let messages = [
            Message::System(INSTRUCTION.into()),
            Message::User(self.excerpt),
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
                match event.map_err(|_| ())? {
                    StreamEvent::TextDelta(delta) => {
                        collected = collected.saturating_add(delta.len());
                        if collected > OUTPUT_BYTES {
                            return Err(());
                        }
                        text.push_str(&delta);
                    }
                    StreamEvent::ReasoningDelta(delta) => {
                        collected = collected.saturating_add(delta.len());
                        if collected > OUTPUT_BYTES {
                            return Err(());
                        }
                    }
                    StreamEvent::Usage(usage) => spend.count(usage),
                    _ => {}
                }
            }
            Ok::<(), ()>(())
        };
        let success = tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = tokio::time::timeout(TIMEOUT, work) => matches!(result, Ok(Ok(()))),
        };
        Outcome {
            session_id: self.session_id,
            expected_revision: self.expected_revision,
            candidate: success.then(|| normalize(&text)).flatten(),
            spend,
        }
    }
}

fn normalize(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let mut clean = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            clean.push(c);
        }
    }
    let clean = clean
        .trim()
        .trim_matches(['"', '\'', '“', '”', '‘', '’'])
        .trim();
    let capped: String = clean.chars().take(100).collect();
    validate(&capped).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::{
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::{BoxError, Listing, Usage};

    use super::*;

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
                        Ok(StreamEvent::TextDelta(
                            "\n  “Rust \u{1b}[31mタイトル”\nextra".into(),
                        ))
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

    #[tokio::test]
    async fn isolated_request_uses_clerk_and_records_usage() {
        let mut session = Session::new("parent", ".".into());
        session.env.subagent = Some(ModelSettings::bare(Llm {
            model: "sub".into(),
            ..Llm::default()
        }));
        session.env.clerk = Some(ModelSettings::bare(Llm {
            model: "other/clerk".into(),
            ..Llm::default()
        }));
        let job = Job::admit(&mut session, &"é".repeat(4_100))
            .unwrap()
            .unwrap();
        assert!(Job::admit(&mut session, "again").unwrap().is_none());
        let provider = fake(false, false);
        let outcome = job.run(&provider, &CancellationToken::new()).await;
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0], "other/clerk");
        assert_ne!(seen[1], seen[2]);
        assert_eq!(seen[2], session.id.to_string());
        assert_eq!(seen[3].chars().count(), 4_000);
        assert_eq!(outcome.candidate.as_deref(), Some("Rust タイトル"));
        assert_eq!(outcome.spend.source, Some(SpendSource::Title));
        assert_eq!(outcome.spend.tokens.input, 42);
        session.usage.record(outcome.spend);
        let back: crate::Ledger =
            serde_json::from_str(&serde_json::to_string(&session.usage).unwrap()).unwrap();
        assert_eq!(back.total().tokens.input, 42);
        assert!(
            crate::usage::turns(&back)
                .next()
                .unwrap()
                .0
                .contains("title")
        );
    }

    #[tokio::test]
    async fn failed_and_cancelled_requests_keep_reported_usage() {
        for (hang, fail) in [(false, true), (true, false)] {
            let mut session = Session::new("model", ".".into());
            let job = Job::admit(&mut session, "prompt").unwrap().unwrap();
            let provider = fake(hang, fail);
            let cancel = CancellationToken::new();
            let future = job.run(&provider, &cancel);
            tokio::pin!(future);
            let outcome = if hang {
                tokio::select! { _ = &mut future => panic!("must wait"), _ = tokio::task::yield_now() => {} }
                cancel.cancel();
                future.await
            } else {
                future.await
            };
            assert!(outcome.candidate.is_none());
            assert_eq!(outcome.spend.tokens.input, 42);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_keeps_usage_without_returning_partial_title() {
        let mut session = Session::new("model", ".".into());
        let job = Job::admit(&mut session, "prompt").unwrap().unwrap();
        let provider = fake(true, false);
        let outcome = job.run(&provider, &CancellationToken::new()).await;
        assert!(outcome.candidate.is_none());
        assert_eq!(outcome.spend.tokens.input, 42);
    }

    #[test]
    fn eligibility_selection_and_normalization() {
        let mut session = Session::new("own", ".".into());
        assert!(Job::admit(&mut session, "  ").unwrap().is_none());
        session.env.auto_title = false;
        assert!(Job::admit(&mut session, "first").unwrap().is_none());
        session.env.auto_title = true;
        assert!(Job::admit(&mut session, "second").unwrap().is_none());
        let mut legacy = Session::new("own", ".".into());
        legacy.title_metadata = None;
        assert!(Job::admit(&mut legacy, "old").unwrap().is_none());
        let mut child = Session::new("own", ".".into()).as_subagent(None);
        assert!(Job::admit(&mut child, "child").unwrap().is_none());
        let mut own = Session::new("own", ".".into());
        own.env.subagent = Some(ModelSettings::bare(Llm {
            model: "sub".into(),
            ..Llm::default()
        }));
        assert_eq!(
            Job::admit(&mut own, "go").unwrap().unwrap().model.llm.model,
            "sub"
        );
        assert_eq!(normalize(&"é".repeat(101)).unwrap().chars().count(), 100);
        assert_eq!(normalize("\n  \n"), None);
    }
}
