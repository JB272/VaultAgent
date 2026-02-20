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
                "Listet Dateien und Ordner in einem Verzeichnis auf (wie `ls`). \
                 Kann zum Navigieren und Erkunden der Verzeichnisstruktur verwendet werden. \
                 Gibt für jeden Eintrag Name, Typ (file/dir) und Größe zurück."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relativer Verzeichnispfad, z.B. 'soul/memory' oder '.' für das aktuelle Verzeichnis"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");

        let safe_path = match sanitize_relative_path(path) {
            Ok(p) => p,
            Err(err) => {
                return json!({ "ok": false, "error": err }).to_string();
            }
        };

        let mut entries = Vec::new();

        let mut read_dir = match tokio::fs::read_dir(&safe_path).await {
            Ok(rd) => rd,
            Err(err) => {
                return json!({
                    "ok": false,
                    "error": format!("Verzeichnis konnte nicht gelesen werden: {}", err),
                })
                .to_string();
            }
        };

        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();

            // Versteckte Dateien und target/ überspringen
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

        // Alphabetisch sortieren, Ordner zuerst
        entries.sort_by(|a, b| {
            let a_type = a.get("type").and_then(Value::as_str).unwrap_or("");
            let b_type = b.get("type").and_then(Value::as_str).unwrap_or("");
            let a_name = a.get("name").and_then(Value::as_str).unwrap_or("");
            let b_name = b.get("name").and_then(Value::as_str).unwrap_or("");
            // Ordner vor Dateien, dann alphabetisch
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
