use std::{
    env,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use serde::Deserialize;
use url::Url;
use vaultagent_audit::{redact, AuditWriter, Summary};
use vaultagent_policy::PolicyConfig;
use vaultagent_providers::{ChatRequest, OpenAiCompatProvider, Provider};
use vaultagent_tools::{execute_tool, ToolCall};

#[derive(Debug, Deserialize)]
pub struct RunnerConfig {
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_output_tokens: u32,
    pub max_total_tokens: u32,
    pub max_retries_per_step: u32,
    pub step_timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
pub struct SecurityConfig {
    pub workspace_root: PathBuf,
    pub allow_network: bool,
    pub allow_shell: bool,
    pub max_file_size_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct NetworkConfig {
    pub allowlist_domains: Vec<String>,
    pub timeout_seconds: u64,
    pub max_response_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct RoutingConfig {
    pub default_model: String,
    pub upgrade_model: String,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: String,
}

#[derive(Debug, Deserialize)]
pub struct AuditConfig {
    pub runs_dir: PathBuf,
    pub redact_secrets: bool,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub runner: RunnerConfig,
    pub security: SecurityConfig,
    pub network: NetworkConfig,
    pub routing: RoutingConfig,
    pub provider: ProviderConfig,
    pub audit: AuditConfig,
}

pub struct RunRequest {
    pub task: String,
    pub enabled_tools: Vec<String>,
}

pub struct RunResult {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub final_text: String,
}

pub fn health() -> &'static str {
    "core-ok"
}

pub fn load_config(path: &PathBuf) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config: {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).context("invalid config TOML")?;
    Ok(cfg)
}

pub fn run(req: RunRequest, cfg: &Config) -> anyhow::Result<RunResult> {
    let run_id = format!("run-{}", now_ms());
    let mut audit = AuditWriter::new(&cfg.audit.runs_dir, &run_id)?;
    let mut summary = Summary {
        final_status: "ok".to_string(),
        ..Summary::default()
    };

    audit.event(
        "routing_decision",
        &serde_json::json!({
            "selected_model": cfg.routing.default_model,
            "reason": "default"
        }),
    )?;

    let task_text = if cfg.audit.redact_secrets {
        redact(&req.task)
    } else {
        req.task.clone()
    };
    audit.event("model_request", &serde_json::json!({ "task": task_text }))?;

    let lower = req.task.to_lowercase();
    let maybe_tool = if let Some(rest) = lower.strip_prefix("read file ") {
        Some(rest.trim().to_string())
    } else {
        None
    };

    let final_text = if let Some(path) = maybe_tool {
        summary.steps += 1;
        if summary.tool_calls >= cfg.runner.max_tool_calls {
            summary.errors += 1;
            summary.final_status = "error_budget_tool_calls".into();
            anyhow::bail!("max_tool_calls exceeded")
        }

        let policy_cfg = PolicyConfig {
            workspace_root: cfg.security.workspace_root.clone(),
            allow_network: cfg.security.allow_network,
            allow_shell: cfg.security.allow_shell,
            max_file_size_bytes: cfg.security.max_file_size_bytes,
        };

        let call = ToolCall {
            name: "fs.read_file".into(),
            input: serde_json::json!({ "path": path }),
        };

        audit.event("tool_call", &call)?;
        match execute_tool(&call, &req.enabled_tools, &policy_cfg) {
            Ok(result) => {
                summary.tool_calls += 1;
                audit.event("tool_result", &result)?;
                result
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            }
            Err(err) => {
                summary.policy_violations += 1;
                summary.errors += 1;
                summary.final_status = "error_policy".into();
                audit.event(
                    "policy_violation",
                    &serde_json::json!({ "error": err.to_string() }),
                )?;
                audit.write_summary(&summary)?;
                return Err(err);
            }
        }
    } else {
        // OpenAI-compatible provider flow
        if !cfg.security.allow_network {
            summary.policy_violations += 1;
            summary.errors += 1;
            summary.final_status = "error_policy_network_disabled".into();
            audit.event(
                "policy_violation",
                &serde_json::json!({ "error": "network is disabled by policy" }),
            )?;
            audit.write_summary(&summary)?;
            anyhow::bail!("network is disabled by policy")
        }

        enforce_allowlist(&cfg.provider.base_url, &cfg.network.allowlist_domains)?;

        let api_key = env::var(&cfg.provider.api_key_env)
            .with_context(|| format!("missing API key env: {}", cfg.provider.api_key_env))?;

        let provider = OpenAiCompatProvider {
            base_url: cfg.provider.base_url.clone(),
            api_key,
        };

        summary.steps += 1;
        let provider_response = provider.chat_complete(ChatRequest {
            model: cfg.routing.default_model.clone(),
            prompt: req.task.clone(),
            max_output_tokens: cfg.runner.max_output_tokens,
        })?;

        let response_text = if cfg.audit.redact_secrets {
            redact(&provider_response.content)
        } else {
            provider_response.content
        };

        audit.event(
            "model_response",
            &serde_json::json!({ "content": response_text.clone() }),
        )?;

        response_text
    };

    let estimated_tokens = ((req.task.len() + final_text.len()) / 4) as u32;
    if estimated_tokens > cfg.runner.max_total_tokens {
        summary.errors += 1;
        summary.final_status = "error_budget_tokens".into();
        audit.event(
            "policy_violation",
            &serde_json::json!({
                "error": "max_total_tokens exceeded",
                "estimated_tokens": estimated_tokens
            }),
        )?;
        audit.write_summary(&summary)?;
        anyhow::bail!("max_total_tokens exceeded")
    }

    audit.event(
        "final",
        &serde_json::json!({
            "result": redact(&final_text),
            "estimated_tokens": estimated_tokens
        }),
    )?;
    audit.write_summary(&summary)?;

    Ok(RunResult {
        run_id,
        run_dir: audit.run_dir().to_path_buf(),
        final_text,
    })
}

fn enforce_allowlist(base_url: &str, allowlist: &[String]) -> anyhow::Result<()> {
    let host = Url::parse(base_url)
        .context("invalid provider.base_url")?
        .host_str()
        .map(|s| s.to_string())
        .context("provider.base_url host missing")?;

    if allowlist.is_empty() {
        anyhow::bail!("network allowlist is empty")
    }

    if allowlist.iter().any(|d| d == &host) {
        Ok(())
    } else {
        anyhow::bail!("provider host '{host}' is not in allowlist")
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
