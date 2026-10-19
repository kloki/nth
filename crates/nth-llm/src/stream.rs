//! What the three protocols' streams share: the loop that turns a
//! response's bytes into `StreamEvent`s, one error shape, and which of its
//! kinds a retry or a compaction fixes. What differs is how a protocol
//! reads its events into a reply (its `Protocol`) and the error it reports
//! inside the stream, after the 200 status (its `Payload`).

use std::{collections::VecDeque, marker::PhantomData, time::Duration};

use futures::{Stream, StreamExt, stream::BoxStream};
use nth_protocol::{BoxError, ContextOverflow, Request, Retry, StreamEvent};

use crate::{
    http::{self, STREAM_IDLE_TIMEOUT, retry_after, retryable, transient},
    sse,
};

/// A protocol's in-stream error and the policy that reads it. Kept per
/// protocol, since each names its retryable failures differently: a
/// chat completions error without a numeric status is not retried, though
/// the same words from another protocol would be.
pub(crate) trait Payload: std::error::Error + Send + Sync + 'static {
    /// A rate limit or a server failure reported after the 200 status.
    fn retryable(&self) -> bool;

    /// A refusal as too long reported after the 200 status, which is
    /// compacted for like one refused upfront. Only chat completions'
    /// gateways say so in-stream.
    fn context_overflow(&self) -> bool {
        false
    }

    /// The message a refused request's body carries, where the protocol
    /// knows the body's shape; `None` keeps the whole body.
    fn status_message(_body: &str) -> Option<String> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error<P: Payload> {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{status}: {message}")]
    Status {
        status: reqwest::StatusCode,
        /// What the refusal says: the error body's message where the
        /// protocol knows its shape, else the whole body.
        message: String,
        /// The server's `Retry-After`, when it sent one.
        retry_after: Option<Duration>,
    },
    #[error("bad stream event: {source} in {line:?}")]
    Parse {
        source: serde_json::Error,
        /// The start of the offending line; see `crate::sse::EXCERPT_CHARS`.
        line: String,
    },
    /// Transparent, so each protocol keeps the wording of its own.
    #[error(transparent)]
    Provider(P),
    #[error("stream ended before the response was complete")]
    Incomplete,
    #[error("stream stalled: nothing arrived for {}s", STREAM_IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("response hit the output token limit")]
    Truncated,
}

/// One endpoint's route for protocol `P`. Each protocol's `stream` signs
/// the request and sets its body, then hands it to [`send`].
pub(crate) struct Client<P> {
    http: reqwest::Client,
    base_url: String,
    protocol: PhantomData<fn() -> P>,
}

impl<P: Protocol> Client<P> {
    pub(crate) fn new(http: reqwest::Client, base_url: String) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            protocol: PhantomData,
        }
    }

    /// A request to the protocol's route, not yet signed.
    pub(crate) fn post(&self) -> reqwest::RequestBuilder {
        self.http.post(format!("{}{}", self.base_url, P::ROUTE))
    }
}

/// Sends `builder`, signed and with its body set, as `request`'s session,
/// and streams the reply once the server accepted it.
pub(crate) async fn send<P: Protocol>(
    builder: reqwest::RequestBuilder,
    request: &Request<'_>,
) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
    let response = http::session(builder, request)
        .send()
        .await
        .map_err(Error::<P::Provider>::from)?;
    let response = success::<P::Provider>(response).await?;
    let bytes = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(Error::from))
        .boxed();
    Ok(events::<P, _>(bytes).map(|e| e.map_err(boxed)).boxed())
}

/// Turns a non-2xx response into the stream's error: the shared
/// [`ContextOverflow`] marker when the body is a refusal as too long, so
/// the loop compacts and replays, else the status error it always was.
async fn success<P: Payload>(response: reqwest::Response) -> Result<reqwest::Response, BoxError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Err(status_error::<P>(status, body, retry_after))
}

/// A failed response as its stream's error; apart from `success` so the
/// classification is testable without an HTTP response in hand.
fn status_error<P: Payload>(
    status: reqwest::StatusCode,
    body: String,
    retry_after: Option<Duration>,
) -> BoxError {
    if http::context_overflow(status, &body) {
        return Box::new(ContextOverflow);
    }
    let message = P::status_message(&body).unwrap_or(body);
    Box::new(Error::<P>::Status {
        status,
        message,
        retry_after,
    })
}

/// A stream's error as the session sees it: an in-stream refusal as too
/// long becomes the shared [`ContextOverflow`] marker, so a gateway that
/// reports one after the 200 status is compacted like one that refused
/// upfront. Everything else stays the protocol's error.
pub(crate) fn boxed<P: Payload>(error: Error<P>) -> BoxError {
    if let Error::Provider(payload) = &error
        && payload.context_overflow()
    {
        return Box::new(ContextOverflow);
    }
    Box::new(error)
}

/// Whether `error`, one a client of protocol `P` produced, may be retried.
/// Reads only the error, so whatever fronts the clients answers the same
/// way; an error of another protocol is none of its business.
pub(crate) fn retry<P: Payload>(error: &BoxError) -> Option<Retry> {
    match error.downcast_ref::<Error<P>>()? {
        // A rate limit or a transient server failure; the server may have
        // said how long to wait.
        Error::Status {
            status,
            retry_after,
            ..
        } if transient(*status) => Some(Retry {
            after: *retry_after,
        }),
        Error::Provider(payload) if payload.retryable() => Some(Retry { after: None }),
        Error::Http(e) if retryable(e) => Some(Retry { after: None }),
        // The stream ended, or went quiet, before the reply did.
        Error::Incomplete | Error::Stalled => Some(Retry { after: None }),
        _ => None,
    }
}

/// What a protocol hands on while it reads its events.
pub(crate) type Out<P> = VecDeque<Result<StreamEvent, Error<P>>>;

/// Whether the protocol's closing event arrived and it emitted the end.
pub(crate) enum Flow {
    Continue,
    Done,
}

/// One wire protocol's stream, as the reply it assembles from its events.
/// A fresh value (`Default`) reads one response.
pub(crate) trait Protocol: Default + Send + 'static {
    type Provider: Payload;
    type Event: Send;
    /// Where requests go, under the endpoint's base URL.
    const ROUTE: &'static str;

    /// One line's events, or the protocol's parse or in-stream error.
    fn parse(line: &str) -> Result<Vec<Self::Event>, Error<Self::Provider>>;

    /// Takes in one event, pushing what goes out; `Done` once the closing
    /// event ended the reply, after pushing its end.
    fn apply(&mut self, event: Self::Event, out: &mut Out<Self::Provider>) -> Flow;

    /// Whether the reply's end marker (a finish or stop reason) arrived
    /// though its closing event has not: what the connection loses after
    /// it is at most that event, so a body error, a stall or the end of the
    /// bytes then ends the reply rather than failing it.
    fn complete(&self) -> bool {
        false
    }

    /// Pushes the end of a reply that is `complete` but never got its
    /// closing event.
    fn finish(&mut self, _out: &mut Out<Self::Provider>) {}
}

type Parse<P> = fn(&str) -> Result<Vec<<P as Protocol>::Event>, Error<<P as Protocol>::Provider>>;

struct Driver<P: Protocol, S> {
    bytes: S,
    parser: sse::Parser<Parse<P>>,
    reply: P,
    pending: Out<P::Provider>,
    finished: bool,
}

/// A response's bytes as the reply's events, the first error ending them.
/// Text and reasoning go out as they arrive; what the protocol holds back
/// (tool calls, which may be partial until the reply is complete) goes
/// out at its end.
pub(crate) fn events<P, S>(bytes: S) -> impl Stream<Item = Result<StreamEvent, Error<P::Provider>>>
where
    P: Protocol,
    S: Stream<Item = Result<bytes::Bytes, Error<P::Provider>>> + Unpin,
{
    let driver = Driver::<P, S> {
        bytes,
        parser: sse::Parser::new(P::parse),
        reply: P::default(),
        pending: VecDeque::new(),
        finished: false,
    };
    futures::stream::unfold(driver, |mut d| async move {
        loop {
            if let Some(event) = d.pending.pop_front() {
                return Some((event, d));
            }
            if d.finished {
                return None;
            }
            let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, d.bytes.next()).await;
            // Above the stall: a reply already complete is not failed by
            // the connection going quiet either.
            let (parsed, ended) = match next {
                Ok(Some(Ok(chunk))) => (d.parser.push(&chunk), false),
                Ok(Some(Err(_))) | Err(_) if d.reply.complete() => (Ok(Vec::new()), true),
                Ok(Some(Err(e))) => (Err(e), false),
                Err(_elapsed) => (Err(Error::Stalled), false),
                // A remainder that does not parse is a line the connection
                // cut short, which the end-of-stream rules below describe
                // better than a parse error would.
                Ok(None) => (Ok(d.parser.finish().unwrap_or_default()), true),
            };
            let parsed = match parsed {
                Ok(parsed) => parsed,
                Err(e) => {
                    d.finished = true;
                    return Some((Err(e), d));
                }
            };
            for event in parsed {
                if let Flow::Done = d.reply.apply(event, &mut d.pending) {
                    d.finished = true;
                    break;
                }
            }
            if ended && !d.finished {
                d.finished = true;
                // Some servers close right after the end marker without the
                // closing event; without either, the connection was cut
                // mid-reply.
                if d.reply.complete() {
                    d.reply.finish(&mut d.pending);
                } else {
                    d.pending.push_back(Err(Error::Incomplete));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::serve_status;

    /// A protocol with no events of its own, for what every protocol
    /// shares: how a refused request and a failed stream are classified.
    #[derive(Default)]
    struct Null;

    #[derive(Debug, thiserror::Error)]
    #[error("fault {kind}")]
    struct Fault {
        kind: &'static str,
    }

    impl Payload for Fault {
        fn retryable(&self) -> bool {
            self.kind == "busy"
        }
    }

    impl Protocol for Null {
        type Provider = Fault;
        type Event = ();
        const ROUTE: &'static str = "/null";

        fn parse(_line: &str) -> Result<Vec<()>, Error<Fault>> {
            Ok(Vec::new())
        }

        fn apply(&mut self, _event: (), _out: &mut Out<Fault>) -> Flow {
            Flow::Continue
        }
    }

    fn retry(error: &BoxError) -> Option<Retry> {
        super::retry::<Fault>(error)
    }

    /// The error a request refused with `status` and `body` becomes.
    async fn refused(status: u16, body: &'static [u8]) -> BoxError {
        let (url, _) = serve_status(status, body).await;
        let client = Client::<Null>::new(reqwest::Client::new(), url);
        let request = Request {
            model: "test-model",
            session_id: "ses_test",
            parent_session_id: None,
            effort: Default::default(),
            messages: &[],
            tools: &[],
        };
        send::<Null>(client.post(), &request)
            .await
            .err()
            .expect("refused")
    }

    /// OpenAI's refusal, as its API sends it.
    const OVERFLOW: &[u8] = br#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 8896 tokens (8896 in the messages, 0 in the completion). Please reduce the length of the messages.","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#;
    /// The refusal phrased in a nested field, and as plain text: the
    /// classification reads the whole body, not one field of it.
    const NESTED_OVERFLOW: &[u8] = br#"{"error":{"message":"Request failed","detail":{"reason":"maximum context length exceeded"}}}"#;
    const PLAIN_OVERFLOW: &[u8] = b"Input is too long for requested model";

    #[tokio::test]
    async fn any_overflow_phrasing_on_a_400_413_or_422_becomes_a_context_overflow() {
        for status in [400, 413, 422] {
            for body in [NESTED_OVERFLOW, PLAIN_OVERFLOW] {
                let error = refused(status, body).await;
                assert!(
                    error.downcast_ref::<ContextOverflow>().is_some(),
                    "{status} with {}: {error}",
                    String::from_utf8_lossy(body)
                );
                assert_eq!(retry(&error), None, "compacting, not retrying, is the fix");
            }
        }
    }

    #[tokio::test]
    async fn an_overflow_body_on_a_transient_status_stays_a_status_error() {
        for status in [429, 500] {
            let error = refused(status, OVERFLOW).await;
            assert!(
                error.downcast_ref::<ContextOverflow>().is_none(),
                "{status}: {error}"
            );
            let expected = reqwest::StatusCode::from_u16(status).expect("a status");
            assert!(
                matches!(
                    error.downcast_ref::<Error<Fault>>(),
                    Some(Error::Status { status, .. }) if *status == expected
                ),
                "{error}"
            );
            assert!(retry(&error).is_some(), "{status} is worth retrying");
        }
    }

    #[test]
    fn only_an_overflow_body_on_an_overflow_status_is_marked() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        let marked = |status, body: &str| {
            status_error::<Fault>(status, body.to_string(), None)
                .downcast_ref::<ContextOverflow>()
                .is_some()
        };

        assert!(marked(
            bad_request,
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(marked(
            bad_request,
            "prompt is too long: 213866 tokens > 200000 maximum"
        ));
        assert!(marked(bad_request, r#"{"code":"context_length_exceeded"}"#));
        assert!(marked(
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "input is too long"
        ));
        assert!(marked(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "PROMPT IS TOO LONG"
        ));

        assert!(!marked(
            bad_request,
            r#"{"error":{"message":"invalid model"}}"#
        ));
        assert!(!marked(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "maximum context length is 8192 tokens"
        ));
        assert!(!marked(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "prompt is too long"
        ));

        let body = r#"{"error":{"message":"invalid model"}}"#;
        let ordinary = status_error::<Fault>(bad_request, body.into(), None);
        assert!(
            matches!(
                ordinary.downcast_ref::<Error<Fault>>(),
                Some(Error::Status { status, message, .. })
                    if *status == bad_request && message == body
            ),
            "a payload that does not read the body keeps all of it"
        );
        assert_eq!(retry(&ordinary), None);

        let limited =
            status_error::<Fault>(reqwest::StatusCode::TOO_MANY_REQUESTS, String::new(), None);
        assert_eq!(retry(&limited), Some(Retry { after: None }));
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        let boxed = |error: Error<Fault>| -> BoxError { Box::new(error) };
        let status = |status, retry_after| {
            boxed(Error::Status {
                status,
                message: String::new(),
                retry_after,
            })
        };

        assert_eq!(
            retry(&status(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                Some(Duration::from_secs(7))
            )),
            Some(Retry {
                after: Some(Duration::from_secs(7))
            })
        );
        assert_eq!(
            retry(&status(reqwest::StatusCode::SERVICE_UNAVAILABLE, None)),
            Some(Retry { after: None })
        );
        let overloaded = reqwest::StatusCode::from_u16(529).expect("valid status");
        assert_eq!(
            retry(&status(overloaded, None)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&status(reqwest::StatusCode::BAD_REQUEST, None)), None);
        assert_eq!(
            retry(&status(reqwest::StatusCode::UNAUTHORIZED, None)),
            None
        );

        assert_eq!(
            retry(&boxed(Error::Provider(Fault { kind: "busy" }))),
            Some(Retry { after: None }),
            "as the payload says"
        );
        assert_eq!(retry(&boxed(Error::Provider(Fault { kind: "nope" }))), None);

        let overflow: BoxError = Box::new(ContextOverflow);
        assert_eq!(
            retry(&overflow),
            None,
            "compacting, not retrying, is the fix"
        );

        assert_eq!(
            retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&boxed(Error::Stalled)), Some(Retry { after: None }));
        assert_eq!(retry(&boxed(Error::Truncated)), None);

        let bad_url = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("not a url");
        assert_eq!(retry(&boxed(Error::Http(bad_url))), None);
    }
}
