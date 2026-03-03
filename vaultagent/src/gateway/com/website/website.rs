use crate::gateway::incoming_actions_queue::{ChatAction, IncomingAction, IncomingActionWriter};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
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
        auth_username: String,
        auth_password: String,
        internal_token: String,
        worker_url: String,
        worker_token: String,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let app_state = WebsiteState {
            incoming_writer,
            default_chat_id: self.default_chat_id,
            messages: Arc::new(Mutex::new(Vec::new())),
            assistant_typing: Arc::new(Mutex::new(false)),
            assistant_stream_text: Arc::new(Mutex::new(None)),
            auth_username,
            auth_password,
            internal_token,
            worker_url,
            worker_token,
            http_client: Client::new(),
        };

        let app = Router::new()
            .route("/", get(index))
            .route("/assets/website.css", get(stylesheet))
            .route("/assets/website.js", get(javascript))
            .route("/api/messages", get(get_messages).post(post_message))
            .route("/api/files/list", get(list_files))
            .route("/api/files/read", get(read_file_content))
            .route("/api/files/write", axum::routing::post(write_file_content))
            .route("/api/files/delete", axum::routing::post(delete_file_handler))
            .route("/api/files/mkdir", axum::routing::post(create_directory))
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
            .layer(middleware::from_fn_with_state(
                app_state.clone(),
                auth_middleware,
            ))
            .with_state(app_state);

        let address = SocketAddr::from(([0, 0, 0, 0], self.port));
        println!("[Website] Server listening on http://0.0.0.0:{} (auth required)", self.port);

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
    internal_token: String,
}

impl WebsiteClient {
    pub fn new(port: u16, internal_token: &str) -> Self {
        Self {
            http_client: Client::new(),
            base_url: format!("http://127.0.0.1:{}", port),
            internal_token: internal_token.to_string(),
        }
    }

    pub async fn push_assistant_message(&self, text: &str) -> Result<(), reqwest::Error> {
        let payload = WebsiteAssistantMessage {
            text: text.to_string(),
            source: "assistant".to_string(),
        };

        self.http_client
            .post(format!("{}/api/messages/system", self.base_url))
            .header("x-internal-token", &self.internal_token)
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
            .header("x-internal-token", &self.internal_token)
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
            .header("x-internal-token", &self.internal_token)
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

    async fn send_file(
        &self,
        _chat_id: i64,
        path: &str,
        caption: Option<&str>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut msg = format!("[File generated: {}]", path);
        if let Some(caption) = caption {
            if !caption.trim().is_empty() {
                msg.push_str("\n");
                msg.push_str(caption);
            }
        }
        self.push_assistant_message(&msg).await?;
        Ok(())
    }
}

pub struct WebsiteSetup {
    pub client: WebsiteClient,
    pub chat_id: i64,
}

pub async fn setup_website(
    incoming_writer: IncomingActionWriter,
    worker_url: &str,
    worker_token: &str,
) -> Result<WebsiteSetup, Box<dyn Error + Send + Sync>> {
    let website = WebsiteGateway::from_env();
    let port = website.port();
    let chat_id = website.chat_id();

    let auth_username = std::env::var("USERNAME").unwrap_or_else(|_| "admin".to_string());
    let auth_password = std::env::var("PASSWORD").expect(
        "PASSWORD must be set in .env.secure for web interface authentication",
    );

    let internal_token = uuid::Uuid::new_v4().to_string();

    website
        .start(
            incoming_writer,
            auth_username,
            auth_password,
            internal_token.clone(),
            worker_url.to_string(),
            worker_token.to_string(),
        )
        .await?;

    Ok(WebsiteSetup {
        client: WebsiteClient::new(port, &internal_token),
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
    auth_username: String,
    auth_password: String,
    internal_token: String,
    worker_url: String,
    worker_token: String,
    http_client: Client,
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

#[derive(Debug, Deserialize)]
struct FilePathQuery {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WriteFileRequest {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct DeleteFileRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
struct CreateDirRequest {
    path: String,
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

// ── Authentication ─────────────────────────────────────────────────────

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

async fn auth_middleware(
    State(state): State<WebsiteState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Internal token (for WebsiteClient within the same process)
    if let Some(token) = request.headers().get("x-internal-token") {
        if let Ok(token_str) = token.to_str() {
            if constant_time_eq(token_str.as_bytes(), state.internal_token.as_bytes()) {
                return next.run(request).await;
            }
        }
    }

    // HTTP Basic Auth
    if let Some(auth_header) = request.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(encoded) = auth_str.strip_prefix("Basic ") {
                if let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(encoded)
                {
                    if let Ok(credentials) = String::from_utf8(decoded) {
                        if let Some((user, pass)) = credentials.split_once(':') {
                            let user_ok = constant_time_eq(
                                user.as_bytes(),
                                state.auth_username.as_bytes(),
                            );
                            let pass_ok = constant_time_eq(
                                pass.as_bytes(),
                                state.auth_password.as_bytes(),
                            );
                            if user_ok && pass_ok {
                                return next.run(request).await;
                            }
                        }
                    }
                }
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"VaultAgent\"")],
        "Unauthorized",
    )
        .into_response()
}

// ── File API (proxied to Docker worker) ────────────────────────────────

async fn call_worker_skill(
    state: &WebsiteState,
    skill: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, StatusCode> {
    let mut req = state
        .http_client
        .post(format!("{}/execute", state.worker_url))
        .json(&json!({ "name": skill, "arguments": arguments }));

    if !state.worker_token.is_empty() {
        req = req.header("x-worker-token", &state.worker_token);
    }

    let response = req.send().await.map_err(|e| {
        eprintln!("[Website][Files] Worker request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let data: serde_json::Value = response.json().await.map_err(|e| {
        eprintln!("[Website][Files] Failed to parse worker response: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    // Worker returns { ok, result: Option<String>, error }.
    // Parse the inner result string as JSON for the frontend.
    if let Some(result_str) = data.get("result").and_then(|v| v.as_str()) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result_str) {
            return Ok(parsed);
        }
    }

    Ok(data)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn list_files(
    State(state): State<WebsiteState>,
    Query(query): Query<FilePathQuery>,
) -> impl IntoResponse {
    let path = query.path.unwrap_or_else(|| ".".to_string());
    match call_worker_skill(&state, "list_directory", json!({ "path": path })).await {
        Ok(data) => Json(data).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn read_file_content(
    State(state): State<WebsiteState>,
    Query(query): Query<FilePathQuery>,
) -> impl IntoResponse {
    let path = query.path.unwrap_or_default();
    if path.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match call_worker_skill(&state, "read_file", json!({ "path": path })).await {
        Ok(data) => Json(data).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn write_file_content(
    State(state): State<WebsiteState>,
    Json(request): Json<WriteFileRequest>,
) -> impl IntoResponse {
    if request.path.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match call_worker_skill(
        &state,
        "write_file",
        json!({ "path": request.path, "content": request.content }),
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn delete_file_handler(
    State(state): State<WebsiteState>,
    Json(request): Json<DeleteFileRequest>,
) -> impl IntoResponse {
    let path = request.path.trim().to_string();
    if path.is_empty() || path == "." || path == "/" || path.contains('\0') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let cmd = format!("rm -rf {}", shell_escape(&path));
    match call_worker_skill(&state, "shell_execute", json!({ "command": cmd })).await {
        Ok(data) => Json(data).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn create_directory(
    State(state): State<WebsiteState>,
    Json(request): Json<CreateDirRequest>,
) -> impl IntoResponse {
    let path = request.path.trim().to_string();
    if path.is_empty() || path.contains('\0') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let cmd = format!("mkdir -p {}", shell_escape(&path));
    match call_worker_skill(&state, "shell_execute", json!({ "command": cmd })).await {
        Ok(data) => Json(data).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        WEBSITE_JS,
    )
}

const INDEX_HTML: &str = include_str!("website.html");
const WEBSITE_CSS: &str = include_str!("website.css");
const WEBSITE_JS: &str = include_str!("website.js");
