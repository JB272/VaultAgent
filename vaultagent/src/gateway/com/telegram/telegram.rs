mod format;
use format::md_to_telegram_html;

use crate::gateway::com::{Gateway, get_non_empty_env, is_token_service_enabled};
use crate::gateway::incoming_actions_queue::{ChatAction, IncomingAction, IncomingActionWriter};
use crate::reasoning::agent::Agent;
use crate::reasoning::llm_interface::LlmInterface;
use crate::reasoning::transcription::TranscriptionService;
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Json, State},
    http::StatusCode,
    routing::{get, post},
};
use reqwest::Client;
use reqwest::multipart;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use std::{collections::HashSet, error::Error, net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct TelegramBot {
    client: Client,
    base_url: String,
    webhook_url: Option<String>,
    port: u16,
    known_chat_ids: Arc<Mutex<HashSet<i64>>>,
    allowed_chat_ids: Option<HashSet<i64>>,
    transcription: Option<Arc<TranscriptionService>>,
    agent: Option<Arc<Agent>>,
    llm: Option<Arc<dyn LlmInterface>>,
}

impl TelegramBot {
    pub fn new(token: impl Into<String>, webhook_url: Option<String>, port: u16) -> Self {
        let token = token.into();
        let base_url = format!("https://api.telegram.org/bot{}", token);

        Self {
            client: Client::new(),
            base_url,
            webhook_url,
            port,
            known_chat_ids: Arc::new(Mutex::new(HashSet::new())),
            allowed_chat_ids: None,
            transcription: None,
            agent: None,
            llm: None,
        }
    }

    pub fn is_enabled() -> bool {
        is_token_service_enabled("TELEGRAM_BOT_TOKEN")
    }

    pub fn from_env() -> Option<Self> {
        let token = get_non_empty_env("TELEGRAM_BOT_TOKEN")?;

        let webhook_url = get_non_empty_env("TELEGRAM_WEBHOOK_URL");

        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8080);

        let mut bot = Self::new(token, webhook_url, port);
        bot.transcription = TranscriptionService::from_env().map(Arc::new);

        // Load allowed chat IDs: from ENV + trusted_chat_ids.md
        let mut allowed: HashSet<i64> = HashSet::new();

        // 1) From environment variable (comma-separated)
        if let Some(ids) = get_non_empty_env("TELEGRAM_ALLOWED_CHAT_IDS") {
            for id in ids.split(',').filter_map(|s| s.trim().parse::<i64>().ok()) {
                allowed.insert(id);
            }
        }

        // 2) From trusted_chat_ids.md (one ID per line, # = comment)
        let trusted_path = std::path::Path::new("trusted_chat_ids.md");
        if trusted_path.exists() {
            if let Ok(content) = std::fs::read_to_string(trusted_path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("<!--")
                    {
                        continue;
                    }
                    if let Ok(id) = trimmed.parse::<i64>() {
                        allowed.insert(id);
                    }
                }
            }
        }

        if !allowed.is_empty() {
            println!(
                "[Telegram] Access list enabled: {} allowed chat ID(s)",
                allowed.len()
            );
            bot.allowed_chat_ids = Some(allowed);
        } else {
            println!(
                "[Telegram] Access list disabled: no IDs set in TELEGRAM_ALLOWED_CHAT_IDS or trusted_chat_ids.md"
            );
        }

        Some(bot)
    }

    pub async fn start(
        &self,
        incoming_writer: IncomingActionWriter,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Register commands with Telegram so they show in the command menu.
        let commands = [
            ("new", "Neue Konversation starten"),
            ("window", "Context Window Auslastung anzeigen"),
            ("tools", "List all available skills/tools"),
            ("stats", "Today's LLM token usage"),
            ("models", "Show or switch the active LLM model"),
            ("stop", "Stop all running jobs/subagents"),
            ("reboot", "Restart the service"),
        ];
        if let Err(err) = self.set_my_commands(&commands).await {
            eprintln!("[Telegram] Could not register commands: {}", err);
        } else {
            println!("[Telegram] Slash commands registered with Telegram.");
        }

        if let Some(ref webhook_url) = self.webhook_url {
            // Webhook mode: set public URL and start HTTP server
            self.set_webhook(webhook_url.clone()).await?;

            let app_state = AppState {
                incoming_writer,
                known_chat_ids: Arc::clone(&self.known_chat_ids),
                bot: self.clone(),
            };

            let app = Router::new()
                .route("/health", get(health))
                .route("/telegram/webhook", post(telegram_webhook))
                .with_state(app_state);

            let address = SocketAddr::from(([0, 0, 0, 0], self.port));
            println!("[Telegram] Webhook server listening on {}", address);

            tokio::spawn(async move {
                let listener = match tokio::net::TcpListener::bind(address).await {
                    Ok(value) => value,
                    Err(err) => {
                        eprintln!("[Telegram] Failed to bind webhook port: {}", err);
                        return;
                    }
                };

                if let Err(err) = axum::serve(listener, app).await {
                    eprintln!("[Telegram] Webhook server exited with error: {}", err);
                }
            });
        } else {
            // Polling mode: delete webhook and start long-polling
            self.delete_webhook().await?;
            println!("[Telegram] Polling mode enabled");

            let bot = self.clone();
            tokio::spawn(async move {
                let mut offset: Option<i64> = None;
                loop {
                    match bot.get_updates(offset, Some(30)).await {
                        Ok(updates) => {
                            for update in updates {
                                offset = Some(update.update_id + 1);

                                if let Some(ref cq) = update.callback_query {
                                    handle_callback_query(cq, &bot).await;
                                    continue;
                                }

                                if let Some(ref message) = update.message {
                                    // Check chat ID allowlist
                                    if let Some(ref allowed) = bot.allowed_chat_ids {
                                        if !allowed.contains(&message.chat.id) {
                                            let _ = bot.send_message(
                                                message.chat.id,
                                                format!("⛔ Access denied. Your chat ID: {}\nPlease ask the bot admin to allowlist your ID.", message.chat.id)
                                            ).await;
                                            continue;
                                        }
                                    }

                                    bot.known_chat_ids.lock().await.insert(message.chat.id);

                                    // Handle slash commands before forwarding to the agent.
                                    if let Some(text) = message.text.as_deref() {
                                        if let Some(result) =
                                            handle_command(text, &bot, message.chat.id).await
                                        {
                                            match result {
                                                CommandResult::Text(reply) => {
                                                    let _ =
                                                        bot.send_html(message.chat.id, reply).await;
                                                }
                                                CommandResult::Keyboard { text, markup } => {
                                                    let _ = bot
                                                        .send_keyboard_message(
                                                            message.chat.id,
                                                            text,
                                                            markup,
                                                        )
                                                        .await;
                                                }
                                            }
                                            continue;
                                        }
                                    }

                                    if let Some(content) = extract_content(&bot, message).await {
                                        let action = IncomingAction::Chat(ChatAction {
                                            chat_id: message.chat.id,
                                            text: content.text,
                                            image_url: content.image_url,
                                        });
                                        incoming_writer.push(action).await;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!("[Telegram] Polling error: {}. Retrying in 5s...", err);
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            });
        }

        Ok(())
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: impl Into<String>,
    ) -> Result<Message, Box<dyn Error + Send + Sync>> {
        let text_str = text.into();
        let html = md_to_telegram_html(&text_str);
        self.send_html(chat_id, html).await
    }

    /// Send a message that is already valid Telegram HTML — skips the Markdown converter.
    pub async fn send_html(
        &self,
        chat_id: i64,
        html: impl Into<String>,
    ) -> Result<Message, Box<dyn Error + Send + Sync>> {
        let request = SendMessageRequest {
            chat_id,
            text: html.into(),
            parse_mode: "HTML".to_string(),
        };

        let response = self
            .client
            .post(format!("{}/sendMessage", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<Message> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }

        body.result
            .ok_or_else(|| "Telegram API returned no message".into())
    }

    /// Registers slash commands with Telegram so they appear in the command menu.
    pub async fn set_my_commands(
        &self,
        commands: &[(&str, &str)],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let commands: Vec<BotCommand> = commands
            .iter()
            .map(|(cmd, desc)| BotCommand {
                command: cmd.to_string(),
                description: desc.to_string(),
            })
            .collect();
        let request = SetMyCommandsRequest { commands };

        let response = self
            .client
            .post(format!("{}/setMyCommands", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<bool> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }
        Ok(())
    }

    pub async fn send_chat_action(
        &self,
        chat_id: i64,
        action: impl Into<String>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let request = SendChatActionRequest {
            chat_id,
            action: action.into(),
        };

        let response = self
            .client
            .post(format!("{}/sendChatAction", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<bool> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }

        if body.result.unwrap_or(false) {
            Ok(())
        } else {
            Err("Telegram API could not send chat action".into())
        }
    }

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_seconds: Option<u64>,
    ) -> Result<Vec<Update>, Box<dyn Error + Send + Sync>> {
        let request = GetUpdatesRequest {
            offset,
            timeout: timeout_seconds,
        };

        let response = self
            .client
            .post(format!("{}/getUpdates", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<Vec<Update>> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }

        Ok(body.result.unwrap_or_default())
    }

    pub async fn set_webhook(
        &self,
        url: impl Into<String>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let request = SetWebhookRequest { url: url.into() };

        let response = self
            .client
            .post(format!("{}/setWebhook", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<bool> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }

        if body.result.unwrap_or(false) {
            Ok(())
        } else {
            Err("Telegram API could not set webhook".into())
        }
    }

    /// Retrieves file info (file_path) for a given file_id.
    pub async fn get_file_path(
        &self,
        file_id: &str,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let response = self
            .client
            .post(format!("{}/getFile", self.base_url))
            .json(&serde_json::json!({ "file_id": file_id }))
            .send()
            .await?;

        let body: ApiResponse<TelegramFile> = response.json().await?;
        if !body.ok {
            return Err(body
                .description
                .unwrap_or("getFile failed".to_string())
                .into());
        }

        body.result
            .and_then(|f| f.file_path)
            .ok_or_else(|| "No file_path in Telegram response".into())
    }

    /// Downloads a file from Telegram's servers.
    pub async fn download_file(
        &self,
        file_path: &str,
    ) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        // base_url is "https://api.telegram.org/bot<token>"
        // file download is "https://api.telegram.org/file/bot<token>/<file_path>"
        let download_url = self.base_url.replace("/bot", "/file/bot");
        let url = format!("{}/{}", download_url, file_path);

        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(format!("Download failed: {}", response.status()).into());
        }

        Ok(response.bytes().await?.to_vec())
    }

    pub async fn delete_webhook(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let request = SetWebhookRequest { url: String::new() };

        let response = self
            .client
            .post(format!("{}/setWebhook", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<bool> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }

        Ok(())
    }

    pub async fn send_keyboard_message(
        &self,
        chat_id: i64,
        text: impl Into<String>,
        markup: InlineKeyboardMarkup,
    ) -> Result<Message, Box<dyn Error + Send + Sync>> {
        let request = SendMessageWithKeyboardRequest {
            chat_id,
            text: text.into(),
            parse_mode: "HTML".to_string(),
            reply_markup: markup,
        };

        let response = self
            .client
            .post(format!("{}/sendMessage", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<Message> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }
        body.result
            .ok_or_else(|| "Telegram API returned no message".into())
    }

    pub async fn send_document(
        &self,
        chat_id: i64,
        relative_path: &str,
        caption: Option<&str>,
    ) -> Result<Message, Box<dyn Error + Send + Sync>> {
        let safe_path = sanitize_relative_path(relative_path)?;
        let bytes = tokio::fs::read(&safe_path).await?;
        let filename = Path::new(&safe_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.bin")
            .to_string();

        let document_part = multipart::Part::bytes(bytes).file_name(filename);

        let mut form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", document_part);

        if let Some(caption) = caption {
            if !caption.trim().is_empty() {
                form = form
                    .text("caption", caption.to_string())
                    .text("parse_mode", "HTML".to_string());
            }
        }

        let response = self
            .client
            .post(format!("{}/sendDocument", self.base_url))
            .multipart(form)
            .send()
            .await?;

        let body: ApiResponse<Message> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }

        body.result
            .ok_or_else(|| "Telegram API returned no message".into())
    }

    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<String>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let request = AnswerCallbackQueryRequest {
            callback_query_id: callback_query_id.to_string(),
            text,
        };

        let response = self
            .client
            .post(format!("{}/answerCallbackQuery", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<bool> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }
        Ok(())
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: impl Into<String>,
        reply_markup: Option<InlineKeyboardMarkup>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let request = EditMessageTextRequest {
            chat_id,
            message_id,
            text: text.into(),
            parse_mode: "HTML".to_string(),
            reply_markup,
        };

        let response = self
            .client
            .post(format!("{}/editMessageText", self.base_url))
            .json(&request)
            .send()
            .await?;

        let body: ApiResponse<Message> = response.json().await?;
        if !body.ok {
            let error_message = body
                .description
                .unwrap_or_else(|| "Telegram API returned an unknown error".to_string());
            return Err(error_message.into());
        }
        Ok(())
    }
}

/// Result returned by a slash-command handler.
enum CommandResult {
    Text(String),
    Keyboard {
        text: String,
        markup: InlineKeyboardMarkup,
    },
}

/// Keeps only chat-relevant models for display in the picker.
/// Works for mixed lists (MultiProvider aggregates all backends).
fn filter_models_for_display(models: Vec<String>, _provider: &str) -> Vec<String> {
    let chat_prefixes = [
        "gpt-4",
        "gpt-3.5-turbo",
        "o1",
        "o3",
        "o4",
        "chatgpt-4o",
        "claude",
    ];
    let mut v: Vec<String> = models
        .into_iter()
        .filter(|m| chat_prefixes.iter().any(|p| m.starts_with(p)))
        .collect();
    v.sort();
    v
}

fn build_model_keyboard(models: &[String], current: &str) -> InlineKeyboardMarkup {
    let rows = models
        .iter()
        .map(|m| {
            let label = if m == current {
                format!("✅ {m}")
            } else {
                m.clone()
            };
            vec![InlineKeyboardButton {
                text: label,
                callback_data: format!("model:{m}"),
            }]
        })
        .collect();
    InlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

async fn handle_callback_query(query: &CallbackQuery, bot: &TelegramBot) {
    let Some(data) = query.data.as_deref() else {
        return;
    };

    if let Some(model_name) = data.strip_prefix("model:") {
        let Some(ref llm) = bot.llm else {
            return;
        };
        llm.set_model(model_name.to_string());

        let _ = bot
            .answer_callback_query(&query.id, Some(format!("✅ {model_name}")))
            .await;

        if let Some(ref msg) = query.message {
            let available = llm.list_models().await;
            let filtered = filter_models_for_display(available, llm.provider_name());
            let markup = build_model_keyboard(&filtered, model_name);
            let _ = bot
                .edit_message_text(
                    msg.chat.id,
                    msg.message_id,
                    format!(
                        "🤖 <b>Select model</b> (✅ = active):\nCurrent: <code>{model_name}</code>"
                    ),
                    Some(markup),
                )
                .await;
        }
    }
}

/// Handles slash commands. Returns `Some(CommandResult)` if handled, `None` to forward to the agent.
async fn handle_command(text: &str, bot: &TelegramBot, _chat_id: i64) -> Option<CommandResult> {
    let text = text.trim();

    if text == "/reboot" {
        println!("[Telegram] Reboot requested");
        // Send the reply, then exit after a short delay so the message is delivered.
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::process::exit(0);
        });
        return Some(CommandResult::Text("♻️ Rebooting...".to_string()));
    }

    if text == "/stop" {
        if let Some(ref agent) = bot.agent {
            agent.stop_all();
            return Some(CommandResult::Text(
                "⏹ Stopped all running jobs/subagents.".to_string(),
            ));
        }
        return Some(CommandResult::Text("No agent configured.".to_string()));
    }

    if text == "/new" {
        if let Some(ref agent) = bot.agent {
            agent.clear_history().await;
        }
        return Some(CommandResult::Text(
            "🧹 Konversation zurückgesetzt. Neuer Chat gestartet!".to_string(),
        ));
    }

    if text == "/window" {
        if let Some(ref agent) = bot.agent {
            return Some(CommandResult::Text(agent.context_window_info().await));
        }
        return Some(CommandResult::Text("No agent configured.".to_string()));
    }

    if text == "/tools" {
        if let Some(ref agent) = bot.agent {
            let names = agent.skill_names();
            let list = names
                .iter()
                .map(|n| format!("• <code>{n}</code>"))
                .collect::<Vec<_>>()
                .join("\n");
            return Some(CommandResult::Text(format!(
                "🛠 <b>Available tools:</b>\n\n{list}"
            )));
        }
        return Some(CommandResult::Text("No agent configured.".to_string()));
    }

    if text == "/stats" {
        if let Some(ref agent) = bot.agent {
            if let Some(ref usage) = agent.usage {
                return Some(CommandResult::Text(usage.stats_message().await));
            }
        }
        return Some(CommandResult::Text("No usage data available.".to_string()));
    }

    // /models — inline keyboard model picker
    if text == "/models" {
        if let Some(ref llm) = bot.llm {
            let current = llm.current_model();
            let available = llm.list_models().await;
            let filtered = filter_models_for_display(available, llm.provider_name());
            if filtered.is_empty() {
                return Some(CommandResult::Text(format!(
                    "🤖 <b>Current model:</b> <code>{current}</code>\n\nCould not fetch model list from provider.\nUse <code>/models &lt;name&gt;</code> to switch."
                )));
            }
            let markup = build_model_keyboard(&filtered, &current);
            return Some(CommandResult::Keyboard {
                text: format!(
                    "🤖 <b>Select model</b> (✅ = active):\nCurrent: <code>{current}</code>"
                ),
                markup,
            });
        }
        return Some(CommandResult::Text("No LLM configured.".to_string()));
    }

    // /models <name> — switch model directly
    if let Some(model) = text.strip_prefix("/models ") {
        let model = model.trim().to_string();
        if model.is_empty() {
            return Some(CommandResult::Text(
                "Usage: <code>/models &lt;model-name&gt;</code>".to_string(),
            ));
        }
        if let Some(ref llm) = bot.llm {
            llm.set_model(model.clone());
            return Some(CommandResult::Text(format!(
                "✅ Switched to model <code>{model}</code>"
            )));
        }
        return Some(CommandResult::Text("No LLM configured.".to_string()));
    }

    None
}

pub async fn setup_telegram(
    incoming_writer: IncomingActionWriter,
    agent: Arc<Agent>,
    llm: Option<Arc<dyn LlmInterface>>,
) -> Option<TelegramBot> {
    if TelegramBot::is_enabled() {
        if let Some(mut telegram) = TelegramBot::from_env() {
            telegram.agent = Some(agent);
            telegram.llm = llm;
            match telegram.start(incoming_writer).await {
                Ok(()) => Some(telegram),
                Err(err) => {
                    eprintln!(
                        "[Telegram] Disabled: startup failed ({}). Website gateway continues.",
                        err
                    );
                    None
                }
            }
        } else {
            println!("[Telegram] Disabled: incomplete Telegram configuration.");
            None
        }
    } else {
        println!("[Telegram] Disabled: no Telegram bot token configured.");
        None
    }
}

#[async_trait]
impl Gateway for TelegramBot {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn send_reply(
        &self,
        chat_id: i64,
        text: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !self.known_chat_ids.lock().await.contains(&chat_id) {
            return Ok(()); // Not our chat
        }
        self.send_message(chat_id, text).await?;
        Ok(())
    }

    async fn notify_typing(
        &self,
        chat_id: i64,
        typing: bool,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !typing || !self.known_chat_ids.lock().await.contains(&chat_id) {
            return Ok(());
        }
        self.send_chat_action(chat_id, "typing").await
    }

    async fn send_file(
        &self,
        chat_id: i64,
        path: &str,
        caption: Option<&str>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !self.known_chat_ids.lock().await.contains(&chat_id) {
            return Ok(());
        }
        self.send_document(chat_id, path, caption).await?;
        Ok(())
    }
}

fn sanitize_relative_path(path: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
    let p = Path::new(path);
    if p.as_os_str().is_empty() {
        return Err("Path must not be empty".into());
    }
    if p.is_absolute() {
        return Err("Only relative paths are allowed for upload".into());
    }
    if p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("Path contains forbidden segments (.. or root)".into());
    }
    Ok(path.to_string())
}

/// Extracted content from a Telegram message.
struct ExtractedContent {
    text: String,
    /// Base64 data-URL for an image, if present.
    image_url: Option<String>,
}

/// Extracts text (and optionally an image) from a Telegram message.
async fn extract_content(bot: &TelegramBot, message: &Message) -> Option<ExtractedContent> {
    // 1) Photo message → download largest resolution, encode as base64 data URL
    if let Some(ref photos) = message.photo {
        if let Some(largest) = photos.last() {
            let text = message
                .caption
                .clone()
                .unwrap_or_else(|| "[Image]".to_string());

            match bot.get_file_path(&largest.file_id).await {
                Ok(file_path) => match bot.download_file(&file_path).await {
                    Ok(data) => {
                        use base64::Engine;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                        let data_url = format!("data:image/jpeg;base64,{}", b64);
                        println!(
                            "[Telegram][Photo] Downloaded {}x{} photo ({} bytes)",
                            largest.width,
                            largest.height,
                            data.len()
                        );
                        return Some(ExtractedContent {
                            text,
                            image_url: Some(data_url),
                        });
                    }
                    Err(err) => {
                        eprintln!("[Telegram][Photo] Failed to download: {}", err);
                    }
                },
                Err(err) => {
                    eprintln!("[Telegram][Photo] Failed to get file path: {}", err);
                }
            }
        }
    }

    // 2) Generic document/file upload (PDF, ZIP, TXT, etc.)
    if let Some(ref document) = message.document {
        let user_text = message
            .caption
            .clone()
            .unwrap_or_else(|| "[File upload]".to_string());

        match bot.get_file_path(&document.file_id).await {
            Ok(file_path) => match bot.download_file(&file_path).await {
                Ok(data) => {
                    let stored_path = persist_telegram_file(
                        &data,
                        document.file_name.as_deref(),
                        &document.file_id,
                    )
                    .await;

                    match stored_path {
                        Ok(path) => {
                            println!(
                                "[Telegram][Document] Saved '{}' ({} bytes) to {}",
                                document.file_name.as_deref().unwrap_or("unnamed file"),
                                data.len(),
                                path
                            );

                            let metadata = format!(
                                "[File upload]\n- name: {}\n- mime: {}\n- size: {}\n- saved_path: {}\n\n{}\n\nUse the saved_path with tools like read_file, shell_execute, file_store, or Python skills.",
                                document
                                    .file_name
                                    .clone()
                                    .unwrap_or_else(|| "unknown".to_string()),
                                document
                                    .mime_type
                                    .clone()
                                    .unwrap_or_else(|| "unknown".to_string()),
                                document
                                    .file_size
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| data.len().to_string()),
                                path,
                                user_text
                            );

                            return Some(ExtractedContent {
                                text: metadata,
                                image_url: None,
                            });
                        }
                        Err(err) => {
                            eprintln!("[Telegram][Document] Failed to store file: {}", err);
                            return Some(ExtractedContent {
                                text: format!(
                                    "[File upload]\nCould not store the uploaded file: {}\n\n{}",
                                    err, user_text
                                ),
                                image_url: None,
                            });
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[Telegram][Document] Failed to download: {}", err);
                    return Some(ExtractedContent {
                        text: format!(
                            "[File upload]\nCould not download the uploaded file: {}\n\n{}",
                            err, user_text
                        ),
                        image_url: None,
                    });
                }
            },
            Err(err) => {
                eprintln!("[Telegram][Document] Failed to get file path: {}", err);
                return Some(ExtractedContent {
                    text: format!(
                        "[File upload]\nCould not resolve the uploaded file path: {}\n\n{}",
                        err, user_text
                    ),
                    image_url: None,
                });
            }
        }
    }

    // 3) Regular text message
    if let Some(ref text) = message.text {
        return Some(ExtractedContent {
            text: text.clone(),
            image_url: None,
        });
    }

    // 4) Voice memo or audio file → transcribe
    let audio_info = message.voice.as_ref().or(message.audio.as_ref())?;
    let transcription_service = bot.transcription.as_ref()?;

    let mime = audio_info.mime_type.as_deref();

    match bot.get_file_path(&audio_info.file_id).await {
        Ok(file_path) => match bot.download_file(&file_path).await {
            Ok(data) => {
                println!(
                    "[Telegram][Voice] Received audio message ({} bytes, {:?}), transcribing...",
                    data.len(),
                    mime
                );
                match transcription_service.transcribe(data, mime).await {
                    Ok(text) if !text.trim().is_empty() => {
                        println!("[Telegram][Voice] Transcription: {}", text);
                        Some(ExtractedContent {
                            text: format!("[Voice message] {}", text),
                            image_url: None,
                        })
                    }
                    Ok(_) => {
                        eprintln!("[Telegram][Voice] Transcription returned empty text.");
                        None
                    }
                    Err(err) => {
                        eprintln!("[Telegram][Voice] Transcription failed: {}", err);
                        None
                    }
                }
            }
            Err(err) => {
                eprintln!("[Telegram][Voice] Failed to download audio file: {}", err);
                None
            }
        },
        Err(err) => {
            eprintln!(
                "[Telegram][Voice] Failed to fetch Telegram file path: {}",
                err
            );
            None
        }
    }
}

async fn persist_telegram_file(
    bytes: &[u8],
    original_name: Option<&str>,
    file_id: &str,
) -> Result<String, String> {
    let uploads_dir = std::path::Path::new("skills").join("uploads");

    tokio::fs::create_dir_all(&uploads_dir)
        .await
        .map_err(|e| format!("Failed to create uploads directory: {}", e))?;

    let safe_name = original_name
        .map(sanitize_filename)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("file_{}.bin", sanitize_filename(file_id)));

    let unique = uuid::Uuid::new_v4().to_string();
    let stored_name = format!("{}_{}", unique, safe_name);
    let file_path = uploads_dir.join(stored_name);

    tokio::fs::write(&file_path, bytes)
        .await
        .map_err(|e| format!("Failed to write uploaded file: {}", e))?;

    Ok(file_path.to_string_lossy().to_string())
}

fn sanitize_filename(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Clone)]
struct AppState {
    incoming_writer: IncomingActionWriter,
    known_chat_ids: Arc<Mutex<HashSet<i64>>>,
    bot: TelegramBot,
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn telegram_webhook(State(state): State<AppState>, Json(update): Json<Update>) -> StatusCode {
    if let Some(ref cq) = update.callback_query {
        handle_callback_query(cq, &state.bot).await;
        return StatusCode::OK;
    }

    if let Some(ref message) = update.message {
        // Check chat ID allowlist
        if let Some(ref allowed) = state.bot.allowed_chat_ids {
            if !allowed.contains(&message.chat.id) {
                let _ = state.bot.send_message(
                    message.chat.id,
                    format!("⛔ Access denied. Your chat ID: {}\nPlease ask the bot admin to allowlist your ID.", message.chat.id)
                ).await;
                return StatusCode::OK;
            }
        }

        // Register chat ID as "known" so Gateway only sends to real Telegram chats
        state.known_chat_ids.lock().await.insert(message.chat.id);

        // Handle slash commands
        if let Some(text) = message.text.as_deref() {
            if let Some(result) = handle_command(text, &state.bot, message.chat.id).await {
                match result {
                    CommandResult::Text(reply) => {
                        let _ = state.bot.send_html(message.chat.id, reply).await;
                    }
                    CommandResult::Keyboard { text, markup } => {
                        let _ = state
                            .bot
                            .send_keyboard_message(message.chat.id, text, markup)
                            .await;
                    }
                }
                return StatusCode::OK;
            }
        }

        if let Some(content) = extract_content(&state.bot, message).await {
            let action = IncomingAction::Chat(ChatAction {
                chat_id: message.chat.id,
                text: content.text,
                image_url: content.image_url,
            });

            state.incoming_writer.push(action).await;
        }
    }

    StatusCode::OK
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    chat_id: i64,
    text: String,
    parse_mode: String,
}

#[derive(Debug, Serialize)]
struct SendChatActionRequest {
    chat_id: i64,
    action: String,
}

#[derive(Debug, Serialize)]
struct GetUpdatesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SetWebhookRequest {
    url: String,
}

#[derive(Debug, Serialize)]
struct BotCommand {
    command: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct SetMyCommandsRequest {
    commands: Vec<BotCommand>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramFile {
    file_id: String,
    file_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub chat: Chat,
    pub voice: Option<Audio>,
    pub audio: Option<Audio>,
    pub document: Option<Document>,
    /// Array of available photo sizes (smallest → largest).
    pub photo: Option<Vec<PhotoSize>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: u32,
    pub height: u32,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    pub file_id: String,
    pub duration: Option<u64>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    pub file_id: String,
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

// ── Callback query ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub message: Option<Message>,
    pub data: Option<String>,
}

// ── Inline keyboard ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
struct InlineKeyboardButton {
    text: String,
    callback_data: String,
}

#[derive(Debug, Serialize, Clone)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Serialize)]
struct SendMessageWithKeyboardRequest {
    chat_id: i64,
    text: String,
    parse_mode: String,
    reply_markup: InlineKeyboardMarkup,
}

#[derive(Debug, Serialize)]
struct EditMessageTextRequest {
    chat_id: i64,
    message_id: i64,
    text: String,
    parse_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Debug, Serialize)]
struct AnswerCallbackQueryRequest {
    callback_query_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}
