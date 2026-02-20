mod gateway;
mod reasoning;
mod skills;
mod soul;

use gateway::com::GatewayRegistry;
use gateway::com::telegram::setup_telegram;
use gateway::com::website::setup_website;
use gateway::incoming_actions_queue::{IncomingAction, IncomingActionQueue};
use reasoning::agent::Agent;
use reasoning::llm_apis::openai::OpenAiCompatibleClient;
use reasoning::llm_interface::LlmInterface;
use skills::SkillRegistry;
use skills::default_skills::memory_save::MemorySaveSkill;
use skills::default_skills::memory_search::MemorySearchSkill;
use skills::default_skills::read_file::ReadFileSkill;
use skills::default_skills::web_search::WebSearchSkill;
use skills::default_skills::write_file::WriteFileSkill;
use skills::python_skill::load_python_skills;
use soul::Soul;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    if dotenvy::dotenv().is_err() {
        let _ = dotenvy::from_filename("vaultagent/.env");
    }

    // ── Soul (Persönlichkeit + Gedächtnis) ────────────
    let soul_dir = std::env::var("SOUL_DIR").unwrap_or_else(|_| "soul".to_string());
    let soul = Arc::new(Soul::load(Path::new(&soul_dir)));

    // ── Skills ──────────────────────────────────────────
    let mut skills = SkillRegistry::new();

    // Default Skills (Rust)
    skills.add(ReadFileSkill);
    skills.add(WriteFileSkill);
    skills.add(WebSearchSkill::new());
    skills.add(MemorySaveSkill::new(Arc::clone(&soul.memory)));
    skills.add(MemorySearchSkill::new(Arc::clone(&soul.memory)));

    // Softcoded Skills (Python-Skripte aus skills/ Verzeichnis)
    let python_skills_dir =
        std::env::var("PYTHON_SKILLS_DIR").unwrap_or_else(|_| "skills".to_string());
    for skill in load_python_skills(Path::new(&python_skills_dir)).await {
        skills.add(skill);
    }

    // ── LLM ─────────────────────────────────────────────
    let llm: Option<Box<dyn LlmInterface>> = match OpenAiCompatibleClient::from_env() {
        Ok(client) => {
            println!("LLM aktiv: {}", client.provider_name());
            Some(Box::new(client))
        }
        Err(err) => {
            eprintln!("LLM deaktiviert: {}", err);
            None
        }
    };

    // ── Agent ───────────────────────────────────────────
    let agent = Agent::new(llm, skills, soul);

    // ── Incoming Queue ──────────────────────────────────
    let incoming = IncomingActionQueue::new();

    // ── Gateways ────────────────────────────────────────
    let mut gateways = GatewayRegistry::new();

    let website = setup_website(incoming.register_service()).await?;
    gateways.add(website.client);

    if let Some(telegram) = setup_telegram(incoming.register_service()).await {
        gateways.add(telegram);
    }

    // ── Event Loop ──────────────────────────────────────
    loop {
        let action = incoming.pop().await;
        match action {
            IncomingAction::Chat(chat) => {
                println!("Chat-Nachricht von {}: {}", chat.chat_id, chat.text);

                gateways.broadcast_typing(chat.chat_id, true).await;
                let reply = agent.process(&chat.text).await;
                gateways.broadcast_reply(chat.chat_id, &reply).await;
                gateways.broadcast_typing(chat.chat_id, false).await;
            }
            IncomingAction::Agent(_) => {}
            IncomingAction::Chron(_) => {}
        }
    }
}
