use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::reasoning::agent::Agent;
use crate::reasoning::llm_interface::{LlmInterface, LlmToolDefinition};
use crate::skills::RemoteSkillProxy;
use crate::skills::Skill;
use crate::skills::SkillRegistry;

/// Skill: Spawns a general-purpose subagent that can execute multi-step tasks.
///
/// The subagent inherits all sandbox tools (shell_execute, write_file, read_file,
/// web_search, web_fetch, etc.) and runs for up to 15 rounds. Use this to
/// delegate complex tasks that require multiple steps without cluttering the
/// main conversation.
pub struct SpawnSubagentSkill {
    llm: Arc<dyn LlmInterface>,
    remote: Option<RemoteSkillProxy>,
}

impl SpawnSubagentSkill {
    pub fn new(llm: Arc<dyn LlmInterface>) -> Self {
        Self { llm, remote: None }
    }

    pub fn with_remote(mut self, remote: RemoteSkillProxy) -> Self {
        self.remote = Some(remote);
        self
    }
}

#[async_trait]
impl Skill for SpawnSubagentSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "spawn_subagent".to_string(),
            description: Some(
                "Spawns a focused subagent to perform a complex, multi-step task autonomously. \
                 The subagent has access to all sandbox tools (shell_execute, write_file, read_file, \
                 web_search, web_fetch, list_directory, etc.) and can execute up to 15 tool-call rounds. \
                 Use this when a task requires multiple steps (e.g. write a script, run it, analyze output, \
                 fix errors, re-run) and you want to delegate the entire workflow. \
                 The subagent works silently and returns only the final result. \
                 Examples: 'set up a Python project and run tests', 'analyze this CSV file and create a summary', \
                 'install dependencies and build a project'."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "A detailed description of the task the subagent should accomplish. Be specific about what the expected output should be."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional additional context, e.g. file paths, data, or constraints the subagent should know about."
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let task = match arguments.get("task").and_then(Value::as_str) {
            Some(t) => t,
            None => return json!({ "ok": false, "error": "'task' is required." }).to_string(),
        };

        let context = arguments
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");

        println!("[SpawnSubagent] Task: {}", task);

        let system_prompt = format!(
            "You are a focused task execution agent. Your job is to accomplish the given task \
             completely and return the result.\n\
             \n\
             Rules:\n\
             1. USE your tools to actually DO the work — do not describe what you would do.\n\
             2. If a step fails, analyze the error and try a different approach.\n\
             3. When the task is complete, provide a concise summary of what was done and the result.\n\
             4. You have access to shell_execute, file operations, and web tools.\n\
             5. Work step by step: execute one action, check the result, then proceed.\n\
             6. If you need to write and run code, write it to a file first, then execute it.\n\
             7. Never claim missing permissions unless a tool actually returned a concrete permission error.\n\
             8. Use relative paths in /workspace and create missing directories automatically.\n\
             9. For file organization tasks, use file_move or shell_execute (mkdir -p + mv/cp) and then verify with list_directory.\n\
             {context_section}",
            context_section = if context.is_empty() {
                String::new()
            } else {
                format!("\nAdditional context:\n{}", context)
            }
        );

        // Build the subagent's skill registry with all sandbox tools.
        let sub_skills = match &self.remote {
            Some(proxy) => SkillRegistry::new_with_remote(proxy.clone()),
            None => SkillRegistry::new(),
        };

        let sub_agent = Agent::subagent(Arc::clone(&self.llm), sub_skills, system_prompt);

        let result = sub_agent.process(task, 0, None).await;

        println!("[SpawnSubagent] Done");

        json!({
            "ok": true,
            "task": task,
            "result": result,
        })
        .to_string()
    }
}
