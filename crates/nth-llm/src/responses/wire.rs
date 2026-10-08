//! Converts nth's messages and tools to the OpenAI responses JSON shape.

use nth_protocol::{Effort, Message, ToolSpec};
use serde_json::{Value, json};

/// `reasons` is whether the catalogue says the model reasons: its summary
/// is then asked for even when the effort is left to the model.
pub(super) fn body(
    model: &str,
    session_id: &str,
    effort: Effort,
    reasons: bool,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    let instructions: Vec<&str> = messages
        .iter()
        .filter_map(|m| match m {
            Message::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let mut body = json!({
        "model": model,
        "stream": true,
        // Nothing is kept server-side: every request carries the whole
        // conversation, as with the other protocols.
        "store": false,
        // Requests of one session share a prefix; this keeps them on the
        // same cache.
        "prompt_cache_key": session_id,
        "input": messages.iter().flat_map(items).collect::<Vec<_>>(),
    });
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions.join("\n\n"));
    }
    if let Some(effort) = effort.wire() {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    } else if reasons {
        body["reasoning"] = json!({ "summary": "auto" });
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(tool).collect();
    }
    body
}

/// A message as input items: an assistant reply is its text, if any, then
/// one item per tool call. Reasoning is not sent back: replaying it needs
/// the encrypted form the API returns, which nth does not keep.
fn items(message: &Message) -> Vec<Value> {
    match message {
        Message::System(_) => Vec::new(),
        Message::User(text) => vec![json!({
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })],
        Message::ToolResult { call_id, content } => vec![json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": content,
        })],
        Message::Assistant(reply) => {
            let text = (!reply.text.is_empty()).then(|| {
                json!({
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": reply.text }],
                })
            });
            text.into_iter()
                .chain(reply.tool_calls.iter().map(|c| {
                    json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.name,
                        "arguments": c.arguments,
                    })
                }))
                .collect()
        }
    }
}

fn tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": spec.name,
        "description": spec.description,
        "parameters": spec.parameters,
        // Strict is the default here, and it rewrites the schema to make
        // every parameter required, so the model fills in the optional ones.
        "strict": false,
    })
}

#[cfg(test)]
mod tests {
    use nth_protocol::{AssistantMessage, ToolCall};

    use super::*;

    #[test]
    fn translates_a_conversation_with_tool_calls() {
        let messages = [
            Message::System("You are nth.".into()),
            Message::User("Read both.".into()),
            Message::Assistant(AssistantMessage {
                text: "Reading.".into(),
                reasoning: "Two files.".into(),
                tool_calls: vec![ToolCall {
                    id: "functions.read:0".into(),
                    name: "read".into(),
                    arguments: r#"{"filePath":"a"}"#.into(),
                }],
            }),
            Message::ToolResult {
                call_id: "functions.read:0".into(),
                content: "A".into(),
            },
            Message::Assistant(AssistantMessage {
                text: "Done.".into(),
                ..AssistantMessage::default()
            }),
        ];
        let tools = [ToolSpec {
            name: "read",
            description: "Reads a file.".into(),
            parameters: json!({ "type": "object" }),
        }];

        let body = body("gpt-x", "ses_1", Effort::Default, false, &messages, &tools);

        assert_eq!(
            body,
            json!({
                "model": "gpt-x",
                "stream": true,
                "store": false,
                "prompt_cache_key": "ses_1",
                "instructions": "You are nth.",
                "input": [
                    { "role": "user", "content": [{ "type": "input_text", "text": "Read both." }] },
                    { "role": "assistant", "content": [{ "type": "output_text", "text": "Reading." }] },
                    {
                        "type": "function_call",
                        "call_id": "functions.read:0",
                        "name": "read",
                        "arguments": r#"{"filePath":"a"}"#,
                    },
                    { "type": "function_call_output", "call_id": "functions.read:0", "output": "A" },
                    { "role": "assistant", "content": [{ "type": "output_text", "text": "Done." }] },
                ],
                "tools": [{
                    "type": "function",
                    "name": "read",
                    "description": "Reads a file.",
                    "parameters": { "type": "object" },
                    "strict": false,
                }],
            })
        );
    }

    #[test]
    fn a_reply_of_only_tool_calls_has_no_text_item() {
        let reply = Message::Assistant(AssistantMessage {
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "glob".into(),
                arguments: "{}".into(),
            }],
            ..AssistantMessage::default()
        });
        let items = items(&reply);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
    }

    #[test]
    fn reasons_with_a_summary_when_an_effort_is_chosen_or_the_model_reasons() {
        let reasoning =
            |effort, reasons| body("gpt-x", "s", effort, reasons, &[], &[])["reasoning"].clone();
        assert_eq!(
            reasoning(Effort::High, false),
            json!({ "effort": "high", "summary": "auto" })
        );
        assert_eq!(
            reasoning(Effort::Default, true),
            json!({ "summary": "auto" })
        );
        assert_eq!(reasoning(Effort::Default, false), Value::Null);
    }
}
