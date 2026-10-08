//! Converts nth's messages and tools to the chat completions JSON shape.

use nth_protocol::{Effort, Message, ToolSpec};
use serde_json::{Value, json};

pub(super) fn body(model: &str, effort: Effort, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let mistral = is_mistral(model);
    let mut body = json!({
        "model": model,
        "stream": true,
        // Without it the stream carries no token counts.
        "stream_options": { "include_usage": true },
        "messages": messages.iter().map(|m| message(m, mistral)).collect::<Vec<_>>(),
    });
    if let Some(effort) = effort.wire() {
        body["reasoning_effort"] = json!(effort);
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(tool).collect();
    }
    body
}

/// Mistral refuses unknown fields, `reasoning_content` among them, and takes
/// reasoning back only as the `thinking` chunk it streams it in. The model id
/// is all that tells it apart, as in opencode.
fn is_mistral(model: &str) -> bool {
    let model = model.to_lowercase();
    [
        "mistral",
        "magistral",
        "devstral",
        "codestral",
        "pixtral",
        "mixtral",
    ]
    .iter()
    .any(|family| model.contains(family))
}

fn message(message: &Message, mistral: bool) -> Value {
    match message {
        Message::System(text) => json!({ "role": "system", "content": text }),
        Message::User(text) => json!({ "role": "user", "content": text }),
        Message::ToolResult { call_id, content } => {
            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
        }
        Message::Assistant(reply) => {
            let mut out = json!({ "role": "assistant", "content": reply.text });
            if !reply.reasoning.is_empty() {
                if mistral {
                    out["content"] = json!([
                        {
                            "type": "thinking",
                            "thinking": [{ "type": "text", "text": reply.reasoning }],
                        },
                        { "type": "text", "text": reply.text },
                    ]);
                } else {
                    out["reasoning_content"] = json!(reply.reasoning);
                }
            }
            if !reply.tool_calls.is_empty() {
                out["tool_calls"] = reply
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments },
                        })
                    })
                    .collect();
            }
            out
        }
    }
}

fn tool(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_an_effort_only_when_one_is_chosen() {
        let chosen = body("glm", Effort::High, &[], &[]);
        assert_eq!(chosen["reasoning_effort"], "high");

        let default = body("glm", Effort::Default, &[], &[]);
        assert!(default.get("reasoning_effort").is_none());
    }

    fn assistant(model: &str) -> Value {
        let reply = Message::Assistant(nth_protocol::AssistantMessage {
            text: "Reading.".into(),
            reasoning: "hmm".into(),
            ..Default::default()
        });
        body(model, Effort::Default, &[reply], &[])["messages"][0].clone()
    }

    #[test]
    fn reasoning_goes_back_as_reasoning_content() {
        let out = assistant("glm");
        assert_eq!(out["content"], "Reading.");
        assert_eq!(out["reasoning_content"], "hmm");
    }

    #[test]
    fn mistral_gets_its_reasoning_back_as_a_thinking_chunk() {
        let out = assistant("mistral-large-4");
        assert!(out.get("reasoning_content").is_none());
        assert_eq!(
            out["content"],
            json!([
                { "type": "thinking", "thinking": [{ "type": "text", "text": "hmm" }] },
                { "type": "text", "text": "Reading." },
            ])
        );
    }
}
