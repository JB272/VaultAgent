use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
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
        }
    }

    /// Creates a focused subagent with a fixed system prompt (no Soul, no history carry-over).
    /// Runs up to 8 tool-call rounds — suited for deep research or multi-step delegated tasks.
    pub fn subagent(
        llm: Arc<dyn LlmInterface>,
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
        }
    }

    /// Verarbeitet eine Chat-Nachricht und gibt die Antwort des Agenten zurück.
    /// Führt bei Bedarf bis zu `max_rounds` Tool-Aufrufe-Zyklen durch.
    /// Die Conversation-History bleibt über Aufrufe hinweg erhalten.
    /// `chat_id` wird als Kontext mitgegeben, damit Skills wie cron_add wissen,
    /// an welchen Chat die Antwort gehen soll.
    pub async fn process(&self, user_text: &str, chat_id: i64) -> String {
        let Some(llm) = &self.llm else {
            return "LLM is not configured. Set LLM_API_KEY to receive responses."
                .to_string();
        };

        // User-Nachricht an die persistente History anhängen
        {
            let mut history = self.history.lock().await;
            history.push(LlmMessage {
                role: LlmRole::User,
                content: LlmMessageContent::Text(user_text.to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            });

            // Sliding Window: älteste Nachrichten kürzen
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
            let user_tz =
                std::env::var("TIMEZONE").unwrap_or_else(|_| "Europe/Berlin".to_string());
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

        // Gesamte History anhängen
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

            // Keine Tool-Calls → fertige Antwort
            if response.tool_calls.is_empty() {
                let content = response.content.trim();
                if content.is_empty() {
                    let fallback = response
                        .refusal
                        .unwrap_or_else(|| "No response received from the LLM.".to_string());
                    // Antwort in History speichern
                    self.history.lock().await.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(fallback.clone()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                    return fallback;
                }
                // Antwort in History speichern
                self.history.lock().await.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: LlmMessageContent::Text(content.to_string()),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
                return content.to_string();
            }

            // Tool-Calls ausführen
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
