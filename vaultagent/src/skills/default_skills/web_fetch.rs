use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

use super::http_utils::fetch_page;

/// Skill: Fetches the readable text content of a given URL.
pub struct WebFetchSkill {
    client: Client,
}

impl WebFetchSkill {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("VaultAgent/1.0")
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }
}

#[async_trait]
impl Skill for WebFetchSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "web_fetch".to_string(),
            description: Some(
                "Fetches the text content of a webpage. \
                 Use this to read a specific URL — for example, a result you found with web_search. \
                 Returns plain-text content (HTML stripped, truncated to ~4000 chars)."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL of the page to fetch."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let url = match arguments.get("url").and_then(Value::as_str) {
            Some(u) => u,
            None => return json!({ "ok": false, "error": "'url' is required." }).to_string(),
        };

        println!("[WebFetch] Fetching: {}", url);
        fetch_page(&self.client, url, 4000).await
    }
}
