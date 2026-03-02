use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Copy or move a file within the workspace.
pub struct FileCopySkill;

fn sanitize_relative_path(raw: &str) -> Result<PathBuf, String> {
    let p = Path::new(raw);
    if p.is_absolute() {
        return Err("Path must be relative.".into());
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err("Path must not contain '..' or root prefixes.".into()),
        }
    }
    Ok(p.to_path_buf())
}

#[async_trait]
impl Skill for FileCopySkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "file_copy".to_string(),
            description: Some(
                "Copy or move a file within the workspace. Use this to relocate uploaded files \
                 to a different folder or rename them. Set move=true to move instead of copy."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Relative path of the source file, e.g. skills/uploads/abc_report.pdf"
                    },
                    "destination": {
                        "type": "string",
                        "description": "Relative path for the destination, e.g. kontoauszuege/report.pdf"
                    },
                    "move": {
                        "type": "boolean",
                        "description": "If true, move (delete source after copy). Default: false (copy)."
                    }
                },
                "required": ["source", "destination"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let source = arguments.get("source").and_then(Value::as_str).unwrap_or_default();
        let destination = arguments.get("destination").and_then(Value::as_str).unwrap_or_default();
        let do_move = arguments.get("move").and_then(Value::as_bool).unwrap_or(false);

        let safe_src = match sanitize_relative_path(source) {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": format!("Invalid source: {}", e)}).to_string(),
        };
        let safe_dst = match sanitize_relative_path(destination) {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": format!("Invalid destination: {}", e)}).to_string(),
        };

        if !safe_src.exists() {
            return json!({"ok": false, "error": format!("Source file not found: {}", source)}).to_string();
        }

        // Create parent directories for destination.
        if let Some(parent) = safe_dst.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return json!({"ok": false, "error": format!("Failed to create destination directory: {}", e)}).to_string();
                }
            }
        }

        if do_move {
            // Try rename first (same filesystem), fall back to copy+delete.
            if tokio::fs::rename(&safe_src, &safe_dst).await.is_err() {
                if let Err(e) = tokio::fs::copy(&safe_src, &safe_dst).await {
                    return json!({"ok": false, "error": format!("Failed to move file: {}", e)}).to_string();
                }
                let _ = tokio::fs::remove_file(&safe_src).await;
            }
            json!({"ok": true, "action": "moved", "destination": destination}).to_string()
        } else {
            match tokio::fs::copy(&safe_src, &safe_dst).await {
                Ok(bytes) => json!({"ok": true, "action": "copied", "bytes": bytes, "destination": destination}).to_string(),
                Err(e) => json!({"ok": false, "error": format!("Failed to copy file: {}", e)}).to_string(),
            }
        }
    }
}
