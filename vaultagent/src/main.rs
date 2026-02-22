mod cron;
mod gateway;
mod reasoning;
mod skills;
mod soul;
mod worker;

use cron::{CronScheduler, CronStore};
use gateway::com::GatewayRegistry;
use gateway::com::telegram::setup_telegram;
use gateway::com::website::setup_website;
use gateway::incoming_actions_queue::{IncomingAction, IncomingActionQueue};
use reasoning::agent::Agent;
use reasoning::llm_apis::openai::OpenAiCompatibleClient;
use reasoning::llm_interface::LlmInterface;
use skills::SkillRegistry;
use skills::default_skills::research::ResearchSkill;
use soul::Soul;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // ── Worker mode ─────────────────────────────────────
    // When started with `--worker`, run as a sandbox HTTP skill server
    // inside Docker and exit.  No Telegram, no LLM, no secrets.
    if std::env::args().any(|a| a == "--worker") {
        // Load worker-specific env
        if dotenvy::from_filename(".env.docker").is_err() {
            let _ = dotenvy::dotenv(); // fallback
        }
        return worker::start_worker().await;
    }

    // ── Host / Orchestrator mode ────────────────────────
    // Load host-only env (secrets)
    if dotenvy::from_filename(".env.secure").is_err() {
        if dotenvy::dotenv().is_err() {
            let _ = dotenvy::from_filename("vaultagent/.env");
        }
    }

    // ── Soul (only needed for system prompt on the host) ──
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

    // ── Skills (always via Docker sandbox worker) ───────
    let worker_url = std::env::var("WORKER_URL")
        .expect("WORKER_URL must be set (e.g. http://localhost:9100). All skills run in the Docker sandbox.");
    let worker_token = std::env::var("WORKER_TOKEN").unwrap_or_default();
    println!("[Main][Sandbox] Connecting to worker at {} …", worker_url);

    let remote = skills::RemoteSkillProxy::connect(&worker_url, &worker_token).await?;
    println!(
        "[Main][Sandbox] Connected — {} remote skills available",
        remote.skill_names().len()
    );

    let mut skills = SkillRegistry::new_with_remote(remote.clone());

    // `research` stays host-side because it orchestrates via the LLM.
    // Its sub-skills (web_search, web_fetch) are routed through the worker.
    if let Some(ref llm_arc) = llm {
        skills.add(ResearchSkill::new(Arc::clone(llm_arc)).with_remote(remote));
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

                // ── Handle slash commands (works across all channels) ──
                let trimmed = chat.text.trim();
                if let Some(reply) = handle_global_command(trimmed, &agent).await {
                    gateways.broadcast_reply(chat.chat_id, &reply).await;
                    continue;
                }

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

                let reply = agent.process(&chat.text, chat.chat_id, chat.image_url.as_deref()).await;
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
                    .process(&cron_action.prompt, cron_action.chat_id, None)
                    .await;
                let _ = cancel_tx.send(());
                typing_task.await.ok();

                gateways.broadcast_reply(cron_action.chat_id, &reply).await;
                gateways.broadcast_typing(cron_action.chat_id, false).await;
            }
        }
    }
}

/// Handles slash commands globally (all channels: Website, Telegram, etc.).
/// Returns `Some(reply)` if handled, `None` to forward to the agent.
async fn handle_global_command(text: &str, agent: &Agent) -> Option<String> {
    match text {
        "/new" => {
            agent.clear_history().await;
            Some("🧹 Konversation zurückgesetzt. Neuer Chat gestartet!".to_string())
        }
        "/window" => Some(agent.context_window_info().await),
        "/tools" => {
            let names = agent.skill_names();
            let list = names
                .iter()
                .map(|n| format!("• {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!("🛠 Available tools:\n\n{list}"))
        }
        "/stats" => {
            if let Some(ref usage) = agent.usage {
                Some(usage.stats_message().await)
            } else {
                Some("No usage data available.".to_string())
            }
        }
        "/reboot" => {
            println!("[Main] Reboot requested via command");
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                std::process::exit(0);
            });
            Some("♻️ Rebooting...".to_string())
        }
        _ => None,
    }
}
