use crate::gateway::com::{get_non_empty_env, is_token_service_enabled};
use crate::gateway::incoming_actions_queue::{ChatAction, IncomingAction, IncomingActionWriter};
use axum::{
    Router,
    extract::{Json, State},
    http::StatusCode,
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{error::Error, net::SocketAddr};

#[derive(Debug, Clone)]
pub struct TelegramBot {
    client: Client,
    base_url: String,
    webhook_url: String,
    port: u16,
}

impl TelegramBot {
    pub fn new(token: impl Into<String>, webhook_url: impl Into<String>, port: u16) -> Self {
        let token = token.into();
        let base_url = format!("https://api.telegram.org/bot{}", token);

        Self {
            client: Client::new(),
            base_url,
            webhook_url: webhook_url.into(),
            port,
        }
    }

    pub fn is_enabled() -> bool {
        is_token_service_enabled("TELEGRAM_BOT_TOKEN")
    }

    pub fn from_env() -> Option<Self> {
        let token = get_non_empty_env("TELEGRAM_BOT_TOKEN")?;

        let webhook_url = get_non_empty_env("TELEGRAM_WEBHOOK_URL");

        let Some(webhook_url) = webhook_url else {
            eprintln!(
                "TELEGRAM_BOT_TOKEN ist gesetzt, aber TELEGRAM_WEBHOOK_URL fehlt. Telegram wird nicht gestartet."
            );
            return None;
        };

        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8080);

        Some(Self::new(token, webhook_url, port))
    }

    pub async fn start(
        &self,
        incoming_writer: IncomingActionWriter,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.set_webhook(self.webhook_url.clone()).await?;

        let app_state = AppState { incoming_writer };

        let app = Router::new()
            .route("/health", get(health))
            .route("/telegram/webhook", post(telegram_webhook))
            .with_state(app_state);

        let address = SocketAddr::from(([0, 0, 0, 0], self.port));
        println!("Webhook server läuft auf {}", address);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(address).await {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("Fehler beim Binden des Webhook-Ports: {}", err);
                    return;
                }
            };

            if let Err(err) = axum::serve(listener, app).await {
                eprintln!("Webhook-Server ist mit Fehler beendet: {}", err);
            }
        });

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
}

#[derive(Clone)]
struct AppState {
    incoming_writer: IncomingActionWriter,
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn telegram_webhook(State(state): State<AppState>, Json(update): Json<Update>) -> StatusCode {
    if let Some(message) = update.message {
        if let Some(text) = message.text {
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
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub text: Option<String>,
    pub chat: Chat,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}
