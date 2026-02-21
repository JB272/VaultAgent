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
                "Removes a scheduled cron job by its ID. \
                 Use cron_list to see available job IDs."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "ID of the job to delete."
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
            return json!({ "ok": false, "error": "job_id is missing." }).to_string();
        }

        match self.store.remove(job_id).await {
            Ok(true) => json!({
                "ok": true,
                "message": format!("Job '{}' deleted.", job_id),
            }).to_string(),
            Ok(false) => json!({
                "ok": false,
                "error": format!("No job found with ID '{}'.", job_id),
            }).to_string(),
            Err(err) => json!({
                "ok": false,
                "error": format!("Failed to delete job: {}", err),
            }).to_string(),
        }
    }
}
