pub mod memory;
pub mod personality;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memory::Memory;
use personality::Personality;

/// Die "Seele" des Agenten — Persönlichkeit + Gedächtnis.
/// Wird beim Start aus dem `soul/` Verzeichnis geladen.
pub struct Soul {
    pub personality: Personality,
    pub memory: Arc<Memory>,
    soul_dir: PathBuf,
}

impl Soul {
    /// Lädt die Soul aus einem Verzeichnis (default: `soul/`).
    pub fn load(soul_dir: &Path) -> Self {
        println!("Soul laden aus: {}", soul_dir.display());

        let personality = Personality::load(soul_dir);
        let memory = Arc::new(Memory::new(soul_dir));

        Self {
            personality,
            memory,
            soul_dir: soul_dir.to_path_buf(),
        }
    }

    /// Baut den vollständigen System-Prompt:
    /// Persönlichkeit + aktueller Memory-Kontext (MEMORY.md + gestern + heute).
    pub fn system_prompt(&self) -> String {
        let base = self.personality.system_prompt();
        let memory_block = self.memory.context_block();

        if memory_block.is_empty() {
            base.to_string()
        } else {
            format!("{}\n{}", base, memory_block)
        }
    }

    pub fn dir(&self) -> &Path {
        &self.soul_dir
    }
}
