use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;
use crate::soul::memory::Memory;

/// Skill: Saves an entry to the daily log or long-term memory.
pub struct MemorySaveSkill {
    memory: Arc<Memory>,
}

impl MemorySaveSkill {
    pub fn new(memory: Arc<Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Skill for MemorySaveSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "memory_save".to_string(),
            description: Some(
                "Saves a memory entry. Use 'daily' for short-term notes \
                 (daily log) or 'long_term' for persistent important facts (MEMORY.md)."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "entry": {
                        "type": "string",
                        "description": "Text to store."
                    },
                    "storage": {
                        "type": "string",
                        "enum": ["daily", "long_term"],
                        "description": "Storage target: 'daily' = today's log, 'long_term' = MEMORY.md"
                    }
                },
                "required": ["entry"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let entry = arguments
            .get("entry")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if entry.trim().is_empty() {
            return json!({ "ok": false, "error": "Entry must not be empty." }).to_string();
        }

        let storage = arguments
            .get("storage")
            .and_then(Value::as_str)
            .unwrap_or("daily");

        println!("[MemorySave] Saving to '{}': {}…", storage, &entry[..entry.len().min(80)]);

        let result = match storage {
            "long_term" => self.memory.append_long_term(entry).await,
            _ => self.memory.append_today(entry).await,
        };

        match result {
            Ok(()) => {
                println!("[MemorySave] OK — saved to {}", storage);
                json!({
                    "ok": true,
                    "storage": storage,
                    "message": format!("Memory saved ({}).", storage),
                })
                .to_string()
            }
            Err(err) => {
                eprintln!("[MemorySave] ERROR: {}", err);
                json!({
                    "ok": false,
                    "error": err,
                })
                .to_string()
            }
        }
    }
}
