//! Provider layer (OpenAI-compatible abstraction).

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    pub prompt: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatResponse {
    pub content: String,
}

pub trait Provider {
    fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatResponse>;
}
