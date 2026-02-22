use std::path::{Path, PathBuf};

/// Loads the personality (system prompt) from a Markdown file.
pub struct Personality {
    content: String,
    path: PathBuf,
}

impl Personality {
    /// Loads personality.md from the Soul directory.
    /// Returns a fallback prompt if the file does not exist.
    pub fn load(soul_dir: &Path) -> Self {
        let path = soul_dir.join("personality.md");
        let content = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            eprintln!(
                "[Soul][Personality] File not found ({}), using fallback prompt.",
                path.display()
            );
            "You are a helpful assistant. Answer in English.".to_string()
        });

        println!("[Soul][Personality] Loaded: {}", path.display());

        Self { content, path }
    }

    /// Returns the system prompt text.
    pub fn system_prompt(&self) -> &str {
        &self.content
    }

    /// Path to the personality file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
