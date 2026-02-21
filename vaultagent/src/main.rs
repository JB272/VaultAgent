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
use skills::default_skills::list_directory::ListDirectorySkill;
use skills::default_skills::memory_save::MemorySaveSkill;
use skills::default_skills::memory_search::MemorySearchSkill;
use skills::default_skills::read_file::ReadFileSkill;
use skills::default_skills::research::ResearchSkill;
use skills::default_skills::web_fetch::WebFetchSkill;
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

    // ── LLM ─────────────────────────────────────────────
    let llm: Option<std::sync::Arc<dyn LlmInterface>> = match OpenAiCompatibleClient::from_env() {
        Ok(client) => {
            println!("[Main][LLM] Enabled provider: {}", client.provider_name());
            Some(std::sync::Arc::new(client))
        }
        Err(err) => {
            eprintln!("[Main][LLM] Disabled: {}", err);
            None
        }
    };

    // ── Skills ──────────────────────────────────────────
    let mut skills = SkillRegistry::new();

    // Default Skills (Rust)
    skills.add(ReadFileSkill);
    skills.add(WriteFileSkill);
    skills.add(ListDirectorySkill);
    skills.add(WebSearchSkill::new());
    skills.add(WebFetchSkill::new());
    if let Some(ref llm_arc) = llm {
        skills.add(ResearchSkill::new(std::sync::Arc::clone(llm_arc)));
    }
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

    // ── Agent ───────────────────────────────────────────
    let agent = Arc::new(Agent::new(llm.clone(), skills, soul));

    // ── Incoming Queue ──────────────────────────────────
    let incoming = IncomingActionQueue::new();

    // ── Gateways ────────────────────────────────────────
    let mut gateways = GatewayRegistry::new();

    let website = setup_website(incoming.register_service()).await?;
    gateways.add(website.client);

    if let Some(telegram) =
        setup_telegram(incoming.register_service(), Arc::clone(&agent), llm).await
    {
        gateways.add(telegram);
    }

    let gateways = Arc::new(gateways);

    // ── Cron Scheduler ──────────────────────────────────
    CronScheduler::start(Arc::clone(&cron_store), incoming.register_service());
    println!("[Main][Cron] Scheduler started");

    // ── Event Loop ──────────────────────────────────────
    loop {
        let action = incoming.pop().await;
        match action {
            IncomingAction::Chat(chat) => {
                println!(
                    "[Main][Chat] Received message from chat {}: {}",
                    chat.chat_id, chat.text
                );

                let gw = Arc::clone(&gateways);
                let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
                let chat_id = chat.chat_id;

                // Keep re-sending typing every 4s — Telegram hides it after ~5s.
                let typing_task = tokio::spawn(async move {
                    loop {
                        gw.broadcast_typing(chat_id, true).await;
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {}
                            _ = &mut cancel_rx => break,
                        }
                    }
                });

                let reply = agent.process(&chat.text, chat.chat_id).await;
                let _ = cancel_tx.send(());
                typing_task.await.ok();

                gateways.broadcast_reply(chat.chat_id, &reply).await;
                gateways.broadcast_typing(chat.chat_id, false).await;
            }
            IncomingAction::Agent(_) => {}
            IncomingAction::Cron(cron_action) => {
                println!(
                    "[Main][Cron] Triggered job \"{}\" for chat {}",
                    cron_action.job_name, cron_action.chat_id
                );

                let gw = Arc::clone(&gateways);
                let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
                let chat_id = cron_action.chat_id;

                let typing_task = tokio::spawn(async move {
                    loop {
                        gw.broadcast_typing(chat_id, true).await;
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(4)) => {}
                            _ = &mut cancel_rx => break,
                        }
                    }
                });

                let reply = agent
                    .process(&cron_action.prompt, cron_action.chat_id)
                    .await;
                let _ = cancel_tx.send(());
                typing_task.await.ok();

                gateways.broadcast_reply(cron_action.chat_id, &reply).await;
                gateways.broadcast_typing(cron_action.chat_id, false).await;
            }
        }
    }
}
