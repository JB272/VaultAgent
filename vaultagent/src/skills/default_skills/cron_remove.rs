use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::cron::store::CronStore;
use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Entfernt einen geplanten Cron-Job anhand seiner ID.
pub struct CronRemoveSkill {
    store: Arc<CronStore>,
}

impl CronRemoveSkill {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Skill for CronRemoveSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "cron_remove".to_string(),
            description: Some(
                "Entfernt einen geplanten Cron-Job anhand seiner ID. \
                 Nutze cron_list, um die IDs der vorhandenen Jobs zu sehen."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Die ID des zu löschenden Jobs."
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let job_id = arguments.get("job_id").and_then(Value::as_str).unwrap_or("");

        if job_id.is_empty() {
            return json!({ "ok": false, "error": "job_id fehlt." }).to_string();
        }

        match self.store.remove(job_id).await {
            Ok(true) => json!({
                "ok": true,
                "message": format!("Job '{}' wurde gelöscht.", job_id),
            }).to_string(),
            Ok(false) => json!({
                "ok": false,
                "error": format!("Kein Job mit ID '{}' gefunden.", job_id),
            }).to_string(),
            Err(err) => json!({
                "ok": false,
                "error": format!("Fehler beim Löschen: {}", err),
            }).to_string(),
        }
    }
}
