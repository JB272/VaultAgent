use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmChatResponse, LlmContentPart, LlmError, LlmInterface, LlmMessage,
    LlmMessageContent, LlmRole, StreamAssembler, StreamDelta,
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

    async fn recent_upload_index_context(&self, max_items: usize) -> Option<String> {
        let soul = self.soul.as_ref()?;
        let index_path = soul.dir().join("uploads_index.md");
        let raw = tokio::fs::read_to_string(index_path).await.ok()?;

        let mut rows: Vec<String> = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with("- ") {
                continue;
            }

            let mut ts = "unknown".to_string();
            let mut kind = "unknown".to_string();
            let mut name = "unknown".to_string();
            let mut path = None::<String>;

            for (idx, part) in trimmed.trim_start_matches("- ").split('|').enumerate() {
                let p = part.trim();
                if idx == 0 {
                    ts = p.to_string();
                    continue;
                }
                if let Some(v) = p.strip_prefix("kind=") {
                    kind = v.trim().to_string();
                    continue;
                }
                if let Some(v) = p.strip_prefix("name=") {
                    name = v.trim().to_string();
                    continue;
                }
                if let Some(v) = p.strip_prefix("path=") {
                    path = Some(v.trim().to_string());
                }
            }

            if let Some(path) = path {
                rows.push(format!(
                    "- {} | kind={} | name={} | path={}",
                    ts, kind, name, path
                ));
            }
        }

        if rows.is_empty() {
            return None;
        }

        let start = rows.len().saturating_sub(max_items);
        Some(rows[start..].join("\n"))
    }

    fn preview_text(text: &str, max_chars: usize) -> String {
        let mut out = String::new();
        for (idx, ch) in text.chars().enumerate() {
            if idx >= max_chars {
                out.push_str("...[truncated]");
                return out;
            }
            out.push(ch);
        }
        out
    }

    /// Accepts NO_REPLY even if the model adds surrounding text accidentally.
    fn is_no_reply_signal(text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.eq_ignore_ascii_case("NO_REPLY") || trimmed.eq_ignore_ascii_case("[NO_REPLY]") {
            return true;
        }

        // Common failure mode: model appends NO_REPLY after a sentence.
        let upper = trimmed.to_ascii_uppercase();
        if upper.ends_with(" NO_REPLY") || upper.ends_with(" [NO_REPLY]") {
            return true;
        }

        // Also accept a line that only contains NO_REPLY (plus punctuation).
        for line in upper.lines() {
            let cleaned = line
                .trim()
                .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_ascii_whitespace());
            if cleaned == "NO_REPLY" {
                return true;
            }
        }

        false
    }

    /// Creates the main agent with a Soul (personality + memory).
    pub fn new(llm: Option<Arc<dyn LlmInterface>>, skills: SkillRegistry, soul: Arc<Soul>) -> Self {
        let context_window_size: u32 = std::env::var("LLM_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(128_000);
        let max_history: usize = std::env::var("MAX_HISTORY_MESSAGES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

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
            max_history,
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
        let json = {
            let history = self.history.lock().await;
            match serde_json::to_string(&*history) {
                Ok(j) => j,
                Err(err) => {
                    eprintln!("[Agent] Failed to serialize history: {}", err);
                    return;
                }
            }
        };
        // Mutex released before the async write.
        if let Err(err) = tokio::fs::write(path, json).await {
            eprintln!(
                "[Agent] Failed to write history to {}: {}",
                path.display(),
                err
            );
        }
    }

    /// Retries an LLM call on transient errors (429, 500, 502, 503, timeout).
    async fn chat_with_retry(
        llm: &dyn LlmInterface,
        request: LlmChatRequest,
        max_retries: u32,
    ) -> Result<LlmChatResponse, LlmError> {
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(3)));
                println!(
                    "[Agent] LLM transient error, retry {}/{} in {:?}",
                    attempt, max_retries, delay
                );
                tokio::time::sleep(delay).await;
            }

            match llm.chat(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(ref e) if attempt < max_retries && Self::is_retryable_error(e) => {
                    eprintln!("[Agent] Transient LLM error: {}", e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    fn is_retryable_error(err: &LlmError) -> bool {
        match err {
            LlmError::Http(_) => true, // network/timeout errors
            LlmError::Api(msg) => {
                msg.contains("status 429")
                    || msg.contains("status 500")
                    || msg.contains("status 502")
                    || msg.contains("status 503")
                    || msg.contains("status 529") // Anthropic overloaded
            }
            _ => false,
        }
    }

    /// Streaming variant of chat_with_retry.
    /// Retries only on connection-level errors (not mid-stream failures).
    async fn chat_stream_with_retry(
        llm: &dyn LlmInterface,
        request: LlmChatRequest,
        max_retries: u32,
        delta_tx: Option<&tokio::sync::mpsc::Sender<StreamDelta>>,
    ) -> Result<LlmChatResponse, LlmError> {
        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(3)));
                println!(
                    "[Agent] LLM stream transient error, retry {}/{} in {:?}",
                    attempt, max_retries, delay
                );
                tokio::time::sleep(delay).await;
            }

            match llm.chat_stream(request.clone()).await {
                Ok(mut rx) => {
                    let mut assembler = StreamAssembler::new();
                    match assembler.consume(&mut rx, delta_tx).await {
                        Ok(response) => return Ok(response),
                        Err(e) => {
                            if attempt < max_retries {
                                eprintln!("[Agent] Stream error: {}", e);
                                continue;
                            }
                            return Err(LlmError::Api(e));
                        }
                    }
                }
                Err(ref e) if attempt < max_retries && Self::is_retryable_error(e) => {
                    eprintln!("[Agent] Transient LLM stream error: {}", e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
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
                "You are a summarization assistant. Summarize the following conversation compactly. \
                 Keep all important facts, decisions, file paths, numbers, and context. \
                 Reply ONLY with the summary, no intro and no commentary."
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
            content: LlmMessageContent::Text("Summarize now.".to_string()),
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
                                "[Summary of previous conversation]\n{}",
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
                eprintln!(
                    "[Agent] Summarization failed: {} — falling back to simple trim",
                    err
                );
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

    /// Returns active provider/model label for status messages.
    pub fn active_model_label(&self) -> Option<String> {
        self.llm
            .as_ref()
            .map(|llm| format!("{}/{}", llm.provider_name(), llm.current_model()))
    }

    /// Clears the shared conversation history. Called by /new.
    /// Snapshots the session to a dated memory file before clearing.
    pub async fn clear_history(&self) {
        self.snapshot_and_save_session().await;
        self.history.lock().await.clear();
        *self.last_prompt_tokens.lock().await = 0;
        self.save_history().await;
    }

    /// Summarises the current conversation and saves it to
    /// `soul/memory/YYYY-MM-DD-<slug>.md` so past sessions can be recalled
    /// on-demand via `memory_search` / `memory_get`.
    async fn snapshot_and_save_session(&self) {
        let (Some(llm), Some(soul)) = (&self.llm, &self.soul) else {
            return;
        };

        // Clone history before doing anything async.
        let messages = {
            let history = self.history.lock().await;
            if history.is_empty() {
                return;
            }
            history.clone()
        };

        // Build a compact text representation of the conversation.
        let convo_text: String = messages
            .iter()
            .filter_map(|msg| {
                let role_label = match msg.role {
                    LlmRole::User => "User",
                    LlmRole::Assistant => "Assistant",
                    _ => return None,
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
                if text.is_empty() {
                    return None;
                }
                Some(format!("[{}] {}", role_label, text))
            })
            .collect::<Vec<_>>()
            .join("\n");

        if convo_text.trim().is_empty() {
            return;
        }

        // Truncate input so the summarisation stays cheap.
        let truncated = &convo_text[..convo_text.len().min(8_000)];

        let prompt = format!(
            "Summarize this conversation compactly in 3-10 bullet points (same language as the conversation). \
             Also generate a short, descriptive slug (2-4 words, lowercase, dashes only, no special chars). \
             Respond with ONLY valid JSON: {{\"slug\": \"...\", \"summary\": \"...\"}}\n\n{}",
            truncated
        );

        let mut req = LlmChatRequest::new(
            "",
            vec![LlmMessage {
                role: LlmRole::User,
                content: LlmMessageContent::Text(prompt),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
        );
        req.max_tokens = Some(512);

        let resp = match llm.chat(req).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[Agent] Session snapshot LLM call failed: {}", e);
                return;
            }
        };

        // Track token usage.
        if let (Some(counter), Some(u)) = (&self.usage, &resp.usage) {
            counter.record(u.prompt_tokens, u.completion_tokens).await;
        }

        // Extract JSON — tolerate surrounding prose.
        let raw = resp.content.trim();
        let json_start = raw.find('{').unwrap_or(0);
        let parsed: serde_json::Value =
            serde_json::from_str(&raw[json_start..]).unwrap_or_default();

        let slug_raw = parsed
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("session");
        let summary = parsed
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or(raw);

        // Sanitise slug: lowercase alphanumeric + dashes only.
        let slug: String = slug_raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();

        let date = chrono::Local::now().date_naive();
        let filename = format!("{}-{}.md", date.format("%Y-%m-%d"), slug);
        let content = format!("# Session: {}\n\n{}\n", slug, summary);

        match soul
            .memory
            .write_session_snapshot(&filename, &content)
            .await
        {
            Ok(()) => println!("[Agent] Session snapshot → memory/{}", filename),
            Err(e) => eprintln!("[Agent] Session snapshot write failed: {}", e),
        }
    }

    /// Returns context window usage info. Called by /window.
    pub async fn context_window_info(&self) -> String {
        let message_count = self.history.lock().await.len();
        let tokens = *self.last_prompt_tokens.lock().await;

        if tokens == 0 && message_count == 0 {
            return "🧠 <b>Context Window</b>\n\nNo active conversation yet. Send a message to begin.".to_string();
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
             {} <b>{:.0}%</b> used\n\n\
             • Tokens: <b>{}</b> / <b>{}</b>\n\
             • Messages: <b>{}</b>\n\n\
             Use /new to reset the conversation.",
            bar, pct, tokens, self.context_window_size, message_count
        )
    }

    /// Processes a chat message and returns the agent's response.
    /// Executes up to `max_rounds` tool-call cycles as needed.
    /// The conversation history is preserved across calls.
    /// `chat_id` is passed as context so skills like cron_add know
    /// which chat to send the response to.
    /// `image_url` — optional base64 data-URL of an attached image (vision).
    pub async fn process(
        &self,
        user_text: &str,
        chat_id: i64,
        image_url: Option<&str>,
        stream_tx: Option<tokio::sync::mpsc::Sender<StreamDelta>>,
    ) -> String {
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
            let base_prompt = soul.system_prompt().await;
            let user_tz_str = std::env::var("TIMEZONE").unwrap_or_else(|_| "Europe/Berlin".to_string());
            let tz: chrono_tz::Tz = user_tz_str.parse().unwrap_or(chrono_tz::Europe::Berlin);
            let now_local = chrono::Utc::now().with_timezone(&tz);
            let now_str = now_local.format("%Y-%m-%d %H:%M (%Z)").to_string();
            let mut prompt = format!(
                "{}\n\n## Current Session\n- Chat ID: {}\n- Current time: {}\n- IMPORTANT: If the user mentions a time (for example \"at 19:20\"), it is ALWAYS in their local timezone ({}). Convert that time to UTC before passing it to cron_add. Example: 19:20 CET = 18:20 UTC.\n\n## Agent Behavior\n- When you have tools available, USE them to accomplish the task. Do NOT describe steps you would take — execute them.\n- Write scripts, run commands, fetch data, create files — then report the RESULT to the user, not the plan.\n- If a task requires multiple steps (e.g. install a package, write a script, run it), do ALL steps yourself using your tools before responding.\n- Only explain your approach if the user explicitly asks for an explanation or if you truly cannot execute the task.\n- Never say 'you could do X' or 'here are the steps' when you can do it yourself with the available tools.\n- If you need to continue working internally without messaging the user (e.g. between tool calls when you need to think about the next step), reply with exactly NO_REPLY — this will suppress the message and let you continue. Use this when intermediate output would just be noise for the user.\n- Never claim missing permissions or installation limits unless a tool call actually failed and you quote the concrete stderr/exit code in your reply.\n\n## File Handling Rules\n- If the user asks to store, move, rename, or organize files (for example: 'store these files'), do ONLY file operations.\n- Do NOT read, extract, summarize, or analyze file contents unless the user explicitly asks for content analysis.\n- For organization tasks, verify paths and report what was moved/stored, not file content.\n- Telegram uploads are persisted under `skills/uploads`.\n- Upload references are also logged to `soul/uploads_index.md`.\n- If the user refers to an earlier upload without an exact path (for example \"the memo from earlier\"), first inspect `soul/uploads_index.md` and/or `skills/uploads` using tools.\n\n## File Upload Reply Format\n- If you created a file that should be sent back into the chat, return JSON in this exact shape: {{\"text\":\"optional short message\",\"upload_path\":\"relative/path/to/file.ext\",\"upload_caption\":\"optional caption\"}}.\n- Use workspace-relative paths only (no absolute paths, no ..).",
                base_prompt, chat_id, now_str, user_tz_str
            );
            if let Some(upload_context) = self.recent_upload_index_context(12).await {
                prompt.push_str(
                    "\n\n## Recent Upload Index (Structured)\n\
- The following entries are authoritative records from `soul/uploads_index.md`.\n\
- When the user references an earlier file/memo, use these paths directly.\n",
                );
                prompt.push_str(&upload_context);
            }
            prompt
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

        // (forced tool retry removed — it caused 2 extra LLM calls for every
        //  simple greeting.  The system prompt already instructs tool usage.)

        // Refresh worker skill definitions once per process() call (not every round).
        self.skills.refresh_remote_definitions().await;

        for round in 0..self.max_rounds {
            println!("[Agent] Round {}/{}", round + 1, self.max_rounds);
            if Self::stop_requested(start_stop_epoch) {
                return "⏹ Stopped.".to_string();
            }

            let mut request = LlmChatRequest::new("", messages.clone());
            request.tools = self.skills.tool_definitions();

            let response = if stream_tx.is_some() {
                match Self::chat_stream_with_retry(
                    &**llm,
                    request,
                    2,
                    stream_tx.as_ref(),
                )
                .await
                {
                    Ok(value) => value,
                    Err(err) => return format!("LLM call failed: {}", err),
                }
            } else {
                match Self::chat_with_retry(&**llm, request, 2).await {
                    Ok(value) => value,
                    Err(err) => return format!("LLM call failed: {}", err),
                }
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
                if Self::is_no_reply_signal(content) {
                    println!("[Agent] NO_REPLY signal — looping for next round");
                    // Clear streaming preview — NO_REPLY means we loop again.
                    if let Some(ref tx) = stream_tx {
                        let _ = tx.send(StreamDelta::Clear).await;
                    }
                    messages.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(content.to_string()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                    continue;
                }

                // Log when the model answers with text while tools are available.
                if self.has_web_capability() || self.has_shell_capability() {
                    println!(
                        "[Agent] Text-only response (tools available) — accepting as final answer: {}",
                        Self::preview_text(content, 120)
                    );
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

            // Execute tool calls concurrently.
            let tool_calls_vec = response.tool_calls;
            println!("[Agent] {} tool call(s) to execute", tool_calls_vec.len());

            // Push the assistant message (with tool_calls) before executing.
            messages.push(LlmMessage {
                role: LlmRole::Assistant,
                content: LlmMessageContent::Text(response.content),
                name: None,
                tool_call_id: None,
                tool_calls: tool_calls_vec.clone(),
            });

            // Launch all tool executions in parallel.
            let futures: Vec<_> = tool_calls_vec
                .iter()
                .map(|tool_call| {
                    let tool_name = tool_call.name.clone();
                    let args = tool_call.arguments.clone();
                    let args_preview =
                        Self::preview_text(&args.to_string(), 500).replace('\n', "\\n");
                    println!("[Agent][Tool] Calling {} args={}", tool_name, args_preview);

                    async move {
                        let result =
                            match self.skills.execute(&tool_name, &args).await {
                                Some(result) => result,
                                None => json!({
                                    "ok": false,
                                    "error": format!("Unknown tool: {}", tool_name),
                                })
                                .to_string(),
                            };
                        let result_preview =
                            Self::preview_text(&result, 3000).replace('\n', "\\n");
                        println!("[Agent][Tool] Result {} => {}", tool_name, result_preview);
                        result
                    }
                })
                .collect();

            let results = futures_util::future::join_all(futures).await;

            if Self::stop_requested(start_stop_epoch) {
                return "⏹ Stopped.".to_string();
            }

            // Append results in original order (join_all preserves order).
            for (i, result) in results.into_iter().enumerate() {
                messages.push(LlmMessage {
                    role: LlmRole::Tool,
                    content: LlmMessageContent::Text(result),
                    name: Some(tool_calls_vec[i].name.clone()),
                    tool_call_id: tool_calls_vec[i].id.clone(),
                    tool_calls: Vec::new(),
                });
            }
        }

        "Could not complete tool execution (too many steps).".to_string()
    }
}
