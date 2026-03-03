pub mod memory;
pub mod personality;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memory::Memory;
use personality::Personality;

/// The "soul" of the agent — personality + memory + constitution.
/// Loaded at startup from the `soul/` directory.
/// The optional constitution is a host-only file that cannot be modified
/// by the agent (it lives outside the Docker container).
pub struct Soul {
    pub personality: Personality,
    pub memory: Arc<Memory>,
    soul_dir: PathBuf,
    constitution_path: Option<PathBuf>,
}

impl Soul {
    /// Loads the Soul from a directory (default: `soul/`).
    pub fn load(soul_dir: &Path) -> Self {
        println!("[Soul] Loading from: {}", soul_dir.display());

        let personality = Personality::load(soul_dir);
        let memory = Arc::new(Memory::new(soul_dir));

        // Constitution: host-only file, path from CONSTITUTION_PATH env or default.
        let constitution_path = {
            let p = std::env::var("CONSTITUTION_PATH")
                .unwrap_or_else(|_| "constitution.md".to_string());
            let path = PathBuf::from(&p);
            if path.exists() {
                println!("[Soul] Constitution loaded from: {}", path.display());
                Some(path)
            } else {
                println!(
                    "[Soul] No constitution found at: {} (optional)",
                    path.display()
                );
                None
            }
        };

        Self {
            personality,
            memory,
            soul_dir: soul_dir.to_path_buf(),
            constitution_path,
        }
    }

    /// Reads the constitution (re-read on every call so edits take effect).
    fn constitution(&self) -> Option<String> {
        self.constitution_path.as_ref().and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
    }

    /// Builds the complete system prompt:
    /// Constitution + Personality + current memory context.
    pub fn system_prompt(&self) -> String {
        let mut parts = Vec::new();

        if let Some(constitution) = self.constitution() {
            parts.push(constitution);
        }

        parts.push(self.personality.system_prompt());

        let memory_block = self.memory.context_block();
        if !memory_block.is_empty() {
            parts.push(memory_block);
        }

        parts.join("\n\n")
    }

    pub fn dir(&self) -> &Path {
        &self.soul_dir
    }
}
