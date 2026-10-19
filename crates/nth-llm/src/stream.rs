//! What the three protocols' streams share once their bytes are events:
//! one error shape, and which of its kinds a retry or a compaction fixes.
//! What differs is the error a protocol reports inside the stream, after
//! the 200 status, and how it reads it: that is its `Payload`.

use std::time::Duration;

use nth_protocol::{BoxError, ContextOverflow, Retry};

use crate::http::{STREAM_IDLE_TIMEOUT, retryable, transient};

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
