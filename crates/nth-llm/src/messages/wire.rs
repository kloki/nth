//! Converts nth's messages and tools to the Anthropic messages JSON shape.

use nth_protocol::{Effort, Message, ToolCall, ToolSpec};
use serde_json::{Value, json};

/// The smallest thinking budget the API takes.
const MIN_THINKING: u64 = 1_024;

pub fn body(
    model: &str,
    effort: Effort,
    max_tokens: u64,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    let system: Vec<&str> = messages
        .iter()
        .filter_map(|m| match m {
            Message::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let mut turns = turns(messages);
    // Nothing is cached unless asked for. The next request repeats this one
    // and adds to it, so everything up to here is worth keeping.
    if let Some(block) = turns
        .last_mut()
        .and_then(|turn| turn["content"].as_array_mut())
        .and_then(|content| content.last_mut())
    {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": turns,
    });
    if !system.is_empty() {
        // Its own breakpoint too, so the tools and the system prompt stay
        // cached when the history changes.
        body["system"] = json!([{
            "type": "text",
            "text": system.join("\n\n"),
            "cache_control": { "type": "ephemeral" },
        }]);
    }
    if let Some(budget) = thinking(effort, max_tokens) {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(tool).collect();
    }
    body
}

/// The conversation as turns of content blocks. Tool results are user
/// blocks, and consecutive messages of one role share a turn, so the
/// results of parallel calls arrive together as the API wants them.
/// Empty text is left out, as the API refuses it, and a message with
/// nothing left goes with it.
fn turns(messages: &[Message]) -> Vec<Value> {
    let mut turns: Vec<(&str, Vec<Value>)> = Vec::new();
    for message in messages {
        let (role, blocks): (_, Vec<_>) = match message {
            Message::System(_) => continue,
            Message::User(text) => ("user", text_block(text).into_iter().collect()),
            Message::ToolResult { call_id, content } => {
                let mut block = json!({ "type": "tool_result", "tool_use_id": tool_id(call_id) });
                if !content.is_empty() {
                    block["content"] = json!(content);
                }
                ("user", vec![block])
            }
            // Reasoning is not sent back: replaying it needs the signature
            // the API signs it with, which nth does not keep.
            Message::Assistant(reply) => (
                "assistant",
                text_block(&reply.text)
                    .into_iter()
                    .chain(reply.tool_calls.iter().map(tool_use))
                    .collect(),
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({ "type": "text", "text": text }))
}

fn tool_use(call: &ToolCall) -> Value {
    // The API wants an object. A call whose arguments were not one already
    // failed with a tool error saying so, which the model reads instead.
    let input = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({ "type": "tool_use", "id": tool_id(&call.id), "name": call.name, "input": input })
}

/// A call id the API accepts. Calls made by another protocol's models carry
/// characters it refuses, such as Kimi's `functions.read:0`.
fn tool_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// How many tokens the model may think for at `effort`: at most half the
/// reply, so the answer has room, and none below the API's minimum.
fn thinking(effort: Effort, max_tokens: u64) -> Option<u64> {
    let budget: u64 = match effort {
        Effort::Default => return None,
        Effort::Low => 2_048,
        Effort::Medium => 8_192,
        Effort::High => 16_000,
    };
    let budget = budget.min(max_tokens / 2);
    (budget >= MIN_THINKING).then_some(budget)
}

fn tool(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.parameters,
    })
}

#[cfg(test)]
mod tests {
    use nth_protocol::AssistantMessage;

    use super::*;

    fn call(id: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn translates_a_conversation_with_tool_calls() {
        let messages = [
            Message::System("You are nth.".into()),
            Message::User("Read both.".into()),
            Message::Assistant(AssistantMessage {
                text: "Reading.".into(),
                reasoning: "Two files.".into(),
                tool_calls: vec![
                    call("toolu_a", r#"{"filePath":"a"}"#),
                    call("functions.read:1", "not json"),
                ],
            }),
            Message::ToolResult {
                call_id: "toolu_a".into(),
                content: "A".into(),
            },
            Message::ToolResult {
                call_id: "functions.read:1".into(),
                content: String::new(),
            },
            Message::User("<task>done</task>".into()),
            Message::Assistant(AssistantMessage::default()),
        ];
        let tools = [ToolSpec {
            name: "read",
            description: "Reads a file.".into(),
            parameters: json!({ "type": "object" }),
        }];

        let body = body("claude-x", Effort::Default, 32_000, &messages, &tools);

        assert_eq!(
            body,
            json!({
                "model": "claude-x",
                "max_tokens": 32_000,
                "stream": true,
                "system": [{
                    "type": "text",
                    "text": "You are nth.",
                    "cache_control": { "type": "ephemeral" },
                }],
                "messages": [
                    { "role": "user", "content": [{ "type": "text", "text": "Read both." }] },
                    { "role": "assistant", "content": [
                        { "type": "text", "text": "Reading." },
                        { "type": "tool_use", "id": "toolu_a", "name": "read", "input": { "filePath": "a" } },
                        { "type": "tool_use", "id": "functions_read_1", "name": "read", "input": {} },
                    ] },
                    { "role": "user", "content": [
                        { "type": "tool_result", "tool_use_id": "toolu_a", "content": "A" },
                        { "type": "tool_result", "tool_use_id": "functions_read_1" },
                        {
                            "type": "text",
                            "text": "<task>done</task>",
                            "cache_control": { "type": "ephemeral" },
                        },
                    ] },
                ],
                "tools": [{
                    "name": "read",
                    "description": "Reads a file.",
                    "input_schema": { "type": "object" },
                }],
            })
        );
    }

    #[test]
    fn thinks_only_when_an_effort_is_chosen_and_leaves_room_to_answer() {
        let thinking =
            |effort, max_tokens| body("claude-x", effort, max_tokens, &[], &[])["thinking"].clone();
        assert_eq!(thinking(Effort::Default, 32_000), Value::Null);
        assert_eq!(
            thinking(Effort::High, 32_000),
            json!({ "type": "enabled", "budget_tokens": 16_000 })
        );
        assert_eq!(thinking(Effort::High, 8_000)["budget_tokens"], 4_000);
        assert_eq!(
            thinking(Effort::Low, 2_000),
            Value::Null,
            "below the minimum"
        );
    }
}
