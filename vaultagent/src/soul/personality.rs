use std::path::{Path, PathBuf};

/// Lädt die Persönlichkeit (System-Prompt) aus einer Markdown-Datei.
pub struct Personality {
    content: String,
    path: PathBuf,
}

impl Personality {
    /// Lädt personality.md aus dem Soul-Verzeichnis.
    /// Gibt einen Fallback-Prompt zurück, wenn die Datei nicht existiert.
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

    /// Gibt den System-Prompt-Text zurück.
    pub fn system_prompt(&self) -> &str {
        &self.content
    }

    /// Pfad zur Personality-Datei.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
