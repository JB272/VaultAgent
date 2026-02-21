use crate::gateway::com::{Gateway, get_non_empty_env, is_token_service_enabled};
use crate::gateway::incoming_actions_queue::{ChatAction, IncomingAction, IncomingActionWriter};
use crate::reasoning::transcription::TranscriptionService;
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Json, State},
    http::StatusCode,
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, error::Error, net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct TelegramBot {
    client: Client,
    base_url: String,
    webhook_url: Option<String>,
    port: u16,
    known_chat_ids: Arc<Mutex<HashSet<i64>>>,
    allowed_chat_ids: Option<HashSet<i64>>,
    transcription: Option<Arc<TranscriptionService>>,
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

        // Erlaubte Chat-IDs laden: aus ENV + trusted_chat_ids.md
        let mut allowed: HashSet<i64> = HashSet::new();

        // 1) Aus Umgebungsvariable (kommasepariert)
        if let Some(ids) = get_non_empty_env("TELEGRAM_ALLOWED_CHAT_IDS") {
            for id in ids.split(',').filter_map(|s| s.trim().parse::<i64>().ok()) {
                allowed.insert(id);
            }
        }

        // 2) Aus trusted_chat_ids.md (eine ID pro Zeile, # = Kommentar)
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
        if let Some(ref webhook_url) = self.webhook_url {
            // Webhook-Modus: öffentliche URL setzen und HTTP-Server starten
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
            // Polling-Modus: Webhook löschen und long-polling starten
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

                                if let Some(ref message) = update.message {
                                    // Chat-ID-Allowlist prüfen
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

                                    // /reboot Kommando: Prozess beenden, systemd startet neu
                                    if message.text.as_deref() == Some("/reboot") {
                                        let _ = bot
                                            .send_message(message.chat.id, "♻️ Rebooting...")
                                            .await;
                                        // Offset bestätigen, damit die Nachricht nicht erneut kommt
                                        let _ = bot
                                            .get_updates(Some(update.update_id + 1), Some(0))
                                            .await;
                                        println!(
                                            "[Telegram] Reboot requested by chat {}",
                                            message.chat.id
                                        );
                                        std::process::exit(0);
                                    }

                                    if let Some(text) =
                                        extract_text_or_transcribe(&bot, message).await
                                    {
                                        let action = IncomingAction::Chat(ChatAction {
                                            chat_id: message.chat.id,
                                            text,
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
        let request = SendMessageRequest {
            chat_id,
            text: text.into(),
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

    /// Ruft die Dateiinfo ab (file_path) für einen gegebenen file_id.
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

    /// Lädt eine Datei von den Telegram-Servern herunter.
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
}

pub async fn setup_telegram(incoming_writer: IncomingActionWriter) -> Option<TelegramBot> {
    if TelegramBot::is_enabled() {
        if let Some(telegram) = TelegramBot::from_env() {
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
            return Ok(()); // Nicht unser Chat
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
}

/// Extrahiert Text aus einer Nachricht – entweder direkt oder durch Transkription von Voice/Audio.
async fn extract_text_or_transcribe(bot: &TelegramBot, message: &Message) -> Option<String> {
    // 1) Normale Textnachricht
    if let Some(ref text) = message.text {
        return Some(text.clone());
    }

    // 2) Voice-Memo oder Audio-Datei → transkribieren
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
                        Some(format!("[Voice message] {}", text))
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
    if let Some(ref message) = update.message {
        // Chat-ID-Allowlist prüfen
        if let Some(ref allowed) = state.bot.allowed_chat_ids {
            if !allowed.contains(&message.chat.id) {
                let _ = state.bot.send_message(
                    message.chat.id,
                    format!("⛔ Access denied. Your chat ID: {}\nPlease ask the bot admin to allowlist your ID.", message.chat.id)
                ).await;
                return StatusCode::OK;
            }
        }

        // Chat-ID als "bekannt" registrieren, damit Gateway nur an echte Telegram-Chats sendet
        state.known_chat_ids.lock().await.insert(message.chat.id);

        // /reboot Kommando: Prozess beenden, systemd startet neu
        if message.text.as_deref() == Some("/reboot") {
            let _ = state
                .bot
                .send_message(message.chat.id, "♻️ Rebooting...")
                .await;
            println!("[Telegram] Reboot requested by chat {}", message.chat.id);
            std::process::exit(0);
        }

        if let Some(text) = extract_text_or_transcribe(&state.bot, message).await {
            let action = IncomingAction::Chat(ChatAction {
                chat_id: message.chat.id,
                text,
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

#[derive(Debug, Clone, Deserialize)]
struct TelegramFile {
    file_id: String,
    file_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub text: Option<String>,
    pub chat: Chat,
    pub voice: Option<Audio>,
    pub audio: Option<Audio>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    pub file_id: String,
    pub duration: Option<u64>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}
