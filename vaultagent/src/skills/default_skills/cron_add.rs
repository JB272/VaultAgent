use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

use crate::cron::store::{CronJob, CronStore, Schedule};
use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Erstellt einen neuen Cron-Job (Erinnerung / geplante Nachricht).
pub struct CronAddSkill {
    store: Arc<CronStore>,
}

impl CronAddSkill {
    pub fn new(store: Arc<CronStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Skill for CronAddSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "cron_add".to_string(),
            description: Some(
                "Schedules a timed task. At the scheduled time, the agent is invoked \
                 with the given prompt and posts the result in chat. \
                 Use this for reminders, daily summaries, weather checks, etc.\n\
                 IMPORTANT for prompt text: write the prompt as a DIRECT TASK for execution time, \
                 for example 'Tell the user to close the window now.' \
                 or 'Summarize today's weather in Berlin.' \
                 NOT as a reminder request like 'Remind me to...'."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short job name, e.g. 'Morning weather report'"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Prompt sent to the LLM at execution time. Write it as a direct task, e.g. 'What is the weather in Berlin today? Give a short summary.'"
                    },
                    "schedule_kind": {
                        "type": "string",
                        "enum": ["at", "cron"],
                        "description": "'at' for one-time execution (ISO-8601 timestamp), 'cron' for recurring execution (cron expression)"
                    },
                    "at": {
                        "type": "string",
                        "description": "Only for schedule_kind='at': ISO-8601 timestamp in UTC. IMPORTANT: convert user-local time to UTC first. Example: user says '19:20' in Europe/Berlin (CET=UTC+1) -> '2026-02-20T18:20:00Z'"
                    },
                    "cron_expr": {
                        "type": "string",
                        "description": "Only for schedule_kind='cron': 5-field cron expression, e.g. '30 6 * * *' for daily at 06:30"
                    },
                    "chat_id": {
                        "type": "integer",
                        "description": "Chat ID where the response should be sent (use the current chat ID)"
                    }
                },
                "required": ["name", "prompt", "schedule_kind", "chat_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed");
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("");
        let schedule_kind = arguments
            .get("schedule_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let chat_id = arguments
            .get("chat_id")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        if prompt.trim().is_empty() {
            return json!({ "ok": false, "error": "Prompt must not be empty." }).to_string();
        }

        if chat_id == 0 {
            return json!({ "ok": false, "error": "chat_id is missing." }).to_string();
        }

        let schedule = match schedule_kind {
            "at" => {
                let at_str = arguments.get("at").and_then(Value::as_str).unwrap_or("");
                match at_str.parse::<DateTime<Utc>>() {
                    Ok(at) => {
                        // Keine Jobs in der Vergangenheit akzeptieren
                        if at < Utc::now() {
                            return json!({
                                "ok": false,
                                "error": format!("Timestamp '{}' is in the past. Please provide a future timestamp.", at_str)
                            })
                            .to_string();
                        }
                        Schedule::At { at }
                    }
                    Err(_) => {
                        return json!({
                            "ok": false,
                            "error": format!("Invalid ISO-8601 timestamp: '{}'", at_str)
                        })
                        .to_string();
                    }
                }
            }
            "cron" => {
                let expr = arguments
                    .get("cron_expr")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if expr.is_empty() {
                    return json!({ "ok": false, "error": "cron_expr is missing." }).to_string();
                }
                // Validieren
                if croner::Cron::new(expr).parse().is_err() {
                    return json!({
                        "ok": false,
                        "error": format!("Invalid cron expression: '{}'", expr)
                    })
                    .to_string();
                }
                Schedule::Cron {
                    expr: expr.to_string(),
                    tz: None,
                }
            }
            _ => {
                return json!({
                    "ok": false,
                    "error": "schedule_kind must be 'at' or 'cron'."
                })
                .to_string();
            }
        };

        let is_one_shot = matches!(schedule, Schedule::At { .. });

        let job = CronJob {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            schedule,
            prompt: prompt.to_string(),
            chat_id,
            enabled: true,
            delete_after_run: is_one_shot,
            last_run: None,
            created_at: Utc::now(),
        };

        let job_id = job.id.clone();
        let job_name = job.name.clone();

        match self.store.add(job).await {
            Ok(_) => json!({
                "ok": true,
                "job_id": job_id,
                "message": format!("Job '{}' created.", job_name),
            })
            .to_string(),
            Err(err) => json!({
                "ok": false,
                "error": format!("Failed to save job: {}", err),
            })
            .to_string(),
        }
    }
}
