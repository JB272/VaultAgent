use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::Skill;
use crate::reasoning::llm_interface::LlmToolDefinition;

/// Ein Skill, der durch ein externes Python-Skript implementiert wird.
///
/// Konvention für das Skript:
///   `python script.py --describe`   → JSON auf stdout: { "name", "description", "parameters" }
///   `python script.py --execute '{...}'` → JSON-Ergebnis auf stdout
///
/// Das Skript wird bei `load_python_skills()` einmal per `--describe` abgefragt
/// und danach bei jedem Tool-Call per `--execute` aufgerufen.
pub struct PythonSkill {
    script_path: PathBuf,
    definition: LlmToolDefinition,
}

#[derive(Debug, Deserialize)]
struct SkriptDescription {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
}

impl PythonSkill {
    /// Lädt ein einzelnes Python-Skript und fragt seine Beschreibung per `--describe` ab.
    pub async fn load(script_path: impl Into<PathBuf>) -> Result<Self, String> {
        let script_path = script_path.into();

        if !script_path.exists() {
            return Err(format!(
                "Python-Skill nicht gefunden: {}",
                script_path.display()
            ));
        }

        let output = Command::new("python3")
            .arg(&script_path)
            .arg("--describe")
            .output()
            .await
            .map_err(|e| format!("Konnte Python-Skill nicht starten: {}", e))?;

        let describe_stdout = String::from_utf8_lossy(&output.stdout);
        let describe_stderr = String::from_utf8_lossy(&output.stderr);
        if !describe_stdout.trim().is_empty() {
            println!(
                "[python_skill:{}] --describe stdout: {}",
                script_path.display(),
                describe_stdout.trim()
            );
        }
        if !describe_stderr.trim().is_empty() {
            eprintln!(
                "[python_skill:{}] --describe stderr: {}",
                script_path.display(),
                describe_stderr.trim()
            );
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Python-Skill --describe fehlgeschlagen ({}): {}",
                script_path.display(),
                stderr.trim()
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let desc: SkriptDescription = serde_json::from_str(stdout.trim()).map_err(|e| {
            format!(
                "Ungültige --describe Antwort von {}: {} (raw: {})",
                script_path.display(),
                e,
                stdout.trim()
            )
        })?;

        let definition = LlmToolDefinition {
            name: desc.name,
            description: desc.description,
            parameters_schema: desc.parameters.unwrap_or_else(|| {
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })
            }),
        };

        Ok(Self {
            script_path,
            definition,
        })
    }
}

#[async_trait]
impl Skill for PythonSkill {
    fn definition(&self) -> LlmToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, arguments: &Value) -> String {
        let args_json = arguments.to_string();

        println!(
            "[python_skill:{}] --execute args: {}",
            self.definition.name, args_json
        );

        let result = Command::new("python3")
            .arg(&self.script_path)
            .arg("--execute")
            .arg(&args_json)
            .output()
            .await;

        match result {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !stdout.trim().is_empty() {
                    println!(
                        "[python_skill:{}] stdout: {}",
                        self.definition.name,
                        stdout.trim()
                    );
                }
                if !stderr.trim().is_empty() {
                    eprintln!(
                        "[python_skill:{}] stderr: {}",
                        self.definition.name,
                        stderr.trim()
                    );
                }

                if output.status.success() {
                    let trimmed = stdout.trim();

                    // Validieren, dass es gültiges JSON ist – falls nicht,
                    // wrappen wir es als { "ok": true, "output": "..." }
                    if serde_json::from_str::<Value>(trimmed).is_ok() {
                        trimmed.to_string()
                    } else {
                        json!({
                            "ok": true,
                            "output": trimmed,
                        })
                        .to_string()
                    }
                } else {
                    json!({
                        "ok": false,
                        "error": format!("Skript beendet mit Code {:?}", output.status.code()),
                        "stderr": stderr.trim(),
                        "stdout": stdout.trim(),
                    })
                    .to_string()
                }
            }
            Err(err) => json!({
                "ok": false,
                "error": format!("Konnte Skript nicht starten: {}", err),
            })
            .to_string(),
        }
    }
}

/// Scannt ein Verzeichnis nach `*.py` Dateien, lädt jede per `--describe`
/// und gibt alle erfolgreich geladenen PythonSkills zurück.
pub async fn load_python_skills(dir: &Path) -> Vec<PythonSkill> {
    let mut skills = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "Python-Skills-Verzeichnis nicht lesbar ({}): {}",
                dir.display(),
                err
            );
            return skills;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("py") {
            match PythonSkill::load(&path).await {
                Ok(skill) => {
                    println!(
                        "  Python-Skill geladen: {} ({})",
                        skill.definition.name,
                        path.display()
                    );
                    skills.push(skill);
                }
                Err(err) => {
                    eprintln!("  Python-Skill übersprungen: {}", err);
                }
            }
        }
    }

    skills
}
