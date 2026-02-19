mod gateway;
mod reasoning;

use gateway::com::{telegram::TelegramBot, website::WebsiteGateway};
use gateway::incoming_actions_queue::{IncomingAction, IncomingActionQueue};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let incoming_actions = IncomingActionQueue::new();

    let website_writer = incoming_actions.register_service();
    let website = WebsiteGateway::from_env();
    website.start(website_writer).await?;

    if TelegramBot::is_enabled() {
        if let Some(telegram) = TelegramBot::from_env() {
            let telegram_writer = incoming_actions.register_service();
            telegram.start(telegram_writer).await?;
        } else {
            println!("Telegram deaktiviert: unvollständige Telegram-Konfiguration.");
        }
    } else {
        println!("Telegram deaktiviert: kein Telegram-Token gefunden.");
    }



	
    loop {
        let action = incoming_actions.pop().await;
        match action {
            IncomingAction::Chat(chat) => {
                println!("Chat-Nachricht von {}: {}", chat.chat_id, chat.text);
            }
            IncomingAction::Agent(_) => {}
            IncomingAction::Chron(_) => {}
        }
    }
}
