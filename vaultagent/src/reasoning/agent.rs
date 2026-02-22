use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmContentPart, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
};
use crate::reasoning::usage::UsageCounter;
use crate::skills::SkillRegistry;
use crate::soul::Soul;

/// The Agent orchestrates LLM calls and tool executions.
/// It holds a single persistent conversation history shared across all
/// communication channels (Telegram, Website, …).
/// The history is persisted to a JSON file so it survives restarts.
/// For subagents, a fixed system prompt can be used instead of a Soul.
pub struct Agent {
    llm: Option<Arc<dyn LlmInterface>>,
    skills: SkillRegistry,
    soul: Option<Arc<Soul>>,
    custom_system_prompt: Option<String>,
    /// Single shared conversation history (one user, multiple channels).
    history: Mutex<Vec<LlmMessage>>,
    /// Tracks the last prompt_tokens returned by the LLM.
    last_prompt_tokens: Mutex<u32>,
    max_rounds: usize,
    max_history: usize,
    /// Maximum context window size in tokens (for /window percentage).
    context_window_size: u32,
    /// Path to the history JSON file (None for subagents).
    history_path: Option<PathBuf>,
    pub usage: Option<Arc<UsageCounter>>,
}

impl Agent {
    /// Creates the main agent with a Soul (personality + memory).
    pub fn new(llm: Option<Arc<dyn LlmInterface>>, skills: SkillRegistry, soul: Arc<Soul>) -> Self {
        let context_window_size: u32 = std::env::var("LLM_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(128_000);

        // History file lives next to soul dir
        let history_path = PathBuf::from(
            std::env::var("HISTORY_FILE").unwrap_or_else(|_| "chat_history.json".to_string()),
        );

        // Load existing history from disk
        let history = Self::load_history(&history_path);
        let msg_count = history.len();
        if msg_count > 0 {
            println!(
                "[Agent] Restored {} messages from {}",
                msg_count,
                history_path.display()
            );
        }

        Self {
            llm,
            skills,
            soul: Some(soul),
            custom_system_prompt: None,
            history: Mutex::new(history),
            last_prompt_tokens: Mutex::new(0),
            max_rounds: 4,
            max_history: 50,
            context_window_size,
            history_path: Some(history_path),
            usage: Some(Arc::new(UsageCounter::new())),
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
            last_prompt_tokens: Mutex::new(0),
            max_rounds: 8,
            max_history: 20,
            context_window_size: 128_000,
            history_path: None, // subagents don't persist
            usage: None,        // subagents don't track usage separately
        }
    }

    /// Loads history from a JSON file. Returns empty vec on any error.
    fn load_history(path: &Path) -> Vec<LlmMessage> {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|err| {
                eprintln!("[Agent] Failed to parse {}: {}", path.display(), err);
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    }

    /// Saves the current history to disk (fire-and-forget, logs errors).
    async fn save_history(&self) {
        let Some(ref path) = self.history_path else {
            return;
        };
        let history = self.history.lock().await;
        match serde_json::to_string(&*history) {
            Ok(json) => {
                if let Err(err) = std::fs::write(path, json) {
                    eprintln!(
                        "[Agent] Failed to write history to {}: {}",
                        path.display(),
                        err
                    );
                }
            }
            Err(err) => {
                eprintln!("[Agent] Failed to serialize history: {}", err);
            }
        }
    }

    /// Returns the names of all registered skills (used by the /tools command).
    pub fn skill_names(&self) -> Vec<String> {
        self.skills.skill_names()
    }

    /// Clears the shared conversation history. Called by /new.
    pub async fn clear_history(&self) {
        self.history.lock().await.clear();
        *self.last_prompt_tokens.lock().await = 0;
        self.save_history().await;
    }

    /// Returns context window usage info. Called by /window.
    pub async fn context_window_info(&self) -> String {
        let message_count = self.history.lock().await.len();
        let tokens = *self.last_prompt_tokens.lock().await;

        if tokens == 0 && message_count == 0 {
            return "🧠 <b>Context Window</b>\n\nKeine aktive Konversation. Sende eine Nachricht um zu starten.".to_string();
        }

        let pct = if self.context_window_size > 0 {
            ((tokens as f64 / self.context_window_size as f64) * 100.0).min(100.0)
        } else {
            0.0
        };

        // Visual progress bar
        let filled = (pct / 5.0).round() as usize;
        let empty = 20_usize.saturating_sub(filled);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));

        format!(
            "🧠 <b>Context Window</b>\n\n\
             {} <b>{:.0}%</b> belegt\n\n\
             • Tokens: <b>{}</b> / <b>{}</b>\n\
             • Nachrichten: <b>{}</b>\n\n\
             Nutze /new um die Konversation zurückzusetzen.",
            bar, pct, tokens, self.context_window_size, message_count
        )
    }

    /// Processes a chat message and returns the agent's response.
    /// Executes up to `max_rounds` tool-call cycles as needed.
    /// The conversation history is preserved across calls.
    /// `chat_id` is passed as context so skills like cron_add know
    /// which chat to send the response to.
    /// `image_url` — optional base64 data-URL of an attached image (vision).
    pub async fn process(&self, user_text: &str, chat_id: i64, image_url: Option<&str>) -> String {
        let Some(llm) = &self.llm else {
            return "LLM is not configured. Set OPENAI_API_KEY or ANTHROPIC_API_KEY to receive responses.".to_string();
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

        // Append user message to shared history
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
        self.save_history().await;

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

        // Append shared history
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
                    // Track last prompt tokens for /window
                    if let Some(pt) = u.prompt_tokens {
                        *self.last_prompt_tokens.lock().await = pt;
                    }
                }
            }

            // No tool calls → final response
            if response.tool_calls.is_empty() {
                let content = response.content.trim();
                if content.is_empty() {
                    let fallback = response
                        .refusal
                        .unwrap_or_else(|| "No response received from the LLM.".to_string());
                    // Save response to shared history
                    self.history.lock().await.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(fallback.clone()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                    self.save_history().await;
                    return fallback;
                }
                // Save response to shared history
                self.history.lock().await.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: LlmMessageContent::Text(content.to_string()),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
                self.save_history().await;
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
