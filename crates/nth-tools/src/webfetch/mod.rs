use std::time::Duration;

use futures::{FutureExt, future::BoxFuture};
use htmd::{
    HtmlToMarkdown,
    options::{BulletListMarker, HrStyle, Options},
};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_json::json;

const MAX_BYTES: usize = 5 * 1024 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
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
    timeout: Option<u64>,
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
                    "timeout": { "type": "integer", "minimum": 1, "description": "Optional timeout in seconds (max 120)" }
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
            let args: Args = crate::parse_args(args)?;
            if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
                return Err("URL must start with http:// or https://".into());
            }
            let secs = args
                .timeout
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS);
            let (content_type, body) =
                tokio::time::timeout(Duration::from_secs(secs), fetch(&args.url, args.format))
                    .await
                    .map_err(|_| format!("request timed out after {secs} s"))??;

            let html = content_type.contains("text/html")
                || content_type.contains("application/xhtml+xml");
            let content = match String::from_utf8(body) {
                Ok(content) => content,
                Err(e) if content_type.starts_with("text/") || html => {
                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                }
                Err(_) => return Err(format!("cannot show binary content ({content_type})")),
            };
            match args.format {
                Format::Markdown if html => markdown(&content),
                Format::Text if html => Ok(text(&content)),
                _ => Ok(content),
            }
        }
        .boxed()
    }
}

/// Returns the content type and a body of at most `MAX_BYTES`.
async fn fetch(url: &str, format: Format) -> Result<(String, Vec<u8>), String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| describe(&e))?;
    let get = |user_agent| {
        client
            .get(url)
            .header(header::USER_AGENT, user_agent)
            .header(header::ACCEPT, format.accept())
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .send()
    };
    let mut response = get(BROWSER_USER_AGENT).await.map_err(|e| describe(&e))?;
    if response.status() == StatusCode::FORBIDDEN
        && response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|v| v == "challenge")
    {
        response = get(HONEST_USER_AGENT).await.map_err(|e| describe(&e))?;
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
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

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
