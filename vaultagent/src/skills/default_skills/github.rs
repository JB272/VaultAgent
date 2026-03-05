use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;
use uuid::Uuid;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct GitHubSkill {
    client: Client,
    worker_url: String,
    worker_token: String,
}

impl GitHubSkill {
    pub fn new(worker_url: String, worker_token: String) -> Self {
        Self {
            client: Client::builder()
                .user_agent("VaultAgent/1.0")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            worker_url: worker_url.trim_end_matches('/').to_string(),
            worker_token,
        }
    }

    fn get_env(&self) -> Result<GithubEnv, String> {
        let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
        if token.trim().is_empty() {
            return Err("Missing GITHUB_TOKEN in .env.secure".to_string());
        }

        let api_base = std::env::var("GITHUB_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.github.com".to_string());

        Ok(GithubEnv {
            token,
            api_base: api_base.trim_end_matches('/').to_string(),
        })
    }

    fn owner_repo_from_args(&self, arguments: &Value) -> Result<(String, String), String> {
        let owner = arguments
            .get("owner")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        let repo = arguments
            .get("repo")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        if owner.is_empty() || repo.is_empty() {
            return Err("owner and repo are required for this action.".to_string());
        }

        Ok((owner, repo))
    }

    fn auth_request(
        &self,
        env: &GithubEnv,
        method: reqwest::Method,
        url: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", env.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn send_json(&self, req: reqwest::RequestBuilder) -> String {
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<Value>().await {
                    Ok(body) => json!({
                        "ok": status.is_success(),
                        "status": status.as_u16(),
                        "data": body,
                    })
                    .to_string(),
                    Err(err) => json!({
                        "ok": false,
                        "status": status.as_u16(),
                        "error": format!("Failed to parse GitHub response: {}", err),
                    })
                    .to_string(),
                }
            }
            Err(err) => json!({
                "ok": false,
                "error": format!("GitHub request failed: {}", err),
            })
            .to_string(),
        }
    }

    async fn run_git_command(
        &self,
        extra_header: &str,
        args: &[String],
    ) -> Result<(i32, String, String), String> {
        let mut cmd = Command::new("git");
        cmd.arg("-c").arg(format!(
            "http.https://github.com/.extraheader={}",
            extra_header
        ));
        cmd.args(args);

        let output =
            match tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await {
                Ok(Ok(out)) => out,
                Ok(Err(err)) => return Err(format!("Failed to run git: {}", err)),
                Err(_) => return Err("git command timed out after 120 seconds".to_string()),
            };

        let code = output.status.code().unwrap_or(-1);
        let stdout = truncate(&String::from_utf8_lossy(&output.stdout), 8000);
        let stderr = truncate(&String::from_utf8_lossy(&output.stderr), 4000);

        Ok((code, stdout, stderr))
    }

    async fn run_git_with_fallback(
        &self,
        token: &str,
        args: &[String],
    ) -> Result<(i32, String, String, String), String> {
        let strategies = vec![
            GitAuthConfig {
                label: "basic(x-access-token)".to_string(),
                extra_header: basic_auth_header("x-access-token", token),
            },
            GitAuthConfig {
                label: "basic(git)".to_string(),
                extra_header: basic_auth_header("git", token),
            },
            GitAuthConfig {
                label: "bearer".to_string(),
                extra_header: format!("AUTHORIZATION: bearer {}", token),
            },
        ];

        let mut failures = Vec::new();

        for strategy in strategies {
            match self.run_git_command(&strategy.extra_header, args).await {
                Ok((exit_code, stdout, stderr)) => {
                    if exit_code == 0 {
                        return Ok((exit_code, stdout, stderr, strategy.label));
                    }

                    failures.push(format!(
                        "{} -> exit {}: {}",
                        strategy.label,
                        exit_code,
                        first_non_empty_line(&stderr).unwrap_or("unknown git error")
                    ));
                }
                Err(err) => failures.push(format!("{} -> {}", strategy.label, err)),
            }
        }

        Err(format!(
            "git authentication failed for all strategies. {}",
            failures.join(" | ")
        ))
    }

    async fn worker_execute_skill(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let mut req = self
            .client
            .post(format!("{}/execute", self.worker_url))
            .json(&json!({ "name": name, "arguments": arguments }));

        if !self.worker_token.is_empty() {
            req = req.header("x-worker-token", &self.worker_token);
        }

        let response = req
            .send()
            .await
            .map_err(|e| format!("worker /execute request failed: {}", e))?;

        let payload: WorkerExecuteResponse = response
            .json()
            .await
            .map_err(|e| format!("worker /execute invalid response: {}", e))?;

        if !payload.ok {
            return Err(payload
                .error
                .unwrap_or_else(|| "worker execute returned ok=false".to_string()));
        }

        let result_raw = payload.result.unwrap_or_else(|| "{}".to_string());
        match serde_json::from_str::<Value>(&result_raw) {
            Ok(v) => Ok(v),
            Err(_) => Ok(json!({ "ok": true, "output": result_raw })),
        }
    }

    async fn worker_workspace_write(
        &self,
        path: &str,
        content_base64: &str,
    ) -> Result<usize, String> {
        let mut req = self
            .client
            .post(format!("{}/workspace/write", self.worker_url))
            .json(&json!({
                "path": path,
                "content_base64": content_base64,
            }));

        if !self.worker_token.is_empty() {
            req = req.header("x-worker-token", &self.worker_token);
        }

        let response = req
            .send()
            .await
            .map_err(|e| format!("worker /workspace/write request failed: {}", e))?;

        let payload: WorkspaceWriteResponse = response
            .json()
            .await
            .map_err(|e| format!("worker /workspace/write invalid response: {}", e))?;

        if !payload.ok {
            return Err(payload
                .error
                .unwrap_or_else(|| "workspace write returned ok=false".to_string()));
        }

        Ok(payload.bytes_written.unwrap_or(0))
    }

    async fn clone_repo_inner(
        &self,
        arguments: &Value,
        env: &GithubEnv,
        temp_root: &Path,
    ) -> Result<Value, String> {
        let (owner, repo) = self.owner_repo_from_args(arguments)?;

        let default_destination = format!("repos/{}-{}", owner, repo);
        let destination_input = arguments
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or(default_destination.as_str());

        let destination_rel = sanitize_workspace_relative_path(destination_input)?;
        let destination_abs = format!("/workspace/{}", destination_rel);

        let repo_url = arguments
            .get("clone_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("https://github.com/{}/{}.git", owner, repo))
            .trim_end_matches('/')
            .to_string();

        let git_ref = arguments
            .get("git_ref")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned);

        let update_if_exists = arguments
            .get("update_if_exists")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if update_if_exists {
            let cleanup_cmd = format!("rm -rf {}", shell_single_quote(&destination_abs));
            let cleanup_result = self
                .worker_execute_skill("shell_execute", json!({ "command": cleanup_cmd }))
                .await?;
            ensure_tool_ok(&cleanup_result)?;
        } else {
            let check_cmd = format!(
                "if [ -e {} ]; then echo EXISTS; else echo MISSING; fi",
                shell_single_quote(&destination_abs)
            );
            let check_result = self
                .worker_execute_skill("shell_execute", json!({ "command": check_cmd }))
                .await?;
            ensure_tool_ok(&check_result)?;
            let stdout = check_result
                .get("stdout")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if stdout.contains("EXISTS") {
                return Err(format!(
                    "Destination already exists in docker workspace: {}",
                    destination_abs
                ));
            }
        }

        let clone_dir = temp_root.join("repo");
        let mut clone_args = vec!["clone".to_string()];
        if let Some(r) = &git_ref {
            clone_args.push("--branch".to_string());
            clone_args.push(r.clone());
            clone_args.push("--single-branch".to_string());
        }
        clone_args.push(repo_url.clone());
        clone_args.push(clone_dir.to_string_lossy().to_string());

        let (exit_code, clone_stdout, clone_stderr, auth_strategy) =
            self.run_git_with_fallback(&env.token, &clone_args).await?;
        if exit_code != 0 {
            return Err(format!(
                "git clone failed (exit {}): {}",
                exit_code,
                first_non_empty_line(&clone_stderr).unwrap_or("unknown git clone error")
            ));
        }

        let files = collect_repo_files(&clone_dir)?;
        let mut uploaded_files = 0usize;
        let mut uploaded_bytes = 0usize;

        for (file_path, rel_path) in files {
            let bytes = fs::read(&file_path)
                .map_err(|e| format!("Failed to read cloned file '{}': {}", rel_path, e))?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let target_path = format!("{}/{}", destination_rel, rel_path);

            let written = self.worker_workspace_write(&target_path, &b64).await?;
            uploaded_files += 1;
            uploaded_bytes += written;
        }

        Ok(json!({
            "ok": true,
            "action": "clone_repo",
            "path": destination_abs,
            "repo_url": repo_url,
            "auth_strategy": auth_strategy,
            "uploaded_files": uploaded_files,
            "uploaded_bytes": uploaded_bytes,
            "clone_stdout": clone_stdout,
            "clone_stderr": clone_stderr,
            "stored_in": "docker_workspace",
        }))
    }

    async fn clone_repo(&self, arguments: &Value, env: &GithubEnv) -> String {
        let temp_root = std::env::temp_dir().join(format!("vaultagent-github-{}", Uuid::new_v4()));
        if let Err(err) = fs::create_dir_all(&temp_root) {
            return json!({
                "ok": false,
                "error": format!("Failed to create temp directory: {}", err),
            })
            .to_string();
        }

        let result = self.clone_repo_inner(arguments, env, &temp_root).await;
        let _ = fs::remove_dir_all(&temp_root);

        match result {
            Ok(v) => v.to_string(),
            Err(err) => json!({
                "ok": false,
                "error": err,
                "hint": "Check token repo access (Contents: Read at least) and owner/repo name.",
            })
            .to_string(),
        }
    }
}

struct GithubEnv {
    token: String,
    api_base: String,
}

struct GitAuthConfig {
    label: String,
    extra_header: String,
}

#[derive(Deserialize)]
struct WorkerExecuteResponse {
    ok: bool,
    result: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct WorkspaceWriteResponse {
    ok: bool,
    bytes_written: Option<usize>,
    error: Option<String>,
}

#[async_trait]
impl Skill for GitHubSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "github".to_string(),
            description: Some(
                "Interact with GitHub using host-side secrets. Supports REST API actions and cloning repos into Docker /workspace via worker file API."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "list_repos",
                            "list_issues",
                            "get_issue",
                            "create_issue",
                            "add_issue_comment",
                            "list_pull_requests",
                            "get_pull_request",
                            "clone_repo"
                        ],
                        "description": "GitHub action to run."
                    },
                    "owner": {
                        "type": "string",
                        "description": "Repository owner/org (required for repo-specific actions)."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Repository name (required for repo-specific actions)."
                    },
                    "issue_number": {
                        "type": "integer",
                        "description": "Issue number for get_issue or add_issue_comment."
                    },
                    "pull_number": {
                        "type": "integer",
                        "description": "Pull request number for get_pull_request."
                    },
                    "title": {
                        "type": "string",
                        "description": "Title for create_issue."
                    },
                    "body": {
                        "type": "string",
                        "description": "Body for create_issue or add_issue_comment."
                    },
                    "state": {
                        "type": "string",
                        "enum": ["open", "closed", "all"],
                        "description": "State filter for list_issues/list_pull_requests. Default: open."
                    },
                    "per_page": {
                        "type": "integer",
                        "description": "Max items per page (1-100). Default: 20."
                    },
                    "page": {
                        "type": "integer",
                        "description": "Page number (>=1). Default: 1."
                    },
                    "destination": {
                        "type": "string",
                        "description": "For clone_repo: relative path in Docker /workspace, e.g. repos/my-repo."
                    },
                    "git_ref": {
                        "type": "string",
                        "description": "For clone_repo: optional branch/tag/ref."
                    },
                    "clone_url": {
                        "type": "string",
                        "description": "For clone_repo: optional full git clone URL. Default: https://github.com/<owner>/<repo>.git"
                    },
                    "update_if_exists": {
                        "type": "boolean",
                        "description": "For clone_repo: if destination exists, clear it first. Default: true."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let env = match self.get_env() {
            Ok(v) => v,
            Err(err) => return json!({ "ok": false, "error": err }).to_string(),
        };

        let action = match arguments.get("action").and_then(Value::as_str) {
            Some(v) => v,
            None => return json!({ "ok": false, "error": "action is required" }).to_string(),
        };

        if action == "clone_repo" {
            return self.clone_repo(arguments, &env).await;
        }

        let per_page = arguments
            .get("per_page")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 100);
        let page = arguments
            .get("page")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let state = arguments
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("open");

        match action {
            "list_repos" => {
                let url = format!(
                    "{}/user/repos?sort=updated&per_page={}&page={}",
                    env.api_base, per_page, page
                );
                let req = self.auth_request(&env, reqwest::Method::GET, &url);
                self.send_json(req).await
            }
            "list_issues" => {
                let (owner, repo) = match self.owner_repo_from_args(arguments) {
                    Ok(v) => v,
                    Err(err) => return json!({ "ok": false, "error": err }).to_string(),
                };

                let url = format!(
                    "{}/repos/{}/{}/issues?state={}&per_page={}&page={}",
                    env.api_base, owner, repo, state, per_page, page
                );
                let req = self.auth_request(&env, reqwest::Method::GET, &url);
                self.send_json(req).await
            }
            "get_issue" => {
                let (owner, repo) = match self.owner_repo_from_args(arguments) {
                    Ok(v) => v,
                    Err(err) => return json!({ "ok": false, "error": err }).to_string(),
                };
                let issue_number = match arguments.get("issue_number").and_then(Value::as_u64) {
                    Some(v) => v,
                    None => {
                        return json!({ "ok": false, "error": "issue_number is required" })
                            .to_string();
                    }
                };

                let url = format!(
                    "{}/repos/{}/{}/issues/{}",
                    env.api_base, owner, repo, issue_number
                );
                let req = self.auth_request(&env, reqwest::Method::GET, &url);
                self.send_json(req).await
            }
            "create_issue" => {
                let (owner, repo) = match self.owner_repo_from_args(arguments) {
                    Ok(v) => v,
                    Err(err) => return json!({ "ok": false, "error": err }).to_string(),
                };
                let title = match arguments.get("title").and_then(Value::as_str) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => return json!({ "ok": false, "error": "title is required" }).to_string(),
                };
                let body = arguments.get("body").and_then(Value::as_str).unwrap_or("");

                let url = format!("{}/repos/{}/{}/issues", env.api_base, owner, repo);
                let req = self
                    .auth_request(&env, reqwest::Method::POST, &url)
                    .json(&json!({ "title": title, "body": body }));
                self.send_json(req).await
            }
            "add_issue_comment" => {
                let (owner, repo) = match self.owner_repo_from_args(arguments) {
                    Ok(v) => v,
                    Err(err) => return json!({ "ok": false, "error": err }).to_string(),
                };
                let issue_number = match arguments.get("issue_number").and_then(Value::as_u64) {
                    Some(v) => v,
                    None => {
                        return json!({ "ok": false, "error": "issue_number is required" })
                            .to_string();
                    }
                };
                let body = match arguments.get("body").and_then(Value::as_str) {
                    Some(v) if !v.trim().is_empty() => v,
                    _ => return json!({ "ok": false, "error": "body is required" }).to_string(),
                };

                let url = format!(
                    "{}/repos/{}/{}/issues/{}/comments",
                    env.api_base, owner, repo, issue_number
                );
                let req = self
                    .auth_request(&env, reqwest::Method::POST, &url)
                    .json(&json!({ "body": body }));
                self.send_json(req).await
            }
            "list_pull_requests" => {
                let (owner, repo) = match self.owner_repo_from_args(arguments) {
                    Ok(v) => v,
                    Err(err) => return json!({ "ok": false, "error": err }).to_string(),
                };

                let url = format!(
                    "{}/repos/{}/{}/pulls?state={}&per_page={}&page={}",
                    env.api_base, owner, repo, state, per_page, page
                );
                let req = self.auth_request(&env, reqwest::Method::GET, &url);
                self.send_json(req).await
            }
            "get_pull_request" => {
                let (owner, repo) = match self.owner_repo_from_args(arguments) {
                    Ok(v) => v,
                    Err(err) => return json!({ "ok": false, "error": err }).to_string(),
                };
                let pull_number = match arguments.get("pull_number").and_then(Value::as_u64) {
                    Some(v) => v,
                    None => {
                        return json!({ "ok": false, "error": "pull_number is required" })
                            .to_string();
                    }
                };

                let url = format!(
                    "{}/repos/{}/{}/pulls/{}",
                    env.api_base, owner, repo, pull_number
                );
                let req = self.auth_request(&env, reqwest::Method::GET, &url);
                self.send_json(req).await
            }
            _ => json!({ "ok": false, "error": format!("Unsupported action: {}", action) })
                .to_string(),
        }
    }
}

fn sanitize_workspace_relative_path(path: &str) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("destination must not be empty".to_string());
    }

    let candidate = Path::new(path.trim());
    if candidate.is_absolute() {
        return Err("destination must be a relative path inside /workspace".to_string());
    }

    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("destination contains forbidden path segments".to_string());
    }

    let normalized = path.trim().trim_start_matches("./").to_string();
    if normalized == "." {
        return Err("destination must not be '.'".to_string());
    }

    Ok(normalized)
}

fn collect_repo_files(base: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    if !base.exists() {
        return Err(format!(
            "Cloned repository path not found: {}",
            base.display()
        ));
    }

    let mut files = Vec::new();
    let mut stack = vec![base.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|e| format!("Failed to read directory '{}': {}", dir.display(), e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .map_err(|e| format!("Failed to compute relative path: {}", e))?;

            let is_git_internal = rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .map(|s| s == ".git")
                .unwrap_or(false);
            if is_git_internal {
                continue;
            }

            if path.is_dir() {
                stack.push(path);
                continue;
            }

            if path.is_file() {
                let rel_string = rel.to_string_lossy().replace('\\', "/");
                files.push((path, rel_string));
            }
        }
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

fn ensure_tool_ok(result: &Value) -> Result<(), String> {
    if result.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(());
    }

    let err = result
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("Tool returned ok=false");
    let stderr = result
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if stderr.trim().is_empty() {
        Err(err.to_string())
    } else {
        Err(format!(
            "{} | stderr: {}",
            err,
            first_non_empty_line(stderr).unwrap_or(stderr)
        ))
    }
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('"', "\\\"").replace('\'', "'\\''"))
}

fn basic_auth_header(username: &str, token: &str) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"));
    format!("AUTHORIZATION: basic {}", basic)
}

fn first_non_empty_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|line| !line.is_empty())
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...\\n[truncated, {} bytes]", &s[..max_len], s.len())
    }
}
