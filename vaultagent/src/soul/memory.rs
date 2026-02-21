use chrono::Local;
use std::path::{Path, PathBuf};

/// Verwaltet das Gedächtnis des Agenten:
/// - `MEMORY.md` — kuratiertes Langzeitgedächtnis
/// - `memory/YYYY-MM-DD.md` — tägliche append-only Logs
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

    // ── Lesen ───────────────────────────────────────────

    /// Lädt das kuratierte Langzeitgedächtnis (MEMORY.md).
    pub fn load_long_term(&self) -> String {
        let path = self.soul_dir.join("MEMORY.md");
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Lädt das Tageslog für heute.
    pub fn load_today(&self) -> String {
        let path = self.daily_path_for_today();
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Lädt das Tageslog für gestern.
    pub fn load_yesterday(&self) -> String {
        let yesterday = Local::now().date_naive() - chrono::Duration::days(1);
        let filename = format!("{}.md", yesterday.format("%Y-%m-%d"));
        let path = self.soul_dir.join("memory").join(filename);
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Baut den Memory-Kontext zusammen, der in den System-Prompt injiziert wird.
    /// Enthält: MEMORY.md + gestern + heute (wenn vorhanden).
    pub fn context_block(&self) -> String {
        let mut parts = Vec::new();

        let long_term = self.load_long_term();
        if !long_term.trim().is_empty() {
            parts.push(format!(
                "## Langzeitgedächtnis (MEMORY.md)\n\n{}",
                long_term.trim()
            ));
        }

        let yesterday = self.load_yesterday();
        if !yesterday.trim().is_empty() {
            let date = Local::now().date_naive() - chrono::Duration::days(1);
            parts.push(format!(
                "## Erinnerungen von gestern ({})\n\n{}",
                date.format("%d.%m.%Y"),
                yesterday.trim()
            ));
        }

        let today = self.load_today();
        if !today.trim().is_empty() {
            let date = Local::now().date_naive();
            parts.push(format!(
                "## Erinnerungen von heute ({})\n\n{}",
                date.format("%d.%m.%Y"),
                today.trim()
            ));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n---\n# Dein Gedächtnis\n\n{}\n---\n",
                parts.join("\n\n")
            )
        }
    }

    // ── Schreiben ───────────────────────────────────────

    /// Hängt einen Eintrag ans heutige Tageslog an (append-only).
    pub async fn append_today(&self, entry: &str) -> Result<(), String> {
        let path = self.daily_path_for_today();

        // Header wenn Datei noch nicht existiert
        let needs_header = !path.exists();
        let mut content = String::new();

        if needs_header {
            let date = Local::now().date_naive();
            content.push_str(&format!("# Tageslog {}\n\n", date.format("%d.%m.%Y")));
        }

        let time = Local::now().format("%H:%M");
        content.push_str(&format!("- **[{}]** {}\n", time, entry.trim()));

        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| format!("Konnte Tageslog nicht öffnen: {}", e))?
            .write_all(content.as_bytes())
            .await
            .map_err(|e| format!("Konnte nicht ins Tageslog schreiben: {}", e))?;

        Ok(())
    }

    /// Hängt einen Eintrag an MEMORY.md an (Langzeitgedächtnis).
    pub async fn append_long_term(&self, entry: &str) -> Result<(), String> {
        let path = self.soul_dir.join("MEMORY.md");

        let content = format!("\n- {}\n", entry.trim());

        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| format!("Konnte MEMORY.md nicht öffnen: {}", e))?
            .write_all(content.as_bytes())
            .await
            .map_err(|e| format!("Konnte nicht in MEMORY.md schreiben: {}", e))?;

        Ok(())
    }

    // ── Suche ───────────────────────────────────────────

    /// Durchsucht alle Memory-Dateien nach einem Suchbegriff (case-insensitive).
    /// Gibt alle Treffer mit Dateiname + Zeile zurück.
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        let mut results = Vec::new();
        let query_lower = query.to_lowercase();

        // MEMORY.md durchsuchen
        let long_term_path = self.soul_dir.join("MEMORY.md");
        if let Ok(content) = std::fs::read_to_string(&long_term_path) {
            Self::search_in_content(&content, "MEMORY.md", &query_lower, &mut results);
        }

        // Alle Tageslogs durchsuchen
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

    // ── Hilfsfunktionen ─────────────────────────────────

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
