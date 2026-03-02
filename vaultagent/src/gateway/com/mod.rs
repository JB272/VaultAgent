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
        for gw in &self.gateways {
            if let Err(e) = gw.send_reply(chat_id, text).await {
                eprintln!("[Gateway:{}] Failed to send reply: {}", gw.name(), e);
            }
        }
    }

    pub async fn broadcast_typing(&self, chat_id: i64, typing: bool) {
        for gw in &self.gateways {
            if let Err(e) = gw.notify_typing(chat_id, typing).await {
                eprintln!("[Gateway:{}] Failed to set typing state: {}", gw.name(), e);
            }
        }
    }

    pub async fn broadcast_file(&self, chat_id: i64, path: &str, caption: Option<&str>) {
        for gw in &self.gateways {
            if let Err(e) = gw.send_file(chat_id, path, caption).await {
                eprintln!("[Gateway:{}] Failed to send file '{}': {}", gw.name(), path, e);
            }
        }
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
