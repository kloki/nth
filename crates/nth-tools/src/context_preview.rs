//! Read-only inspection of the latest main-session request projection.
use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct ContextPreview;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Zero-based index into the overview's entries.
    entry: Option<usize>,
    /// Name from the overview's tools.
    tool: Option<String>,
}

impl Tool for ContextPreview {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "context",
            description: "Inspect the main session's logical next-request preview. With no arguments, return budget and entry metadata without their full text. Pass entry (its zero-based index) or tool (its name) to read one complete entry or tool definition. Indices refer to the latest snapshot and may change between calls; request a fresh overview. Token counts are estimates; provider packaging and accounting may differ.".into(),
            parameters: json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "entry": {"type": "integer", "minimum": 0, "description": "Index of a conversation or system entry in the overview"},
                    "tool": {"type": "string", "description": "Name of an offered tool"}
                }
            }),
        }
    }

    fn call<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = nth_protocol::parse_args(args)?;
            if ctx.is_subagent {
                return Err("context preview is available only to the main session".into());
            }
            let snapshot = ctx.context_preview.get();
            inspect(snapshot.as_deref(), args)
        }
        .boxed()
    }
}

/// Consumes no snapshot state. The serialized projection follows the session
/// preview's public model; image data is excluded by Image's serialization.
fn inspect(preview: Option<&Value>, args: Args) -> ToolResult {
    if args.entry.is_some() && args.tool.is_some() {
        return Err("specify either entry or tool, not both".into());
    }
    let preview = preview.ok_or("context preview is unavailable in this session")?;
    let entries = preview["entries"]
        .as_array()
        .ok_or("invalid context preview: missing entries")?;
    let tools = preview["tools"]
        .as_array()
        .ok_or("invalid context preview: missing tools")?;
    if let Some(index) = args.entry {
        let entry = entries
            .get(index)
            .ok_or("context entry index out of range; request a fresh overview")?;
        return Ok(json!({"entry": index, "detail": entry}).to_string());
    }
    if let Some(name) = args.tool {
        let tool = tools
            .iter()
            .find(|tool| tool["name"].as_str() == Some(&name))
            .ok_or("tool not found in context preview; request a fresh overview")?;
        return Ok(json!({"tool": tool}).to_string());
    }
    let entries: Vec<_> = entries.iter().enumerate().map(|(index, entry)| {
        let message = &entry["message"];
        let role = message.as_object().and_then(|object| object.keys().next()).map(String::as_str).unwrap_or("unknown");
        json!({"index": index, "role": role, "estimated_tokens": entry["estimated_tokens"], "checkpoint": entry["checkpoint"]})
    }).collect();
    let tools: Vec<_> = tools
        .iter()
        .map(|tool| json!({"name": tool["name"], "estimated_tokens": tool["estimated_tokens"]}))
        .collect();
    Ok(json!({
        "phase": preview["phase"], "model": preview["model"], "effort": preview["effort"], "mode": preview["mode"],
        "agent": preview["agent"], "budget": preview["budget"], "last_usage": preview["last_usage"],
        "compaction_required": preview["compaction_required"],
        "pending_count": preview["pending"].as_array().map_or(0, Vec::len),
        "pending_types": preview["pending"].as_array().map(|pending| pending.iter().filter_map(|item| item.get(0).and_then(Value::as_str)).collect::<Vec<_>>()).unwrap_or_default(),
        "entries": entries, "tools": tools
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview() -> Value {
        json!({"phase":"pending_results", "model":"test/model", "mode":"plan", "budget":{"input":30},
            "entries":[{"message":{"system":"private system text"},"estimated_tokens":5,"checkpoint":false},
                {"message":{"tool_result":{"call_id":"1","content":"file contents"}},"estimated_tokens":10,"checkpoint":true}],
            "tools":[{"name":"read","description":"full description","parameters":{"type":"object"},"estimated_tokens":15}],
            "pending":[["prompt","pending text"]]})
    }

    fn args(entry: Option<usize>, tool: Option<&str>) -> Args {
        Args {
            entry,
            tool: tool.map(str::to_owned),
        }
    }

    #[test]
    fn overview_is_metadata_only_and_detail_is_complete() {
        let snapshot = preview();
        let overview = inspect(Some(&snapshot), args(None, None)).unwrap();
        assert!(!overview.contains("private system text"));
        assert!(!overview.contains("file contents"));
        assert!(!overview.contains("full description"));
        assert!(!overview.contains("pending text"));
        let parsed: Value = serde_json::from_str(&overview).unwrap();
        assert_eq!(parsed["entries"][1]["role"], "tool_result");
        assert_eq!(parsed["pending_count"], 1);
        assert_eq!(parsed["pending_types"], json!(["prompt"]));
        assert_eq!(parsed["phase"], "pending_results");
        assert!(
            inspect(Some(&snapshot), args(Some(0), None))
                .unwrap()
                .contains("private system text")
        );
        assert!(
            inspect(Some(&snapshot), args(None, Some("read")))
                .unwrap()
                .contains("full description")
        );
        assert_eq!(snapshot, preview());
    }

    #[test]
    fn invalid_selections_are_reported() {
        let snapshot = preview();
        assert!(inspect(Some(&snapshot), args(Some(2), None)).is_err());
        assert!(inspect(Some(&snapshot), args(None, Some("missing"))).is_err());
        assert!(inspect(Some(&snapshot), args(Some(0), Some("read"))).is_err());
        assert!(
            inspect(None, args(None, None))
                .unwrap_err()
                .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn headless_call_reports_unavailable_snapshot() {
        let ctx = ToolContext::new(".".into());
        assert!(
            ContextPreview
                .call(json!({}), &ctx)
                .await
                .unwrap_err()
                .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn headless_reads_shared_snapshot_without_consuming_it() {
        let ctx = ToolContext::new(".".into());
        ctx.context_preview.set(std::sync::Arc::new(preview()));
        let overview = ContextPreview.call(json!({}), &ctx).await.unwrap();
        let detail = ContextPreview
            .call(json!({"entry": 1}), &ctx)
            .await
            .unwrap();
        assert!(detail.contains("file contents"));
        assert_eq!(
            overview,
            ContextPreview.call(json!({}), &ctx).await.unwrap()
        );
        assert_eq!(ctx.context_preview.get().as_deref(), Some(&preview()));
    }

    #[tokio::test]
    async fn subagents_cannot_read_main_session_snapshot() {
        let mut ctx = ToolContext::new(".".into());
        ctx.is_subagent = true;
        ctx.context_preview.set(std::sync::Arc::new(preview()));
        assert!(
            ContextPreview
                .call(json!({}), &ctx)
                .await
                .unwrap_err()
                .contains("main session")
        );
    }
}
