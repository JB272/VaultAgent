use serde_json::json;

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
};
use crate::skills::SkillRegistry;

/// Der Agent orchestriert LLM-Aufrufe und Tool-Ausführungen.
/// Er nimmt eine User-Nachricht entgegen, führt bei Bedarf mehrere Tool-Runden durch
/// und gibt die finale Antwort zurück.
pub struct Agent {
    llm: Option<Box<dyn LlmInterface>>,
    skills: SkillRegistry,
    system_prompt: String,
    max_rounds: usize,
}

impl Agent {
    pub fn new(llm: Option<Box<dyn LlmInterface>>, skills: SkillRegistry) -> Self {
        Self {
            llm,
            skills,
            system_prompt: "Du bist ein Coding-Agent. Wenn der Nutzer Dateien lesen oder \
                schreiben möchte, nutze die bereitgestellten Tools. Verwende nur relative \
                Pfade ohne '..'. Gib nach Tool-Nutzung eine kurze Bestätigung auf Deutsch \
                zurück."
                .to_string(),
            max_rounds: 4,
        }
    }

    /// Verarbeitet eine Chat-Nachricht und gibt die Antwort des Agenten zurück.
    /// Führt bei Bedarf bis zu `max_rounds` Tool-Aufrufe-Zyklen durch.
    pub async fn process(&self, user_text: &str) -> String {
        let Some(llm) = &self.llm else {
            return "LLM ist nicht konfiguriert. Setze LLM_API_KEY, um Antworten zu erhalten."
                .to_string();
        };

        let mut messages = vec![
            LlmMessage {
                role: LlmRole::Developer,
                content: LlmMessageContent::Text(self.system_prompt.clone()),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            },
            LlmMessage {
                role: LlmRole::User,
                content: LlmMessageContent::Text(user_text.to_string()),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            },
        ];

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
                    return response
                        .refusal
                        .unwrap_or_else(|| "Keine Antwort vom LLM erhalten.".to_string());
                }
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
