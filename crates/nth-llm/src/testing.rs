//! What the three protocols' stream tests share: a recorded fixture fed in
//! chunks of any size, cut at a marker, and checked for tool calls.

use nth_protocol::StreamEvent;

/// `input` as the chunks a connection might deliver it in.
pub(crate) fn chunks<E>(input: &[u8], chunk_size: usize) -> Vec<Result<bytes::Bytes, E>> {
    input
        .chunks(chunk_size)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect()
}

/// `fixture` up to where `marker` starts, as a connection cut there would
/// have delivered it.
pub(crate) fn until<'a>(fixture: &'a [u8], marker: &str) -> &'a [u8] {
    let text = std::str::from_utf8(fixture).expect("utf-8 fixture");
    let end = text.find(marker).expect("marker in fixture");
    &fixture[..end]
}

/// Whether no tool call came out: what a stream that ends badly must
/// guarantee, since its calls may be partial.
pub(crate) fn runs_no_tools<E>(events: &[Result<StreamEvent, E>]) -> bool {
    !events
        .iter()
        .any(|e| matches!(e, Ok(StreamEvent::ToolCall(_))))
}

/// A server on a free local port that answers one request with `body` as
/// an event stream, and hands back the request's head (its request line
/// and headers) and its body.
pub(crate) async fn serve_once(
    body: &'static [u8],
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("bound"));
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("a request");
        // The whole request, body included: closing on unread bytes would
        // reset the connection under the reply.
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        let head = loop {
            let n = stream.read(&mut buf).await.expect("readable");
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break String::from_utf8_lossy(&request[..end]).into_owned();
            }
            assert!(n > 0, "the request ended inside its head");
        };
        let length = head
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
            })
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < head.len() + 4 + length {
            let n = stream.read(&mut buf).await.expect("readable");
            assert!(n > 0, "the request ended inside its body");
            request.extend_from_slice(&buf[..n]);
        }
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(reply.as_bytes()).await.expect("writable");
        stream.write_all(body).await.expect("writable");
        let sent = String::from_utf8_lossy(&request[head.len() + 4..]).into_owned();
        let _ = tx.send((head, sent));
    });
    (url, rx)
}
