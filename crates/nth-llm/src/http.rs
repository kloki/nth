//! What every protocol's requests share: the one HTTP client, the headers
//! and deadlines they agree on, and the facts about an HTTP failure that
//! decide a retry. Nothing here knows any protocol's shapes.

use std::time::Duration;

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
}
