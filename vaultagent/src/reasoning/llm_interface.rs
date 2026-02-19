use async_trait::async_trait;
use serde_json::Value;
use std::fmt::{Display, Formatter};

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub enum LlmMessageContent {
	Text(String),
	Parts(Vec<LlmContentPart>),
}

#[derive(Debug, Clone)]
pub enum LlmContentPart {
	Text { text: String },
	ImageUrl {
		url: String,
		detail: Option<String>,
	},
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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

#[async_trait]
pub trait LlmInterface: Send + Sync {
	async fn chat(&self, request: LlmChatRequest) -> Result<LlmChatResponse, LlmError>;

	fn provider_name(&self) -> &'static str;
}
