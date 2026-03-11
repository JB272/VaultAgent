use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmChatResponse, LlmContentPart, LlmError, LlmInterface, LlmMessage,
    LlmMessageContent, LlmRole, LlmStreamEvent, LlmToolCall, LlmToolChoice, LlmUsage,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

#[derive(Clone)]
pub struct AnthropicClient {
    client: Client,
    api_key: String,
    base_url: String,
    default_model: Arc<Mutex<String>>,
}

impl AnthropicClient {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            api_key: api_key.into(),
            base_url: base_url.into(),
            default_model: Arc::new(Mutex::new(default_model.into())),
        }
    }

    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            LlmError::Config("Set ANTHROPIC_API_KEY as an environment variable.".to_string())
        })?;

        let base_url =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());

        let default_model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-3-5-sonnet-latest".to_string());

        Ok(Self::new(api_key, base_url, default_model))
    }

    /// Splits the messages list into an optional system prompt string and
    /// the remaining messages formatted for Anthropic's API.
    ///
    /// Key differences from OpenAI:
    /// - System/Developer messages become a top-level `system` field.
    /// - Tool results (LlmRole::Tool) are grouped into a single `user` message
    ///   with `tool_result` content blocks (Anthropic requires this).
    /// - Assistant messages that include tool calls use `tool_use` content blocks.
    fn map_messages(messages: Vec<LlmMessage>) -> (Option<String>, Vec<Value>) {
        let mut system_parts: Vec<String> = Vec::new();
        let mut result: Vec<Value> = Vec::new();
        let mut pending_tool_results: Vec<Value> = Vec::new();

        for msg in messages {
            match msg.role {
                LlmRole::System | LlmRole::Developer => {
                    // Flush pending tool results before extracting system content.
                    if !pending_tool_results.is_empty() {
                        result.push(json!({
                            "role": "user",
                            "content": pending_tool_results.drain(..).collect::<Vec<_>>()
                        }));
                    }
                    if let LlmMessageContent::Text(text) = msg.content {
                        system_parts.push(text);
                    }
                }

                LlmRole::Tool => {
                    let content_str = match msg.content {
                        LlmMessageContent::Text(text) => text,
                        LlmMessageContent::Parts(parts) => parts
                            .into_iter()
                            .filter_map(|p| match p {
                                LlmContentPart::Text { text } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    pending_tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.unwrap_or_default(),
                        "content": content_str,
                    }));
                }

                LlmRole::User => {
                    // Flush any accumulated tool results first.
                    if !pending_tool_results.is_empty() {
                        result.push(json!({
                            "role": "user",
                            "content": pending_tool_results.drain(..).collect::<Vec<_>>()
                        }));
                    }
                    result.push(json!({
                        "role": "user",
                        "content": Self::map_user_content(msg.content),
                    }));
                }

                LlmRole::Assistant => {
                    // Flush any accumulated tool results first.
                    if !pending_tool_results.is_empty() {
                        result.push(json!({
                            "role": "user",
                            "content": pending_tool_results.drain(..).collect::<Vec<_>>()
                        }));
                    }

                    let mut content_parts: Vec<Value> = Vec::new();

                    let text = match msg.content {
                        LlmMessageContent::Text(t) => t,
                        LlmMessageContent::Parts(parts) => parts
                            .into_iter()
                            .filter_map(|p| match p {
                                LlmContentPart::Text { text } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    if !text.is_empty() {
                        content_parts.push(json!({"type": "text", "text": text}));
                    }

                    for tool_call in &msg.tool_calls {
                        content_parts.push(json!({
                            "type": "tool_use",
                            "id": tool_call.id.as_deref().unwrap_or(""),
                            "name": tool_call.name,
                            "input": tool_call.arguments,
                        }));
                    }

                    // Anthropic requires at least one content block.
                    if content_parts.is_empty() {
                        content_parts.push(json!({"type": "text", "text": ""}));
                    }

                    result.push(json!({
                        "role": "assistant",
                        "content": content_parts,
                    }));
                }
            }
        }

        // Flush any remaining tool results.
        if !pending_tool_results.is_empty() {
            result.push(json!({
                "role": "user",
                "content": pending_tool_results,
            }));
        }

        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };

        (system, result)
    }

    fn map_user_content(content: LlmMessageContent) -> Value {
        match content {
            LlmMessageContent::Text(text) => json!([{"type": "text", "text": text}]),
            LlmMessageContent::Parts(parts) => Value::Array(
                parts
                    .into_iter()
                    .map(|part| match part {
                        LlmContentPart::Text { text } => json!({"type": "text", "text": text}),
                        LlmContentPart::ImageUrl { url, .. } => {
                            if url.starts_with("data:") {
                                if let Some((media_type, data)) = Self::parse_data_url(&url) {
                                    return json!({
                                        "type": "image",
                                        "source": {
                                            "type": "base64",
                                            "media_type": media_type,
                                            "data": data,
                                        }
                                    });
                                }
                            }
                            json!({
                                "type": "image",
                                "source": {"type": "url", "url": url}
                            })
                        }
                    })
                    .collect(),
            ),
        }
    }

    fn parse_data_url(url: &str) -> Option<(String, String)> {
        let without_scheme = url.strip_prefix("data:")?;
        let (header, data) = without_scheme.split_once(',')?;
        let media_type = header.strip_suffix(";base64")?;
        Some((media_type.to_string(), data.to_string()))
    }

    /// Returns the max output tokens supported by a given Claude model.
    fn max_output_tokens(model: &str) -> u32 {
        // Claude 3.5+ and Claude 4+ support 8192 output tokens.
        // Older Claude 3 models (haiku, sonnet, opus) are limited to 4096.
        if model.contains("3-5")
            || model.contains("claude-sonnet-4")
            || model.contains("claude-opus-4")
        {
            8192
        } else {
            4096
        }
    }

    /// Builds the JSON payload shared by chat() and chat_stream().
    fn build_payload(
        &self,
        request: &LlmChatRequest,
    ) -> Result<(Map<String, Value>, String), LlmError> {
        let model = if request.model.is_empty() {
            self.default_model.lock().unwrap().clone()
        } else {
            request.model.clone()
        };

        let (system_prompt, messages) = Self::map_messages(request.messages.clone());

        let mut payload = Map::new();
        payload.insert("model".to_string(), Value::String(model.clone()));
        payload.insert("messages".to_string(), Value::Array(messages));

        let model_max = Self::max_output_tokens(&model);
        let max_tokens = request
            .max_tokens
            .map(|m| m.min(model_max))
            .unwrap_or(model_max);
        payload.insert("max_tokens".to_string(), json!(max_tokens));

        if let Some(system) = system_prompt {
            payload.insert("system".to_string(), Value::String(system));
        }
        if let Some(value) = request.temperature {
            payload.insert("temperature".to_string(), json!(value));
        }
        if let Some(value) = request.top_p {
            payload.insert("top_p".to_string(), json!(value));
        }

        if !request.tools.is_empty() {
            payload.insert(
                "tools".to_string(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool.name,
                                "description": tool.description,
                                "input_schema": tool.parameters_schema,
                            })
                        })
                        .collect(),
                ),
            );
        }

        if let Some(ref tool_choice) = request.tool_choice {
            let tc_value = match tool_choice {
                LlmToolChoice::None => json!({"type": "none"}),
                LlmToolChoice::Auto => json!({"type": "auto"}),
                LlmToolChoice::Required => json!({"type": "any"}),
                LlmToolChoice::Tool { name } => json!({"type": "tool", "name": name}),
            };
            payload.insert("tool_choice".to_string(), tc_value);
        }

        Ok((payload, model))
    }
}

/// Parses an Anthropic SSE stream and sends events to the channel.
async fn parse_anthropic_sse(response: reqwest::Response, tx: mpsc::Sender<LlmStreamEvent>) {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut model: Option<String> = None;
    let mut finish_reason: Option<String> = None;
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    // Track current content block index for tool call deltas
    let mut _current_tool_index: usize = 0;

    while let Some(chunk_result) = stream.next().await {
        let bytes = match chunk_result {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(LlmStreamEvent::Error(e.to_string())).await;
                return;
            }
        };

        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(boundary) = buffer.find("\n\n") {
            let raw_event = buffer[..boundary].to_string();
            buffer = buffer[boundary + 2..].to_string();

            let mut event_type: Option<&str> = None;
            let mut data_str: Option<String> = None;

            for line in raw_event.lines() {
                if let Some(et) = line.strip_prefix("event: ") {
                    event_type = Some(et.trim());
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data_str = Some(d.to_string());
                }
            }

            // event_type borrows from raw_event, so convert to owned for matching
            let event_type = event_type.map(|s| s.to_string());

            let data: Option<Value> = data_str
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());

            match event_type.as_deref() {
                Some("message_start") => {
                    if let Some(ref data) = data {
                        model = data
                            .pointer("/message/model")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        input_tokens = data
                            .pointer("/message/usage/input_tokens")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32);
                    }
                }
                Some("content_block_start") => {
                    if let Some(ref data) = data {
                        let index =
                            data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                        if let Some(block) = data.get("content_block") {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                _current_tool_index = index;
                                let _ = tx
                                    .send(LlmStreamEvent::ToolCallDelta {
                                        index,
                                        id: block
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string()),
                                        name: block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string()),
                                        arguments_delta: String::new(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
                Some("content_block_delta") => {
                    if let Some(ref data) = data {
                        let index =
                            data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                        if let Some(delta) = data.get("delta") {
                            match delta.get("type").and_then(|v| v.as_str()) {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                        if !text.is_empty() {
                                            let _ = tx
                                                .send(LlmStreamEvent::TextDelta(text.to_string()))
                                                .await;
                                        }
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(partial) =
                                        delta.get("partial_json").and_then(|v| v.as_str())
                                    {
                                        let _ = tx
                                            .send(LlmStreamEvent::ToolCallDelta {
                                                index,
                                                id: None,
                                                name: None,
                                                arguments_delta: partial.to_string(),
                                            })
                                            .await;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some("message_delta") => {
                    if let Some(ref data) = data {
                        finish_reason = data
                            .pointer("/delta/stop_reason")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        output_tokens = data
                            .pointer("/usage/output_tokens")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32);
                    }
                }
                Some("message_stop") => {
                    // Send usage if available
                    if input_tokens.is_some() || output_tokens.is_some() {
                        let prompt = input_tokens;
                        let completion = output_tokens;
                        let total = match (prompt, completion) {
                            (Some(p), Some(c)) => Some(p + c),
                            _ => None,
                        };
                        let _ = tx
                            .send(LlmStreamEvent::Usage(LlmUsage {
                                prompt_tokens: prompt,
                                completion_tokens: completion,
                                total_tokens: total,
                            }))
                            .await;
                    }
                    let _ = tx
                        .send(LlmStreamEvent::Done {
                            finish_reason: finish_reason.take(),
                            model: model.take(),
                        })
                        .await;
                    return;
                }
                Some("error") => {
                    let msg = data
                        .as_ref()
                        .and_then(|d| d.get("error"))
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown streaming error")
                        .to_string();
                    let _ = tx.send(LlmStreamEvent::Error(msg)).await;
                    return;
                }
                _ => {}
            }
        }
    }

    // Stream ended without message_stop
    let _ = tx
        .send(LlmStreamEvent::Done {
            finish_reason: finish_reason.take(),
            model: model.take(),
        })
        .await;
}

#[async_trait]
impl LlmInterface for AnthropicClient {
    async fn chat(&self, request: LlmChatRequest) -> Result<LlmChatResponse, LlmError> {
        let (payload, _) = self.build_payload(&request)?;

        let response = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&Value::Object(payload))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Api(format!("status {}: {}", status, body)));
        }

        let body: AnthropicResponse = response.json().await?;
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<LlmToolCall> = Vec::new();

        for block in body.content {
            match block {
                AnthropicContentBlock::Text { text } => text_parts.push(text),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(LlmToolCall {
                        id: Some(id),
                        name,
                        arguments: input,
                    });
                }
            }
        }

        Ok(LlmChatResponse {
            model: Some(body.model),
            content: text_parts.join("\n"),
            refusal: None,
            tool_calls,
            finish_reason: Some(body.stop_reason),
            usage: Some(LlmUsage {
                prompt_tokens: Some(body.usage.input_tokens),
                completion_tokens: Some(body.usage.output_tokens),
                total_tokens: Some(body.usage.input_tokens + body.usage.output_tokens),
            }),
            raw_response: None,
        })
    }

    async fn chat_stream(
        &self,
        request: LlmChatRequest,
    ) -> Result<mpsc::Receiver<LlmStreamEvent>, LlmError> {
        let (mut payload, _model) = self.build_payload(&request)?;
        payload.insert("stream".to_string(), Value::Bool(true));

        let response = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&Value::Object(payload))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Api(format!("status {}: {}", status, body)));
        }

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(parse_anthropic_sse(response, tx));
        Ok(rx)
    }

    fn provider_name(&self) -> &'static str {
        "anthropic"
    }

    fn current_model(&self) -> String {
        self.default_model.lock().unwrap().clone()
    }

    fn set_model(&self, model: String) {
        *self.default_model.lock().unwrap() = model;
    }

    async fn list_models(&self) -> Vec<String> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .send()
            .await;

        let response = match response {
            Ok(r) if r.status().is_success() => r,
            _ => return Vec::new(),
        };

        #[derive(serde::Deserialize)]
        struct ModelEntry {
            id: String,
        }
        #[derive(serde::Deserialize)]
        struct ModelList {
            data: Vec<ModelEntry>,
        }

        match response.json::<ModelList>().await {
            Ok(list) => {
                let mut ids: Vec<String> = list.data.into_iter().map(|m| m.id).collect();
                ids.sort();
                ids
            }
            Err(_) => Vec::new(),
        }
    }
}

// ── Anthropic response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
struct AnthropicResponse {
    model: String,
    content: Vec<AnthropicContentBlock>,
    stop_reason: String,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}
