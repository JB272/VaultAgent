use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;
use crate::soul::memory::Memory;

/// Skill: Reads the full content of a specific memory file.
///
/// Use this after `memory_search` has returned a file path you want to
/// inspect in full (or in part).
pub struct MemoryGetSkill {
    memory: Arc<Memory>,
}

impl MemoryGetSkill {
    pub fn new(memory: Arc<Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Skill for MemoryGetSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "memory_get".to_string(),
            description: Some(
                "Reads the content of a specific memory file. \
                 Use the path returned by memory_search, e.g. \
                 \"memory/2026-03-01-project-setup.md\" or \"MEMORY.md\". \
                 Optionally limit output with from_line / to_line (1-based)."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the memory file, e.g. \"MEMORY.md\" or \"memory/2026-03-01-slug.md\"."
                    },
                    "from_line": {
                        "type": "integer",
                        "description": "First line to return (1-based, inclusive). Defaults to 1."
                    },
                    "to_line": {
                        "type": "integer",
                        "description": "Last line to return (1-based, inclusive). Omit for the full file."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let path = match arguments.get("path").and_then(Value::as_str) {
            Some(p) if !p.trim().is_empty() => p,
            _ => {
                return json!({ "ok": false, "error": "path is required." }).to_string();
            }
        };

        let content = match self.memory.load_file(path) {
            Ok(c) => c,
            Err(e) => return json!({ "ok": false, "error": e }).to_string(),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let from = arguments
            .get("from_line")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).saturating_sub(1))
            .unwrap_or(0);

        let to = arguments
            .get("to_line")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).min(total))
            .unwrap_or(total);

        let slice = if from < to && from < total {
            lines[from..to].join("\n")
        } else {
            String::new()
        };

        json!({
            "ok": true,
            "path": path,
            "total_lines": total,
            "content": slice,
        })
        .to_string()
    }
}
