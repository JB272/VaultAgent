use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::cron::store::{CronStore, Schedule};
use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Listet alle geplanten Cron-Jobs auf.
pub struct CronListSkill {
    store: Arc<CronStore>,
}

impl CronListSkill {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Skill for CronListSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "cron_list".to_string(),
            description: Some(
                "Lists all scheduled cron jobs. Shows name, schedule, status, and ID."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, _arguments: &Value) -> String {
        let jobs = self.store.list().await;

        if jobs.is_empty() {
            return json!({
                "ok": true,
                "jobs": [],
                "message": "No cron jobs found."
            }).to_string();
        }

        let job_summaries: Vec<Value> = jobs
            .iter()
            .map(|job| {
                let schedule_desc = match &job.schedule {
                    Schedule::At { at } => format!("once at {}", at.format("%Y-%m-%d %H:%M UTC")),
                    Schedule::Every { every_secs } => format!("every {} seconds", every_secs),
                    Schedule::Cron { expr, tz } => {
                        let tz_str = tz.as_deref().unwrap_or("UTC");
                        format!("cron '{}' ({})", expr, tz_str)
                    }
                };

                json!({
                    "id": job.id,
                    "name": job.name,
                    "schedule": schedule_desc,
                    "prompt": job.prompt,
                    "chat_id": job.chat_id,
                    "enabled": job.enabled,
                    "last_run": job.last_run.map(|t| t.to_rfc3339()),
                })
            })
            .collect();

        json!({
            "ok": true,
            "count": job_summaries.len(),
            "jobs": job_summaries,
        }).to_string()
    }
}
