pub mod default_skills;
pub mod python_skill;

use async_trait::async_trait;
use serde_json::Value;

use crate::reasoning::llm_interface::LlmToolDefinition;

/// Every skill describes itself (tool definition for the LLM)
/// and can be executed with arbitrary JSON arguments.
#[async_trait]
pub trait Skill: Send + Sync {
    /// Returns the tool/function definition (name, description, parameter schema).
    fn definition(&self) -> LlmToolDefinition;

    /// Executes the skill with the given arguments and returns the result as a JSON string.
    async fn execute(&self, arguments: &Value) -> String;
}

// ── Remote Skill Proxy ──────────────────────────────────────

/// Forwards skill execution to a sandbox worker over HTTP.
#[derive(Clone)]
pub struct RemoteSkillProxy {
    client: reqwest::Client,
    base_url: String,
    token: String,
    definitions: std::sync::Arc<std::sync::RwLock<Vec<LlmToolDefinition>>>,
}

impl RemoteSkillProxy {
    /// Connect to the worker, retrying up to 30 times (60 s total).
    /// Fetches and caches the available tool definitions on success.
    pub async fn connect(
        base_url: &str,
        token: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::new();
        let mut last_err = String::from("no attempts");

        for attempt in 1..=30 {
            match client
                .get(format!("{}/definitions", base_url))
                .header("x-worker-token", token)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    #[derive(serde::Deserialize)]
                    struct Def {
                        name: String,
                        description: Option<String>,
                        parameters_schema: Value,
                    }
                    let defs: Vec<Def> = resp.json().await?;
                    let definitions = defs
                        .into_iter()
                        .map(|d| LlmToolDefinition {
                            name: d.name,
                            description: d.description,
                            parameters_schema: d.parameters_schema,
                        })
                        .collect();
                    return Ok(Self {
                        client,
                        base_url: base_url.to_string(),
                        token: token.to_string(),
                        definitions: std::sync::Arc::new(std::sync::RwLock::new(definitions)),
                    });
                }
                Ok(resp) => {
                    last_err = format!("HTTP {}", resp.status());
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            }

            if attempt < 30 {
                println!(
                    "[Sandbox] Worker not ready ({}), retrying in 2 s… ({}/30)",
                    last_err, attempt
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        Err(format!(
            "Could not connect to sandbox worker after 30 attempts: {}",
            last_err
        )
        .into())
    }

    /// Refreshes tool definitions from the worker at runtime.
    /// Keeps the previous cache if the fetch fails.
    pub async fn refresh_definitions(&self) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        struct Def {
            name: String,
            description: Option<String>,
            parameters_schema: Value,
        }

        let response = self
            .client
            .get(format!("{}/definitions", self.base_url))
            .header("x-worker-token", &self.token)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }

        let defs: Vec<Def> = response
            .json()
            .await
            .map_err(|e| format!("invalid definitions payload: {}", e))?;

        let mapped: Vec<LlmToolDefinition> = defs
            .into_iter()
            .map(|d| LlmToolDefinition {
                name: d.name,
                description: d.description,
                parameters_schema: d.parameters_schema,
            })
            .collect();

        match self.definitions.write() {
            Ok(mut lock) => {
                *lock = mapped;
                Ok(())
            }
            Err(_) => Err("definitions lock poisoned".to_string()),
        }
    }

    pub fn tool_definitions(&self) -> Vec<LlmToolDefinition> {
        self.definitions
            .read()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn skill_names(&self) -> Vec<String> {
        self.definitions
            .read()
            .map(|defs| defs.iter().map(|d| d.name.clone()).collect())
            .unwrap_or_default()
    }

    pub async fn execute(&self, name: &str, arguments: &Value) -> Option<String> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            name: &'a str,
            arguments: &'a Value,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            ok: bool,
            result: Option<String>,
            error: Option<String>,
        }

        let resp = self
            .client
            .post(format!("{}/execute", self.base_url))
            .header("x-worker-token", &self.token)
            .json(&Req { name, arguments })
            .send()
            .await
            .ok()?;

        let body: Resp = resp.json().await.ok()?;
        if body.ok {
            body.result
        } else {
            Some(
                serde_json::json!({
                    "ok": false,
                    "error": body.error.unwrap_or_else(|| "Unknown worker error".to_string()),
                })
                .to_string(),
            )
        }
    }
}

// ── Skill Registry ──────────────────────────────────────────

/// Registry where skills can be registered via `.add(MySkill)`.
/// Automatically provides tool definitions for the LLM and dispatches tool calls.
///
/// Supports two modes:
/// - **Local**: skills execute in-process (default, also used by the worker)
/// - **Hybrid**: local skills + remote proxy to a sandbox worker
pub struct SkillRegistry {
    skills: Vec<Box<dyn Skill>>,
    remote: Option<RemoteSkillProxy>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
            remote: None,
        }
    }

    /// Create a registry that delegates unknown skills to a remote worker.
    /// Local skills (added via `add()`) take priority.
    pub fn new_with_remote(remote: RemoteSkillProxy) -> Self {
        Self {
            skills: Vec::new(),
            remote: Some(remote),
        }
    }

    /// Returns the remote proxy, if configured.
    pub fn remote_proxy(&self) -> Option<&RemoteSkillProxy> {
        self.remote.as_ref()
    }

    /// Register a skill — builder pattern, returns `&mut Self`.
    pub fn add<S: Skill + 'static>(&mut self, skill: S) -> &mut Self {
        self.skills.push(Box::new(skill));
        self
    }

    /// All registered skills as LLM tool definitions.
    pub fn tool_definitions(&self) -> Vec<LlmToolDefinition> {
        let mut defs: Vec<_> = self.skills.iter().map(|s| s.definition()).collect();
        if let Some(ref remote) = self.remote {
            defs.extend(remote.tool_definitions());
        }
        defs
    }

    /// Refreshes remote tool definitions if a remote worker is configured.
    pub async fn refresh_remote_definitions(&self) {
        if let Some(ref remote) = self.remote {
            if let Err(err) = remote.refresh_definitions().await {
                eprintln!("[Sandbox] Failed to refresh tool definitions: {}", err);
            }
        }
    }

    /// Returns the names of all registered skills.
    pub fn skill_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .skills
            .iter()
            .map(|s| s.definition().name.clone())
            .collect();
        if let Some(ref remote) = self.remote {
            names.extend(remote.skill_names());
        }
        names
    }

    /// Executes a tool call by name.
    /// Returns `None` if no skill with that name is registered.
    /// Local skills are checked first, then the remote worker.
    pub async fn execute(&self, name: &str, arguments: &Value) -> Option<String> {
        // Try local skills first
        for skill in &self.skills {
            if skill.definition().name == name {
                return Some(skill.execute(arguments).await);
            }
        }
        // Fall through to remote worker
        if let Some(ref remote) = self.remote {
            return remote.execute(name, arguments).await;
        }
        None
    }
}
