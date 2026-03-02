use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Stores files in the workspace, including binary files via base64.
///
/// This complements `write_file` (text-only) by allowing the agent to persist
/// arbitrary bytes so Python skills/scripts can process them afterwards.
pub struct FileStoreSkill;

fn looks_like_binary_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("png")
            | Some("jpg")
            | Some("jpeg")
            | Some("gif")
            | Some("webp")
            | Some("pdf")
            | Some("zip")
            | Some("tar")
            | Some("gz")
            | Some("mp3")
            | Some("wav")
            | Some("mp4")
            | Some("bin")
    )
}

#[async_trait]
impl Skill for FileStoreSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "file_store".to_string(),
            description: Some(
                "Stores a file in the workspace. Supports plain text (content) and binary data (content_base64). \
                 Use this when you need to save uploaded/generated files (PDFs, images, audio, archives, etc.) \
                 for later processing by Python skills or shell commands."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative file path, e.g. uploads/report.pdf"
                    },
                    "content": {
                        "type": "string",
                        "description": "Plain text content to store (UTF-8)."
                    },
                    "content_base64": {
                        "type": "string",
                        "description": "Base64-encoded file bytes for binary files."
                    },
                    "append": {
                        "type": "boolean",
                        "description": "If true, appends to the target file. Default: false (overwrite/create)."
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

        let content = arguments.get("content").and_then(Value::as_str);
        let content_base64 = arguments.get("content_base64").and_then(Value::as_str);
        let append = arguments
            .get("append")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let safe_path = match sanitize_relative_path(path) {
            Ok(p) => p,
            Err(err) => {
                return json!({ "ok": false, "error": err }).to_string();
            }
        };

        let bytes: Vec<u8> = match (content, content_base64) {
            (Some(text), None) => text.as_bytes().to_vec(),
            (None, Some(b64)) => {
                if b64.trim().is_empty() {
                    return json!({
                        "ok": false,
                        "error": "content_base64 must not be empty.",
                    })
                    .to_string();
                }

                match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(decoded) => decoded,
                    Err(err) => {
                        return json!({
                            "ok": false,
                            "error": format!("Invalid content_base64: {}", err),
                        })
                        .to_string();
                    }
                }
            }
            (Some(_), Some(_)) => {
                return json!({
                    "ok": false,
                    "error": "Provide either 'content' or 'content_base64', not both.",
                })
                .to_string();
            }
            (None, None) => {
                return json!({
                    "ok": false,
                    "error": "Either 'content' or 'content_base64' is required.",
                })
                .to_string();
            }
        };

        if content.is_some() && looks_like_binary_path(&safe_path) {
            return json!({
                "ok": false,
                "error": "Refusing to write plain text to a binary-looking file path. Use content_base64 for binary files.",
            })
            .to_string();
        }

        if content_base64.is_some() && bytes.is_empty() {
            return json!({
                "ok": false,
                "error": "Decoded content_base64 is empty; refusing to overwrite file with 0 bytes.",
            })
            .to_string();
        }

        println!(
            "[FileStore] Writing '{}' ({} bytes, append={})",
            path,
            bytes.len(),
            append
        );

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

        let write_result = if append {
            match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&safe_path)
                .await
            {
                Ok(mut file) => file.write_all(&bytes).await,
                Err(err) => Err(err),
            }
        } else {
            tokio::fs::write(&safe_path, &bytes).await
        };

        match write_result {
            Ok(()) => json!({
                "ok": true,
                "path": safe_path.to_string_lossy(),
                "bytes_written": bytes.len(),
                "append": append,
            })
            .to_string(),
            Err(err) => json!({
                "ok": false,
                "error": format!("Failed to store file: {}", err),
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
