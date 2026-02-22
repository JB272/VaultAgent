use chrono::Local;
use std::path::{Path, PathBuf};

/// Manages the agent's memory:
/// - `MEMORY.md` — curated long-term memory
/// - `memory/YYYY-MM-DD.md` — daily append-only logs
pub struct Memory {
    soul_dir: PathBuf,
}

impl Memory {
    pub fn new(soul_dir: &Path) -> Self {
        let memory_dir = soul_dir.join("memory");
        if !memory_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&memory_dir) {
                eprintln!("[Soul][Memory] Failed to create memory/ directory: {}", e);
            }
        }
        Self {
            soul_dir: soul_dir.to_path_buf(),
        }
    }

    // ── Reading ─────────────────────────────────────────────────

    /// Loads the curated long-term memory (MEMORY.md).
    pub fn load_long_term(&self) -> String {
        let path = self.soul_dir.join("MEMORY.md");
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Loads today's daily log.
    pub fn load_today(&self) -> String {
        let path = self.daily_path_for_today();
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Loads yesterday's daily log.
    pub fn load_yesterday(&self) -> String {
        let yesterday = Local::now().date_naive() - chrono::Duration::days(1);
        let filename = format!("{}.md", yesterday.format("%Y-%m-%d"));
        let path = self.soul_dir.join("memory").join(filename);
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Builds the memory context that gets injected into the system prompt.
    /// Contains: MEMORY.md + yesterday + today (if available).
    pub fn context_block(&self) -> String {
        let mut parts = Vec::new();

        let long_term = self.load_long_term();
        if !long_term.trim().is_empty() {
            parts.push(format!(
                "## Long-term Memory (MEMORY.md)\n\n{}",
                long_term.trim()
            ));
        }

        let yesterday = self.load_yesterday();
        if !yesterday.trim().is_empty() {
            let date = Local::now().date_naive() - chrono::Duration::days(1);
            parts.push(format!(
                "## Memories from Yesterday ({})\n\n{}",
                date.format("%d.%m.%Y"),
                yesterday.trim()
            ));
        }

        let today = self.load_today();
        if !today.trim().is_empty() {
            let date = Local::now().date_naive();
            parts.push(format!(
                "## Memories from Today ({})\n\n{}",
                date.format("%d.%m.%Y"),
                today.trim()
            ));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n---\n# Your Memory\n\n{}\n---\n",
                parts.join("\n\n")
            )
        }
    }

    // ── Writing ────────────────────────────────────────────────

    /// Appends an entry to today's daily log (append-only).
    pub async fn append_today(&self, entry: &str) -> Result<(), String> {
        let path = self.daily_path_for_today();

        // Header if file does not exist yet
        let needs_header = !path.exists();
        let mut content = String::new();

        if needs_header {
            let date = Local::now().date_naive();
            content.push_str(&format!("# Daily Log {}\n\n", date.format("%d.%m.%Y")));
        }

        let time = Local::now().format("%H:%M");
        content.push_str(&format!("- **[{}]** {}\n", time, entry.trim()));

        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| format!("Could not open daily log: {}", e))?
            .write_all(content.as_bytes())
            .await
            .map_err(|e| format!("Could not write to daily log: {}", e))?;

        Ok(())
    }

    /// Appends an entry to MEMORY.md (long-term memory).
    pub async fn append_long_term(&self, entry: &str) -> Result<(), String> {
        let path = self.soul_dir.join("MEMORY.md");

        let content = format!("\n- {}\n", entry.trim());

        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| format!("Could not open MEMORY.md: {}", e))?
            .write_all(content.as_bytes())
            .await
            .map_err(|e| format!("Could not write to MEMORY.md: {}", e))?;

        Ok(())
    }

    // ── Search ─────────────────────────────────────────────────

    /// Searches all memory files for a query term (case-insensitive).
    /// Returns all matches with filename + line number.
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();

        // Search MEMORY.md
        let long_term_path = self.soul_dir.join("MEMORY.md");
        if let Ok(content) = std::fs::read_to_string(&long_term_path) {
            Self::search_in_content(&content, "MEMORY.md", &query_lower, &mut results);
        }

        // Search all daily logs
        let memory_dir = self.soul_dir.join("memory");
        if let Ok(entries) = std::fs::read_dir(&memory_dir) {
            let mut files: Vec<_> = entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
                .collect();
            files.sort_by_key(|e| e.file_name());

            for entry in files {
                let path = entry.path();
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");

                if let Ok(content) = std::fs::read_to_string(&path) {
                    Self::search_in_content(
                        &content,
                        &format!("memory/{}", filename),
                        &query_lower,
                        &mut results,
                    );
                }
            }
        }

        results
    }

    fn search_in_content(
        content: &str,
        file_label: &str,
        query_lower: &str,
        results: &mut Vec<SearchResult>,
    ) {
        for (line_no, line) in content.lines().enumerate() {
            if line.to_lowercase().contains(query_lower) {
                results.push(SearchResult {
                    file: file_label.to_string(),
                    line_number: line_no + 1,
                    text: line.trim().to_string(),
                });
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────

    fn daily_path_for_today(&self) -> PathBuf {
        let date = Local::now().date_naive();
        let filename = format!("{}.md", date.format("%Y-%m-%d"));
        self.soul_dir.join("memory").join(filename)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file: String,
    pub line_number: usize,
    pub text: String,
}

use tokio::io::AsyncWriteExt;
