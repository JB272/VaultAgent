use axum::{
	Router,
	extract::{Json, State},
	http::StatusCode,
	routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, error::Error, net::SocketAddr, sync::Arc};
use tokio::sync::{Mutex, Notify};

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

	pub fn from_env() -> Result<Self, Box<dyn Error + Send + Sync>> {
		let token = std::env::var("TELEGRAM_BOT_TOKEN")
			.map_err(|_| "Bitte TELEGRAM_BOT_TOKEN als Umgebungsvariable setzen.")?;

		let webhook_url = std::env::var("TELEGRAM_WEBHOOK_URL")
			.map_err(|_| "Bitte TELEGRAM_WEBHOOK_URL als Umgebungsvariable setzen.")?;

		let port: u16 = std::env::var("PORT")
			.ok()
			.and_then(|value| value.parse().ok())
			.unwrap_or(8080);

		Ok(Self::new(token, webhook_url, port))
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

		body.result.ok_or_else(|| "Telegram API returned no message".into())
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

#[derive(Clone)]
pub struct IncomingActionQueue {
	inner: Arc<IncomingActionQueueInner>,
}

impl IncomingActionQueue {
	pub fn new() -> Self {
		Self {
			inner: Arc::new(IncomingActionQueueInner {
				queue: Mutex::new(VecDeque::new()),
				notify: Notify::new(),
			}),
		}
	}

	pub fn writer(&self) -> IncomingActionWriter {
		IncomingActionWriter {
			inner: Arc::clone(&self.inner),
		}
	}

	pub fn register_service(&self) -> IncomingActionWriter {
		self.writer()
	}

	pub async fn pop(&self) -> IncomingAction {
		loop {
			if let Some(action) = {
				let mut queue = self.inner.queue.lock().await;
				queue.pop_front()
			} {
				return action;
			}

			self.inner.notify.notified().await;
		}
	}
}

struct IncomingActionQueueInner {
	queue: Mutex<VecDeque<IncomingAction>>,
	notify: Notify,
}

#[derive(Clone)]
pub struct IncomingActionWriter {
	inner: Arc<IncomingActionQueueInner>,
}

impl IncomingActionWriter {
	pub async fn push(&self, action: IncomingAction) {
		let mut queue = self.inner.queue.lock().await;
		queue.push_back(action);
		drop(queue);
		self.inner.notify.notify_one();
	}
}

#[derive(Debug, Clone)]
pub enum IncomingAction {
	Chat(ChatAction),
	Agent(AgentAction),
	Chron(ChronAction),
}

#[derive(Debug, Clone)]
pub struct ChatAction {
	pub chat_id: i64,
	pub text: String,
}

#[derive(Debug, Clone)]
pub struct AgentAction;

#[derive(Debug, Clone)]
pub struct ChronAction;

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
