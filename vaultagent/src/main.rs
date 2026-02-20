mod cron;
mod gateway;
mod reasoning;
mod skills;
mod soul;

use cron::{CronScheduler, CronStore};
use gateway::com::GatewayRegistry;
use gateway::com::telegram::setup_telegram;
use gateway::com::website::setup_website;
use gateway::incoming_actions_queue::{IncomingAction, IncomingActionQueue};
use reasoning::agent::Agent;
use reasoning::llm_apis::openai::OpenAiCompatibleClient;
use reasoning::llm_interface::LlmInterface;
use skills::SkillRegistry;
use skills::default_skills::cron_add::CronAddSkill;
use skills::default_skills::cron_list::CronListSkill;
use skills::default_skills::cron_remove::CronRemoveSkill;
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

    // ── Cron-Store ────────────────────────────────────────
    let cron_dir = std::env::var("CRON_DIR").unwrap_or_else(|_| "cron".to_string());
    let cron_store = Arc::new(CronStore::load(Path::new(&cron_dir)));

    // ── Skills ──────────────────────────────────────────
    let mut skills = SkillRegistry::new();

    // Default Skills (Rust)
    skills.add(ReadFileSkill);
    skills.add(WriteFileSkill);
    skills.add(WebSearchSkill::new());
    skills.add(MemorySaveSkill::new(Arc::clone(&soul.memory)));
    skills.add(MemorySearchSkill::new(Arc::clone(&soul.memory)));

    // Cron Skills
    skills.add(CronAddSkill::new(Arc::clone(&cron_store)));
    skills.add(CronListSkill::new(Arc::clone(&cron_store)));
    skills.add(CronRemoveSkill::new(Arc::clone(&cron_store)));

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

    // ── Cron Scheduler ──────────────────────────────────
    CronScheduler::start(Arc::clone(&cron_store), incoming.register_service());
    println!("  Cron-Scheduler aktiv");

    // ── Event Loop ──────────────────────────────────────
    loop {
        let action = incoming.pop().await;
        match action {
            IncomingAction::Chat(chat) => {
                println!("Chat-Nachricht von {}: {}", chat.chat_id, chat.text);

                gateways.broadcast_typing(chat.chat_id, true).await;
                let reply = agent.process(&chat.text, chat.chat_id).await;
                gateways.broadcast_reply(chat.chat_id, &reply).await;
                gateways.broadcast_typing(chat.chat_id, false).await;
            }
            IncomingAction::Agent(_) => {}
            IncomingAction::Cron(cron_action) => {
                println!(
                    "Cron-Job ausgelöst: \"{}\" → Chat {}",
                    cron_action.job_name, cron_action.chat_id
                );

                gateways.broadcast_typing(cron_action.chat_id, true).await;
                let reply = agent.process(&cron_action.prompt, cron_action.chat_id).await;
                gateways
                    .broadcast_reply(cron_action.chat_id, &reply)
                    .await;
                gateways
                    .broadcast_typing(cron_action.chat_id, false)
                    .await;
            }
        }
    }
}
