use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmRole {
    Developer,
    System,
    User,
    Assistant,
    Tool,
}

impl LlmRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmRole::Developer => "developer",
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmMessageContent {
    Text(String),
    Parts(Vec<LlmContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmContentPart {
    Text { text: String },
    ImageUrl { url: String, detail: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: LlmMessageContent,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<LlmToolCall>,
}

#[derive(Debug, Clone)]
pub struct LlmToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub parameters_schema: Value,
}

#[derive(Debug, Clone)]
pub enum LlmToolChoice {
    None,
    Auto,
    Required,
    Tool { name: String },
}

#[derive(Debug, Clone)]
pub enum LlmResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        strict: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct LlmChatRequest {
    pub model: String,
    pub messages: Vec<LlmMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub stream: bool,
    pub tools: Vec<LlmToolDefinition>,
    pub tool_choice: Option<LlmToolChoice>,
    pub response_format: Option<LlmResponseFormat>,
    pub metadata: Option<Value>,
    pub extra_body: Option<Value>,
}

impl LlmChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<LlmMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            max_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stream: false,
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            metadata: None,
            extra_body: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmChatResponse {
    pub model: Option<String>,
    pub content: String,
    pub refusal: Option<String>,
    pub tool_calls: Vec<LlmToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<LlmUsage>,
    pub raw_response: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct LlmUsage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

#[derive(Debug)]
pub enum LlmError {
    Http(reqwest::Error),
    Api(String),
    Config(String),
    InvalidResponse(String),
}

impl Display for LlmError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Http(err) => write!(f, "HTTP error: {}", err),
            LlmError::Api(message) => write!(f, "LLM API error: {}", message),
            LlmError::Config(message) => write!(f, "LLM config error: {}", message),
            LlmError::InvalidResponse(message) => write!(f, "Invalid LLM response: {}", message),
        }
    }
}

impl std::error::Error for LlmError {}

impl From<reqwest::Error> for LlmError {
    fn from(value: reqwest::Error) -> Self {
        LlmError::Http(value)
    }
}

// ── Streaming Types ──────────────────────────────────────

/// Events emitted during a streaming LLM response.
#[derive(Debug, Clone)]
pub enum LlmStreamEvent {
    /// Incremental text content from the model.
    TextDelta(String),
    /// Incremental tool call construction.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// Token usage information.
    Usage(LlmUsage),
    /// Stream has completed.
    Done {
        finish_reason: Option<String>,
        model: Option<String>,
    },
    /// An error occurred during streaming.
    Error(String),
}

/// Sent from the agent to the gateway streaming consumer.
#[derive(Debug)]
pub enum StreamDelta {
    /// Append this text to the accumulated stream.
    Text(String),
    /// Clear accumulated text (current response was not the final one).
    Clear,
}

/// Consumes `LlmStreamEvent`s from a receiver, optionally forwards text
/// deltas through a `StreamDelta` sender, and assembles the final
/// `LlmChatResponse`.
pub struct StreamAssembler {
    text: String,
    tool_calls: Vec<ToolCallAccumulator>,
    usage: Option<LlmUsage>,
    finish_reason: Option<String>,
    model: Option<String>,
    saw_tool_call: bool,
    sent_clear: bool,
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl StreamAssembler {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: None,
            model: None,
            saw_tool_call: false,
            sent_clear: false,
        }
    }

    /// Consumes all events from `rx`.
    /// Forwards text deltas via `delta_tx` until a tool call is seen.
    pub async fn consume(
        &mut self,
        rx: &mut tokio::sync::mpsc::Receiver<LlmStreamEvent>,
        delta_tx: Option<&tokio::sync::mpsc::Sender<StreamDelta>>,
    ) -> Result<LlmChatResponse, String> {
        while let Some(event) = rx.recv().await {
            match event {
                LlmStreamEvent::TextDelta(delta) => {
                    self.text.push_str(&delta);
                    if !self.saw_tool_call {
                        if let Some(tx) = delta_tx {
                            let _ = tx.send(StreamDelta::Text(delta)).await;
                        }
                    }
                }
                LlmStreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => {
                    if !self.saw_tool_call {
                        self.saw_tool_call = true;
                        if !self.sent_clear {
                            self.sent_clear = true;
                            if let Some(tx) = delta_tx {
                                let _ = tx.send(StreamDelta::Clear).await;
                            }
                        }
                    }
                    while self.tool_calls.len() <= index {
                        self.tool_calls.push(ToolCallAccumulator::default());
                    }
                    if let Some(id) = id {
                        self.tool_calls[index].id = Some(id);
                    }
                    if let Some(name) = name {
                        self.tool_calls[index].name = Some(name);
                    }
                    self.tool_calls[index].arguments.push_str(&arguments_delta);
                }
                LlmStreamEvent::Usage(u) => {
                    self.merge_usage(u);
                }
                LlmStreamEvent::Done {
                    finish_reason,
                    model,
                } => {
                    self.finish_reason = finish_reason;
                    if model.is_some() {
                        self.model = model;
                    }
                }
                LlmStreamEvent::Error(e) => return Err(e),
            }
        }

        let tool_calls = self
            .tool_calls
            .drain(..)
            .filter_map(|tc| {
                let name = tc.name?;
                let arguments = serde_json::from_str::<Value>(&tc.arguments)
                    .unwrap_or_else(|_| Value::String(tc.arguments));
                Some(LlmToolCall {
                    id: tc.id,
                    name,
                    arguments,
                })
            })
            .collect();

        Ok(LlmChatResponse {
            model: self.model.take(),
            content: std::mem::take(&mut self.text),
            refusal: None,
            tool_calls,
            finish_reason: self.finish_reason.take(),
            usage: self.usage.take(),
            raw_response: None,
        })
    }

    fn merge_usage(&mut self, u: LlmUsage) {
        if let Some(ref mut existing) = self.usage {
            if u.prompt_tokens.is_some() {
                existing.prompt_tokens = u.prompt_tokens;
            }
            if u.completion_tokens.is_some() {
                existing.completion_tokens = u.completion_tokens;
            }
            if u.total_tokens.is_some() {
                existing.total_tokens = u.total_tokens;
            }
        } else {
            self.usage = Some(u);
        }
    }
}

// ── LLM Interface Trait ──────────────────────────────────

#[async_trait]
pub trait LlmInterface: Send + Sync {
    async fn chat(&self, request: LlmChatRequest) -> Result<LlmChatResponse, LlmError>;

    /// Streaming variant of chat(). Returns a channel receiver that yields
    /// LlmStreamEvents as the LLM generates tokens.
    ///
    /// The default implementation falls back to non-streaming: it calls chat()
    /// and emits the complete response as a single TextDelta + Done.
    async fn chat_stream(
        &self,
        request: LlmChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<LlmStreamEvent>, LlmError> {
        let response = self.chat(request).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if !response.content.is_empty() {
                let _ = tx.send(LlmStreamEvent::TextDelta(response.content)).await;
            }
            for (i, tc) in response.tool_calls.iter().enumerate() {
                let _ = tx
                    .send(LlmStreamEvent::ToolCallDelta {
                        index: i,
                        id: tc.id.clone(),
                        name: Some(tc.name.clone()),
                        arguments_delta: tc.arguments.to_string(),
                    })
                    .await;
            }
            if let Some(u) = response.usage {
                let _ = tx.send(LlmStreamEvent::Usage(u)).await;
            }
            let _ = tx
                .send(LlmStreamEvent::Done {
                    finish_reason: response.finish_reason,
                    model: response.model,
                })
                .await;
        });
        Ok(rx)
    }

    fn provider_name(&self) -> &'static str;

    /// Returns the currently active model name.
    fn current_model(&self) -> String;

    /// Switches the active model at runtime.
    fn set_model(&self, model: String);

    /// Returns all models available from the provider, sorted.
    /// Returns an empty Vec if the provider does not support listing.
    async fn list_models(&self) -> Vec<String>;
}
