use crate::gateway::incoming_actions_queue::{ChatAction, IncomingAction, IncomingActionWriter};
use axum::{
		Json, Router,
		extract::State,
		http::header,
		http::StatusCode,
		response::{Html, IntoResponse},
		routing::get,
};
use serde::{Deserialize, Serialize};
use std::{error::Error, net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct WebsiteGateway {
		port: u16,
		default_chat_id: i64,
}

impl WebsiteGateway {
		pub fn new(port: u16, default_chat_id: i64) -> Self {
				Self {
						port,
						default_chat_id,
				}
		}

		pub fn from_env() -> Self {
				let port = std::env::var("WEBSITE_PORT")
						.ok()
						.and_then(|value| value.parse().ok())
						.unwrap_or(8090);

				let default_chat_id = std::env::var("WEBSITE_CHAT_ID")
						.ok()
						.and_then(|value| value.parse().ok())
						.unwrap_or(9_001);

				Self::new(port, default_chat_id)
		}

		pub async fn start(
				&self,
				incoming_writer: IncomingActionWriter,
		) -> Result<(), Box<dyn Error + Send + Sync>> {
				let app_state = WebsiteState {
						incoming_writer,
						default_chat_id: self.default_chat_id,
						messages: Arc::new(Mutex::new(Vec::new())),
				};

				let app = Router::new()
						.route("/", get(index))
						.route("/assets/website.css", get(stylesheet))
						.route("/api/messages", get(get_messages).post(post_message))
						.with_state(app_state);

				let address = SocketAddr::from(([127, 0, 0, 1], self.port));
				println!("Website chat läuft auf http://{}", address);

				tokio::spawn(async move {
						let listener = match tokio::net::TcpListener::bind(address).await {
								Ok(value) => value,
								Err(err) => {
										eprintln!("Fehler beim Binden des Website-Ports: {}", err);
										return;
								}
						};

						if let Err(err) = axum::serve(listener, app).await {
								eprintln!("Website-Server ist mit Fehler beendet: {}", err);
						}
				});

				Ok(())
		}
}

#[derive(Clone)]
struct WebsiteState {
		incoming_writer: IncomingActionWriter,
		default_chat_id: i64,
		messages: Arc<Mutex<Vec<WebMessage>>>,
}

#[derive(Debug, Clone, Serialize)]
struct WebMessage {
		text: String,
		source: String,
}

#[derive(Debug, Deserialize)]
struct PostMessageRequest {
		text: String,
}

async fn index() -> Html<&'static str> {
		Html(INDEX_HTML)
}

async fn stylesheet() -> impl IntoResponse {
		(
				[(header::CONTENT_TYPE, "text/css; charset=utf-8")],
				WEBSITE_CSS,
		)
}

async fn get_messages(State(state): State<WebsiteState>) -> Json<Vec<WebMessage>> {
		let messages = state.messages.lock().await.clone();
		Json(messages)
}

async fn post_message(
		State(state): State<WebsiteState>,
		Json(request): Json<PostMessageRequest>,
) -> impl IntoResponse {
		if request.text.trim().is_empty() {
				return StatusCode::BAD_REQUEST;
		}

		let trimmed_text = request.text.trim().to_string();

		state
				.incoming_writer
				.push(IncomingAction::Chat(ChatAction {
						chat_id: state.default_chat_id,
						text: trimmed_text.clone(),
				}))
				.await;

		let mut messages = state.messages.lock().await;
		messages.push(WebMessage {
				text: trimmed_text,
				source: "you".to_string(),
		});
		if messages.len() > 200 {
				let remove_count = messages.len() - 200;
				messages.drain(0..remove_count);
		}

		StatusCode::ACCEPTED
}
const INDEX_HTML: &str = include_str!("website.html");
const WEBSITE_CSS: &str = include_str!("website.css");
