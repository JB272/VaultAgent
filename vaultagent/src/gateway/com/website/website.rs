use crate::gateway::incoming_actions_queue::{ChatAction, IncomingAction, IncomingActionWriter};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    http::header,
    response::{Html, IntoResponse},
    routing::get,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{error::Error, net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;

use super::Gateway;

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
            assistant_typing: Arc::new(Mutex::new(false)),
            assistant_stream_text: Arc::new(Mutex::new(None)),
        };

        let app = Router::new()
            .route("/", get(index))
            .route("/assets/website.css", get(stylesheet))
            .route("/api/messages", get(get_messages).post(post_message))
            .route(
                "/api/messages/system",
                axum::routing::post(post_system_message),
            )
            .route(
                "/api/assistant/typing",
                axum::routing::post(post_assistant_typing),
            )
            .route(
                "/api/assistant/stream",
                axum::routing::post(post_assistant_stream),
            )
            .with_state(app_state);

        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        println!("[Website] Chat server listening on http://{}", address);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(address).await {
                Ok(value) => value,
                Err(err) => {
                        eprintln!("[Website] Failed to bind port: {}", err);
                    return;
                }
            };

            if let Err(err) = axum::serve(listener, app).await {
                    eprintln!("[Website] Server exited with error: {}", err);
            }
        });

        Ok(())
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn chat_id(&self) -> i64 {
        self.default_chat_id
    }
}

#[derive(Clone)]
pub struct WebsiteClient {
    http_client: Client,
    base_url: String,
}

impl WebsiteClient {
    pub fn new(port: u16) -> Self {
        Self {
            http_client: Client::new(),
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    pub async fn push_assistant_message(&self, text: &str) -> Result<(), reqwest::Error> {
        let payload = WebsiteAssistantMessage {
            text: text.to_string(),
            source: "assistant".to_string(),
        };

        self.http_client
            .post(format!("{}/api/messages/system", self.base_url))
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    pub async fn set_typing(&self, typing: bool) -> Result<(), reqwest::Error> {
        let payload = WebsiteTypingState { typing };

        self.http_client
            .post(format!("{}/api/assistant/typing", self.base_url))
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    pub async fn set_stream_text(&self, text: Option<String>) -> Result<(), reqwest::Error> {
        let payload = WebsiteAssistantStreamState { text };

        self.http_client
            .post(format!("{}/api/assistant/stream", self.base_url))
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    pub fn streaming_base_url(&self) -> String {
        self.base_url.clone()
    }
}

#[async_trait]
impl Gateway for WebsiteClient {
    fn name(&self) -> &str {
        "website"
    }

    async fn send_reply(
        &self,
        _chat_id: i64,
        text: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.push_assistant_message(text).await?;
        Ok(())
    }

    async fn notify_typing(
        &self,
        _chat_id: i64,
        typing: bool,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.set_typing(typing).await?;
        Ok(())
    }
}

pub struct WebsiteSetup {
    pub client: WebsiteClient,
    pub chat_id: i64,
}

pub async fn setup_website(
    incoming_writer: IncomingActionWriter,
) -> Result<WebsiteSetup, Box<dyn Error + Send + Sync>> {
    let website = WebsiteGateway::from_env();
    let port = website.port();
    let chat_id = website.chat_id();
    website.start(incoming_writer).await?;

    Ok(WebsiteSetup {
        client: WebsiteClient::new(port),
        chat_id,
    })
}

#[derive(Clone)]
struct WebsiteState {
    incoming_writer: IncomingActionWriter,
    default_chat_id: i64,
    messages: Arc<Mutex<Vec<WebMessage>>>,
    assistant_typing: Arc<Mutex<bool>>,
    assistant_stream_text: Arc<Mutex<Option<String>>>,
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

#[derive(Debug, Deserialize)]
struct PostSystemMessageRequest {
    text: String,
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PostAssistantTypingRequest {
    typing: bool,
}

#[derive(Debug, Deserialize)]
struct PostAssistantStreamRequest {
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct WebsiteAssistantMessage {
    text: String,
    source: String,
}

#[derive(Debug, Serialize)]
struct WebsiteTypingState {
    typing: bool,
}

#[derive(Debug, Serialize)]
struct WebsiteAssistantStreamState {
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct MessagesResponse {
    messages: Vec<WebMessage>,
    assistant_typing: bool,
    assistant_stream_text: Option<String>,
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

async fn get_messages(State(state): State<WebsiteState>) -> Json<MessagesResponse> {
    let messages = state.messages.lock().await.clone();
    let assistant_typing = *state.assistant_typing.lock().await;
    let assistant_stream_text = state.assistant_stream_text.lock().await.clone();
    Json(MessagesResponse {
        messages,
        assistant_typing,
        assistant_stream_text,
    })
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
            image_url: None,
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

async fn post_system_message(
    State(state): State<WebsiteState>,
    Json(request): Json<PostSystemMessageRequest>,
) -> impl IntoResponse {
    if request.text.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    let mut messages = state.messages.lock().await;
    messages.push(WebMessage {
        text: request.text.trim().to_string(),
        source: request.source.unwrap_or_else(|| "system".to_string()),
    });
    if messages.len() > 200 {
        let remove_count = messages.len() - 200;
        messages.drain(0..remove_count);
    }

    let mut stream_text = state.assistant_stream_text.lock().await;
    *stream_text = None;

    StatusCode::ACCEPTED
}

async fn post_assistant_typing(
    State(state): State<WebsiteState>,
    Json(request): Json<PostAssistantTypingRequest>,
) -> impl IntoResponse {
    let mut typing = state.assistant_typing.lock().await;
    *typing = request.typing;
    StatusCode::ACCEPTED
}

async fn post_assistant_stream(
    State(state): State<WebsiteState>,
    Json(request): Json<PostAssistantStreamRequest>,
) -> impl IntoResponse {
    let mut stream_text = state.assistant_stream_text.lock().await;
    *stream_text = request.text.and_then(|text| {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    StatusCode::ACCEPTED
}
const INDEX_HTML: &str = include_str!("website.html");
const WEBSITE_CSS: &str = include_str!("website.css");
