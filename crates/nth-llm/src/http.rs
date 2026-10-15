//! What every protocol's requests share: the one HTTP client, the headers
//! and deadlines they agree on, and the facts about an HTTP failure that
//! decide a retry and tell a refusal as too long from the rest. Nothing
//! here knows any protocol's shapes.

use std::time::Duration;

use nth_protocol::Request;

pub(crate) const USER_AGENT: &str = concat!("nth/", env!("CARGO_PKG_VERSION"));
/// A server that does not answer the handshake this fast is down or
/// unreachable; waiting longer only delays the retry.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest silence tolerated between two chunks of a reply. A whole
/// reply may take minutes, so there is no overall deadline, but a live
/// stream keeps sending (if only keep-alive comments), while a dead
/// connection would otherwise hang the turn forever.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How every endpoint is reached; `Providers` shares one between them.
pub(crate) fn client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
}

/// Which session `request` is for, as opencode names it: Go routes and
/// caches on `x-opencode-session`, and the parent ties a subagent's
/// requests to the session that started it.
pub(crate) fn session(
    builder: reqwest::RequestBuilder,
    request: &Request<'_>,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header("x-opencode-session", request.session_id)
        .header("x-opencode-session-id", request.session_id);
    match request.parent_session_id {
        Some(parent) => builder.header("x-opencode-parent-session-id", parent),
        None => builder,
    }
}

/// How long the server asked to wait, from `Retry-After-Ms` or the usual
/// `Retry-After` in seconds; the HTTP-date form is not supported, and a
/// value no `Duration` holds (negative, `inf`, huge) counts as absent.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok())
    };
    if let Some(ms) = number("retry-after-ms") {
        return Duration::try_from_secs_f64(ms / 1000.0).ok();
    }
    number("retry-after").and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
}

/// A rate limit or a transient server failure, worth trying again.
pub(crate) fn transient(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Whether the request itself may be retried: the connection failed or
/// timed out, possibly part-way through the stream. Other errors (a bad
/// URL, an undecodable body) fail the same way every time.
pub(crate) fn retryable(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request() || error.is_body()
}

/// How providers phrase a refusal as the input being too long, lowercased:
/// OpenAI's `context_length_exceeded`, Anthropic's `prompt is too long`,
/// Gemini's `exceeds the maximum`, and the rest of the family. Every phrase
/// is about the request itself; a bare `token limit` is not, since an
/// account's billing refusal says it too.
const TOO_LONG: &[&str] = &[
    "context length",
    "context_length_exceeded",
    // Anthropic's usual wording: the input plus the requested `max_tokens`
    // exceed the window.
    "context limit",
    "maximum context",
    "prompt is too long",
    "input is too long",
    "exceeds the maximum",
    "exceeds the token limit",
    "too many tokens",
];

/// Whether `text`, an error body or message, says the input is too long.
/// A substring match, case-insensitively: providers wrap the phrase in
/// JSON, punctuation and words of their own, and on a 400, 413 or 422 a
/// body that says this is a context overflow far more often than not.
pub(crate) fn too_long(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    TOO_LONG.iter().any(|pattern| lower.contains(pattern))
}

/// Whether a non-2xx response refused the request as the conversation no
/// longer fitting the model's context window. Best-effort and
/// conservative: only the statuses providers use for it, and only when the
/// body says the input is too long, so an ordinary failure stays the
/// status error it was.
pub(crate) fn context_overflow(status: reqwest::StatusCode, body: &str) -> bool {
    matches!(status.as_u16(), 400 | 413 | 422) && too_long(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_reads_seconds_or_milliseconds() {
        let header = |name: &'static str, value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(name, value.parse().expect("header"));
            headers
        };

        assert_eq!(
            retry_after(&header("retry-after", "3")),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_after(&header("retry-after-ms", "1500")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            retry_after(&header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")),
            None,
            "the HTTP-date form is not parsed"
        );
        for unholdable in ["inf", "1e30", "-1", "NaN"] {
            assert_eq!(
                retry_after(&header("retry-after", unholdable)),
                None,
                "{unholdable} is not a wait"
            );
        }
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn only_connection_failures_are_retryable() {
        let bad_url = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("not a url");
        assert!(!retryable(&bad_url));
        assert!(transient(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(transient(reqwest::StatusCode::BAD_GATEWAY));
        assert!(!transient(reqwest::StatusCode::BAD_REQUEST));
    }

    #[test]
    fn the_bodies_providers_send_for_a_too_long_request_are_recognised() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        // OpenAI chat completions and its responses API.
        assert!(context_overflow(
            bad_request,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 8896 tokens (8896 in the messages, 0 in the completion). Please reduce the length of the messages.","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#
        ));
        assert!(context_overflow(
            bad_request,
            r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#
        ));
        // Anthropic.
        assert!(context_overflow(
            bad_request,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213866 tokens > 200000 maximum"}}"#
        ));
        // Its usual wording when the input plus the requested `max_tokens`
        // exceed the window: what nth hits when it asks for a large output.
        assert!(context_overflow(
            bad_request,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"input length and `max_tokens` exceed context limit: 198157 + 32000 > 200000, decrease input length or `max_tokens` and try again"}}"#
        ));
        // Gemini, and the bare sentence some gateways send.
        assert!(context_overflow(
            bad_request,
            r#"{"error":{"code":400,"message":"The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).","status":"INVALID_ARGUMENT"}}"#
        ));
        assert!(context_overflow(
            bad_request,
            "Input is too long for requested model"
        ));
        // Cohere, and a server that only says the token limit was hit.
        assert!(context_overflow(
            bad_request,
            r#"{"message":"too many tokens: the total number of tokens must be <= 4096"}"#
        ));
        assert!(context_overflow(
            bad_request,
            r#"{"error":{"message":"the request exceeds the token limit"}}"#
        ));
        // The other statuses providers use for it.
        assert!(context_overflow(
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            r#"{"error":{"message":"input is too long"}}"#
        ));
        assert!(context_overflow(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"detail":"prompt is too long"}"#
        ));
        // Case does not matter, whatever the JSON uses.
        assert!(context_overflow(bad_request, "CONTEXT LENGTH EXCEEDED"));
        assert!(context_overflow(
            bad_request,
            "Maximum Context Length is 4096"
        ));
        assert!(too_long(
            "This model's Maximum Context Length is 4096 tokens"
        ));
    }

    #[test]
    fn an_ordinary_failure_is_not_an_overflow() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        assert!(!too_long(
            r#"{"error":{"message":"invalid model","type":"invalid_request_error"}}"#
        ));
        assert!(!too_long(
            r#"{"error":{"message":"missing required parameter: messages"}}"#
        ));
        assert!(!context_overflow(bad_request, "invalid model"));

        // A rate limit or an outage is not a refusal as too long, however
        // its body reads.
        assert!(!context_overflow(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"Rate limit reached: too many tokens per minute"}}"#
        ));
        assert!(!context_overflow(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "maximum context length exceeded"
        ));
        assert!(!context_overflow(
            reqwest::StatusCode::BAD_GATEWAY,
            "prompt is too long"
        ));
        assert!(!context_overflow(
            reqwest::StatusCode::UNAUTHORIZED,
            "the context length is not the problem"
        ));
        // Nor is a success, though `success` never asks about one.
        assert!(!context_overflow(
            reqwest::StatusCode::OK,
            "maximum context length"
        ));
        assert!(!too_long(""));
    }

    /// A billing refusal that happens to say "token limit" is about the
    /// account, not the input, so it must not be compacted for.
    #[test]
    fn a_billing_error_naming_a_token_limit_is_not_an_overflow() {
        let billing = r#"{"error":{"message":"You have reached your token limit for this month. Update your billing details to continue.","type":"insufficient_quota","code":"billing_hard_limit_reached"}}"#;

        assert!(
            !context_overflow(reqwest::StatusCode::BAD_REQUEST, billing),
            "{billing}"
        );
    }
}
