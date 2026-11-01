use std::{sync::Arc, time::Duration};

mod guard;

use futures::{FutureExt, future::BoxFuture};
use htmd::{
    HtmlToMarkdown,
    options::{BulletListMarker, HrStyle, Options},
};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::OnceCell;

use crate::output;

/// Built once, on the first fetch: a client per call would open a new
/// connection pool and read the TLS roots again every time. Built lazily
/// and fallibly, so a TLS backend that fails to set up is an error for
/// the model, not a panic.
static CLIENT: OnceCell<reqwest::Client> = OnceCell::const_new();

/// The most that is downloaded. The model gets at most
/// [`output::MAX_CHARS`] of it; the whole page is read because the
/// markdown converter needs the HTML complete.
const MAX_BYTES: usize = 5 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
/// Many sites serve a browser something better than they serve a bot.
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
/// Cloudflare challenges a browser user agent whose TLS fingerprint is not a
/// browser's, but lets an honest one through.
const HONEST_USER_AGENT: &str = concat!("nth/", env!("CARGO_PKG_VERSION"));
/// Elements whose content is not text a reader sees.
const SKIP_TAGS: [&str; 7] = [
    "script", "style", "noscript", "iframe", "object", "embed", "template",
];
/// Elements that start a new line in plain text.
const BLOCK_TAGS: [&str; 22] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dt",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "li",
    "p",
    "pre",
    "section",
    "title",
    "tr",
];

pub struct WebFetch;

#[derive(Deserialize)]
struct Args {
    url: String,
    #[serde(default)]
    format: Format,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    #[default]
    Markdown,
    Text,
    Html,
}

impl Format {
    /// Prefers the format asked for, so a server that can send it directly
    /// spares the conversion.
    fn accept(self) -> &'static str {
        match self {
            Format::Markdown => {
                "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
            }
            Format::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            Format::Html => {
                "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
            }
        }
    }
}

impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "webfetch",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to fetch content from" },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "text", "html"],
                        "default": "markdown",
                        "description": "The format to return the content in (text, markdown, or html). Defaults to markdown."
                    },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "description": "Optional timeout in milliseconds (default 30000, max 120000)" }
                },
                "required": ["url"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            crate::process::reject_old_timeout(&args)?;
            let args: Args = nth_protocol::parse_args(args)?;
            let ms = args
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS);
            let (content_type, body) =
                tokio::time::timeout(Duration::from_millis(ms), fetch(&args.url, args.format))
                    .await
                    .map_err(|_| format!("request timed out after {ms} ms"))??;

            let html = content_type.contains("text/html")
                || content_type.contains("application/xhtml+xml");
            let content = match String::from_utf8(body) {
                Ok(content) => content,
                Err(e) if content_type.starts_with("text/") || html => {
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                }
                Err(_) => return Err(format!("cannot show binary content ({content_type})")),
            };
            let content = match args.format {
                Format::Markdown if html => markdown(&content)?,
                Format::Text if html => text(&content),
                _ => content,
            };
            Ok(output::head(&content, output::MAX_CHARS))
        }
        .boxed()
    }
}

/// Returns the content type and a body of at most `MAX_BYTES`.
async fn fetch(url: &str, format: Format) -> Result<(String, Vec<u8>), String> {
    let client = CLIENT
        .get_or_try_init(|| async { client(Arc::new(guard::SystemResolver)) })
        .await
        .map_err(|e| describe(&e))?;
    fetch_with_client(client, url, format).await
}

fn client(resolver: Arc<dyn reqwest::dns::Resolve>) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(guard::GuardResolver(resolver)))
        .build()
}

async fn request(
    client: &reqwest::Client,
    url: &str,
    format: Format,
    user_agent: &str,
) -> Result<reqwest::Response, String> {
    let mut url = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    for redirects in 0..=10 {
        guard::check_url(&url)?;
        let response = client
            .get(url.clone())
            .header(header::USER_AGENT, user_agent)
            .header(header::ACCEPT, format.accept())
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|e| describe(&e))?;
        if !matches!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT
        ) {
            return Ok(response);
        }
        let Some(location) = response.headers().get(header::LOCATION) else {
            return Ok(response);
        };
        if redirects == 10 {
            return Err("too many redirects".into());
        }
        let location = location
            .to_str()
            .map_err(|e| format!("invalid redirect location: {e}"))?;
        url = url
            .join(location)
            .map_err(|e| format!("invalid redirect location: {e}"))?;
    }
    unreachable!()
}

async fn fetch_with_client(
    client: &reqwest::Client,
    url: &str,
    format: Format,
) -> Result<(String, Vec<u8>), String> {
    let mut response = request(client, url, format, BROWSER_USER_AGENT).await?;
    if response.status() == StatusCode::FORBIDDEN
        && response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|v| v == "challenge")
    {
        response = request(client, url, format, HONEST_USER_AGENT).await?;
    }
    let status = response.status();
    if !status.is_success() {
        return Err(format!("request failed with status {status}"));
    }

    let too_large = || "response too large (exceeds 5 MB limit)".to_string();
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        return Err(too_large());
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
        if body.len() + chunk.len() > MAX_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((content_type, body))
}

/// reqwest's own message rarely says what went wrong; its sources do.
fn describe(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(e) = source {
        out.push_str(&format!(": {e}"));
        source = e.source();
    }
    out
}

fn markdown(html: &str) -> ToolResult {
    let mut skip = SKIP_TAGS.to_vec();
    skip.extend(["meta", "link", "head"]);
    HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            hr_style: HrStyle::Dashes,
            ul_bullet_spacing: 1,
            ..Options::default()
        })
        .skip_tags(skip)
        .build()
        .convert(html)
        .map_err(|e| format!("cannot convert HTML to markdown: {e}"))
}

/// The text between tags, without the content of `SKIP_TAGS`, comments, or
/// blank lines.
fn text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut text = String::new();
    let mut at = 0;
    while let Some(open) = html[at..].find('<').map(|i| at + i) {
        text.push_str(&html[at..open]);
        let close = if lower[open..].starts_with("<!--") {
            lower[open..].find("-->").map(|i| open + i + 3)
        } else {
            let end = lower[open..].find('>').map(|i| open + i + 1);
            let name: String = lower[open + 1..]
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if BLOCK_TAGS.contains(&name.as_str()) {
                text.push('\n');
            }
            match end {
                Some(end)
                    if SKIP_TAGS.contains(&name.as_str()) && lower.as_bytes()[open + 1] != b'/' =>
                {
                    lower[end..]
                        .find(&format!("</{name}"))
                        .and_then(|i| lower[end + i..].find('>').map(|j| end + i + j + 1))
                }
                end => end,
            }
        };
        at = close.unwrap_or(html.len());
    }
    text.push_str(&html[at..]);

    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    let mut out = String::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    struct FixedResolver(Vec<SocketAddr>);

    impl reqwest::dns::Resolve for FixedResolver {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let addresses = self.0.clone();
            Box::pin(async move { Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs) })
        }
    }

    #[test]
    fn guards_metadata_and_link_local_literals_and_hosts() {
        for host in [
            "169.254.169.254",
            "169.254.1.2",
            "[fe80::1]",
            "[::ffff:169.254.1.2]",
            "100.100.100.200",
            "[::ffff:100.100.100.200]",
            "[fd00:ec2::254]",
            "metadata.google.internal.",
            "user@169.254.169.254:80",
            "METADATA.GOOGLE.INTERNAL",
        ] {
            let url = reqwest::Url::parse(&format!("http://{host}/")).unwrap();
            assert!(guard::check_url(&url).is_err(), "{host}");
        }
        for host in ["127.0.0.1", "192.168.1.1", "10.0.0.1", "[::1]", "[fd00::1]"] {
            let url = reqwest::Url::parse(&format!("http://{host}/")).unwrap();
            assert!(guard::check_url(&url).is_ok(), "{host}");
        }
    }

    #[tokio::test]
    async fn rejects_mixed_dns_answers_before_connecting() {
        let resolver = FixedResolver(vec![
            "127.0.0.1:0".parse().unwrap(),
            "169.254.169.254:0".parse().unwrap(),
        ]);
        let client = client(Arc::new(resolver)).unwrap();
        let err = fetch_with_client(&client, "http://example.test/", Format::Text)
            .await
            .unwrap_err();
        assert!(err.contains("169.254.169.254"), "{err}");
    }

    #[tokio::test]
    async fn rejects_redirect_to_metadata() {
        let url = serve(vec![response(
            "302 Found",
            "Location: http://169.254.169.254/latest\r\n",
            "",
        )])
        .await;
        let err = fetch(json!({"url": url})).await.unwrap_err();
        assert!(err.contains("blocked destination 169.254.169.254"), "{err}");
    }

    #[tokio::test]
    async fn dns_resolved_host_cannot_redirect_to_forbidden_host() {
        let url = serve(vec![response(
            "302 Found",
            "Location: http://metadata.google.internal./latest\r\n",
            "",
        )])
        .await;
        let port = reqwest::Url::parse(&url).unwrap().port().unwrap();
        let client = client(Arc::new(FixedResolver(vec![
            "127.0.0.1:0".parse().unwrap(),
        ])))
        .unwrap();
        let err = fetch_with_client(
            &client,
            &format!("http://public.example:{port}/page"),
            Format::Text,
        )
        .await
        .unwrap_err();
        assert!(err.contains("metadata.google.internal"), "{err}");
    }

    #[tokio::test]
    async fn connects_using_checked_dns_addresses() {
        let url = serve(vec![response(
            "200 OK",
            "Content-Type: text/plain\r\n",
            "resolved",
        )])
        .await;
        let parsed = reqwest::Url::parse(&url).unwrap();
        let client = client(Arc::new(FixedResolver(vec![
            "127.0.0.1:0".parse().unwrap(),
        ])))
        .unwrap();
        let target = format!("http://example.test:{}/page", parsed.port().unwrap());
        let (_, body) = fetch_with_client(&client, &target, Format::Text)
            .await
            .unwrap();
        assert_eq!(body, b"resolved");
    }

    /// Serves `responses` to one connection each, in order, and returns the
    /// URL to fetch them from.
    async fn serve(responses: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                // The client hangs up on an oversized body; that is the point.
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}/page")
    }

    fn response(status: &str, headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn html(body: &str) -> Vec<u8> {
        response("200 OK", "Content-Type: text/html; charset=utf-8\r\n", body)
    }

    async fn fetch(args: serde_json::Value) -> ToolResult {
        let ctx = ToolContext::new(std::env::temp_dir());
        WebFetch.call(args, &ctx).await
    }

    const PAGE: &str = "<html><head><title>T</title><style>p{}</style></head><body>\
        <h1>Title</h1><script>alert(1)</script>\
        <p>Some <b>bold</b> &amp; <a href=\"/x\">a link</a>.</p>\
        <ul><li>one</li><li>two</li></ul></body></html>";

    #[tokio::test]
    async fn converts_html_to_markdown() {
        let url = serve(vec![html(PAGE)]).await;
        let out = fetch(json!({ "url": url })).await.expect("fetch");
        assert_eq!(
            out,
            "# Title\n\nSome **bold** & [a link](/x).\n\n- one\n- two"
        );
    }

    #[tokio::test]
    async fn strips_html_to_text() {
        let url = serve(vec![html(PAGE)]).await;
        let out = fetch(json!({ "url": url, "format": "text" }))
            .await
            .expect("fetch");
        assert_eq!(out, "T\nTitle\nSome bold & a link.\none\ntwo");
    }

    #[test]
    fn text_skips_comments_and_keeps_lines() {
        let html = "<div>\n  first <!-- a > b -->line\n\n</div>\n<SCRIPT type=x>if (a < b) {}</SCRIPT>\n<p>second<br>third</p>";
        assert_eq!(text(html), "first line\nsecond\nthird");
    }

    #[tokio::test]
    async fn returns_html_and_other_content_as_is() {
        let url = serve(vec![html(PAGE)]).await;
        let out = fetch(json!({ "url": url, "format": "html" }))
            .await
            .expect("fetch");
        assert_eq!(out, PAGE);

        let json = response("200 OK", "Content-Type: application/json\r\n", "{\"a\":1}");
        let url = serve(vec![json]).await;
        assert_eq!(
            fetch(json!({ "url": url })).await.expect("fetch"),
            "{\"a\":1}"
        );
    }

    #[tokio::test]
    async fn rejects_responses_over_the_cap() {
        let big = "x".repeat(MAX_BYTES + 1);
        // Without a Content-Length, so the cap applies while streaming.
        let mut streamed =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n".to_vec();
        streamed.extend_from_slice(big.as_bytes());
        let url = serve(vec![streamed]).await;
        let err = fetch(json!({ "url": url })).await.expect_err("too large");
        assert!(err.contains("too large"), "{err}");

        let declared = response("200 OK", "Content-Type: text/plain\r\n", &big);
        let url = serve(vec![declared]).await;
        let err = fetch(json!({ "url": url })).await.expect_err("too large");
        assert!(err.contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn a_long_page_keeps_its_head() {
        let long = "x".repeat(output::MAX_CHARS + 5);
        let url = serve(vec![response(
            "200 OK",
            "Content-Type: text/plain\r\n",
            &long,
        )])
        .await;
        let out = fetch(json!({ "url": url })).await.expect("fetch");
        assert_eq!(
            out,
            format!(
                "{}\n\n...output truncated, showing the first {} of {} characters...",
                &long[..output::MAX_CHARS],
                output::MAX_CHARS,
                output::MAX_CHARS + 5
            )
        );
    }

    #[tokio::test]
    async fn follows_relative_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            for (path, reply) in [
                (
                    "/page",
                    response("302 Found", "Location: /destination\r\n", ""),
                ),
                (
                    "/destination",
                    response("200 OK", "Content-Type: text/plain\r\n", "arrived"),
                ),
            ] {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut request = [0; 4096];
                let n = socket.read(&mut request).await.expect("request");
                assert!(
                    String::from_utf8_lossy(&request[..n]).starts_with(&format!("GET {path} "))
                );
                socket.write_all(&reply).await.expect("reply");
            }
        });
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            fetch(json!({ "url": format!("http://{addr}/page") })),
        )
        .await
        .expect("deadline")
        .expect("fetch");
        assert_eq!(out, "arrived");
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("server deadline")
            .expect("server");
    }

    #[tokio::test]
    async fn timeout_covers_a_stalled_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (ready, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0; 4096];
            let n = socket.read(&mut request).await.expect("request");
            assert!(n > 0, "request is empty");
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 10\r\n\r\nstart").await.expect("headers");
            ready.send(()).expect("ready");
            let mut buf = [0; 1];
            socket.read(&mut buf).await.expect("client closed")
        });
        let request = fetch(json!({ "url": format!("http://{addr}/page"), "timeout_ms": 1000 }));
        let (out, _) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(request, async { received.await.expect("server ready") })
        })
        .await
        .expect("deadline");
        assert_eq!(out.expect_err("timeout"), "request timed out after 1000 ms");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), server)
                .await
                .expect("server deadline")
                .expect("server"),
            0
        );
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let url = serve(vec![response("404 Not Found", "", "gone")]).await;
        let err = fetch(json!({ "url": url })).await.expect_err("404");
        assert_eq!(err, "request failed with status 404 Not Found");
    }

    #[tokio::test]
    async fn retries_a_cloudflare_challenge_with_an_honest_user_agent() {
        let challenge = response("403 Forbidden", "cf-mitigated: challenge\r\n", "");
        let url = serve(vec![challenge, html("<p>in</p>")]).await;
        assert_eq!(fetch(json!({ "url": url })).await.expect("fetch"), "in");
    }

    #[tokio::test]
    async fn rejects_other_schemes_and_bad_args() {
        let err = fetch(json!({ "url": "file:///etc/passwd" }))
            .await
            .expect_err("scheme");
        assert_eq!(err, "URL must start with http:// or https://");
        assert!(
            fetch(json!({ "url": "https://x", "format": "pdf" }))
                .await
                .is_err()
        );
        assert!(fetch(json!({})).await.is_err());
    }
}
