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

    fn owner_repo_and_url_for_clone(
        &self,
        arguments: &Value,
    ) -> Result<(String, String, String), String> {
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

        let clone_url = arguments
            .get("clone_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned);

        if !owner.is_empty() && !repo.is_empty() {
            let raw =
                clone_url.unwrap_or_else(|| format!("https://github.com/{}/{}.git", owner, repo));
            let repo_url = canonicalize_clone_url(&raw)
                .trim_end_matches('/')
                .to_string();

            if let Some((parsed_owner, parsed_repo)) = parse_owner_repo_from_clone_url(&repo_url) {
                if !parsed_owner.eq_ignore_ascii_case(&owner)
                    || !parsed_repo.eq_ignore_ascii_case(&repo)
                {
                    return Err(format!(
                        "clone_url points to '{}/{}' but owner/repo is '{}/{}'.",
                        parsed_owner, parsed_repo, owner, repo
                    ));
                }
            }

            return Ok((owner, repo, repo_url));
        }

        if let Some(repo_url) = clone_url {
            let canonical = canonicalize_clone_url(&repo_url);
            if let Some((parsed_owner, parsed_repo)) = parse_owner_repo_from_clone_url(&canonical) {
                return Ok((
                    parsed_owner,
                    parsed_repo,
                    canonical.trim_end_matches('/').to_string(),
                ));
            }
            return Err(
                "Could not parse owner/repo from clone_url. Provide owner+repo explicitly."
                    .to_string(),
            );
        }

        Err("For clone_repo you must provide either owner+repo or clone_url.".to_string())
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

    async fn diagnose_repo_access(&self, env: &GithubEnv, owner: &str, repo: &str) -> String {
        let url = format!("{}/repos/{}/{}", env.api_base, owner, repo);
        let req = self.auth_request(env, reqwest::Method::GET, &url);

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let scopes = resp
                    .headers()
                    .get("x-oauth-scopes")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .trim()
                    .to_string();

                if status.is_success() {
                    if scopes.is_empty() {
                        return "GitHub API preflight succeeded: token can access this repo."
                            .to_string();
                    }
                    return format!(
                        "GitHub API preflight succeeded: token can access this repo. Scopes: {}",
                        scopes
                    );
                }

                let body = resp.text().await.unwrap_or_default();
                let message = extract_github_error_message(&body)
                    .or_else(|| first_non_empty_line(&body).map(|s| s.to_string()))
                    .unwrap_or_else(|| "no details".to_string());

                let status_hint = match status.as_u16() {
                    401 => "token invalid or expired",
                    403 => "token lacks required permissions or org SSO authorization is missing",
                    404 => "repo does not exist or token has no access to private repo",
                    _ => "unexpected API status",
                };

                if scopes.is_empty() {
                    format!(
                        "GitHub API preflight failed with {} ({}): {}",
                        status.as_u16(),
                        status_hint,
                        message
                    )
                } else {
                    format!(
                        "GitHub API preflight failed with {} ({}): {} | Scopes: {}",
                        status.as_u16(),
                        status_hint,
                        message,
                        scopes
                    )
                }
            }
            Err(err) => format!("GitHub API preflight request failed: {}", err),
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

    async fn run_git_plain_command(
        &self,
        args: &[String],
    ) -> Result<(i32, String, String), String> {
        let mut cmd = Command::new("git");
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

    async fn resolve_git_username(&self, env: &GithubEnv) -> Option<String> {
        let url = format!("{}/user", env.api_base);
        let req = self.auth_request(env, reqwest::Method::GET, &url);

        let response = req.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }

        let user = response.json::<GithubUserResponse>().await.ok()?;
        let login = user.login.trim();
        if login.is_empty() {
            None
        } else {
            Some(login.to_string())
        }
    }

    async fn run_git_with_fallback(
        &self,
        token: &str,
        username_hint: Option<&str>,
        args: &[String],
    ) -> Result<(i32, String, String, String), String> {
        let mut strategies = Vec::new();
        if let Some(username) = username_hint {
            let u = username.trim();
            if !u.is_empty() {
                strategies.push(GitAuthConfig {
                    label: format!("basic({})", u),
                    extra_header: basic_auth_header(u, token),
                });
            }
        }
        strategies.extend([
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
        ]);

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
                        best_error_line(&stderr).unwrap_or("unknown git error")
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

    async fn run_git_clone_with_url_auth(
        &self,
        repo_url: &str,
        git_ref: Option<&str>,
        clone_dir: &Path,
        token: &str,
        username_hint: Option<&str>,
    ) -> Result<(i32, String, String, String), String> {
        let mut clone_urls: Vec<(String, String)> = Vec::new();

        if let Some(username) = username_hint {
            let u = username.trim();
            if !u.is_empty() {
                if let Ok(url) = url_with_basic_auth(repo_url, u, token) {
                    clone_urls.push((format!("url-basic({})", u), url));
                }
            }
        }

        if let Ok(url) = url_with_basic_auth(repo_url, "x-access-token", token) {
            clone_urls.push(("url-basic(x-access-token)".to_string(), url));
        }

        if let Ok(url) = url_with_basic_auth(repo_url, "git", token) {
            clone_urls.push(("url-basic(git)".to_string(), url));
        }

        let mut failures = Vec::new();
        let clone_dir_str = clone_dir.to_string_lossy().to_string();

        for (label, auth_url) in clone_urls {
            let _ = fs::remove_dir_all(clone_dir);

            let mut args = vec!["clone".to_string()];
            if let Some(r) = git_ref {
                args.push("--branch".to_string());
                args.push(r.to_string());
                args.push("--single-branch".to_string());
            }
            args.push(auth_url);
            args.push(clone_dir_str.clone());

            match self.run_git_plain_command(&args).await {
                Ok((exit_code, stdout, stderr)) => {
                    let safe_stdout = redact_secret(&stdout, token);
                    let safe_stderr = redact_secret(&stderr, token);

                    if exit_code == 0 {
                        let _ = self
                            .run_git_plain_command(&[
                                "-C".to_string(),
                                clone_dir_str.clone(),
                                "remote".to_string(),
                                "set-url".to_string(),
                                "origin".to_string(),
                                repo_url.to_string(),
                            ])
                            .await;

                        return Ok((exit_code, safe_stdout, safe_stderr, label));
                    }

                    failures.push(format!(
                        "{} -> exit {}: {}",
                        label,
                        exit_code,
                        best_error_line(&safe_stderr).unwrap_or("unknown git error")
                    ));
                }
                Err(err) => failures.push(format!("{} -> {}", label, err)),
            }
        }

        Err(format!(
            "git clone with URL auth failed for all strategies. {}",
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

        let status = response.status();
        let body = response.bytes().await.map_err(|e| {
            format!(
                "worker /workspace/write failed to read response body: {}",
                e
            )
        })?;
        let body_text = String::from_utf8_lossy(&body).to_string();

        if !status.is_success() {
            return Err(format!(
                "worker /workspace/write HTTP {}: {}",
                status.as_u16(),
                truncate(&body_text, 500)
            ));
        }

        let payload: WorkspaceWriteResponse = serde_json::from_slice(&body).map_err(|e| {
            format!(
                "worker /workspace/write invalid JSON (HTTP {}): {} | body: {}",
                status.as_u16(),
                e,
                truncate(&body_text, 500)
            )
        })?;

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
        let (owner, repo, repo_url) = self.owner_repo_and_url_for_clone(arguments)?;

        let default_destination = format!("repos/{}-{}", owner, repo);
        let destination_input = arguments
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or(default_destination.as_str());

        let destination_rel = sanitize_workspace_relative_path(destination_input)?;
        let destination_abs = format!("/workspace/{}", destination_rel);

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

        if !update_if_exists {
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
        let git_username = self.resolve_git_username(env).await;
        let clone_dir_str = clone_dir.to_string_lossy().to_string();
        let mut plain_clone_args = vec!["clone".to_string()];
        if let Some(r) = &git_ref {
            plain_clone_args.push("--branch".to_string());
            plain_clone_args.push(r.clone());
            plain_clone_args.push("--single-branch".to_string());
        }
        plain_clone_args.push(repo_url.clone());
        plain_clone_args.push(clone_dir_str.clone());

        let (exit_code, clone_stdout, clone_stderr, auth_strategy) = match self
            .run_git_plain_command(&plain_clone_args)
            .await
        {
            Ok((exit_code, stdout, stderr)) if exit_code == 0 => {
                (exit_code, stdout, stderr, "none".to_string())
            }
            Ok((exit_code, _stdout, stderr)) => {
                let no_auth_failure = format!(
                    "exit {}: {}",
                    exit_code,
                    best_error_line(&stderr).unwrap_or("unknown git error")
                );
                match self
                    .run_git_clone_with_url_auth(
                        &repo_url,
                        git_ref.as_deref(),
                        &clone_dir,
                        &env.token,
                        git_username.as_deref(),
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(url_auth_err) => {
                        match self
                            .run_git_with_fallback(
                                &env.token,
                                git_username.as_deref(),
                                &plain_clone_args,
                            )
                            .await
                        {
                            Ok(v) => v,
                            Err(header_err) => {
                                let api_diagnosis =
                                    self.diagnose_repo_access(env, &owner, &repo).await;
                                return Err(format!(
                                    "git clone failed. no-auth: {} | url auth: {} | header auth: {} | {}",
                                    no_auth_failure, url_auth_err, header_err, api_diagnosis
                                ));
                            }
                        }
                    }
                }
            }
            Err(err) => {
                let no_auth_failure = err;
                match self
                    .run_git_clone_with_url_auth(
                        &repo_url,
                        git_ref.as_deref(),
                        &clone_dir,
                        &env.token,
                        git_username.as_deref(),
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(url_auth_err) => {
                        match self
                            .run_git_with_fallback(
                                &env.token,
                                git_username.as_deref(),
                                &plain_clone_args,
                            )
                            .await
                        {
                            Ok(v) => v,
                            Err(header_err) => {
                                let api_diagnosis =
                                    self.diagnose_repo_access(env, &owner, &repo).await;
                                return Err(format!(
                                    "git clone failed. no-auth: {} | url auth: {} | header auth: {} | {}",
                                    no_auth_failure, url_auth_err, header_err, api_diagnosis
                                ));
                            }
                        }
                    }
                }
            }
        };

        if exit_code != 0 {
            return Err(format!(
                "git clone failed (exit {}): {}",
                exit_code,
                best_error_line(&clone_stderr).unwrap_or("unknown git clone error")
            ));
        }

        if update_if_exists {
            let cleanup_cmd = format!("rm -rf {}", shell_single_quote(&destination_abs));
            let cleanup_result = self
                .worker_execute_skill("shell_execute", json!({ "command": cleanup_cmd }))
                .await?;
            ensure_tool_ok(&cleanup_result)?;
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
            Err(err) => {
                let (error_type, hint) = classify_clone_error(&err);
                json!({
                    "ok": false,
                    "error_type": error_type,
                    "error": err,
                    "hint": hint,
                    "retry_with_shell_execute_recommended": false,
                })
                .to_string()
            }
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
struct GithubUserResponse {
    login: String,
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
                "Use for all GitHub tasks with host-side secrets. IMPORTANT: when the user asks to clone a GitHub repo (for example text containing 'git clone' or a github.com URL), prefer action='clone_repo' instead of shell_execute so GITHUB_TOKEN is used."
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
                        "description": "GitHub action to run. For git clone requests use clone_repo."
                    },
                    "owner": {
                        "type": "string",
                        "description": "Repository owner/org (required for repo-specific actions; optional for clone_repo if clone_url is provided)."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Repository name (required for repo-specific actions; optional for clone_repo if clone_url is provided)."
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
                        "description": "For clone_repo: full git clone URL (supports web/SSH URLs and normalizes to clone URL). If owner/repo are missing, they are parsed from this URL. Default when omitted: https://github.com/<owner>/<repo>.git"
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

fn canonicalize_clone_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return canonicalize_clone_url(&format!(
            "https://github.com/{}",
            rest.trim_start_matches('/')
        ));
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        return canonicalize_clone_url(&format!(
            "https://github.com/{}",
            rest.trim_start_matches('/')
        ));
    }

    if let Ok(mut parsed) = reqwest::Url::parse(trimmed) {
        let _ = parsed.set_scheme("https");
        parsed.set_fragment(None);

        if parsed
            .host_str()
            .map(|h| h.eq_ignore_ascii_case("github.com"))
            .unwrap_or(false)
        {
            let parts: Vec<String> = parsed
                .path_segments()
                .map(|segments| {
                    segments
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();

            if parts.len() >= 2 {
                let owner = parts[0].trim();
                let repo = parts[1].trim().trim_end_matches(".git");
                if !owner.is_empty() && !repo.is_empty() {
                    return format!("https://github.com/{}/{}.git", owner, repo);
                }
            }
        }

        parsed.set_query(None);
        return parsed.to_string().trim_end_matches('/').to_string();
    }

    trimmed.to_string()
}

fn parse_owner_repo_from_clone_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let path_owned = if let Some(after_host) = trimmed.strip_prefix("git@github.com:") {
        after_host.to_string()
    } else if let Ok(parsed) = reqwest::Url::parse(trimmed) {
        parsed.path().trim_start_matches('/').to_string()
    } else {
        trimmed.trim_start_matches('/').to_string()
    };

    let mut parts = path_owned.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?.trim().to_string();
    let repo_raw = parts.next()?.trim().to_string();
    let repo = repo_raw
        .strip_suffix(".git")
        .unwrap_or(repo_raw.as_str())
        .trim()
        .to_string();

    if owner.is_empty() || repo.is_empty() {
        None
    } else {
        Some((owner, repo))
    }
}

fn url_with_basic_auth(repo_url: &str, username: &str, token: &str) -> Result<String, String> {
    let mut parsed = reqwest::Url::parse(repo_url)
        .map_err(|e| format!("Invalid clone_url '{}': {}", repo_url, e))?;
    parsed
        .set_username(username)
        .map_err(|_| "Failed to set username on clone URL".to_string())?;
    parsed
        .set_password(Some(token))
        .map_err(|_| "Failed to set password on clone URL".to_string())?;
    Ok(parsed.to_string())
}

fn basic_auth_header(username: &str, token: &str) -> String {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"));
    format!("AUTHORIZATION: basic {}", basic)
}

fn extract_github_error_message(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok()?;
    parsed
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn redact_secret(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        s.to_string()
    } else {
        s.replace(secret, "***")
    }
}

fn first_non_empty_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|line| !line.is_empty())
}

fn best_error_line(s: &str) -> Option<&str> {
    // Git often writes a progress line first ("Cloning into ...") and the
    // actionable cause later ("fatal: ..."). Prefer the most useful line.
    let lines: Vec<&str> = s
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }

    for line in &lines {
        let lower = line.to_lowercase();
        if lower.starts_with("fatal:")
            || lower.starts_with("error:")
            || lower.contains("permission denied")
            || lower.contains("repository not found")
            || lower.contains("authentication failed")
        {
            return Some(line);
        }
    }

    lines.last().copied()
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...\\n[truncated, {} bytes]", &s[..max_len], s.len())
    }
}

fn classify_clone_error(err: &str) -> (&'static str, &'static str) {
    let lower = err.to_lowercase();

    if (lower.contains("worker /workspace/write") && lower.contains("http 413"))
        || lower.contains("payload too large")
        || lower.contains("failed to buffer the request body")
    {
        return (
            "worker_workspace_write_payload_too_large",
            "Worker rejected a large file upload while copying the cloned repo into /workspace. Increase worker request body limit (already patched in recent version) and restart worker/service.",
        );
    }

    if lower.contains("worker /workspace/write") {
        return (
            "worker_workspace_write_failed",
            "Cloning on host likely succeeded, but copying files into docker /workspace failed. Check worker /workspace/write endpoint logs and worker token/path configuration.",
        );
    }

    if lower.contains("github api preflight failed with 401")
        || lower.contains("token invalid or expired")
    {
        return (
            "token_invalid_or_expired",
            "GITHUB_TOKEN is invalid or expired. Create a new token and update .env.secure.",
        );
    }

    if lower.contains("github api preflight failed with 403")
        || lower.contains("sso")
        || lower.contains("lacks required permissions")
    {
        return (
            "token_scope_or_sso_problem",
            "Token exists but lacks required permissions or SSO authorization. For private repos grant Contents: Read and authorize the token for org SSO.",
        );
    }

    if lower.contains("github api preflight failed with 404")
        || (lower.contains("requested url returned error: 403")
            && lower.contains("authentication failed"))
    {
        return (
            "repo_access_denied_or_not_found",
            "Token cannot access this repository (or owner/repo is wrong). For collaborator repos across different owners, use a Classic PAT from a dedicated bot account (repo scope), then verify repo access and org SSO authorization.",
        );
    }

    if lower.contains("invalid clone_url") || lower.contains("could not parse owner/repo") {
        return (
            "invalid_clone_url",
            "clone_url format is invalid. Use https://github.com/<owner>/<repo>.git (or provide owner+repo).",
        );
    }

    (
        "clone_failed_unknown",
        "Check GITHUB_TOKEN, repo permissions, org SSO authorization, and clone_url format.",
    )
}

#[cfg(test)]
mod tests {
    use super::{
        best_error_line, canonicalize_clone_url, classify_clone_error,
        parse_owner_repo_from_clone_url,
    };

    #[test]
    fn canonicalizes_ssh_url() {
        let got = canonicalize_clone_url("git@github.com:octocat/hello-world.git");
        assert_eq!(got, "https://github.com/octocat/hello-world.git");
    }

    #[test]
    fn canonicalizes_web_tree_url() {
        let got = canonicalize_clone_url("https://github.com/octocat/hello-world/tree/main");
        assert_eq!(got, "https://github.com/octocat/hello-world.git");
    }

    #[test]
    fn parses_owner_repo_from_web_url() {
        let got =
            parse_owner_repo_from_clone_url("https://github.com/octocat/hello-world/tree/main");
        assert_eq!(
            got,
            Some(("octocat".to_string(), "hello-world".to_string()))
        );
    }

    #[test]
    fn best_error_line_prefers_fatal_over_progress() {
        let stderr = "Cloning into '/tmp/repo'...\nfatal: Repository not found.\n";
        assert_eq!(
            best_error_line(stderr),
            Some("fatal: Repository not found.")
        );
    }

    #[test]
    fn classify_clone_error_detects_repo_access_problem() {
        let (error_type, _) = classify_clone_error(
            "git clone failed ... The requested URL returned error: 403 ... GitHub API preflight failed with 404 ...",
        );
        assert_eq!(error_type, "repo_access_denied_or_not_found");
    }

    #[test]
    fn classify_clone_error_detects_worker_write_issue() {
        let (error_type, _) =
            classify_clone_error("worker /workspace/write invalid JSON (HTTP 413): body too large");
        assert_eq!(error_type, "worker_workspace_write_payload_too_large");
    }
}
