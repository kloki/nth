#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    Assistant(AssistantMessage),
    ToolResult { call_id: String, content: String },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantMessage {
    pub text: String,
    /// Some models (Kimi, DeepSeek) require their reasoning to be sent back
    /// on later requests, so it is kept rather than dropped after display.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON as the model produced it; parsed only when the tool runs, so
    /// a malformed call becomes a tool error the model can correct.
    pub arguments: String,
}
