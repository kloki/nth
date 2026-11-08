//! Read-only inspection of the latest main-session request projection.
use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Message, Preview, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct ContextPreview;

/// What a call finds before the loop has built any snapshot.
const UNBUILT: &str =
    "the context preview is built from the next step on; call this tool again then";

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
            // Without a front-end watching, the loop builds snapshots only
            // once something asks, so the first call may find none.
            ctx.context_preview.want();
            let snapshot = ctx.context_preview.get();
            inspect(snapshot.as_deref(), args)
        }
        .boxed()
    }
}

/// Consumes no snapshot state. Only the one entry or tool returned is
/// serialized in full; image data is excluded by Image's serialization.
fn inspect(preview: Option<&Preview>, args: Args) -> ToolResult {
    if args.entry.is_some() && args.tool.is_some() {
        return Err("specify either entry or tool, not both".into());
    }
    let preview = preview.ok_or(UNBUILT)?;
    if let Some(index) = args.entry {
        let entry = preview
            .entries
            .get(index)
            .ok_or("context entry index out of range; request a fresh overview")?;
        return Ok(json!({"entry": index, "detail": entry}).to_string());
    }
    if let Some(name) = args.tool {
        let tool = preview
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .ok_or("tool not found in context preview; request a fresh overview")?;
        return Ok(json!({"tool": tool}).to_string());
    }
    let entries: Vec<_> = preview
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            json!({
                "index": index,
                "role": role(&entry.message),
                "estimated_tokens": entry.estimated_tokens,
                "checkpoint": entry.checkpoint,
            })
        })
        .collect();
    let tools: Vec<_> = preview
        .tools
        .iter()
        .map(|tool| json!({"name": tool.name, "estimated_tokens": tool.estimated_tokens}))
        .collect();
    Ok(json!({
        "phase": preview.phase, "model": preview.model, "effort": preview.effort, "mode": preview.mode,
        "agent": preview.agent, "budget": preview.budget, "last_usage": preview.last_usage,
        "compaction_required": preview.compaction_required,
        "pending_count": preview.pending.len(),
        "pending_types": preview.pending.iter().map(|(kind, _)| kind).collect::<Vec<_>>(),
        "entries": entries, "tools": tools
    })
    .to_string())
}

/// The message's variant as its serialized tag names it, which is what an
/// `entry` detail shows the model.
fn role(message: &Message) -> &'static str {
    match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult { .. } => "tool_result",
        Message::Compaction { .. } => "compaction",
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Budget, Entry, Phase, ToolDefinition};

    use super::*;

    fn preview() -> Preview {
        Preview {
            phase: Phase::PendingResults,
            model: "test/model".into(),
            effort: Default::default(),
            mode: nth_protocol::Mode::Plan,
            agent: None,
            entries: vec![
                Entry {
                    message: Message::System("private system text".into()),
                    estimated_tokens: 5,
                    checkpoint: false,
                    images_available: Vec::new(),
                },
                Entry {
                    message: Message::ToolResult {
                        call_id: "1".into(),
                        content: "file contents".into(),
                        images: Vec::new(),
                    },
                    estimated_tokens: 10,
                    checkpoint: true,
                    images_available: Vec::new(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "full description".into(),
                parameters: json!({"type": "object"}),
                estimated_tokens: 15,
            }],
            budget: Budget {
                input: 30,
                ..Budget::default()
            },
            last_usage: None,
            compaction_required: false,
            pending: vec![("prompt".into(), "pending text".into())],
        }
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
        assert_eq!(inspect(None, args(None, None)).unwrap_err(), UNBUILT);
    }

    #[tokio::test]
    async fn the_context_tool_asks_for_a_snapshot_headless() {
        let ctx = ToolContext::new(".".into());
        assert!(!ctx.context_preview.builds());
        assert_eq!(
            ContextPreview.call(json!({}), &ctx).await.unwrap_err(),
            UNBUILT
        );
        assert!(ctx.context_preview.builds(), "the call asked for snapshots");
        ctx.context_preview.publish(std::sync::Arc::new(preview()));
        let overview = ContextPreview.call(json!({}), &ctx).await.unwrap();
        let parsed: Value = serde_json::from_str(&overview).unwrap();
        assert_eq!(parsed["phase"], "pending_results", "{overview}");
    }

    #[tokio::test]
    async fn headless_reads_shared_snapshot_without_consuming_it() {
        let ctx = ToolContext::new(".".into());
        ctx.context_preview.publish(std::sync::Arc::new(preview()));
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
        ctx.context_preview.publish(std::sync::Arc::new(preview()));
        assert!(
            ContextPreview
                .call(json!({}), &ctx)
                .await
                .unwrap_err()
                .contains("main session")
        );
    }
}
