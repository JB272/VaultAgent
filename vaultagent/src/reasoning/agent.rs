use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmContentPart, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
    LlmToolChoice,
};
use crate::reasoning::usage::UsageCounter;
use crate::skills::SkillRegistry;
use crate::soul::Soul;

static GLOBAL_STOP_EPOCH: AtomicU64 = AtomicU64::new(0);

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
    fn stop_requested(start_epoch: u64) -> bool {
        GLOBAL_STOP_EPOCH.load(Ordering::Relaxed) != start_epoch
    }

    /// Cancels all currently running agent/subagent loops in this process.
    pub fn stop_all(&self) {
        GLOBAL_STOP_EPOCH.fetch_add(1, Ordering::Relaxed);
    }

    fn has_web_capability(&self) -> bool {
        self.skills
            .skill_names()
            .iter()
            .any(|n| n == "research" || n == "web_search" || n == "web_fetch")
    }

    fn has_shell_capability(&self) -> bool {
        self.skills
            .skill_names()
            .iter()
            .any(|n| n == "shell_execute")
    }

    fn looks_like_no_internet_claim(text: &str) -> bool {
        let t = text.to_lowercase();
        t.contains("can't browse")
            || t.contains("cannot browse")
            || t.contains("can't access external websites")
            || t.contains("do not have internet access")
            || t.contains("i don't have internet access")
            || t.contains("ich kann nicht im internet")
            || t.contains("ich habe keinen zugriff auf das internet")
            || t.contains("ich kann nicht auf externe websites zugreifen")
    }

    fn looks_like_permission_claim(text: &str) -> bool {
        let t = text.to_lowercase();
        t.contains("keine berechtigung")
            || t.contains("habe keine berechtigung")
            || t.contains("nicht die erforderlichen berechtigungen")
            || t.contains("permission denied")
            || t.contains("do not have permission")
            || t.contains("don't have permission")
            || t.contains("cannot install")
            || t.contains("can't install")
    }

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
            max_rounds: 25,
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
            max_rounds: 15,
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

    /// Checks if the context window is getting full and, if so, summarises
    /// older messages into a single compact message to free up space.
    /// Keeps the most recent `KEEP_RECENT` messages untouched.
    async fn maybe_summarize_history(&self) {
        const THRESHOLD: f64 = 0.70;
        const KEEP_RECENT: usize = 10;

        // Only summarise for the main agent (subagents are short-lived).
        if self.history_path.is_none() {
            return;
        }

        let prompt_tokens = *self.last_prompt_tokens.lock().await;
        if prompt_tokens == 0 {
            return;
        }
        let limit = (self.context_window_size as f64 * THRESHOLD) as u32;
        if prompt_tokens < limit {
            return;
        }

        let llm = match &self.llm {
            Some(llm) => llm.clone(),
            None => return,
        };

        let old_messages = {
            let history = self.history.lock().await;
            if history.len() <= KEEP_RECENT + 2 {
                return; // Not enough messages to summarize
            }
            let split = history.len() - KEEP_RECENT;
            history[..split].to_vec()
        };

        println!(
            "[Agent] Context at {:.0}% — summarising {} older messages",
            (prompt_tokens as f64 / self.context_window_size as f64) * 100.0,
            old_messages.len()
        );

        // Build a short summarisation request from the old messages.
        let mut sum_msgs = vec![LlmMessage {
            role: LlmRole::Developer,
            content: LlmMessageContent::Text(
                "Du bist ein Zusammenfassungs-Assistent. Fasse die folgende Konversation kompakt zusammen. \
                 Behalte alle wichtigen Fakten, Entscheidungen, Datei-Pfade, genannte Zahlen und Kontext. \
                 Antworte NUR mit der Zusammenfassung, keine Einleitung, kein Kommentar."
                    .to_string(),
            ),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }];

        // Include each old message as a simplified text representation.
        for msg in &old_messages {
            let role_label = match msg.role {
                LlmRole::User => "User",
                LlmRole::Assistant => "Assistant",
                LlmRole::Tool => "Tool",
                _ => continue, // skip system/developer
            };
            let text = match &msg.content {
                LlmMessageContent::Text(t) => t.clone(),
                LlmMessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        LlmContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            if text.is_empty() && msg.tool_calls.is_empty() {
                continue;
            }
            let mut line = format!("[{}] {}", role_label, text);
            for tc in &msg.tool_calls {
                line.push_str(&format!("\n  → tool_call: {}(…)", tc.name));
            }
            if let Some(ref name) = msg.name {
                line = format!("[Tool:{}] {}", name, text);
            }
            sum_msgs.push(LlmMessage {
                role: LlmRole::User,
                content: LlmMessageContent::Text(line),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            });
        }

        sum_msgs.push(LlmMessage {
            role: LlmRole::User,
            content: LlmMessageContent::Text("Fasse zusammen.".to_string()),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        });

        let mut req = LlmChatRequest::new("", sum_msgs);
        req.max_tokens = Some(1024);

        match llm.chat(req).await {
            Ok(resp) => {
                let summary = resp.content.trim().to_string();
                if summary.is_empty() {
                    eprintln!("[Agent] Summarization returned empty — falling back to simple trim");
                    let mut history = self.history.lock().await;
                    let split = history.len().saturating_sub(KEEP_RECENT);
                    history.drain(0..split);
                } else {
                    println!("[Agent] Summarization complete ({} chars)", summary.len());
                    let mut history = self.history.lock().await;
                    let split = history.len().saturating_sub(KEEP_RECENT);
                    history.drain(0..split);
                    // Insert the summary as the first message.
                    history.insert(
                        0,
                        LlmMessage {
                            role: LlmRole::User,
                            content: LlmMessageContent::Text(format!(
                                "[Zusammenfassung bisheriger Konversation]\n{}",
                                summary
                            )),
                            name: None,
                            tool_call_id: None,
                            tool_calls: Vec::new(),
                        },
                    );
                }
                // Track summary usage
                if let Some(ref counter) = self.usage {
                    if let Some(ref u) = resp.usage {
                        counter.record(u.prompt_tokens, u.completion_tokens).await;
                    }
                }
            }
            Err(err) => {
                eprintln!("[Agent] Summarization failed: {} — falling back to simple trim", err);
                let mut history = self.history.lock().await;
                let split = history.len().saturating_sub(KEEP_RECENT);
                history.drain(0..split);
            }
        }
        self.save_history().await;
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

        let start_stop_epoch = GLOBAL_STOP_EPOCH.load(Ordering::Relaxed);

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

            // Hard cap: if history is way too large, do a simple trim first
            // (the auto-summarizer handles the nuanced case below).
            if history.len() > self.max_history * 2 {
                let excess = history.len() - self.max_history;
                history.drain(0..excess);
            }
        }
        self.save_history().await;

        // Auto-summarize if context window is filling up.
        self.maybe_summarize_history().await;

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
                "{}\n\n## Current Session\n- Chat ID: {}\n- User timezone: {}\n- Current UTC time: {}\n- IMPORTANT: If the user mentions a time (for example \"at 19:20\"), it is ALWAYS in their local timezone ({}). Convert that time to UTC before passing it to cron_add. Example: 19:20 CET = 18:20 UTC.\n\n## Agent Behavior\n- When you have tools available, USE them to accomplish the task. Do NOT describe steps you would take — execute them.\n- Write scripts, run commands, fetch data, create files — then report the RESULT to the user, not the plan.\n- If a task requires multiple steps (e.g. install a package, write a script, run it), do ALL steps yourself using your tools before responding.\n- Only explain your approach if the user explicitly asks for an explanation or if you truly cannot execute the task.\n- Never say 'you could do X' or 'here are the steps' when you can do it yourself with the available tools.\n- If you need to continue working internally without messaging the user (e.g. between tool calls when you need to think about the next step), reply with exactly NO_REPLY — this will suppress the message and let you continue. Use this when intermediate output would just be noise for the user.\n- Never claim missing permissions or installation limits unless a tool call actually failed and you quote the concrete stderr/exit code in your reply.\n\n## File Handling Rules\n- If the user asks to store, move, rename, or organize files (for example: 'lege die Dateien ab'), do ONLY file operations.\n- Do NOT read, extract, summarize, or analyze file contents unless the user explicitly asks for content analysis.\n- For organization tasks, verify paths and report what was moved/stored, not file content.\n\n## File Upload Reply Format\n- If you created a file that should be sent back into the chat, return JSON in this exact shape: {{\"text\":\"optional short message\",\"upload_path\":\"relative/path/to/file.ext\",\"upload_caption\":\"optional caption\"}}.\n- Use workspace-relative paths only (no absolute paths, no ..).",
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

        let mut forced_tool_retry = false;
        let mut retried_after_no_web_claim = false;
        let mut retried_after_permission_claim = false;

        for _ in 0..self.max_rounds {
            if Self::stop_requested(start_stop_epoch) {
                return "⏹ Stopped.".to_string();
            }

            let mut request = LlmChatRequest::new("", messages.clone());
            request.tools = self.skills.tool_definitions();
            if forced_tool_retry {
                request.tool_choice = Some(LlmToolChoice::Required);
                forced_tool_retry = false;
            }

            let response = match llm.chat(request).await {
                Ok(value) => value,
                Err(err) => return format!("LLM call failed: {}", err),
            };

            if Self::stop_requested(start_stop_epoch) {
                return "⏹ Stopped.".to_string();
            }

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

            // No tool calls → check for NO_REPLY or final response
            if response.tool_calls.is_empty() {
                let content = response.content.trim();

                // NO_REPLY: the model signals it wants to continue thinking
                // without sending anything to the user. Add to messages and
                // loop so it can issue more tool calls or produce a real reply.
                if content == "NO_REPLY" || content == "[NO_REPLY]" {
                    messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(content.to_string()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                    continue;
                }

                // Safety net: if the model claims it cannot browse while web tools exist,
                // give it one forced tool-call retry instead of returning the refusal.
                if !retried_after_no_web_claim
                    && self.has_web_capability()
                    && Self::looks_like_no_internet_claim(content)
                {
                    retried_after_no_web_claim = true;
                    forced_tool_retry = true;

                    messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(response.content),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });

                    messages.push(LlmMessage {
                        role: LlmRole::Developer,
                        content: LlmMessageContent::Text(
                            "You have web access through tools (research, web_search, web_fetch). Do not claim that you cannot browse the internet. Use the available tools now. If a tool fails, report the concrete error and continue with the best available result."
                                .to_string(),
                        ),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });

                    continue;
                }

                // Safety net: if the model claims missing permissions/install limits,
                // force one retry with required tool-use and explicit stderr reporting.
                if !retried_after_permission_claim
                    && self.has_shell_capability()
                    && Self::looks_like_permission_claim(content)
                {
                    retried_after_permission_claim = true;
                    forced_tool_retry = true;

                    messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(response.content),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });

                    messages.push(LlmMessage {
                        role: LlmRole::Developer,
                        content: LlmMessageContent::Text(
                            "Do not claim missing permissions unless a real tool call failed with permission/installation errors. Use shell_execute now and report the exact stderr and exit_code if it fails. If install is needed, attempt it (for example with sudo apt-get / pip) before responding."
                                .to_string(),
                        ),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });

                    continue;
                }

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
                if Self::stop_requested(start_stop_epoch) {
                    return "⏹ Stopped.".to_string();
                }

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
