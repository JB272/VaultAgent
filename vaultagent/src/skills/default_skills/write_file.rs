use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct WriteFileSkill;

#[async_trait]
impl Skill for WriteFileSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "write_file".to_string(),
            description: Some(
                "Writes content to a file in the workspace. Creates file/directories if needed."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative file path, e.g. test.txt"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write into the file"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match sanitize_relative_path(path) {
            Ok(safe_path) => {
                if let Some(parent) = safe_path.parent() {
                    if !parent.as_os_str().is_empty() {
                        if let Err(err) = tokio::fs::create_dir_all(parent).await {
                            return json!({
                                "ok": false,
                                "error": format!("Failed to create directories: {}", err),
                            })
                            .to_string();
                        }
                    }
                }

                match tokio::fs::write(&safe_path, content).await {
                    Ok(()) => json!({
                        "ok": true,
                        "path": safe_path.to_string_lossy(),
                        "bytes_written": content.len(),
                    })
                    .to_string(),
                    Err(err) => json!({
                        "ok": false,
                        "error": format!("Failed to write file: {}", err),
                    })
                    .to_string(),
                }
            }
            Err(err) => json!({
                "ok": false,
                "error": err,
            })
            .to_string(),
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

    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err("Path contains forbidden segments (.. or root).".to_string());
    }

    Ok(PathBuf::from(path))
}
