use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

use super::http_utils::{fetch_page, strip_html};

/// Skill: Web search via DuckDuckGo.
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

    /// Query the DuckDuckGo Instant Answer API.
    async fn search_ddg(&self, query: &str) -> String {
        println!("[WebSearch] Search started: {}", query);
        let url = format!(
            "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=0",
            urlencoding::encode(query)
        );

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return json!({ "ok": false, "error": format!("HTTP error: {}", e) }).to_string();
            }
        };

        let body = match response.text().await {
            Ok(t) => t,
            Err(e) => {
                return json!({ "ok": false, "error": format!("Failed to read response body: {}", e) })
                    .to_string();
            }
        };

        let ddg: DdgResponse = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                return json!({ "ok": false, "error": format!("JSON parse error: {}", e) })
                    .to_string();
            }
        };

        let mut results = Vec::new();

        if !ddg.r#abstract.is_empty() {
            results.push(json!({
                "type": "abstract",
                "text": ddg.r#abstract,
                "source": ddg.abstract_source,
                "url": ddg.abstract_url,
            }));
        }

        if !ddg.answer.is_empty() {
            results.push(json!({
                "type": "answer",
                "text": ddg.answer,
            }));
        }

        for topic in flatten_related_topics(&ddg.related_topics)
            .into_iter()
            .take(6)
        {
            if !topic.text.is_empty() {
                results.push(json!({
                    "type": "related",
                    "text": topic.text,
                    "url": topic.first_url,
                }));
            }
        }

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
            let html_fallback = self.search_ddg_html(query).await;
            if !html_fallback.is_empty() {
                println!(
                    "[WebSearch] API returned no results, HTML fallback returned {} result(s)",
                    html_fallback.len()
                );
                json!({
                    "ok": true,
                    "query": query,
                    "count": html_fallback.len(),
                    "results": html_fallback,
                    "source": "duckduckgo_html_fallback"
                })
                .to_string()
            } else {
                println!("[WebSearch] No results for: {}", query);
                json!({
                    "ok": true,
                    "results": [],
                    "message": format!("No results for '{}'. Try different wording.", query),
                })
                .to_string()
            }
        } else {
            println!("[WebSearch] API returned {} result(s)", results.len());
            json!({
                "ok": true,
                "query": query,
                "count": results.len(),
                "results": results,
            })
            .to_string()
        }
    }

    async fn search_ddg_html(&self, query: &str) -> Vec<Value> {
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding::encode(query)
        );

        let response = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let body = match response.text().await {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };

        extract_html_results(&body, 6)
    }
}

#[async_trait]
impl Skill for WebSearchSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "web_search".to_string(),
            description: Some(
                "Searches the web and returns ONLY a list of links with short snippets — \
                 it does NOT return full page content. \
                 IMPORTANT: Do NOT use this tool if the user wants actual content such as \
                 recipes, articles, instructions, summaries, or detailed information. \
                 For any of those cases, always use the 'research' tool instead. \
                 Only use web_search when the user explicitly asks for a list of links, \
                 or when you just need a quick URL to pass to web_fetch."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query."
                    },
                    "url": {
                        "type": "string",
                        "description": "Optional: a specific URL to fetch alongside the search."
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
                let search_result = self.search_ddg(q).await;
                let fetch_result = fetch_page(&self.client, u, 4000).await;
                json!({
                    "search": serde_json::from_str::<Value>(&search_result).unwrap_or(Value::Null),
                    "fetch": serde_json::from_str::<Value>(&fetch_result).unwrap_or(Value::Null),
                })
                .to_string()
            }
            (Some(q), None) => self.search_ddg(q).await,
            (None, Some(u)) => fetch_page(&self.client, u, 4000).await,
            (None, None) => {
                json!({ "ok": false, "error": "Either 'query' or 'url' must be provided." })
                    .to_string()
            }
        }
    }
}

// ── HTML helpers (shared via http_utils, only HTML-result extractor kept here) ──

fn extract_html_results(html: &str, max_results: usize) -> Vec<Value> {
    let mut results = Vec::new();
    let mut cursor = 0usize;

    while let Some(pos) = html[cursor..].find("result__a") {
        if results.len() >= max_results {
            break;
        }

        let start = cursor + pos;
        let anchor_start = match html[..start].rfind("<a ") {
            Some(v) => v,
            None => {
                cursor = start + "result__a".len();
                continue;
            }
        };

        let href = match extract_attr_value(&html[anchor_start..], "href") {
            Some(v) => v,
            None => {
                cursor = start + "result__a".len();
                continue;
            }
        };

        let gt_pos = match html[anchor_start..].find('>') {
            Some(v) => anchor_start + v + 1,
            None => break,
        };

        let end_tag = match html[gt_pos..].find("</a>") {
            Some(v) => gt_pos + v,
            None => break,
        };

        let raw_title = &html[gt_pos..end_tag];
        let title = strip_html(raw_title);
        if !title.is_empty() && !href.is_empty() {
            results.push(json!({
                "type": "result",
                "text": title,
                "url": href,
            }));
        }

        cursor = end_tag + 4;
    }

    results
}

fn extract_attr_value(fragment: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = fragment.find(&needle)? + needle.len();
    let end = fragment[start..].find('"')? + start;
    let value = &fragment[start..end];
    Some(
        value
            .replace("&amp;", "&")
            .replace("&#x2F;", "/")
            .replace("&#47;", "/"),
    )
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
    related_topics: Vec<DdgTopicEntry>,
    #[serde(rename = "Results", default)]
    results: Vec<DdgTopic>,
}

#[derive(Debug, Deserialize, Clone)]
struct DdgTopic {
    #[serde(rename = "Text", default)]
    text: String,
    #[serde(rename = "FirstURL", default)]
    first_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DdgTopicEntry {
    Topic(DdgTopic),
    Group(DdgTopicGroup),
}

#[derive(Debug, Deserialize)]
struct DdgTopicGroup {
    #[serde(rename = "Topics", default)]
    topics: Vec<DdgTopic>,
}

fn flatten_related_topics(items: &[DdgTopicEntry]) -> Vec<DdgTopic> {
    let mut out = Vec::new();
    for item in items {
        match item {
            DdgTopicEntry::Topic(topic) => out.push(topic.clone()),
            DdgTopicEntry::Group(group) => {
                for topic in &group.topics {
                    out.push(topic.clone());
                }
            }
        }
    }
    out
}
