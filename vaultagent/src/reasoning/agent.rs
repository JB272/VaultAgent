use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::reasoning::usage::UsageCounter;
use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmContentPart, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
};
use crate::skills::SkillRegistry;
use crate::soul::Soul;

/// The Agent orchestrates LLM calls and tool executions.
/// It holds a persistent conversation history and builds the system prompt
/// dynamically from the Soul (personality + memory).
/// For subagents, a fixed system prompt can be used instead of a Soul.
pub struct Agent {
    llm: Option<Arc<dyn LlmInterface>>,
    skills: SkillRegistry,
    soul: Option<Arc<Soul>>,
    custom_system_prompt: Option<String>,
    history: Mutex<Vec<LlmMessage>>,
    max_rounds: usize,
    max_history: usize,
    pub usage: Option<Arc<UsageCounter>>,
}

impl Agent {
    /// Creates the main agent with a Soul (personality + memory).
    pub fn new(llm: Option<Arc<dyn LlmInterface>>, skills: SkillRegistry, soul: Arc<Soul>) -> Self {
        Self {
            llm,
            skills,
            soul: Some(soul),
            custom_system_prompt: None,
            history: Mutex::new(Vec::new()),
            max_rounds: 4,
            max_history: 50,
            usage: Some(Arc::new(UsageCounter::new())),
        }
    }

    /// Creates a focused subagent with a fixed system prompt (no Soul, no history carry-over).
    /// Runs up to 8 tool-call rounds — suited for deep research or multi-step delegated tasks.
    pub fn subagent(        llm: Arc<dyn LlmInterface>,
        skills: SkillRegistry,
        system_prompt: String,
    ) -> Self {
        Self {
            llm: Some(llm),
            skills,
            soul: None,
            custom_system_prompt: Some(system_prompt),
            history: Mutex::new(Vec::new()),
            max_rounds: 8,
            max_history: 20,
            usage: None, // subagents don't track usage separately
        }
    }

    /// Returns the names of all registered skills (used by the /tools command).
    pub fn skill_names(&self) -> Vec<String> {
        self.skills.skill_names()
    }

    /// Processes a chat message and returns the agent's response.
    /// Executes up to `max_rounds` tool-call cycles as needed.
    /// The conversation history is preserved across calls.
    /// `chat_id` is passed as context so skills like cron_add know
    /// which chat to send the response to.
    /// `image_url` — optional base64 data-URL of an attached image (vision).
    pub async fn process(&self, user_text: &str, chat_id: i64, image_url: Option<&str>) -> String {
        let Some(llm) = &self.llm else {
            return "LLM is not configured. Set LLM_API_KEY to receive responses.".to_string();
        };

        // Build user message content — with optional image for vision
        let user_content = if let Some(url) = image_url {
            LlmMessageContent::Parts(vec![
                LlmContentPart::Text {
                    text: user_text.to_string(),
                },
                LlmContentPart::ImageUrl {
                    url: url.to_string(),
                    detail: Some("auto".to_string()),
                },
            ])
        } else {
            LlmMessageContent::Text(user_text.to_string())
        };

        // Append user message to persistent history
        {
            let mut history = self.history.lock().await;
            history.push(LlmMessage {
                role: LlmRole::User,
                content: user_content,
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            });

            // Sliding window: trim oldest messages
            if history.len() > self.max_history {
                let excess = history.len() - self.max_history;
                history.drain(0..excess);
            }
        }

        // Build system prompt — use the custom override for subagents,
        // otherwise derive dynamically from Soul (personality + memory + session context).
        let system_prompt = if let Some(prompt) = &self.custom_system_prompt {
            prompt.clone()
        } else {
            let soul = self
                .soul
                .as_ref()
                .expect("Agent must have either a Soul or a custom_system_prompt");
            let base_prompt = soul.system_prompt();
            let user_tz = std::env::var("TIMEZONE").unwrap_or_else(|_| "Europe/Berlin".to_string());
            let now_utc = chrono::Utc::now().to_rfc3339();
            format!(
                "{}\n\n## Current Session\n- Chat ID: {}\n- User timezone: {}\n- Current UTC time: {}\n- IMPORTANT: If the user mentions a time (for example \"at 19:20\"), it is ALWAYS in their local timezone ({}). Convert that time to UTC before passing it to cron_add. Example: 19:20 CET = 18:20 UTC.",
                base_prompt, chat_id, user_tz, now_utc, user_tz
            )
        };

        let mut messages = vec![LlmMessage {
            role: LlmRole::Developer,
            content: LlmMessageContent::Text(system_prompt),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }];

        // Append full history
        {
            let history = self.history.lock().await;
            messages.extend(history.clone());
        }

        for _ in 0..self.max_rounds {
            let mut request = LlmChatRequest::new("", messages.clone());
            request.tools = self.skills.tool_definitions();

            let response = match llm.chat(request).await {
                Ok(value) => value,
                Err(err) => return format!("LLM call failed: {}", err),
            };

            // Record token usage
            if let Some(ref counter) = self.usage {
                if let Some(ref u) = response.usage {
                    counter.record(u.prompt_tokens, u.completion_tokens).await;
                }
            }

            // No tool calls → final response
            if response.tool_calls.is_empty() {
                let content = response.content.trim();
                if content.is_empty() {
                    let fallback = response
                        .refusal
                        .unwrap_or_else(|| "No response received from the LLM.".to_string());
                    // Save response to history
                    self.history.lock().await.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(fallback.clone()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                    return fallback;
                }
                // Save response to history
                self.history.lock().await.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: LlmMessageContent::Text(content.to_string()),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
                return content.to_string();
            }

            // Execute tool calls
            let tool_calls = response.tool_calls.clone();
            messages.push(LlmMessage {
                role: LlmRole::Assistant,
                content: LlmMessageContent::Text(response.content),
                name: None,
                tool_call_id: None,
                tool_calls,
            });

            for tool_call in response.tool_calls {
                let result = match self
                    .skills
                    .execute(&tool_call.name, &tool_call.arguments)
                    .await
                {
                    Some(result) => result,
                    None => json!({
                        "ok": false,
                        "error": format!("Unknown tool: {}", tool_call.name),
                    })
                    .to_string(),
                };

                messages.push(LlmMessage {
                    role: LlmRole::Tool,
                    content: LlmMessageContent::Text(result),
                    name: Some(tool_call.name),
                    tool_call_id: tool_call.id,
                    tool_calls: Vec::new(),
                });
            }
        }

        "Could not complete tool execution (too many steps).".to_string()
    }
}
