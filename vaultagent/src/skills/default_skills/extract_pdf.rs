use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct ExtractPdfSkill;

#[async_trait]
impl Skill for ExtractPdfSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "extract_pdf".to_string(),
            description: Some("Extracts plain text from a PDF file in the workspace.".to_string()),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to a PDF file, e.g. docs/manual.pdf"
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

        println!("[ExtractPdf] Extracting text from '{}'", path);

        let safe_path = match sanitize_relative_path(path) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("[ExtractPdf] Path rejected: {}", err);
                return json!({
                    "ok": false,
                    "error": err,
                })
                .to_string();
            }
        };

        let is_pdf = safe_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);

        if !is_pdf {
            return json!({
                "ok": false,
                "error": "File must have a .pdf extension.",
            })
            .to_string();
        }

        let bytes = match tokio::fs::read(&safe_path).await {
            Ok(data) => data,
            Err(err) => {
                eprintln!(
                    "[ExtractPdf] ERROR reading '{}': {}",
                    safe_path.display(),
                    err
                );
                return json!({
                    "ok": false,
                    "error": format!("Failed to read PDF: {}", err),
                })
                .to_string();
            }
        };

        let parse_result =
            tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem(&bytes)).await;

        let text = match parse_result {
            Ok(Ok(text)) => text,
            Ok(Err(err)) => {
                eprintln!("[ExtractPdf] ERROR parsing PDF: {}", err);
                return json!({
                    "ok": false,
                    "error": format!("Failed to parse PDF: {}", err),
                })
                .to_string();
            }
            Err(err) => {
                eprintln!("[ExtractPdf] Worker thread failed: {}", err);
                return json!({
                    "ok": false,
                    "error": format!("PDF extraction task failed: {}", err),
                })
                .to_string();
            }
        };

        println!(
            "[ExtractPdf] OK — extracted {} chars from {}",
            text.chars().count(),
            safe_path.display()
        );

        json!({
            "ok": true,
            "path": safe_path.to_string_lossy(),
            "text": text,
            "char_count": text.chars().count(),
        })
        .to_string()
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
