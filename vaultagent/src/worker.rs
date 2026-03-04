//! Sandbox worker — a lightweight HTTP server that exposes skill execution.
//!
//! Runs inside Docker (started with `vaultagent --worker`).  The host
//! orchestrator sends tool-call requests here; no LLM keys or Telegram
//! tokens ever enter this container.

use axum::{
    Router,
    extract::{Json, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cron::CronStore;
use crate::skills::Skill;
use crate::skills::SkillRegistry;
use crate::skills::default_skills::cron_add::CronAddSkill;
use crate::skills::default_skills::cron_list::CronListSkill;
use crate::skills::default_skills::cron_remove::CronRemoveSkill;
use crate::skills::default_skills::extract_pdf::ExtractPdfSkill;
use crate::skills::default_skills::file_copy::FileCopySkill;
use crate::skills::default_skills::file_store::FileStoreSkill;
use crate::skills::default_skills::github::GitHubSkill;
use crate::skills::default_skills::list_directory::ListDirectorySkill;
use crate::skills::default_skills::memory_get::MemoryGetSkill;
use crate::skills::default_skills::memory_save::MemorySaveSkill;
use crate::skills::default_skills::memory_search::MemorySearchSkill;
use crate::skills::default_skills::email_mailbox::EmailMailboxSkill;
use crate::skills::default_skills::read_file::ReadFileSkill;
use crate::skills::default_skills::shell_execute::ShellExecuteSkill;
use crate::skills::default_skills::web_fetch::WebFetchSkill;
use crate::skills::default_skills::web_search::WebSearchSkill;
use crate::skills::default_skills::write_file::WriteFileSkill;
use crate::skills::python_skill::load_python_skills;
use crate::soul::Soul;

// ── State ────────────────────────────────────────────────

#[derive(Clone)]
struct WorkerState {
    skills: Arc<Mutex<SkillRegistry>>,
    token: String,
    python_skills_dir: String,
}

async fn reload_python_skills_if_needed(state: &WorkerState) {
    let loaded = load_python_skills(Path::new(&state.python_skills_dir)).await;
    if loaded.is_empty() {
        return;
    }

    let mut reg = state.skills.lock().await;
    let existing = reg.skill_names();
    let mut added = 0usize;

    for skill in loaded {
        let name = skill.definition().name;
        if existing.iter().any(|n| n == &name) {
            continue;
        }
        reg.add(skill);
        added += 1;
    }

    if added > 0 {
        println!("[Worker] Hot-loaded {} new Python skill(s)", added);
    }
}

// ── Request / Response types ─────────────────────────────

#[derive(Deserialize)]
struct ExecuteRequest {
    name: String,
    arguments: Value,
}

#[derive(Serialize)]
struct ExecuteResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DefinitionEntry {
    name: String,
    description: Option<String>,
    parameters_schema: Value,
}

// ── Auth helper ──────────────────────────────────────────

fn check_token(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return true; // no token configured → allow (dev mode)
    }
    headers
        .get("x-worker-token")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false)
}

// ── Handlers ─────────────────────────────────────────────

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn definitions(
    State(state): State<WorkerState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DefinitionEntry>>, StatusCode> {
    if !check_token(&headers, &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Refresh Python skills so newly created scripts are exposed immediately.
    reload_python_skills_if_needed(&state).await;

    let reg = state.skills.lock().await;
    let defs = reg
        .tool_definitions()
        .into_iter()
        .map(|d| DefinitionEntry {
            name: d.name,
            description: d.description,
            parameters_schema: d.parameters_schema,
        })
        .collect();
    Ok(Json(defs))
}

async fn execute(
    State(state): State<WorkerState>,
    headers: HeaderMap,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, StatusCode> {
    if !check_token(&headers, &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    println!(
        "[Worker] Executing skill: {} | args: {}",
        req.name, req.arguments
    );

    // First attempt with current registry.
    let first_try = {
        let reg = state.skills.lock().await;
        reg.execute(&req.name, &req.arguments).await
    };

    // If unknown, try hot-reloading Python skills once and retry.
    let result = if first_try.is_none() {
        reload_python_skills_if_needed(&state).await;
        let reg = state.skills.lock().await;
        reg.execute(&req.name, &req.arguments).await
    } else {
        first_try
    };

    match result {
        Some(result) => {
            // Log first 200 chars of result for debugging
            let preview: String = result.chars().take(200).collect();
            println!("[Worker] Skill '{}' result: {}…", req.name, preview);
            Ok(Json(ExecuteResponse {
                ok: true,
                result: Some(result),
                error: None,
            }))
        }
        None => {
            eprintln!("[Worker] Unknown skill requested: {}", req.name);
            Ok(Json(ExecuteResponse {
                ok: false,
                result: None,
                error: Some(format!("Unknown skill: {}", req.name)),
            }))
        }
    }
}

// ── Entrypoint ───────────────────────────────────────────

/// Start the worker HTTP server.  Called when the binary runs with `--worker`.
pub async fn start_worker() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let token = std::env::var("WORKER_TOKEN").unwrap_or_else(|_| {
        eprintln!("[Worker] WARNING: WORKER_TOKEN not set — running without auth");
        String::new()
    });

    let port: u16 = std::env::var("WORKER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9100);

    // ── Startup diagnostics ───────────────────────────────
    println!("[Worker] PID: {}, UID: {}", std::process::id(), unsafe {
        libc::getuid()
    });
    println!(
        "[Worker] CWD: {:?}",
        std::env::current_dir().unwrap_or_default()
    );

    // ── Load Soul (for memory skills) ────────────────────
    let soul_dir = std::env::var("SOUL_DIR").unwrap_or_else(|_| "soul".to_string());
    println!("[Worker] SOUL_DIR = {}", soul_dir);

    // Quick write-permission check
    let test_path = Path::new(&soul_dir).join(".write_test");
    match std::fs::write(&test_path, "ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&test_path);
            println!("[Worker] ✓ Soul directory is writable");
        }
        Err(e) => {
            eprintln!("[Worker] ✗ Soul directory is NOT writable: {}", e);
            eprintln!("[Worker]   Ensure the mounted volume has correct ownership (UID 1000).");
        }
    }

    let soul = Arc::new(Soul::load(Path::new(&soul_dir)));

    // ── Load Cron Store ──────────────────────────────────
    let cron_dir = std::env::var("CRON_DIR").unwrap_or_else(|_| "cron".to_string());
    let cron_store = Arc::new(CronStore::load(Path::new(&cron_dir)));

    // ── Register all skills locally ──────────────────────
    let mut skills = SkillRegistry::new();

    // File skills
    skills.add(ReadFileSkill);
    skills.add(WriteFileSkill);
    skills.add(ExtractPdfSkill);
    skills.add(FileCopySkill);
    skills.add(FileStoreSkill);
    skills.add(ListDirectorySkill);

    // Shell execution
    skills.add(ShellExecuteSkill);

    // Web skills
    skills.add(WebSearchSkill::new());
    skills.add(WebFetchSkill::new());
    skills.add(EmailMailboxSkill);
    skills.add(GitHubSkill::new());

    // Memory skills
    skills.add(MemorySaveSkill::new(Arc::clone(&soul.memory)));
    skills.add(MemorySearchSkill::new(Arc::clone(&soul.memory)));
    skills.add(MemoryGetSkill::new(Arc::clone(&soul.memory)));

    // Cron skills
    skills.add(CronAddSkill::new(Arc::clone(&cron_store)));
    skills.add(CronListSkill::new(Arc::clone(&cron_store)));
    skills.add(CronRemoveSkill::new(Arc::clone(&cron_store)));

    // Python skills
    let python_skills_dir =
        std::env::var("PYTHON_SKILLS_DIR").unwrap_or_else(|_| "skills".to_string());
    for skill in load_python_skills(Path::new(&python_skills_dir)).await {
        skills.add(skill);
    }

    let skill_count = skills.skill_names().len();

    // ── Start HTTP server ────────────────────────────────
    let state = WorkerState {
        skills: Arc::new(Mutex::new(skills)),
        token,
        python_skills_dir,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/definitions", get(definitions))
        .route("/execute", post(execute))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!(
        "[Worker] Sandbox worker listening on {} ({} skills registered)",
        addr, skill_count
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
