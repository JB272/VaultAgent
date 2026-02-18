use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vaultagent_policy::{check_file_size, check_fs_path, check_tool_enabled, PolicyConfig};

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub input: Value,
}

pub fn execute_tool(
    call: &ToolCall,
    enabled_tools: &[String],
    policy: &PolicyConfig,
) -> anyhow::Result<Value> {
    check_tool_enabled(enabled_tools, &call.name).map_err(anyhow::Error::from)?;

    match call.name.as_str() {
        "fs.read_file" => {
            let path = call
                .input
                .get("path")
                .and_then(|v| v.as_str())
                .context("fs.read_file requires input.path (string)")?;

            let resolved = check_fs_path(policy, Path::new(path)).map_err(anyhow::Error::from)?;
            let metadata = fs::metadata(&resolved)
                .with_context(|| format!("cannot stat file: {}", resolved.display()))?;
            check_file_size(policy, metadata.len()).map_err(anyhow::Error::from)?;

            let content = fs::read_to_string(&resolved)
                .with_context(|| format!("cannot read file: {}", resolved.display()))?;
            Ok(serde_json::json!({ "path": resolved.display().to_string(), "content": content }))
        }
        _ => anyhow::bail!("unknown tool: {}", call.name),
    }
}
