#[path = "telegram/telegram.rs"]
pub mod telegram;

#[path = "website/website.rs"]
pub mod website;

use async_trait::async_trait;

/// Every communication channel (Website, Telegram, …) implements this trait.
/// Any number of gateways can be registered — the agent broadcasts to all.
#[async_trait]
pub trait Gateway: Send + Sync {
    fn name(&self) -> &str;

    /// Send a reply to a specific chat.
    async fn send_reply(
        &self,
        chat_id: i64,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Set/clear the typing indicator.
    async fn notify_typing(
        &self,
        chat_id: i64,
        typing: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Send a local file as chat attachment (if supported by the gateway).
    async fn send_file(
        &self,
        _chat_id: i64,
        _path: &str,
        _caption: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("File upload is not supported by this gateway".into())
    }

    /// Push incremental streaming text to the user.
    /// Gateways that don't support streaming can ignore this (no-op default).
    async fn stream_text(
        &self,
        _chat_id: i64,
        _text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    /// Clear any active streaming preview for the chat.
    async fn clear_stream(
        &self,
        _chat_id: i64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

/// Registry of all active gateways.
/// Broadcasts messages and typing status to all registered channels.
pub struct GatewayRegistry {
    gateways: Vec<Box<dyn Gateway>>,
}

impl GatewayRegistry {
    pub fn new() -> Self {
        Self {
            gateways: Vec::new(),
        }
    }

    pub fn add<G: Gateway + 'static>(&mut self, gateway: G) -> &mut Self {
        println!("[Gateway] Registered: {}", gateway.name());
        self.gateways.push(Box::new(gateway));
        self
    }

    pub async fn broadcast_reply(&self, chat_id: i64, text: &str) {
        let futures: Vec<_> = self
            .gateways
            .iter()
            .map(|gw| async move {
                if let Err(e) = gw.send_reply(chat_id, text).await {
                    eprintln!("[Gateway:{}] Failed to send reply: {}", gw.name(), e);
                }
            })
            .collect();
        futures_util::future::join_all(futures).await;
    }

    pub async fn broadcast_typing(&self, chat_id: i64, typing: bool) {
        let futures: Vec<_> = self
            .gateways
            .iter()
            .map(|gw| async move {
                if let Err(e) = gw.notify_typing(chat_id, typing).await {
                    eprintln!("[Gateway:{}] Failed to set typing state: {}", gw.name(), e);
                }
            })
            .collect();
        futures_util::future::join_all(futures).await;
    }

    pub async fn broadcast_file(&self, chat_id: i64, path: &str, caption: Option<&str>) {
        let futures: Vec<_> = self
            .gateways
            .iter()
            .map(|gw| async move {
                if let Err(e) = gw.send_file(chat_id, path, caption).await {
                    eprintln!(
                        "[Gateway:{}] Failed to send file '{}': {}",
                        gw.name(),
                        path,
                        e
                    );
                }
            })
            .collect();
        futures_util::future::join_all(futures).await;
    }

    pub async fn broadcast_stream_text(&self, chat_id: i64, text: &str) {
        let futures: Vec<_> = self
            .gateways
            .iter()
            .map(|gw| async move {
                if let Err(e) = gw.stream_text(chat_id, text).await {
                    eprintln!("[Gateway:{}] Failed to stream text: {}", gw.name(), e);
                }
            })
            .collect();
        futures_util::future::join_all(futures).await;
    }

    pub async fn broadcast_clear_stream(&self, chat_id: i64) {
        let futures: Vec<_> = self
            .gateways
            .iter()
            .map(|gw| async move {
                if let Err(e) = gw.clear_stream(chat_id).await {
                    eprintln!("[Gateway:{}] Failed to clear stream: {}", gw.name(), e);
                }
            })
            .collect();
        futures_util::future::join_all(futures).await;
    }
}

pub(crate) fn get_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn is_token_service_enabled(token_env_name: &str) -> bool {
    get_non_empty_env(token_env_name).is_some()
}
