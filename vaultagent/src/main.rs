mod gateway;
mod reasoning;

use gateway::com::telegram::TelegramBot;
use gateway::incoming_actions_queue::{IncomingAction, IncomingActionQueue};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let incoming_actions = IncomingActionQueue::new();
    let telegram_writer = incoming_actions.register_service();

    let telegram = TelegramBot::from_env()?;
    telegram.start(telegram_writer).await?;

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
