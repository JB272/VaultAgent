use async_trait::async_trait;
use reqwest::Client;
use serde_json::{Value, json};

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
        let default_owner = std::env::var("GITHUB_DEFAULT_OWNER").unwrap_or_default();
        let default_repo = std::env::var("GITHUB_DEFAULT_REPO").unwrap_or_default();

        Ok(GithubEnv {
            token,
            api_base: api_base.trim_end_matches('/').to_string(),
            default_owner,
            default_repo,
        })
    }

    fn owner_repo_from_args(
        &self,
        arguments: &Value,
        env: &GithubEnv,
    ) -> Result<(String, String), String> {
        let owner = arguments
            .get("owner")
            .and_then(Value::as_str)
            .unwrap_or(env.default_owner.as_str())
            .trim()
            .to_string();

        let repo = arguments
            .get("repo")
            .and_then(Value::as_str)
            .unwrap_or(env.default_repo.as_str())
            .trim()
            .to_string();

        if owner.is_empty() || repo.is_empty() {
            return Err("owner and repo are required (or set GITHUB_DEFAULT_OWNER/GITHUB_DEFAULT_REPO).".to_string());
        }

        Ok((owner, repo))
    }

    fn auth_request(&self, env: &GithubEnv, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
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
}

struct GithubEnv {
    token: String,
    api_base: String,
    default_owner: String,
    default_repo: String,
}

#[async_trait]
impl Skill for GitHubSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "github".to_string(),
            description: Some(
                "Interact with GitHub REST API using a token from env. \
                 Supports listing repos, listing/getting/creating issues, adding issue comments, and listing/getting pull requests."
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
                            "get_pull_request"
                        ],
                        "description": "GitHub action to run."
                    },
                    "owner": {
                        "type": "string",
                        "description": "Repository owner/org. Optional if GITHUB_DEFAULT_OWNER is set."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Repository name. Optional if GITHUB_DEFAULT_REPO is set."
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
                let (owner, repo) = match self.owner_repo_from_args(arguments, &env) {
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
                let (owner, repo) = match self.owner_repo_from_args(arguments, &env) {
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
                let (owner, repo) = match self.owner_repo_from_args(arguments, &env) {
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
                let (owner, repo) = match self.owner_repo_from_args(arguments, &env) {
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
                let (owner, repo) = match self.owner_repo_from_args(arguments, &env) {
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
                let (owner, repo) = match self.owner_repo_from_args(arguments, &env) {
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
