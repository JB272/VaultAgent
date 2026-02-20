#[path = "telegram/telegram.rs"]
pub mod telegram;

#[path = "website/website.rs"]
pub mod website;

use async_trait::async_trait;

/// Jeder Kommunikationskanal (Website, Telegram, …) implementiert diesen Trait.
/// Man kann beliebig viele Gateways registrieren – der Agent broadcastet an alle.
#[async_trait]
pub trait Gateway: Send + Sync {
    fn name(&self) -> &str;

    /// Antwort an einen bestimmten Chat senden.
    async fn send_reply(
        &self,
        chat_id: i64,
        text: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Typing-Indikator setzen/aufheben.
    async fn notify_typing(
        &self,
        chat_id: i64,
        typing: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Registry aller aktiven Gateways.
/// Broadcastet Nachrichten und Typing-Status an alle registrierten Kanäle.
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
        println!("  Gateway registriert: {}", gateway.name());
        self.gateways.push(Box::new(gateway));
        self
    }

    pub async fn broadcast_reply(&self, chat_id: i64, text: &str) {
        for gw in &self.gateways {
            if let Err(e) = gw.send_reply(chat_id, text).await {
                eprintln!("[{}] Nachricht senden fehlgeschlagen: {}", gw.name(), e);
            }
        }
    }

    pub async fn broadcast_typing(&self, chat_id: i64, typing: bool) {
        for gw in &self.gateways {
            if let Err(e) = gw.notify_typing(chat_id, typing).await {
                eprintln!("[{}] Typing setzen fehlgeschlagen: {}", gw.name(), e);
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
