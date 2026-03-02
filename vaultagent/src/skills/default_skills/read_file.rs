use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct ReadFileSkill;

#[async_trait]
impl Skill for ReadFileSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "read_file".to_string(),
            description: Some(
                "Reads a text file from a relative path in the workspace. \
                 Use only when the user explicitly asks to inspect, summarize, or analyze file content. \
                 Do not use this for pure file organization tasks (move/store/rename)."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative file path, e.g. notes/test.txt"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();

        println!("[ReadFile] Reading '{}'", path);

        match sanitize_relative_path(path) {
            Ok(safe_path) => match tokio::fs::read_to_string(&safe_path).await {
                Ok(content) => {
                    println!(
                        "[ReadFile] OK — {} bytes from {}",
                        content.len(),
                        safe_path.display()
                    );
                    json!({
                        "ok": true,
                        "path": safe_path.to_string_lossy(),
                        "content": content,
                    })
                    .to_string()
                }
                Err(err) => {
                    eprintln!(
                        "[ReadFile] ERROR reading '{}': {}",
                        safe_path.display(),
                        err
                    );
                    json!({
                        "ok": false,
                        "error": format!("Failed to read file: {}", err),
                    })
                    .to_string()
                }
            },
            Err(err) => {
                eprintln!("[ReadFile] Path rejected: {}", err);
                json!({
                    "ok": false,
                    "error": err,
                })
                .to_string()
            }
        }
    }
}

fn sanitize_relative_path(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("Path must not be empty.".to_string());
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err("Only relative paths inside the workspace are allowed.".to_string());
    }

    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("Path contains forbidden segments (.. or root).".to_string());
    }

    Ok(PathBuf::from(path))
}
