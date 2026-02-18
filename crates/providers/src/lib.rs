//! Provider layer (OpenAI-compatible abstraction).

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub prompt: String,
    pub max_output_tokens: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatResponse {
    pub content: String,
}

pub trait Provider {
    fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse>;
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    pub base_url: String,
    pub api_key: String,
}

impl Provider for OpenAiCompatProvider {
    fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let endpoint = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = serde_json::json!({
            "model": req.model,
            "messages": [{"role":"user","content": req.prompt}],
            "max_tokens": req.max_output_tokens,
            "temperature": 0.2
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;

        let resp = client
            .post(endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .context("provider request failed")?
            .error_for_status()
            .context("provider returned non-success status")?;

        let json: serde_json::Value = resp.json().context("invalid provider JSON")?;
        let content = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .context("missing choices[0].message.content in provider response")?;

        Ok(ChatResponse { content })
    }
}
