use std::path::{Path, PathBuf};

/// Loads the personality (system prompt) from a Markdown file.
/// Re-reads the file on every call so changes (e.g. from the onboarding
/// flow) take effect immediately without restarting.
pub struct Personality {
    path: PathBuf,
}

impl Personality {
    /// Initialises the Personality with the path to personality.md.
    pub fn load(soul_dir: &Path) -> Self {
        let path = soul_dir.join("personality.md");
        println!("[Soul][Personality] Path: {}", path.display());
        Self { path }
    }

    /// Returns the current system prompt text (re-read from disk).
    pub fn system_prompt(&self) -> String {
        match std::fs::read_to_string(&self.path) {
            Ok(content) if !content.trim().is_empty() => content,
            _ => Self::default_onboarding_prompt().to_string(),
        }
    }

    /// Path to the personality file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The built-in onboarding prompt used when personality.md is empty or missing.
    fn default_onboarding_prompt() -> &'static str {
        include_str!("onboarding_prompt.md")
    }
}
