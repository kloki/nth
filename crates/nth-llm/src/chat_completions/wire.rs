//! Converts nth's messages and tools to the chat completions JSON shape.

use nth_protocol::{Message, ToolSpec};
use serde_json::{Value, json};

pub fn body(model: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let mut body = json!({
        "model": model,
        "stream": true,
        "messages": messages.iter().map(message).collect::<Vec<_>>(),
    });
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(tool).collect();
    }
    body
}

fn message(message: &Message) -> Value {
    match message {
        Message::System(text) => json!({ "role": "system", "content": text }),
        Message::User(text) => json!({ "role": "user", "content": text }),
        Message::ToolResult { call_id, content } => {
            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
        }
        Message::Assistant(reply) => {
            let mut out = json!({ "role": "assistant", "content": reply.text });
            if !reply.reasoning.is_empty() {
                out["reasoning_content"] = json!(reply.reasoning);
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
