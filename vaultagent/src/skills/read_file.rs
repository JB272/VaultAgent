use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::reasoning::llm_interface::LlmToolDefinition;
use super::Skill;

pub struct ReadFileSkill;

#[async_trait]
impl Skill for ReadFileSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "read_file".to_string(),
            description: Some(
                "Liest eine Textdatei aus einem relativen Pfad im Workspace.".to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relativer Dateipfad, z.B. notes/test.txt"
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

        match sanitize_relative_path(path) {
            Ok(safe_path) => match tokio::fs::read_to_string(&safe_path).await {
                Ok(content) => json!({
                    "ok": true,
                    "path": safe_path.to_string_lossy(),
                    "content": content,
                })
                .to_string(),
                Err(err) => json!({
                    "ok": false,
                    "error": format!("Datei konnte nicht gelesen werden: {}", err),
                })
                .to_string(),
            },
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
        return Err("Pfad darf nicht leer sein.".to_string());
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err("Nur relative Pfade im Workspace sind erlaubt.".to_string());
    }

    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("Pfad enthält unzulässige Segmente (.. oder Root).".to_string());
    }

    Ok(PathBuf::from(path))
}
