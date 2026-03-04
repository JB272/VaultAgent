use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use serde_json::{Value, json};
use std::path::{Component, Path};
use tokio::process::Command;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct GitHubSkill {
    client: Client,
}

impl GitHubSkill {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("VaultAgent/1.0")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
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

    fn git_auth_configs(&self, env: &GithubEnv) -> Vec<GitAuthConfig> {
        vec![
            // Common GitHub username aliases for token auth.
            GitAuthConfig {
                label: "basic(x-access-token)".to_string(),
                extra_header: basic_auth_header("x-access-token", &env.token),
            },
            GitAuthConfig {
                label: "basic(git)".to_string(),
                extra_header: basic_auth_header("git", &env.token),
            },
            // Some setups accept bearer auth for git HTTP endpoints.
            GitAuthConfig {
                label: "bearer".to_string(),
                extra_header: format!("AUTHORIZATION: bearer {}", env.token),
            },
        ]
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
        env: &GithubEnv,
        args: &[String],
    ) -> Result<(i32, String, String, String), String> {
        let mut failures = Vec::new();

        for cfg in self.git_auth_configs(env) {
            match self.run_git_command(&cfg.extra_header, args).await {
                Ok((exit_code, stdout, stderr)) => {
                    if exit_code == 0 {
                        return Ok((exit_code, stdout, stderr, cfg.label));
                    }

                    failures.push(format!(
                        "{} -> exit {}: {}",
                        cfg.label,
                        exit_code,
                        first_non_empty_line(&stderr).unwrap_or("unknown git error")
                    ));
                }
                Err(err) => failures.push(format!("{} -> {}", cfg.label, err)),
            }
        }

        Err(format!(
            "git authentication failed for all strategies. {}",
            failures.join(" | ")
        ))
    }

    async fn clone_repo(&self, arguments: &Value, env: &GithubEnv) -> String {
        let (owner, repo) = match self.owner_repo_from_args(arguments) {
            Ok(v) => v,
            Err(err) => return json!({ "ok": false, "error": err }).to_string(),
        };

        let default_destination = format!("repos/{}-{}", owner, repo);
        let destination_input = arguments
            .get("destination")
            .and_then(Value::as_str)
            .unwrap_or(default_destination.as_str());

        let destination_rel = match sanitize_workspace_relative_path(destination_input) {
            Ok(v) => v,
            Err(err) => return json!({ "ok": false, "error": err }).to_string(),
        };

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

        let exists = tokio::fs::metadata(&destination_abs).await.is_ok();

        if exists {
            if !update_if_exists {
                return json!({
                    "ok": false,
                    "error": format!("Destination already exists: {}", destination_abs),
                })
                .to_string();
            }

            let mut args = vec![
                "-C".to_string(),
                destination_abs.clone(),
                "pull".to_string(),
                "--ff-only".to_string(),
            ];
            if let Some(r) = &git_ref {
                args.push("origin".to_string());
                args.push(r.clone());
            }

            match self.run_git_with_fallback(env, &args).await {
                Ok((exit_code, stdout, stderr, auth_strategy)) => json!({
                    "ok": exit_code == 0,
                    "action": "clone_repo",
                    "updated_existing": true,
                    "path": destination_abs,
                    "repo_url": repo_url,
                    "auth_strategy": auth_strategy,
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                })
                .to_string(),
                Err(err) => json!({
                    "ok": false,
                    "error": err,
                    "hint": "Check token repo access (Contents: Read at least) and owner/repo name.",
                }).to_string(),
            }
        } else {
            let mut args = vec!["clone".to_string()];
            if let Some(r) = &git_ref {
                args.push("--branch".to_string());
                args.push(r.clone());
                args.push("--single-branch".to_string());
            }
            args.push(repo_url.clone());
            args.push(destination_abs.clone());

            match self.run_git_with_fallback(env, &args).await {
                Ok((exit_code, stdout, stderr, auth_strategy)) => json!({
                    "ok": exit_code == 0,
                    "action": "clone_repo",
                    "updated_existing": false,
                    "path": destination_abs,
                    "repo_url": repo_url,
                    "auth_strategy": auth_strategy,
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                })
                .to_string(),
                Err(err) => json!({
                    "ok": false,
                    "error": err,
                    "hint": "Check token repo access (Contents: Read at least) and owner/repo name.",
                }).to_string(),
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

#[async_trait]
impl Skill for GitHubSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "github".to_string(),
            description: Some(
                "Interact with GitHub from inside the Docker worker. Supports REST API actions and cloning repos into /workspace."
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
                        "description": "For clone_repo: relative path inside /workspace, e.g. repos/my-repo."
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
                        "description": "For clone_repo: if destination exists, run git pull --ff-only. Default: true."
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

    Ok(path.trim().trim_start_matches("./").to_string())
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
