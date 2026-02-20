pub mod default_skills;
pub mod python_skill;

use async_trait::async_trait;
use serde_json::Value;

use crate::reasoning::llm_interface::LlmToolDefinition;

/// Jeder Skill beschreibt sich selbst (Tool-Definition für das LLM)
/// und kann mit beliebigen JSON-Argumenten ausgeführt werden.
#[async_trait]
pub trait Skill: Send + Sync {
    /// Gibt die Tool-/Funktions-Definition zurück (Name, Beschreibung, Parameter-Schema).
    fn definition(&self) -> LlmToolDefinition;

    /// Führt den Skill mit den gegebenen Argumenten aus und gibt das Ergebnis als JSON-String zurück.
    async fn execute(&self, arguments: &Value) -> String;
}

/// Registry, in die man Skills per `.add(MySkill)` einhängen kann.
/// Liefert automatisch die Tool-Definitionen fürs LLM und dispatcht Tool-Calls.
pub struct SkillRegistry {
    skills: Vec<Box<dyn Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Skill registrieren – Builder-Pattern, gibt `&mut Self` zurück.
    pub fn add<S: Skill + 'static>(&mut self, skill: S) -> &mut Self {
        self.skills.push(Box::new(skill));
        self
    }

    /// Alle registrierten Skills als LLM-Tool-Definitionen.
    pub fn tool_definitions(&self) -> Vec<LlmToolDefinition> {
        self.skills.iter().map(|s| s.definition()).collect()
    }

    /// Führt einen Tool-Call anhand des Namens aus.
    /// Gibt `None` zurück, wenn kein Skill mit dem Namen registriert ist.
    pub async fn execute(&self, name: &str, arguments: &Value) -> Option<String> {
        for skill in &self.skills {
            if skill.definition().name == name {
                return Some(skill.execute(arguments).await);
            }
        }
        None
    }
}
