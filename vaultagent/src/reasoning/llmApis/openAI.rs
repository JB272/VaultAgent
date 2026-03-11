use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmChatResponse, LlmContentPart, LlmError, LlmInterface, LlmMessage,
    LlmMessageContent, LlmResponseFormat, LlmStreamEvent, LlmToolCall, LlmToolChoice, LlmUsage,
};

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    client: Client,
    api_key: String,
    base_url: String,
    /// Shared mutable model name — switchable at runtime via /models.
    default_model: Arc<Mutex<String>>,
}

impl OpenAiCompatibleClient {
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
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("LLM_API_KEY"))
            .map_err(|_| {
                LlmError::Config("Set OPENAI_API_KEY as an environment variable.".to_string())
            })?;

        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("LLM_BASE_URL"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        let default_model = std::env::var("OPENAI_MODEL")
            .or_else(|_| std::env::var("LLM_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".to_string());

        Ok(Self::new(api_key, base_url, default_model))
    }

    /// Builds the JSON payload shared by chat() and chat_stream().
    fn build_payload(&self, request: &LlmChatRequest) -> Result<Map<String, Value>, LlmError> {
        let model = if request.model.is_empty() {
            self.default_model.lock().unwrap().clone()
        } else {
            request.model.clone()
        };

        let mut payload = Map::new();
        payload.insert("model".to_string(), Value::String(model));
        payload.insert(
            "messages".to_string(),
            serde_json::to_value(
                request
                    .messages
                    .iter()
                    .cloned()
                    .map(Self::map_message)
                    .collect::<Vec<_>>(),
            )
            .map_err(|err| LlmError::InvalidResponse(err.to_string()))?,
        );

        if let Some(value) = request.temperature {
            payload.insert("temperature".to_string(), json!(value));
        }
        if let Some(value) = request.max_tokens {
            payload.insert("max_tokens".to_string(), json!(value));
        }
        if let Some(value) = request.top_p {
            payload.insert("top_p".to_string(), json!(value));
        }
        if let Some(value) = request.frequency_penalty {
            payload.insert("frequency_penalty".to_string(), json!(value));
        }
        if let Some(value) = request.presence_penalty {
            payload.insert("presence_penalty".to_string(), json!(value));
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
                                "type": "function",
                                "function": {
                                    "name": tool.name,
                                    "description": tool.description,
                                    "parameters": tool.parameters_schema,
                                }
                            })
                        })
                        .collect(),
                ),
            );
        }
        if let Some(ref value) = request.tool_choice {
            payload.insert(
                "tool_choice".to_string(),
                Self::map_tool_choice(value.clone()),
            );
        }
        if let Some(ref value) = request.response_format {
            payload.insert(
                "response_format".to_string(),
                Self::map_response_format(value.clone()),
            );
        }
        if let Some(ref value) = request.metadata {
            payload.insert("metadata".to_string(), value.clone());
        }
        if let Some(Value::Object(ref extra)) = request.extra_body {
            for (key, value) in extra {
                payload.insert(key.clone(), value.clone());
            }
        }

        Ok(payload)
    }

    fn map_content(content: LlmMessageContent) -> Value {
        match content {
            LlmMessageContent::Text(text) => Value::String(text),
            LlmMessageContent::Parts(parts) => Value::Array(
                parts
                    .into_iter()
                    .map(|part| match part {
                        LlmContentPart::Text { text } => json!({
                            "type": "text",
                            "text": text,
                        }),
                        LlmContentPart::ImageUrl { url, detail } => {
                            let mut image_url = Map::new();
                            image_url.insert("url".to_string(), Value::String(url));
                            if let Some(detail_value) = detail {
                                image_url
                                    .insert("detail".to_string(), Value::String(detail_value));
                            }

                            let mut part_object = Map::new();
                            part_object
                                .insert("type".to_string(), Value::String("image_url".to_string()));
                            part_object.insert("image_url".to_string(), Value::Object(image_url));
                            Value::Object(part_object)
                        }
                    })
                    .collect(),
            ),
        }
    }

    fn map_tool_choice(tool_choice: LlmToolChoice) -> Value {
        match tool_choice {
            LlmToolChoice::None => Value::String("none".to_string()),
            LlmToolChoice::Auto => Value::String("auto".to_string()),
            LlmToolChoice::Required => Value::String("required".to_string()),
            LlmToolChoice::Tool { name } => json!({
                "type": "function",
                "function": {
                    "name": name,
                }
            }),
        }
    }

    fn map_response_format(format: LlmResponseFormat) -> Value {
        match format {
            LlmResponseFormat::Text => json!({"type": "text"}),
            LlmResponseFormat::JsonObject => json!({"type": "json_object"}),
            LlmResponseFormat::JsonSchema {
                name,
                schema,
                strict,
            } => {
                let mut json_schema_object = Map::new();
                json_schema_object.insert("name".to_string(), Value::String(name));
                json_schema_object.insert("schema".to_string(), schema);
                if let Some(strict_value) = strict {
                    json_schema_object.insert("strict".to_string(), Value::Bool(strict_value));
                }

                let mut root = Map::new();
                root.insert("type".to_string(), Value::String("json_schema".to_string()));
                root.insert("json_schema".to_string(), Value::Object(json_schema_object));
                Value::Object(root)
            }
        }
    }

    fn map_message(message: LlmMessage) -> OpenAiMessage {
        OpenAiMessage {
            role: message.role.as_str().to_string(),
            content: Self::map_content(message.content),
            name: message.name,
            tool_call_id: message.tool_call_id,
            tool_calls: if message.tool_calls.is_empty() {
                None
            } else {
                Some(
                    message
                        .tool_calls
                        .into_iter()
                        .map(|tool_call| OpenAiToolCall {
                            id: tool_call.id,
                            type_name: "function".to_string(),
                            function: OpenAiToolCallFunction {
                                name: tool_call.name,
                                arguments: tool_call.arguments.to_string(),
                            },
                        })
                        .collect(),
                )
            },
        }
    }
}

/// Parses an OpenAI SSE stream and sends events to the channel.
async fn parse_openai_sse(response: reqwest::Response, tx: mpsc::Sender<LlmStreamEvent>) {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut model: Option<String> = None;
    let mut finish_reason: Option<String> = None;

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

            for line in raw_event.lines() {
                let data = match line.strip_prefix("data: ") {
                    Some(d) => d.trim(),
                    None => continue,
                };

                if data == "[DONE]" {
                    let _ = tx
                        .send(LlmStreamEvent::Done {
                            finish_reason: finish_reason.take(),
                            model: model.take(),
                        })
                        .await;
                    return;
                }

                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if model.is_none() {
                    model = parsed
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }

                if let Some(choice) = parsed
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                {
                    // Text delta
                    if let Some(content) = choice
                        .pointer("/delta/content")
                        .and_then(|v| v.as_str())
                    {
                        if !content.is_empty() {
                            let _ = tx
                                .send(LlmStreamEvent::TextDelta(content.to_string()))
                                .await;
                        }
                    }

                    // Tool call deltas
                    if let Some(tool_calls) = choice
                        .pointer("/delta/tool_calls")
                        .and_then(|v| v.as_array())
                    {
                        for tc in tool_calls {
                            let _ = tx
                                .send(LlmStreamEvent::ToolCallDelta {
                                    index: tc
                                        .get("index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as usize,
                                    id: tc
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    name: tc
                                        .pointer("/function/name")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    arguments_delta: tc
                                        .pointer("/function/arguments")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                })
                                .await;
                        }
                    }

                    // Finish reason
                    if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                        finish_reason = Some(fr.to_string());
                    }
                }

                // Usage (from stream_options.include_usage)
                if let Some(usage) = parsed.get("usage") {
                    if usage.is_object() {
                        let _ = tx
                            .send(LlmStreamEvent::Usage(LlmUsage {
                                prompt_tokens: usage
                                    .get("prompt_tokens")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32),
                                completion_tokens: usage
                                    .get("completion_tokens")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32),
                                total_tokens: usage
                                    .get("total_tokens")
                                    .and_then(|v| v.as_u64())
                                    .map(|v| v as u32),
                            }))
                            .await;
                    }
                }
            }
        }
    }

    // Stream ended without [DONE]
    let _ = tx
        .send(LlmStreamEvent::Done {
            finish_reason: finish_reason.take(),
            model: model.take(),
        })
        .await;
}

#[async_trait]
impl LlmInterface for OpenAiCompatibleClient {
    async fn chat(&self, request: LlmChatRequest) -> Result<LlmChatResponse, LlmError> {
        let mut payload = self.build_payload(&request)?;

        if request.stream {
            payload.insert("stream".to_string(), Value::Bool(true));
        }

        let response = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(&Value::Object(payload))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Api(format!("status {}: {}", status, body)));
        }

        let body: OpenAiChatResponse = response.json().await?;
        let first = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::InvalidResponse("No choices returned".to_string()))?;

        let mapped_tool_calls = first
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tool_call| {
                let parsed_arguments =
                    serde_json::from_str::<Value>(&tool_call.function.arguments)
                        .unwrap_or_else(|_| Value::String(tool_call.function.arguments));

                LlmToolCall {
                    id: tool_call.id,
                    name: tool_call.function.name,
                    arguments: parsed_arguments,
                }
            })
            .collect();

        Ok(LlmChatResponse {
            model: body.model,
            content: first.message.content.unwrap_or_default(),
            refusal: first.message.refusal,
            tool_calls: mapped_tool_calls,
            finish_reason: first.finish_reason,
            usage: body.usage.map(|usage| LlmUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
            }),
            raw_response: None,
        })
    }

    async fn chat_stream(
        &self,
        request: LlmChatRequest,
    ) -> Result<mpsc::Receiver<LlmStreamEvent>, LlmError> {
        let mut payload = self.build_payload(&request)?;
        payload.insert("stream".to_string(), Value::Bool(true));
        payload.insert(
            "stream_options".to_string(),
            json!({"include_usage": true}),
        );

        let response = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(&Value::Object(payload))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::Api(format!("status {}: {}", status, body)));
        }

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(parse_openai_sse(response, tx));
        Ok(rx)
    }

    fn provider_name(&self) -> &'static str {
        "openai-compatible"
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
            .bearer_auth(&self.api_key)
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

#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: String,
    content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type")]
    type_name: String,
    function: OpenAiToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAiChatResponse {
    model: Option<String>,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAiChoice {
    message: OpenAiAssistantMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAiAssistantMessage {
    content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}
