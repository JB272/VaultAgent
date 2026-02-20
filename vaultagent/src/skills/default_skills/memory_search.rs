use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;
use crate::soul::memory::Memory;

/// Skill: Durchsucht das Gedächtnis nach einem Suchbegriff.
pub struct MemorySearchSkill {
    memory: Arc<Memory>,
}

impl MemorySearchSkill {
    pub fn new(memory: Arc<Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Skill for MemorySearchSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "memory_search".to_string(),
            description: Some(
                "Durchsucht dein Gedächtnis (MEMORY.md + alle Tageslogs) nach einem Suchbegriff. \
                 Gibt alle Treffer mit Dateiname und Zeilennummer zurück."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Suchbegriff (case-insensitive)."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if query.trim().is_empty() {
            return json!({ "ok": false, "error": "Suchbegriff darf nicht leer sein." })
                .to_string();
        }

        let results = self.memory.search(query);

        if results.is_empty() {
            json!({
                "ok": true,
                "results": [],
                "message": format!("Keine Treffer für '{}'.", query),
            })
            .to_string()
        } else {
            let hits: Vec<Value> = results
                .iter()
                .map(|r| {
                    json!({
                        "file": r.file,
                        "line": r.line_number,
                        "text": r.text,
                    })
                })
                .collect();

            json!({
                "ok": true,
                "count": hits.len(),
                "results": hits,
            })
            .to_string()
        }
    }
}
