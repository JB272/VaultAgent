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
                "Plant eine zeitgesteuerte Aufgabe. Der Agent wird zum geplanten Zeitpunkt \
                 mit dem angegebenen Prompt geweckt und die Antwort wird im Chat angezeigt. \
                 Nutze dies für Erinnerungen, tägliche Zusammenfassungen, Wetterberichte etc."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Kurzer Name für den Job, z.B. 'Wetterbericht morgens'"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Der Prompt, der zum geplanten Zeitpunkt an das LLM gesendet wird. Schreibe ihn so, als würdest du eine Aufgabe stellen, z.B. 'Wie wird das Wetter heute in Berlin? Gib eine kurze Zusammenfassung.'"
                    },
                    "schedule_kind": {
                        "type": "string",
                        "enum": ["at", "cron"],
                        "description": "'at' für einmalig (ISO-8601-Zeitpunkt), 'cron' für wiederkehrend (Cron-Ausdruck)"
                    },
                    "at": {
                        "type": "string",
                        "description": "Nur bei schedule_kind='at': ISO-8601-Zeitpunkt in UTC. WICHTIG: Rechne die vom Nutzer genannte Lokalzeit in UTC um! Beispiel: Nutzer sagt '19:20' in Europe/Berlin (CET=UTC+1) → '2026-02-20T18:20:00Z'"
                    },
                    "cron_expr": {
                        "type": "string",
                        "description": "Nur bei schedule_kind='cron': 5-Feld-Cron-Ausdruck, z.B. '30 6 * * *' für täglich um 6:30"
                    },
                    "chat_id": {
                        "type": "integer",
                        "description": "Die Chat-ID, an die die Antwort gesendet wird (aktuelle Chat-ID verwenden)"
                    }
                },
                "required": ["name", "prompt", "schedule_kind", "chat_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let name = arguments.get("name").and_then(Value::as_str).unwrap_or("Unnamed");
        let prompt = arguments.get("prompt").and_then(Value::as_str).unwrap_or("");
        let schedule_kind = arguments.get("schedule_kind").and_then(Value::as_str).unwrap_or("");
        let chat_id = arguments.get("chat_id").and_then(Value::as_i64).unwrap_or(0);

        if prompt.trim().is_empty() {
            return json!({ "ok": false, "error": "Prompt darf nicht leer sein." }).to_string();
        }

        if chat_id == 0 {
            return json!({ "ok": false, "error": "chat_id fehlt." }).to_string();
        }

        let schedule = match schedule_kind {
            "at" => {
                let at_str = arguments.get("at").and_then(Value::as_str).unwrap_or("");
                match at_str.parse::<DateTime<Utc>>() {
                    Ok(at) => Schedule::At { at },
                    Err(_) => {
                        return json!({
                            "ok": false,
                            "error": format!("Ungültiges ISO-8601-Datum: '{}'", at_str)
                        }).to_string();
                    }
                }
            }
            "cron" => {
                let expr = arguments.get("cron_expr").and_then(Value::as_str).unwrap_or("");
                if expr.is_empty() {
                    return json!({ "ok": false, "error": "cron_expr fehlt." }).to_string();
                }
                // Validieren
                if croner::Cron::new(expr).parse().is_err() {
                    return json!({
                        "ok": false,
                        "error": format!("Ungültiger Cron-Ausdruck: '{}'", expr)
                    }).to_string();
                }
                Schedule::Cron {
                    expr: expr.to_string(),
                    tz: None,
                }
            }
            _ => {
                return json!({
                    "ok": false,
                    "error": "schedule_kind muss 'at' oder 'cron' sein."
                }).to_string();
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
                "message": format!("Job '{}' wurde erstellt.", job_name),
            }).to_string(),
            Err(err) => json!({
                "ok": false,
                "error": format!("Job konnte nicht gespeichert werden: {}", err),
            }).to_string(),
        }
    }
}
