use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct ListDirectorySkill;

#[async_trait]
impl Skill for ListDirectorySkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "list_directory".to_string(),
            description: Some(
                "Lists files and directories in a folder (similar to `ls`). \
                 Can be used to navigate and explore the directory structure. \
                 Returns each entry with name, type (file/dir), and size."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative directory path, e.g. 'soul/memory' or '.' for current directory"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");

        println!("[ListDir] Listing '{}'", path);

        let safe_path = match sanitize_relative_path(path) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("[ListDir] Path rejected: {}", err);
                return json!({ "ok": false, "error": err }).to_string();
            }
        };

        let mut entries = Vec::new();

        let mut read_dir = match tokio::fs::read_dir(&safe_path).await {
            Ok(rd) => rd,
            Err(err) => {
                eprintln!("[ListDir] ERROR reading '{}': {}", safe_path.display(), err);
                return json!({
                    "ok": false,
                    "error": format!("Failed to read directory: {}", err),
                })
                .to_string();
            }
        };

        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip hidden files and target/
            if name.starts_with('.') || name == "target" {
                continue;
            }

            let meta = entry.metadata().await;
            let (kind, size) = match meta {
                Ok(m) => {
                    let kind = if m.is_dir() { "dir" } else { "file" };
                    let size = if m.is_file() { Some(m.len()) } else { None };
                    (kind, size)
                }
                Err(_) => ("unknown", None),
            };

            entries.push(json!({
                "name": name,
                "type": kind,
                "size": size,
            }));
        }

        // Sort alphabetically, directories first
        entries.sort_by(|a, b| {
            let a_type = a.get("type").and_then(Value::as_str).unwrap_or("");
            let b_type = b.get("type").and_then(Value::as_str).unwrap_or("");
            let a_name = a.get("name").and_then(Value::as_str).unwrap_or("");
            let b_name = b.get("name").and_then(Value::as_str).unwrap_or("");
            // Directories before files, then alphabetical
            b_type.cmp(a_type).then(a_name.cmp(b_name))
        });

        json!({
            "ok": true,
            "path": safe_path.to_string_lossy(),
            "entries": entries,
            "count": entries.len(),
        })
        .to_string()
    }
}

fn sanitize_relative_path(path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    if path.is_empty() {
        return Ok(PathBuf::from("."));
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
