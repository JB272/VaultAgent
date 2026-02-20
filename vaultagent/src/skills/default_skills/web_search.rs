use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Websuche via DuckDuckGo + optional direkter URL-Abruf.
pub struct WebSearchSkill {
    client: Client,
}

impl WebSearchSkill {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("VaultAgent/1.0")
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    /// DuckDuckGo Instant Answer API abfragen.
    async fn search_ddg(&self, query: &str) -> String {
        let url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
            urlencoding::encode(query)
        );

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return json!({ "ok": false, "error": format!("HTTP-Fehler: {}", e) }).to_string()
            }
        };

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => {
                return json!({ "ok": false, "error": format!("Body lesen fehlgeschlagen: {}", e) })
                    .to_string()
            }
        };

        let ddg: DdgResponse = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                return json!({ "ok": false, "error": format!("JSON-Parse-Fehler: {}", e) })
                    .to_string()
            }
        };

        let mut results = Vec::new();

        // Abstract (Hauptantwort)
        if !ddg.r#abstract.is_empty() {
            results.push(json!({
                "type": "abstract",
                "text": ddg.r#abstract,
                "source": ddg.abstract_source,
                "url": ddg.abstract_url,
            }));
        }

        // Answer (direkte Antwort)
        if !ddg.answer.is_empty() {
            results.push(json!({
                "type": "answer",
                "text": ddg.answer,
            }));
        }

        // Related Topics
        for topic in ddg.related_topics.iter().take(5) {
            if !topic.text.is_empty() {
                results.push(json!({
                    "type": "related",
                    "text": topic.text,
                    "url": topic.first_url,
                }));
            }
        }

        // Results
        for result in ddg.results.iter().take(5) {
            if !result.text.is_empty() {
                results.push(json!({
                    "type": "result",
                    "text": result.text,
                    "url": result.first_url,
                }));
            }
        }

        if results.is_empty() {
            json!({
                "ok": true,
                "results": [],
                "message": format!("Keine Ergebnisse für '{}'. Versuche eine andere Formulierung oder nutze eine URL direkt.", query),
            })
            .to_string()
        } else {
            json!({
                "ok": true,
                "query": query,
                "count": results.len(),
                "results": results,
            })
            .to_string()
        }
    }

    /// Eine URL abrufen und den Textinhalt extrahieren.
    async fn fetch_url(&self, url: &str) -> String {
        let response = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                return json!({ "ok": false, "error": format!("HTTP-Fehler: {}", e) }).to_string()
            }
        };

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return json!({
                "ok": false,
                "error": format!("HTTP {}", status),
            })
            .to_string();
        }

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => {
                return json!({ "ok": false, "error": format!("Body lesen fehlgeschlagen: {}", e) })
                    .to_string()
            }
        };

        // Einfache HTML-zu-Text Extraktion
        let text = strip_html(&body);

        // Auf vernünftige Länge kürzen (max ~4000 Zeichen)
        let truncated = if text.len() > 4000 {
            format!("{}...\n[gekürzt, {} Zeichen gesamt]", &text[..4000], text.len())
        } else {
            text
        };

        json!({
            "ok": true,
            "url": url,
            "status": status,
            "content": truncated,
        })
        .to_string()
    }
}

#[async_trait]
impl Skill for WebSearchSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "web_search".to_string(),
            description: Some(
                "Sucht im Web nach Informationen oder ruft eine URL ab. \
                 Nutze 'query' für eine Websuche oder 'url' um eine bestimmte Seite zu lesen. \
                 Beides gleichzeitig ist auch möglich."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Suchbegriff für die Websuche."
                    },
                    "url": {
                        "type": "string",
                        "description": "URL einer Webseite, deren Inhalt abgerufen werden soll."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let query = arguments.get("query").and_then(Value::as_str);
        let url = arguments.get("url").and_then(Value::as_str);

        match (query, url) {
            (Some(q), Some(u)) => {
                // Beides: Suche + URL-Abruf
                let search_result = self.search_ddg(q).await;
                let fetch_result = self.fetch_url(u).await;
                json!({
                    "search": serde_json::from_str::<Value>(&search_result).unwrap_or(Value::Null),
                    "fetch": serde_json::from_str::<Value>(&fetch_result).unwrap_or(Value::Null),
                })
                .to_string()
            }
            (Some(q), None) => self.search_ddg(q).await,
            (None, Some(u)) => self.fetch_url(u).await,
            (None, None) => {
                json!({ "ok": false, "error": "Entweder 'query' oder 'url' muss angegeben werden." })
                    .to_string()
            }
        }
    }
}

// ── HTML → Text ─────────────────────────────────────────

fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut last_was_whitespace = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if starts_with_at(&lower_chars, i, "<script") {
            in_script = true;
            in_tag = true;
        } else if starts_with_at(&lower_chars, i, "</script") {
            in_script = false;
            in_tag = true;
        } else if starts_with_at(&lower_chars, i, "<style") {
            in_style = true;
            in_tag = true;
        } else if starts_with_at(&lower_chars, i, "</style") {
            in_style = false;
            in_tag = true;
        } else if chars[i] == '<' {
            in_tag = true;
        }

        if chars[i] == '>' && in_tag {
            in_tag = false;
            i += 1;
            continue;
        }

        if !in_tag && !in_script && !in_style {
            let ch = chars[i];
            if ch.is_whitespace() {
                if !last_was_whitespace {
                    result.push(' ');
                    last_was_whitespace = true;
                }
            } else {
                result.push(ch);
                last_was_whitespace = false;
            }
        }

        i += 1;
    }

    // HTML-Entities dekodieren
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn starts_with_at(chars: &[char], pos: usize, needle: &str) -> bool {
    let needle_chars: Vec<char> = needle.chars().collect();
    if pos + needle_chars.len() > chars.len() {
        return false;
    }
    for (j, nc) in needle_chars.iter().enumerate() {
        if chars[pos + j] != *nc {
            return false;
        }
    }
    true
}

// ── DuckDuckGo API Structs ──────────────────────────────

#[derive(Debug, Deserialize)]
struct DdgResponse {
    #[serde(rename = "Abstract", default)]
    r#abstract: String,
    #[serde(rename = "AbstractSource", default)]
    abstract_source: String,
    #[serde(rename = "AbstractURL", default)]
    abstract_url: String,
    #[serde(rename = "Answer", default)]
    answer: String,
    #[serde(rename = "RelatedTopics", default)]
    related_topics: Vec<DdgTopic>,
    #[serde(rename = "Results", default)]
    results: Vec<DdgTopic>,
}

#[derive(Debug, Deserialize)]
struct DdgTopic {
    #[serde(rename = "Text", default)]
    text: String,
    #[serde(rename = "FirstURL", default)]
    first_url: String,
}
