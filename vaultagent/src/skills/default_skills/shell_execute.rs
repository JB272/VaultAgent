use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

/// Skill: Executes a shell command inside the sandbox container.
///
/// Runs with `/bin/sh -c` so pipes, redirects, and chaining work.
/// Stdout and stderr are captured and returned.  A timeout prevents
/// runaway commands from blocking the worker.
pub struct ShellExecuteSkill;

fn is_package_manager_command(cmd: &str) -> bool {
    let c = cmd.trim_start().to_lowercase();
    c.starts_with("apt-get")
        || c.starts_with("apt ")
        || c.starts_with("dpkg")
        || c.starts_with("pip ")
        || c.starts_with("pip3 ")
        || c.starts_with("python -m pip")
        || c.starts_with("python3 -m pip")
}

fn has_sudo_prefix(cmd: &str) -> bool {
    cmd.trim_start().to_lowercase().starts_with("sudo ")
}

fn looks_like_permission_error(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("permission denied")
        || s.contains("are you root")
        || s.contains("could not open lock file")
        || s.contains("/var/lib/apt/lists/partial is missing")
}

async fn run_shell(command: &str, working_dir: &str) -> Result<std::process::Output, std::io::Error> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .output()
        .await
}

#[async_trait]
impl Skill for ShellExecuteSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "shell_execute".to_string(),
            description: Some(
                "Executes a shell command inside the sandbox and returns stdout + stderr. \
                 Use this for tasks like installing packages (sudo apt-get install -y <pkg>), \
                 running scripts, checking system info, data processing with CLI tools, \
                 compiling code, or any task that benefits from shell access. \
                 Use 'sudo' for apt-get/dpkg/pip when installing system packages. \
                 Commands run as an unprivileged user in a Docker container with no access \
                 to the host system. The working directory is /workspace. \
                 Timeout: 120 seconds. Output is truncated to ~8000 chars."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute, e.g. 'ls -la' or 'pip install requests && python script.py'"
                    },
                    "working_dir": {
                        "type": "string",
                        "description": "Optional working directory (relative to /workspace or absolute). Defaults to /workspace."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let command = match arguments.get("command").and_then(Value::as_str) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return json!({ "ok": false, "error": "'command' is required and must not be empty." })
                    .to_string();
            }
        };

        let working_dir = arguments
            .get("working_dir")
            .and_then(Value::as_str)
            .unwrap_or("/workspace");

        println!("[ShellExecute] Running: {}", command);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            run_shell(command, working_dir),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let first_stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let first_exit_code = output.status.code().unwrap_or(-1);

                // Auto-retry once with sudo for package-manager commands if permissions failed.
                if first_exit_code != 0
                    && is_package_manager_command(command)
                    && !has_sudo_prefix(command)
                    && looks_like_permission_error(&first_stderr)
                {
                    let sudo_command = format!("sudo {}", command.trim_start());
                    println!("[ShellExecute] Permission error detected, retrying with sudo: {}", sudo_command);

                    let sudo_result = tokio::time::timeout(
                        std::time::Duration::from_secs(120),
                        run_shell(&sudo_command, working_dir),
                    )
                    .await;

                    match sudo_result {
                        Ok(Ok(sudo_output)) => {
                            let stdout = String::from_utf8_lossy(&sudo_output.stdout);
                            let stderr = String::from_utf8_lossy(&sudo_output.stderr);
                            let exit_code = sudo_output.status.code().unwrap_or(-1);

                            let max_len = 8000;
                            let stdout_truncated = truncate_str(&stdout, max_len);
                            let stderr_truncated = truncate_str(&stderr, max_len / 2);

                            return json!({
                                "ok": exit_code == 0,
                                "exit_code": exit_code,
                                "stdout": stdout_truncated,
                                "stderr": stderr_truncated,
                                "retried_with_sudo": true,
                                "original_command": command,
                                "executed_command": sudo_command,
                            })
                            .to_string();
                        }
                        Ok(Err(err)) => {
                            return json!({
                                "ok": false,
                                "error": format!("Failed to execute sudo retry: {}", err),
                                "retried_with_sudo": true,
                            })
                            .to_string();
                        }
                        Err(_) => {
                            return json!({
                                "ok": false,
                                "error": "Sudo retry timed out after 120 seconds.",
                                "retried_with_sudo": true,
                            })
                            .to_string();
                        }
                    }
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = first_exit_code;

                // Truncate long output to avoid blowing up the LLM context
                let max_len = 8000;
                let stdout_truncated = truncate_str(&stdout, max_len);
                let stderr_truncated = truncate_str(&stderr, max_len / 2);

                println!(
                    "[ShellExecute] Exit code: {} | stdout: {} bytes | stderr: {} bytes",
                    exit_code,
                    stdout.len(),
                    stderr.len()
                );

                json!({
                    "ok": exit_code == 0,
                    "exit_code": exit_code,
                    "stdout": stdout_truncated,
                    "stderr": stderr_truncated,
                })
                .to_string()
            }
            Ok(Err(err)) => json!({
                "ok": false,
                "error": format!("Failed to execute command: {}", err),
            })
            .to_string(),
            Err(_) => json!({
                "ok": false,
                "error": "Command timed out after 120 seconds.",
            })
            .to_string(),
        }
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…\n[truncated, {} total bytes]", &s[..max_len], s.len())
    }
}
