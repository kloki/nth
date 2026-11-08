//! What the three protocols' streams share: the loop that turns a
//! response's bytes into `StreamEvent`s, one error shape, and which of its
//! kinds a retry or a compaction fixes. What differs is how a protocol
//! reads its events into a reply (its `Protocol`), how it reads a refused
//! request's body (its `Refusal`) and how it classifies the error it
//! reports inside the stream, after the 200 status, into
//! [`Error::Provider`].

use std::{collections::VecDeque, time::Duration};

use futures::{Stream, StreamExt, stream::BoxStream};
use nth_protocol::{BoxError, ContextOverflow, Request, Retry, StreamEvent};

use crate::{
    http::{
        self, ERROR_BODY_TIMEOUT, RESPONSE_TIMEOUT, STREAM_IDLE_TIMEOUT, retry_after, retryable,
        transient,
    },
    sse,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
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
        /// The body said retrying cannot help, whatever the status.
        final_refusal: bool,
    },
    #[error("bad stream event: {source} in {line:?}")]
    Parse {
        source: serde_json::Error,
        /// The start of the offending line; see `crate::sse::EXCERPT_CHARS`.
        line: String,
    },
    /// An error the provider reported inside its stream, after the 200
    /// status, classified by the protocol that read it, since each names
    /// its retryable failures differently.
    #[error("{message}")]
    Provider {
        /// In the wording of the protocol that reported it.
        message: String,
        /// A rate limit or a server failure.
        retryable: bool,
        /// A refusal as too long, which is compacted for like one refused
        /// upfront. Only chat completions' gateways say so in-stream.
        overflow: bool,
    },
    /// Signing the request failed before it was sent: a ChatGPT login
    /// whose token could not be refreshed. Classified by
    /// `providers::auth_error`, which knows the login's errors.
    #[error("{message}")]
    Auth {
        /// The login's own wording, which says what to do about it.
        message: String,
        /// The token server could not be reached or failed, so the same
        /// refresh may work in a moment.
        retryable: bool,
    },
    #[error("stream ended before the response was complete")]
    Incomplete,
    #[error("stream stalled: nothing arrived for {}s", STREAM_IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("no response within {}s", RESPONSE_TIMEOUT.as_secs())]
    Unanswered,
}

/// How a protocol reads a refused request's body, where it knows the
/// body's shape.
#[derive(Clone, Copy)]
pub(crate) struct Refusal {
    /// The message the body carries; `None` keeps the whole body.
    pub(crate) message: fn(&str) -> Option<String>,
    /// Whether the body says waiting cannot help, though its status (a
    /// 429, say) would otherwise be retried.
    pub(crate) is_final: fn(&str) -> bool,
}

impl Refusal {
    /// For a protocol that knows nothing of the body: all of it is the
    /// message, and only the status decides a retry.
    pub(crate) const PLAIN: Refusal = Refusal {
        message: |_| None,
        is_final: |_| false,
    };
}

/// One endpoint's route for one protocol. Each protocol's `stream` signs
/// the request and sets its body, then hands it to [`send`].
pub(crate) struct Client {
    http: reqwest::Client,
    base_url: String,
    route: &'static str,
}

impl Client {
    /// `route` is where the protocol's requests go, under `base_url`.
    pub(crate) fn new(http: reqwest::Client, base_url: String, route: &'static str) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            route,
        }
    }

    /// A request to the protocol's route, not yet signed.
    pub(crate) fn post(&self) -> reqwest::RequestBuilder {
        self.http.post(format!("{}{}", self.base_url, self.route))
    }
}

/// Sends `builder`, signed and with its body set, as `request`'s session,
/// and streams the reply once the server accepted it; a refusal's body is
/// read as `refusal` says.
pub(crate) async fn send<P: Protocol>(
    builder: reqwest::RequestBuilder,
    request: &Request<'_>,
    refusal: Refusal,
) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
    let response = send_within(builder, request).await?;
    let response = success(response, refusal).await?;
    let bytes = response
        .bytes_stream()
        .map(|chunk| chunk.map_err(Error::from))
        .boxed();
    Ok(events::<P, _>(bytes).map(|e| e.map_err(boxed)).boxed())
}

/// Sends `builder` as `request`'s session, failing as `Unanswered` when
/// the response's headers take longer than [`RESPONSE_TIMEOUT`]. Only the
/// headers: the body streams under the idle deadline instead.
async fn send_within(
    builder: reqwest::RequestBuilder,
    request: &Request<'_>,
) -> Result<reqwest::Response, Error> {
    let sent = http::session(builder, request).send();
    match tokio::time::timeout(RESPONSE_TIMEOUT, sent).await {
        Ok(response) => response.map_err(Error::from),
        Err(_elapsed) => Err(Error::Unanswered),
    }
}

/// Turns a non-2xx response into the stream's error: the shared
/// [`ContextOverflow`] marker when the body is a refusal as too long, so
/// the loop compacts and replays, else the status error it always was.
async fn success(
    response: reqwest::Response,
    refusal: Refusal,
) -> Result<reqwest::Response, BoxError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = tokio::time::timeout(ERROR_BODY_TIMEOUT, response.text())
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Err(status_error(status, body, retry_after, refusal))
}

/// A failed response as its stream's error; apart from `success` so the
/// classification is testable without an HTTP response in hand.
pub(crate) fn status_error(
    status: reqwest::StatusCode,
    body: String,
    retry_after: Option<Duration>,
    refusal: Refusal,
) -> BoxError {
    if http::context_overflow(status, &body) {
        return Box::new(ContextOverflow);
    }
    let final_refusal = (refusal.is_final)(&body);
    let message = (refusal.message)(&body).unwrap_or(body);
    Box::new(Error::Status {
        status,
        message,
        retry_after,
        final_refusal,
    })
}

/// A stream's error as the session sees it: an in-stream refusal as too
/// long becomes the shared [`ContextOverflow`] marker, so a gateway that
/// reports one after the 200 status is compacted like one that refused
/// upfront. Everything else stays the stream's error.
pub(crate) fn boxed(error: Error) -> BoxError {
    match error {
        Error::Provider { overflow: true, .. } => Box::new(ContextOverflow),
        error => Box::new(error),
    }
}

/// Whether `error`, as the session holds it, may be retried: only the
/// stream's own error ever is, a [`ContextOverflow`] being compacted for
/// instead. Reads only the error, so whatever fronts the clients answers
/// the same way for every protocol.
pub(crate) fn retry_boxed(error: &BoxError) -> Option<Retry> {
    retry(error.downcast_ref::<Error>()?)
}

fn retry(error: &Error) -> Option<Retry> {
    match error {
        // A rate limit or a transient server failure; the server may have
        // said how long to wait.
        Error::Status {
            status,
            retry_after,
            final_refusal: false,
            ..
        } if transient(*status) => Some(Retry {
            after: *retry_after,
        }),
        Error::Provider {
            retryable: true, ..
        }
        | Error::Auth {
            retryable: true, ..
        } => Some(Retry { after: None }),
        Error::Http(e) if retryable(e) => Some(Retry { after: None }),
        // The server never answered, or the stream ended or went quiet
        // before the reply did.
        Error::Unanswered | Error::Incomplete | Error::Stalled => Some(Retry { after: None }),
        _ => None,
    }
}

/// What a protocol hands on while it reads its events.
pub(crate) type Out = VecDeque<Result<StreamEvent, Error>>;

/// Ends a reply cut at the model's output limit. Its text is an answer that
/// ran long, so it is kept and the turn ends as answered; its tool calls
/// have partial arguments and must not run, so the caller drops them. The
/// note tells the model and the user why the reply stops where it does, and
/// a model whose calls were dropped why nothing ran. It is set apart from
/// the reply's text, but a reply with none is the note alone, never one
/// that starts with blank lines.
pub(crate) fn cut(after_text: bool, calls_dropped: bool, out: &mut Out) {
    let note = if calls_dropped {
        CUT_NOTE_WITH_CALLS
    } else {
        CUT_NOTE
    };
    let note = if after_text {
        format!("\n\n{note}")
    } else {
        note.to_string()
    };
    out.push_back(Ok(StreamEvent::TextDelta(note)));
}

pub(crate) const CUT_NOTE: &str = "[reply cut at the output token limit]";
pub(crate) const CUT_NOTE_WITH_CALLS: &str =
    "[reply cut at the output token limit; its tool calls were dropped]";

/// Whether the protocol's closing event arrived and it emitted the end.
pub(crate) enum Flow {
    Continue,
    Done,
}

/// One wire protocol's stream, as the reply it assembles from its events.
/// A fresh value (`Default`) reads one response.
pub(crate) trait Protocol: Default + Send + 'static {
    type Event: Send;

    /// One line's events, or the protocol's parse or in-stream error.
    fn parse(line: &str) -> Result<Vec<Self::Event>, Error>;

    /// Takes in one event, pushing what goes out; `Done` once the closing
    /// event ended the reply, after pushing its end.
    fn apply(&mut self, event: Self::Event, out: &mut Out) -> Flow;

    /// Whether the reply's end marker (a finish or stop reason) arrived
    /// though its closing event has not: what the connection loses after
    /// it is at most that event, so a body error, a stall or the end of the
    /// bytes then ends the reply rather than failing it.
    fn complete(&self) -> bool {
        false
    }

    /// Pushes the end of a reply that is `complete` but never got its
    /// closing event.
    fn finish(&mut self, _out: &mut Out) {}
}

struct Driver<P: Protocol, S> {
    bytes: S,
    parser: sse::Parser<P::Event>,
    reply: P,
    pending: Out,
    finished: bool,
}

/// A response's bytes as the reply's events, the first error ending them.
/// Text and reasoning go out as they arrive; what the protocol holds back
/// (tool calls, which may be partial until the reply is complete) goes
/// out at its end.
pub(crate) fn events<P, S>(bytes: S) -> impl Stream<Item = Result<StreamEvent, Error>>
where
    P: Protocol,
    S: Stream<Item = Result<bytes::Bytes, Error>> + Unpin,
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
    use crate::testing::{retry, serve_silent, serve_status, serve_unfinished};

    /// A protocol with no events of its own, for what every protocol
    /// shares: how a refused request and a failed stream are classified.
    #[derive(Default)]
    struct Null;

    impl Protocol for Null {
        type Event = ();

        fn parse(_line: &str) -> Result<Vec<()>, Error> {
            Ok(Vec::new())
        }

        fn apply(&mut self, _event: (), _out: &mut Out) -> Flow {
            Flow::Continue
        }
    }

    /// The error a request to `url` fails with.
    async fn failed(url: String) -> BoxError {
        let client = Client::new(reqwest::Client::new(), url, "/null");
        let request = Request {
            model: "test-model",
            session_id: "ses_test",
            parent_session_id: None,
            effort: Default::default(),
            messages: &[],
            tools: &[],
        };
        send::<Null>(client.post(), &request, Refusal::PLAIN)
            .await
            .err()
            .expect("failed")
    }

    /// The error a request refused with `status` and `body` becomes.
    async fn refused(status: u16, body: &'static [u8]) -> BoxError {
        let (url, _) = serve_status(status, body).await;
        failed(url).await
    }

    // Paused, the clock jumps to the deadline whenever nothing is ready;
    // whether that is before or after the connection opens, the server
    // has not answered.
    #[tokio::test(start_paused = true)]
    async fn a_server_that_never_answers_the_headers_fails_as_unanswered_and_is_retried() {
        let start = tokio::time::Instant::now();
        // Free on the paused clock; fails a regression instead of hanging.
        let error = tokio::time::timeout(RESPONSE_TIMEOUT * 2, async {
            failed(serve_silent().await).await
        })
        .await
        .expect("answered within the deadline");

        assert!(
            matches!(error.downcast_ref::<Error>(), Some(Error::Unanswered)),
            "{error}"
        );
        assert_eq!(error.to_string(), "no response within 300s");
        assert!(start.elapsed() >= RESPONSE_TIMEOUT);
        assert_eq!(retry(&error), Some(Retry { after: None }));
    }

    #[tokio::test]
    async fn a_refusal_whose_body_never_arrives_still_classifies_by_status() {
        let url = serve_unfinished(429, 7).await;
        // The headers on real time, then the clock paused: it would jump
        // past the response deadline too, were the headers still on their
        // way.
        let response = reqwest::Client::new()
            .post(url)
            .send()
            .await
            .expect("headers");
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        // Free on the paused clock; fails a regression instead of hanging.
        let error = tokio::time::timeout(ERROR_BODY_TIMEOUT * 2, success(response, Refusal::PLAIN))
            .await
            .expect("classified within the deadline")
            .expect_err("refused");

        assert!(start.elapsed() >= ERROR_BODY_TIMEOUT);
        assert!(
            matches!(
                error.downcast_ref::<Error>(),
                Some(Error::Status { status, message, retry_after, .. })
                    if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        && message.is_empty()
                        && *retry_after == Some(Duration::from_secs(7))
            ),
            "{error}"
        );
        assert_eq!(
            retry(&error),
            Some(Retry {
                after: Some(Duration::from_secs(7))
            })
        );
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
                    error.downcast_ref::<Error>(),
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
            status_error(status, body.to_string(), None, Refusal::PLAIN)
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
        let ordinary = status_error(bad_request, body.into(), None, Refusal::PLAIN);
        assert!(
            matches!(
                ordinary.downcast_ref::<Error>(),
                Some(Error::Status { status, message, .. })
                    if *status == bad_request && message == body
            ),
            "a payload that does not read the body keeps all of it"
        );
        assert_eq!(retry(&ordinary), None);

        let limited = status_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            String::new(),
            None,
            Refusal::PLAIN,
        );
        assert_eq!(retry(&limited), Some(Retry { after: None }));
    }

    /// Which errors a retry fixes: a transient status, an in-stream error
    /// its protocol marked retryable, and a stream that never answered,
    /// stalled or was cut. Each protocol's tests pin how it marks its own
    /// in-stream errors.
    #[test]
    fn only_transient_errors_are_retryable() {
        let boxed = |error: Error| -> BoxError { Box::new(error) };
        let status = |status, retry_after| {
            boxed(Error::Status {
                status,
                message: String::new(),
                retry_after,
                final_refusal: false,
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

        let in_stream = |retryable| {
            boxed(Error::Provider {
                message: "from the stream".into(),
                retryable,
                overflow: false,
            })
        };
        assert_eq!(
            retry(&in_stream(true)),
            Some(Retry { after: None }),
            "as the protocol classified it"
        );
        assert_eq!(retry(&in_stream(false)), None);

        let overflow: BoxError = Box::new(ContextOverflow);
        assert_eq!(
            retry(&overflow),
            None,
            "compacting, not retrying, is the fix"
        );
        let in_stream_overflow = super::boxed(Error::Provider {
            message: "too long".into(),
            retryable: false,
            overflow: true,
        });
        assert!(
            in_stream_overflow
                .downcast_ref::<ContextOverflow>()
                .is_some(),
            "{in_stream_overflow}"
        );

        assert_eq!(
            retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&boxed(Error::Stalled)), Some(Retry { after: None }));
        assert_eq!(
            retry(&boxed(Error::Unanswered)),
            Some(Retry { after: None })
        );

        let bad_url = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("not a url");
        assert_eq!(retry(&boxed(Error::Http(bad_url))), None);
    }
}
