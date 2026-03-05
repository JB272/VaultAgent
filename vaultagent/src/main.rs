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
use reasoning::llm_apis::anthropic::AnthropicClient;
use reasoning::llm_apis::multi_provider::MultiProvider;
use reasoning::llm_apis::openai::OpenAiCompatibleClient;
use reasoning::llm_interface::LlmInterface;
use skills::SkillRegistry;
use skills::default_skills::email_mailbox::EmailMailboxSkill;
use skills::default_skills::github::GitHubSkill;
use skills::default_skills::research::ResearchSkill;
use skills::default_skills::spawn_subagent::SpawnSubagentSkill;
use soul::Soul;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

fn parse_upload_directive(reply: &str) -> (String, Option<String>, Option<String>) {
    // Preferred format from the model:
    // {"text":"...","upload_path":"relative/path.ext","upload_caption":"..."}
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(reply.trim()) {
        let text = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let upload_path = value
            .get("upload_path")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let upload_caption = value
            .get("upload_caption")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);

        if upload_path.is_some() {
            return (text, upload_path, upload_caption);
        }
    }

    // Fallback format: UPLOAD_FILE: relative/path.ext
    if let Some(path) = reply.trim().strip_prefix("UPLOAD_FILE:") {
        let clean = path.trim();
        if !clean.is_empty() {
            return (
                String::new(),
                Some(clean.to_string()),
                Some("Generated file".to_string()),
            );
        }
    }

    // Markdown fallback: [label](relative/path.ext)
    // If the model only returns a local markdown link, treat it as upload target.
    if let Some(open) = reply.rfind("](") {
        let tail = &reply[open + 2..];
        if let Some(close) = tail.find(')') {
            let candidate = tail[..close].trim();
            let looks_local = !candidate.starts_with("http://")
                && !candidate.starts_with("https://")
                && !candidate.starts_with('/')
                && !candidate.contains("..");
            let has_ext = std::path::Path::new(candidate).extension().is_some();

            if looks_local && has_ext {
                return (
                    String::new(),
                    Some(candidate.to_string()),
                    Some("Generated file".to_string()),
                );
            }
        }
    }

    (reply.to_string(), None, None)
}

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
    // Try to initialise all available providers.
    // LLM_PROVIDER ("anthropic" | "openai") selects the *default* backend;
    // the others are still available and switchable via /models.
    let preferred = std::env::var("LLM_PROVIDER")
        .unwrap_or_default()
        .to_lowercase();

    let mut backends: Vec<Arc<dyn LlmInterface>> = Vec::new();

    let openai_result = OpenAiCompatibleClient::from_env();
    let anthropic_result = AnthropicClient::from_env();

    // Insert preferred provider first so it becomes index 0 (= default).
    if preferred == "anthropic" {
        if let Ok(client) = anthropic_result {
            println!(
                "[Main][LLM] Enabled provider: {} (default)",
                client.provider_name()
            );
            backends.push(Arc::new(client));
        }
        if let Ok(client) = openai_result {
            println!("[Main][LLM] Enabled provider: {}", client.provider_name());
            backends.push(Arc::new(client));
        }
    } else {
        if let Ok(client) = openai_result {
            println!(
                "[Main][LLM] Enabled provider: {} (default)",
                client.provider_name()
            );
            backends.push(Arc::new(client));
        }
        if let Ok(client) = anthropic_result {
            println!("[Main][LLM] Enabled provider: {}", client.provider_name());
            backends.push(Arc::new(client));
        }
    }

    let llm: Option<Arc<dyn LlmInterface>> = if backends.is_empty() {
        eprintln!("[Main][LLM] No LLM providers configured.");
        None
    } else {
        Some(Arc::new(MultiProvider::new(backends)))
    };

    // ── Skills (remote worker + host secret-aware bridge skills) ───────
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

    // Host-side secret-aware skills.
    // They can still read/write Docker files through worker APIs.
    skills.add(EmailMailboxSkill);
    skills.add(GitHubSkill::new(worker_url.clone(), worker_token.clone()));

    // Host-side orchestration skills.
    // Their actual work uses remote worker tools.
    if let Some(ref llm_arc) = llm {
        skills.add(ResearchSkill::new(Arc::clone(llm_arc)).with_remote(remote.clone()));
        skills.add(SpawnSubagentSkill::new(Arc::clone(llm_arc)).with_remote(remote));
    }

    // ── Agent ───────────────────────────────────────────
    let agent = Arc::new(Agent::new(llm.clone(), skills, soul));

    // ── Incoming Queue ──────────────────────────────────
    let incoming = IncomingActionQueue::new();

    // ── Gateways ────────────────────────────────────────
    let mut gateways = GatewayRegistry::new();

    let website = setup_website(incoming.register_service(), &worker_url, &worker_token).await?;
    gateways.add(website.client);

    if let Some(telegram) = setup_telegram(
        incoming.register_service(),
        Arc::clone(&agent),
        llm,
        worker_url.clone(),
        worker_token.clone(),
    )
    .await
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

                let raw_reply = agent
                    .process(&chat.text, chat.chat_id, chat.image_url.as_deref())
                    .await;
                let _ = cancel_tx.send(());
                typing_task.await.ok();

                let (reply_text, upload_path, upload_caption) = parse_upload_directive(&raw_reply);
                if let Some(path) = upload_path {
                    gateways
                        .broadcast_file(chat.chat_id, &path, upload_caption.as_deref())
                        .await;
                }
                if !reply_text.trim().is_empty() {
                    gateways.broadcast_reply(chat.chat_id, &reply_text).await;
                }
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

                let raw_reply = agent
                    .process(&cron_action.prompt, cron_action.chat_id, None)
                    .await;
                let _ = cancel_tx.send(());
                typing_task.await.ok();

                let (reply_text, upload_path, upload_caption) = parse_upload_directive(&raw_reply);
                if let Some(path) = upload_path {
                    gateways
                        .broadcast_file(cron_action.chat_id, &path, upload_caption.as_deref())
                        .await;
                }
                if !reply_text.trim().is_empty() {
                    gateways
                        .broadcast_reply(cron_action.chat_id, &reply_text)
                        .await;
                }
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
        "/stop" => {
            println!("[Main] Stop requested via command");
            agent.stop_all();
            Some("⏹ Stopped all running jobs/subagents.".to_string())
        }
        _ => None,
    }
}
