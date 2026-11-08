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

/// The text a reply streamed, every event having come out as no error:
/// what a reply cut at the output limit must still give.
pub(crate) fn text<E: std::fmt::Debug>(events: Vec<Result<StreamEvent, E>>) -> Vec<String> {
    events
        .into_iter()
        .filter_map(|e| match e.expect("no error") {
            StreamEvent::TextDelta(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// A `*_cut.sse` fixture without the lines that stream its text: a reply
/// cut before it said anything, having only thought, if that.
pub(crate) fn without_text(fixture: &[u8]) -> String {
    let fixture = std::str::from_utf8(fixture).expect("utf-8 fixture");
    fixture
        .lines()
        .filter(|line| !line.contains("The essay so far") && !line.contains(" runs lo"))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Whether the reply reported a usage of `output` tokens.
pub(crate) fn reports_output<E>(events: &[Result<StreamEvent, E>], output: u64) -> bool {
    events
        .iter()
        .any(|e| matches!(e, Ok(StreamEvent::Usage(usage)) if usage.output == output))
}

/// A server on a free local port that answers one request with `body` as
/// an event stream, and hands back the request's head (its request line
/// and headers) and its body.
pub(crate) async fn serve_once(
    body: &'static [u8],
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    serve(200, "text/event-stream", body).await
}

/// As `serve_once`, but answering with a JSON body, as a list does.
pub(crate) async fn serve_json(
    body: &'static [u8],
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    serve(200, "application/json", body).await
}

/// As `serve_once`, but answering with a non-2xx `status` and a JSON body,
/// as a refused request does.
pub(crate) async fn serve_status(
    status: u16,
    body: &'static [u8],
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    serve(status, "application/json", body).await
}

/// The reason phrase for the statuses the tests serve; HTTP parsers ignore
/// it, so whatever it says will do.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

/// A server on a free local port that accepts a connection and never
/// writes to it, as a server that hangs before its headers does.
pub(crate) async fn serve_silent() -> String {
    let (listener, url) = listen().await;
    tokio::spawn(async move {
        // Held, not dropped: closing would end the request at once.
        let _connection = listener.accept().await.expect("a request");
        std::future::pending::<()>().await;
    });
    url
}

/// A server on a free local port that refuses one request with `status`
/// and `Retry-After: retry_after` seconds, then never sends the body its
/// headers promise, as a proxy that holds a refusal's body back does.
pub(crate) async fn serve_unfinished(status: u16, retry_after: u64) -> String {
    use tokio::io::AsyncWriteExt;

    let (listener, url) = listen().await;
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("a request");
        read_request(&mut stream).await;
        let head = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: 64\r\nretry-after: {retry_after}\r\n\r\n",
            reason(status),
        );
        stream.write_all(head.as_bytes()).await.expect("writable");
        std::future::pending::<()>().await;
    });
    url
}

async fn listen() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let url = format!("http://{}", listener.local_addr().expect("bound"));
    (listener, url)
}

/// The whole request, as its head (request line and headers) and its body:
/// closing on unread bytes would reset the connection under the reply.
async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, String) {
    use tokio::io::AsyncReadExt;

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
    let body = String::from_utf8_lossy(&request[head.len() + 4..]).into_owned();
    (head, body)
}

async fn serve(
    status: u16,
    content_type: &'static str,
    body: &'static [u8],
) -> (String, tokio::sync::oneshot::Receiver<(String, String)>) {
    use tokio::io::AsyncWriteExt;

    let (listener, url) = listen().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("a request");
        let request = read_request(&mut stream).await;
        let reply = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            reason(status),
            body.len()
        );
        stream.write_all(reply.as_bytes()).await.expect("writable");
        stream.write_all(body).await.expect("writable");
        let _ = tx.send(request);
    });
    (url, rx)
}
