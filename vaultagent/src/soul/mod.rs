pub mod memory;
pub mod personality;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memory::Memory;
use personality::Personality;

/// The "soul" of the agent — personality + memory.
/// Loaded at startup from the `soul/` directory.
pub struct Soul {
    pub personality: Personality,
    pub memory: Arc<Memory>,
    soul_dir: PathBuf,
}

impl Soul {
    /// Loads the Soul from a directory (default: `soul/`).
    pub fn load(soul_dir: &Path) -> Self {
        println!("[Soul] Loading from: {}", soul_dir.display());

        let personality = Personality::load(soul_dir);
        let memory = Arc::new(Memory::new(soul_dir));

        Self {
            personality,
            memory,
            soul_dir: soul_dir.to_path_buf(),
        }
    }

    /// Builds the complete system prompt:
    /// Personality + current memory context (MEMORY.md + yesterday + today).
    pub fn system_prompt(&self) -> String {
        let base = self.personality.system_prompt();
        let memory_block = self.memory.context_block();

        if memory_block.is_empty() {
            base
        } else {
            format!("{}\n{}", base, memory_block)
        }
    }

    pub fn dir(&self) -> &Path {
        &self.soul_dir
    }
}
