mod gateway;
mod reasoning;
mod skills;

use gateway::com::{telegram::setup_telegram, website::setup_website};
use gateway::incoming_actions_queue::{IncomingAction, IncomingActionQueue};
use reasoning::llm_apis::openai::OpenAiCompatibleClient;
use reasoning::llm_interface::{
    LlmChatRequest, LlmInterface, LlmMessage, LlmMessageContent, LlmRole,
};
use serde_json::{Value, json};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    if dotenvy::dotenv().is_err() {
        let _ = dotenvy::from_filename("vaultagent/.env");
    }

    let incoming_actions = IncomingActionQueue::new();

    let website_setup = setup_website(incoming_actions.register_service()).await?;
    let website_client = website_setup.client;
    let website_chat_id = website_setup.chat_id;

    let telegram = setup_telegram(incoming_actions.register_service()).await;

    let llm = match OpenAiCompatibleClient::from_env() {
        Ok(client) => {
            println!("LLM aktiv: {}", client.provider_name());
            Some(client)
        }
        Err(err) => {
            eprintln!("LLM deaktiviert: {}", err);
            None
        }
    };
    loop {
        let action = incoming_actions.pop().await;
        match action {
            IncomingAction::Chat(chat) => {
                println!("Chat-Nachricht von {}: {}", chat.chat_id, chat.text);

                if chat.chat_id != website_chat_id {
                    if let Some(telegram) = telegram.as_ref() {
                        if let Err(err) = telegram.send_chat_action(chat.chat_id, "typing").await {
                            eprintln!("Konnte Telegram-Typing nicht senden: {}", err);
                        }
                    }
                }

                if let Err(err) = website_client.set_typing(true).await {
                    eprintln!("Konnte Assistant-Typing nicht setzen: {}", err);
                }

                let reply = generate_reply_with_tools(llm.as_ref(), &chat.text).await;

                if let Err(err) = website_client.push_assistant_message(&reply).await {
                    eprintln!("Konnte Website-Chat nicht updaten: {}", err);
                }

                if let Err(err) = website_client.set_typing(false).await {
                    eprintln!("Konnte Assistant-Typing nicht zurücksetzen: {}", err);
                }

                if chat.chat_id != website_chat_id {
                    if let Some(telegram) = telegram.as_ref() {
                        if let Err(err) = telegram.send_message(chat.chat_id, &reply).await {
                            eprintln!("Konnte Telegram-Antwort nicht senden: {}", err);
                        }
                    }
                }
            }
            IncomingAction::Agent(_) => {}
            IncomingAction::Chron(_) => {}
        }
    }
}

async fn generate_reply_with_tools(
    llm: Option<&OpenAiCompatibleClient>,
    user_text: &str,
) -> String {
    let Some(llm) = llm else {
        return "LLM ist nicht konfiguriert. Setze LLM_API_KEY/OPENAI_API_KEY, um Antworten zu erhalten.".to_string();
    };

    let mut messages = vec![
        LlmMessage {
            role: LlmRole::Developer,
            content: LlmMessageContent::Text(
                "Du bist ein Coding-Agent. Wenn der Nutzer Dateien lesen oder schreiben möchte, nutze die bereitgestellten Tools read_file und write_file. Verwende nur relative Pfade ohne '..'. Gib nach Tool-Nutzung eine kurze Bestätigung auf Deutsch zurück.".to_string(),
            ),
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

    for _ in 0..4 {
        let mut request = LlmChatRequest::new("", messages.clone());
        request.tools = tool_definitions();

        let response = match llm.chat(request).await {
            Ok(value) => value,
            Err(err) => return format!("Fehler beim LLM-Aufruf: {}", err),
        };

        if response.tool_calls.is_empty() {
            let content = response.content.trim();
            if content.is_empty() {
                return response
                    .refusal
                    .unwrap_or_else(|| "Keine Antwort vom LLM erhalten.".to_string());
            }
            return content.to_string();
        }

        let tool_calls = response.tool_calls.clone();
        messages.push(LlmMessage {
            role: LlmRole::Assistant,
            content: LlmMessageContent::Text(response.content),
            name: None,
            tool_call_id: None,
            tool_calls,
        });

        for tool_call in response.tool_calls {
            let tool_result = execute_tool_call(&tool_call.name, &tool_call.arguments).await;
            messages.push(LlmMessage {
                role: LlmRole::Tool,
                content: LlmMessageContent::Text(tool_result),
                name: Some(tool_call.name),
                tool_call_id: tool_call.id,
                tool_calls: Vec::new(),
            });
        }
    }

    "Ich konnte die Tool-Ausführung nicht abschließen (zu viele Schritte).".to_string()
}

fn tool_definitions() -> Vec<reasoning::llm_interface::LlmToolDefinition> {
    vec![
        reasoning::llm_interface::LlmToolDefinition {
            name: "read_file".to_string(),
            description: Some(
                "Liest eine Textdatei aus einem relativen Pfad im Workspace.".to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relativer Dateipfad, z.B. notes/test.txt"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        reasoning::llm_interface::LlmToolDefinition {
            name: "write_file".to_string(),
            description: Some(
                "Schreibt Inhalt in eine Datei im Workspace. Erstellt Datei/Ordner falls nötig."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relativer Dateipfad, z.B. test.txt"
                    },
                    "content": {
                        "type": "string",
                        "description": "Inhalt, der in die Datei geschrieben werden soll"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
    ]
}

async fn execute_tool_call(name: &str, arguments: &Value) -> String {
    match name {
        "read_file" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            skills::read_file::execute(path).await
        }
        "write_file" => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            skills::write_file::execute(path, content).await
        }
        _ => json!({
            "ok": false,
            "error": format!("Unbekanntes Tool: {}", name),
        })
        .to_string(),
    }
}
