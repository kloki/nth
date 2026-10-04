//! Web search through Exa's hosted MCP server. This is not an MCP client:
//! it skips the initialize handshake and tool listing and sends one
//! `tools/call` for `web_search_exa` to a fixed endpoint, as opencode does.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(25);
const DEFAULT_NUM_RESULTS: u32 = 8;
/// Sent as `exaApiKey` when set. Exa answers without a key too, at a lower
/// rate limit.
const API_KEY_ENV: &str = "EXA_API_KEY";

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebsearchConfig {
    /// Exa's MCP endpoint.
    pub url: String,
}

impl Default for WebsearchConfig {
    fn default() -> Self {
        Self {
            url: "https://mcp.exa.ai/mcp".into(),
        }
    }
}

pub struct Websearch {
    config: WebsearchConfig,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl Websearch {
    pub fn new(config: WebsearchConfig) -> Self {
        let api_key = std::env::var(API_KEY_ENV).ok().filter(|k| !k.is_empty());
        Self {
            config,
            api_key,
            http: reqwest::Client::new(),
        }
    }

    async fn search(&self, args: Args) -> Result<String, String> {
        let mut request = self
            .http
            .post(&self.config.url)
            .timeout(TIMEOUT)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "web_search_exa",
                    "arguments": {
                        "query": args.query,
                        "type": "auto",
                        "numResults": args.num_results.unwrap_or(DEFAULT_NUM_RESULTS),
                        "livecrawl": "fallback",
                    }
                }
            }));
        if let Some(key) = &self.api_key {
            request = request.query(&[("exaApiKey", key)]);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("web search failed: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("web search failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("web search failed: HTTP {status}: {}", body.trim()));
        }
        parse_response(&body)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    query: String,
    num_results: Option<u32>,
}

impl Tool for Websearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "websearch",
            description: include_str!("description.txt")
                .replace("{year}", &current_year().to_string()),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Websearch query" },
                    "numResults": { "type": "integer", "minimum": 1, "description": "Number of search results to return (default: 8)" }
                },
                "required": ["query"]
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
            self.search(args).await
        }
        .boxed()
    }
}

#[derive(Deserialize)]
struct Reply {
    result: Option<CallResult>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct CallResult {
    #[serde(default)]
    content: Vec<Content>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

/// The server picks the format: one JSON body, or server-sent events whose
/// `data:` lines carry the JSON-RPC reply.
fn parse_response(body: &str) -> Result<String, String> {
    let trimmed = body.trim();
    let payloads: Vec<&str> = if trimmed.starts_with('{') {
        vec![trimmed]
    } else {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|data| data.starts_with('{'))
            .collect()
    };
    for payload in payloads {
        let reply: Reply = serde_json::from_str(payload)
            .map_err(|e| format!("web search returned an invalid reply: {e}"))?;
        if let Some(error) = reply.error {
            return Err(format!("web search failed: {}", error.message));
        }
        let Some(result) = reply.result else { continue };
        let text = result.content.into_iter().find(|c| !c.text.is_empty());
        match (text, result.is_error) {
            (Some(c), true) => return Err(format!("web search failed: {}", c.text)),
            (Some(c), false) => return Ok(c.text),
            (None, true) => return Err("web search failed".into()),
            (None, false) => {}
        }
    }
    Err("No search results found. Please try a different query.".into())
}

/// The UTC year, for the description, so the model searches for this year
/// rather than the one its training data ends in.
fn current_year() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Howard Hinnant's days-to-civil algorithm, reduced to the year.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let year = yoe + era * 400;
    if mp >= 10 { year + 1 } else { year }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;

    /// Serves one canned HTTP response and hands back the request it got.
    async fn server(status: &str, content_type: &str, body: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.expect("read");
                request.extend_from_slice(&buf[..n]);
                if n == 0 || complete(&request) {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.expect("write");
            String::from_utf8(request).expect("utf8")
        });
        (url, handle)
    }

    fn complete(request: &[u8]) -> bool {
        let text = String::from_utf8_lossy(request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let length = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
            })
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        body.len() >= length
    }

    async fn search(url: String, api_key: Option<&str>, args: serde_json::Value) -> ToolResult {
        let tool = Websearch {
            config: WebsearchConfig { url },
            api_key: api_key.map(String::from),
            http: reqwest::Client::new(),
        };
        tool.call(args, &ToolContext::new(".".into())).await
    }

    fn rpc_result(text: &str) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": text }] } })
            .to_string()
    }

    #[tokio::test]
    async fn json_reply_returns_text_and_sends_exa_call() {
        let (url, request) =
            server("200 OK", "application/json", &rpc_result("ratatui 0.30")).await;
        let out = search(url, None, json!({ "query": "ratatui version" }))
            .await
            .expect("search");
        assert_eq!(out, "ratatui 0.30");

        let request = request.await.expect("server");
        assert!(request.starts_with("POST /mcp HTTP/1.1"), "{request}");
        assert!(
            request.contains("accept: application/json, text/event-stream"),
            "{request}"
        );
        let (_, body) = request.split_once("\r\n\r\n").expect("body");
        let body: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert_eq!(
            body,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "web_search_exa",
                    "arguments": {
                        "query": "ratatui version",
                        "type": "auto",
                        "numResults": 8,
                        "livecrawl": "fallback"
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn sse_reply_returns_text() {
        let body = format!("event: message\ndata: {}\n\n", rpc_result("from sse"));
        let (url, _) = server("200 OK", "text/event-stream", &body).await;
        let out = search(url, None, json!({ "query": "q", "numResults": 3 }))
            .await
            .expect("search");
        assert_eq!(out, "from sse");
    }

    #[tokio::test]
    async fn api_key_goes_in_the_query_string_encoded() {
        let (url, request) = server("200 OK", "application/json", &rpc_result("ok")).await;
        search(url, Some("a b&c"), json!({ "query": "q" }))
            .await
            .expect("search");
        let request = request.await.expect("server");
        assert!(
            request.starts_with("POST /mcp?exaApiKey=a+b%26c HTTP/1.1"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn json_rpc_error_is_an_error() {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "bad query" } })
            .to_string();
        let (url, _) = server("200 OK", "application/json", &body).await;
        let err = search(url, None, json!({ "query": "q" }))
            .await
            .expect_err("rpc error");
        assert_eq!(err, "web search failed: bad query");
    }

    #[tokio::test]
    async fn http_error_is_an_error() {
        let (url, _) = server("429 Too Many Requests", "text/plain", "slow down").await;
        let err = search(url, None, json!({ "query": "q" }))
            .await
            .expect_err("http error");
        assert_eq!(
            err,
            "web search failed: HTTP 429 Too Many Requests: slow down"
        );
    }

    #[test]
    fn empty_and_tool_error_replies_are_errors() {
        assert!(parse_response("").is_err());
        assert!(parse_response(&rpc_result("")).is_err());
        let tool_error = json!({ "result": { "content": [{ "text": "quota" }], "isError": true } });
        assert_eq!(
            parse_response(&tool_error.to_string()),
            Err("web search failed: quota".into())
        );
    }

    #[test]
    fn current_year_is_plausible() {
        assert!((2026..2200).contains(&current_year()));
    }
}
