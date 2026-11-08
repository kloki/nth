//! `nth context`: a next-request preview reconstructed from a saved session.

use std::{
    io::{self, IsTerminal, Write},
    sync::Arc,
};

use anyhow::Result;
use nth_icons::icons;
use nth_llm::{Credentials, EndpointConfig, Providers};
use nth_protocol::{Entry, Inbox, Message, Preview, Tool};
use nth_session::{Session, Store, Subagents};
use owo_colors::OwoColorize;
use serde_json::json;

use crate::runtime::{Runtime, Start};

const SOURCE: &str = "Reconstructed from saved state; another process's unsaved running state and inbox are unavailable.";

pub async fn run(id: Option<String>, json: bool, runtime: &Runtime) -> Result<()> {
    let store = Store::open()?;
    let session = crate::session::pick(&store, id.as_deref()).await?;
    let (session, tools, warnings) = reconstruct(session, runtime).await?;
    for warning in session.context().warnings.iter().chain(&warnings) {
        eprintln!("{} {warning}", icons().warn.yellow().bold());
    }
    let preview = session.context_preview(&tools, &Inbox::default());
    let mut out = io::stdout().lock();
    let json = json || !out.is_terminal();
    if json {
        write_json(&mut out, &session, &preview)?;
    } else {
        write_text(&mut out, &session, &preview)?;
    }
    Ok(())
}

/// The saved session as the chat would resume it, on offline providers,
/// with the tools its next request offers and the warnings preparing it
/// gave. Preparing rewrites the system prompt, and the offline provider
/// knows none of the config's endpoints, so it would qualify the model
/// under its own: the model and messages are exactly the saved ones.
async fn reconstruct(
    session: Session,
    runtime: &Runtime,
) -> Result<(Session, Arc<Vec<Box<dyn Tool>>>, Vec<String>)> {
    let (model, messages) = (session.model.clone(), session.messages.clone());
    let start = Start::Saved(Box::new(session));
    let config = runtime.config_for(&start)?;
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let prepared = runtime
        .prepare(config, offline_provider()?, start, Subagents::new(tx))
        .await?;
    let mut session = prepared.session;
    session.model = model;
    session.messages = messages;
    Ok((session, prepared.model_tools, prepared.warnings))
}

/// Providers that never send a request: the handle only satisfies the tool
/// constructors, and no tool is invoked.
fn offline_provider() -> Result<Arc<Providers>> {
    let provider = Providers::offline(
        vec![EndpointConfig {
            id: "offline".into(),
            name: "offline".into(),
            base_url: "http://127.0.0.1".into(),
            credentials: Credentials::Key("unused".into()),
            models: Vec::new(),
        }],
        Vec::new(),
        "offline/unused",
    )?;
    Ok(Arc::new(provider))
}

fn write_json(out: &mut impl Write, session: &Session, preview: &Preview) -> Result<()> {
    writeln!(
        out,
        "{}",
        json!({"session_id": session.id, "title": session.title(), "source": "saved_state", "note": SOURCE, "preview": preview})
    )?;
    Ok(())
}

fn write_text(out: &mut impl Write, session: &Session, preview: &Preview) -> Result<()> {
    let budget = &preview.budget;
    writeln!(
        out,
        "Next-request preview · {} · {} · {}",
        preview.model,
        preview.agent.as_deref().unwrap_or(match preview.mode {
            nth_protocol::Mode::Plan => "plan",
            nth_protocol::Mode::Act => "act",
        }),
        preview.effort.name()
    )?;
    writeln!(out, "Session {} · {SOURCE}", session.id)?;
    writeln!(
        out,
        "Logical request; estimates use a byte-based heuristic and provider accounting may differ."
    )?;
    writeln!(
        out,
        "Estimated input ~{} / {} context",
        budget.input,
        known(budget.context_window)
    )?;
    writeln!(out, "  System prompt       ~{}", budget.system)?;
    writeln!(out, "  Conversation        ~{}", budget.conversation)?;
    writeln!(out, "  Tool definitions    ~{}", budget.tools)?;
    writeln!(out, "  Images              ~{}", budget.images)?;
    writeln!(
        out,
        "  Output reserve       {} · usable {} · available {}",
        budget.output_reserve,
        known(budget.usable),
        known(budget.available)
    )?;
    if let Some(usage) = preview.last_usage {
        writeln!(
            out,
            "Last reported input: {} · cached: {}",
            usage.input,
            known(usage.cache_read)
        )?;
    }
    if preview.compaction_required {
        writeln!(out, "Compaction required before next request")?;
    }
    for (kind, text) in &preview.pending {
        writeln!(out, "Pending {kind}: {text}")?;
    }
    writeln!(out, "\nSystem prompt")?;
    for entry in preview
        .entries
        .iter()
        .filter(|e| matches!(e.message, Message::System(_)))
    {
        write_entry(out, entry)?;
    }
    let count = preview
        .entries
        .iter()
        .filter(|e| !matches!(e.message, Message::System(_)))
        .count();
    writeln!(out, "\nConversation · {count} messages")?;
    for entry in preview
        .entries
        .iter()
        .filter(|e| !matches!(e.message, Message::System(_)))
    {
        write_entry(out, entry)?;
    }
    writeln!(out, "\nTool definitions · {} tools", preview.tools.len())?;
    for tool in &preview.tools {
        writeln!(
            out,
            "\n{} · ~{} tokens\n{}\nSchema:\n{}",
            tool.name,
            tool.estimated_tokens,
            tool.description,
            serde_json::to_string_pretty(&tool.parameters)?
        )?;
    }
    Ok(())
}

fn known(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".into(), |n| n.to_string())
}

fn write_entry(out: &mut impl Write, entry: &Entry) -> Result<()> {
    let checkpoint = if entry.checkpoint {
        " · checkpoint"
    } else {
        ""
    };
    match &entry.message {
        Message::System(text) => writeln!(
            out,
            "\nSystem{checkpoint} · ~{} tokens\n{text}",
            entry.estimated_tokens
        )?,
        Message::User(text) => writeln!(
            out,
            "\nUser{checkpoint} · ~{} tokens\n{text}",
            entry.estimated_tokens
        )?,
        Message::Assistant(message) => {
            writeln!(
                out,
                "\nAssistant{checkpoint} · ~{} tokens",
                entry.estimated_tokens
            )?;
            if !message.reasoning.is_empty() {
                writeln!(out, "Reasoning:\n{}", message.reasoning)?;
            }
            if !message.text.is_empty() {
                writeln!(out, "{}", message.text)?;
            }
            for call in &message.tool_calls {
                writeln!(
                    out,
                    "Tool call {} · {}\n{}",
                    call.name, call.id, call.arguments
                )?;
            }
        }
        Message::ToolResult {
            call_id,
            content,
            images,
        } => {
            writeln!(
                out,
                "\nTool result · {call_id}{checkpoint} · ~{} tokens\n{content}",
                entry.estimated_tokens
            )?;
            for (index, image) in images.iter().enumerate() {
                let availability = if entry.images_available.get(index).copied().unwrap_or(false) {
                    "available"
                } else {
                    "unavailable"
                };
                writeln!(
                    out,
                    "Image: {} · {} · {}x{} · {availability} · ~2500 tokens",
                    image.path.display(),
                    image.mime,
                    image.width,
                    image.height
                )?;
            }
        }
        Message::Compaction { summary, recent } => writeln!(
            out,
            "\nCheckpoint · ~{} tokens\n{summary}\n{recent}",
            entry.estimated_tokens
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use nth_config::Config;
    use nth_protocol::{AssistantMessage, Image, ToolCall};

    use super::*;
    use crate::runtime::tests::runtime;

    #[test]
    fn text_and_json_show_complete_preview_without_image_bytes() {
        let mut session = Session::new("example/model", "/repo".into());
        session.messages = vec![
            Message::System("saved system".into()),
            Message::User("hello".into()),
            Message::Assistant(AssistantMessage {
                text: "answer".into(),
                reasoning: "thought".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"x\"}".into(),
                }],
            }),
            Message::ToolResult {
                call_id: "call-1".into(),
                content: "result".into(),
                images: vec![Image {
                    mime: "image/png".into(),
                    sha256: "abc".into(),
                    width: 10,
                    height: 20,
                    path: "/tmp/image.png".into(),
                    data: Some(Arc::from("secret-base64")),
                }],
            },
        ];
        let preview = session.context_preview(&[], &Inbox::default());
        let mut text = Vec::new();
        write_text(&mut text, &session, &preview).unwrap();
        let text = String::from_utf8(text).unwrap();
        for expected in [
            "saved system",
            "hello",
            "answer",
            "thought",
            "Tool call read",
            "call-1",
            "result",
            "/tmp/image.png",
            "available",
            "~2500 tokens",
            "Tool definitions",
            "Reconstructed from saved state",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("secret-base64"));
        let mut json = Vec::new();
        write_json(&mut json, &session, &preview).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(json["source"], "saved_state");
        assert_eq!(json["preview"]["entries"][3]["images_available"][0], true);
        assert!(!json.to_string().contains("secret-base64"));
    }

    #[tokio::test]
    async fn context_selects_latest_and_prefix_from_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("sessions"));
        let mut session = Session::new("model", dir.path().into());
        session.messages.push(Message::User("hello".into()));
        store.save(&session).await.unwrap();
        let id = session.id.to_string();
        assert_eq!(
            crate::session::pick(&store, None).await.unwrap().id,
            session.id
        );
        assert_eq!(
            crate::session::pick(&store, Some(&id[..8]))
                .await
                .unwrap()
                .id,
            session.id
        );
        assert!(crate::session::pick(&store, Some("missing")).await.is_err());
    }

    #[tokio::test]
    async fn the_saved_model_and_messages_are_previewed_as_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = \"zen/b\"").expect("writes");
        let mut saved = Session::new("zen/a", dir.path().into());
        saved.messages[0] = Message::System("saved system".into());
        saved.messages.push(Message::User("hello".into()));

        let (session, tools, _) = reconstruct(saved.clone(), &runtime(dir.path(), Some(path)))
            .await
            .expect("reconstructs");

        assert_eq!(session.model, "zen/a");
        assert_eq!(session.messages, saved.messages);
        assert!(tools.iter().any(|tool| tool.spec().name == "read"));
    }

    #[tokio::test]
    async fn normal_tools_are_included_in_text_and_json() {
        let config = Config::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let tools = crate::runtime::tools(
            &config,
            crate::runtime::post_write(&config),
            offline_provider().unwrap(),
            Subagents::new(tx),
        );
        let session = Session::new("model", "/repo".into());
        let preview = session.context_preview(&tools, &Inbox::default());
        let read = preview
            .tools
            .iter()
            .find(|tool| tool.name == "read")
            .unwrap();
        assert!(!read.description.is_empty());
        assert!(read.parameters.is_object());
        let mut text = Vec::new();
        write_text(&mut text, &session, &preview).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains(&read.description));
        assert!(text.contains("Schema:"));
        let mut json = Vec::new();
        write_json(&mut json, &session, &preview).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert!(
            json["preview"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "read" && tool["parameters"].is_object())
        );
    }
}
