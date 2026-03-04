use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::reasoning::agent::Agent;
use crate::reasoning::llm_interface::{LlmInterface, LlmToolDefinition};
use crate::skills::Skill;
use crate::skills::SkillRegistry;
use crate::skills::RemoteSkillProxy;

/// Skill: Spawns a focused research subagent.
///
/// The subagent has access to `web_search` and `web_fetch`, runs for up to
/// 8 rounds, and returns a synthesised, cited answer. Use this whenever you
/// need detailed information from the web — not just a list of links.
///
/// Web research tool calls are always routed through the Docker worker.
pub struct ResearchSkill {
    llm: Arc<dyn LlmInterface>,
    remote: Option<RemoteSkillProxy>,
}

impl ResearchSkill {
    /// Create a ResearchSkill. Use `with_remote()` before executing.
    pub fn new(llm: Arc<dyn LlmInterface>) -> Self {
        Self { llm, remote: None }
    }

    /// Attach the Docker worker proxy used for all tool calls.
    pub fn with_remote(mut self, remote: RemoteSkillProxy) -> Self {
        self.remote = Some(remote);
        self
    }
}

#[async_trait]
impl Skill for ResearchSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "research".to_string(),
            description: Some(
                "Use this tool whenever the user wants actual information from the web — \
                 for example: recipes, how-to guides, news, documentation, product details, \
                 comparisons, or any question that requires reading a webpage. \
                 This tool automatically searches the web AND reads the most relevant pages, \
                 then returns a detailed, cited answer. \
                 Examples of when to use research: 'find me a recipe', 'what is X', \
                 'how does Y work', 'latest news on Z', 'compare A and B'. \
                 Do NOT use web_search for these — always use research."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The research question or topic to investigate in detail."
                    },
                    "language": {
                        "type": "string",
                        "description": "The language to write the answer in, e.g. 'German', 'English', 'French'. Must match the language the user is writing in."
                    }
                },
                "required": ["task", "language"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let task = match arguments.get("task").and_then(Value::as_str) {
            Some(t) => t,
            None => return json!({ "ok": false, "error": "'task' is required." }).to_string(),
        };

        let language = arguments
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("English");

        println!(
            "[Research] Spawning subagent for: {} (language: {})",
            task, language
        );

        let system_prompt = format!(
            "You are a focused web research assistant. Your only job is to answer the given \
             research task thoroughly and accurately.\n\
             \n\
             IMPORTANT: You MUST write your entire answer in {language}. \
             Do not use any other language, regardless of the source language of the web pages you read.\n\
             \n\
             Guidelines:\n\
             1. Use web_search to find relevant pages for the topic.\n\
             2. Use web_fetch on at least 1-2 of the most relevant URLs to read the actual content.\n\
             3. Synthesise the information into a clear, concise answer in {language}.\n\
             4. Always include source URLs inline (Markdown links).\n\
             5. If the first search yields no useful results, try a different query.\n\
             6. Do NOT just list links — always read and summarise the content.",
            language = language
        );

        let proxy = match &self.remote {
            Some(proxy) => proxy.clone(),
            None => {
                return json!({
                    "ok": false,
                    "error": "research requires a configured RemoteSkillProxy (Docker worker).",
                })
                .to_string();
            }
        };

        // Route all tool calls through the sandbox worker.
        let sub_skills = SkillRegistry::new_with_remote(proxy);

        let sub_agent = Agent::subagent(Arc::clone(&self.llm), sub_skills, system_prompt);

        // Pass the task as the user message so the subagent processes it naturally.
        let result = sub_agent.process(task, 0, None).await;

        println!("[Research] Subagent done");

        json!({
            "ok": true,
            "task": task,
            "result": result,
        })
        .to_string()
    }
}
