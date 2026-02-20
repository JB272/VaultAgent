use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
};
use crate::skills::SkillRegistry;
use crate::soul::Soul;

/// Der Agent orchestriert LLM-Aufrufe und Tool-Ausführungen.
/// Er hält eine persistente Conversation-History und baut den System-Prompt
/// dynamisch aus der Soul (Persönlichkeit + Gedächtnis).
pub struct Agent {
    llm: Option<Box<dyn LlmInterface>>,
    skills: SkillRegistry,
    soul: Arc<Soul>,
    history: Mutex<Vec<LlmMessage>>,
    max_rounds: usize,
    max_history: usize,
}

impl Agent {
    pub fn new(llm: Option<Box<dyn LlmInterface>>, skills: SkillRegistry, soul: Arc<Soul>) -> Self {
        Self {
            llm,
            skills,
            soul,
            history: Mutex::new(Vec::new()),
            max_rounds: 4,
            max_history: 50,
        }
    }

    /// Verarbeitet eine Chat-Nachricht und gibt die Antwort des Agenten zurück.
    /// Führt bei Bedarf bis zu `max_rounds` Tool-Aufrufe-Zyklen durch.
    /// Die Conversation-History bleibt über Aufrufe hinweg erhalten.
    /// `chat_id` wird als Kontext mitgegeben, damit Skills wie cron_add wissen,
    /// an welchen Chat die Antwort gehen soll.
    pub async fn process(&self, user_text: &str, chat_id: i64) -> String {
        let Some(llm) = &self.llm else {
            return "LLM ist nicht konfiguriert. Setze LLM_API_KEY, um Antworten zu erhalten."
                .to_string();
        };

        // User-Nachricht an die persistente History anhängen
        {
            let mut history = self.history.lock().await;
            history.push(LlmMessage {
                role: LlmRole::User,
                content: LlmMessageContent::Text(user_text.to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            });

            // Sliding Window: älteste Nachrichten kürzen
            if history.len() > self.max_history {
                let excess = history.len() - self.max_history;
                history.drain(0..excess);
            }
        }

        // System-Prompt dynamisch aus Soul bauen (Persönlichkeit + Memory-Kontext)
        let base_prompt = self.soul.system_prompt();
        let system_prompt = format!(
            "{}\n\n## Aktuelle Session\n- Chat-ID: {}\n- Wenn du cron_add aufrufst, verwende diese chat_id.",
            base_prompt, chat_id
        );

        let mut messages = vec![LlmMessage {
            role: LlmRole::Developer,
            content: LlmMessageContent::Text(system_prompt),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }];

        // Gesamte History anhängen
        {
            let history = self.history.lock().await;
            messages.extend(history.clone());
        }

        for _ in 0..self.max_rounds {
            let mut request = LlmChatRequest::new("", messages.clone());
            request.tools = self.skills.tool_definitions();

            let response = match llm.chat(request).await {
                Ok(value) => value,
                Err(err) => return format!("Fehler beim LLM-Aufruf: {}", err),
            };

            // Keine Tool-Calls → fertige Antwort
            if response.tool_calls.is_empty() {
                let content = response.content.trim();
                if content.is_empty() {
                    let fallback = response
                        .refusal
                        .unwrap_or_else(|| "Keine Antwort vom LLM erhalten.".to_string());
                    // Antwort in History speichern
                    self.history.lock().await.push(LlmMessage {
                        role: LlmRole::Assistant,
                        content: LlmMessageContent::Text(fallback.clone()),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                    return fallback;
                }
                // Antwort in History speichern
                self.history.lock().await.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: LlmMessageContent::Text(content.to_string()),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                });
                return content.to_string();
            }

            // Tool-Calls ausführen
            let tool_calls = response.tool_calls.clone();
            messages.push(LlmMessage {
                role: LlmRole::Assistant,
                content: LlmMessageContent::Text(response.content),
                name: None,
                tool_call_id: None,
                tool_calls,
            });

            for tool_call in response.tool_calls {
                let result = match self
                    .skills
                    .execute(&tool_call.name, &tool_call.arguments)
                    .await
                {
                    Some(result) => result,
                    None => json!({
                        "ok": false,
                        "error": format!("Unbekanntes Tool: {}", tool_call.name),
                    })
                    .to_string(),
                };

                messages.push(LlmMessage {
                    role: LlmRole::Tool,
                    content: LlmMessageContent::Text(result),
                    name: Some(tool_call.name),
                    tool_call_id: tool_call.id,
                    tool_calls: Vec::new(),
                });
            }
        }

        "Ich konnte die Tool-Ausführung nicht abschließen (zu viele Schritte).".to_string()
    }
}
