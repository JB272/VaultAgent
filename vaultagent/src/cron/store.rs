use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// Ein geplanter Job – persistiert in `cron/jobs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Eindeutige Job-ID (UUID).
    pub id: String,
    /// Menschenlesbarer Name (z.B. "Wetterbericht morgens").
    pub name: String,
    /// Wann der Job laufen soll.
    pub schedule: Schedule,
    /// Der Prompt, der an das LLM geschickt wird, wenn der Job feuert.
    pub prompt: String,
    /// Chat-ID, an die die Antwort gesendet wird.
    pub chat_id: i64,
    /// Ob der Job aktiv ist.
    pub enabled: bool,
    /// Bei One-Shot-Jobs: nach dem Ausführen automatisch löschen?
    pub delete_after_run: bool,
    /// Zeitpunkt der letzten Ausführung (für Duplikat-Schutz).
    pub last_run: Option<DateTime<Utc>>,
    /// Erstellt am.
    pub created_at: DateTime<Utc>,
}

/// Schedule-Typen, inspiriert von OpenClaw.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Schedule {
    /// Einmaliger Zeitpunkt (ISO 8601).
    #[serde(rename = "at")]
    At { at: DateTime<Utc> },
    /// Wiederkehrendes Intervall in Sekunden.
    #[serde(rename = "every")]
    Every { every_secs: u64 },
    /// Cron-Ausdruck (5-Feld: "30 6 * * *") mit optionaler Zeitzone.
    #[serde(rename = "cron")]
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
}

/// Persistenter Store für Cron-Jobs.  
/// Liest/schreibt `jobs.json` im angegebenen Verzeichnis.
pub struct CronStore {
    path: PathBuf,
    jobs: Mutex<Vec<CronJob>>,
}

impl CronStore {
    /// Lädt bestehende Jobs aus `dir/jobs.json` oder startet leer.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("jobs.json");
        let jobs = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => serde_json::from_str(&content).unwrap_or_else(|err| {
                    eprintln!("[CronStore] Failed to parse {}: {}", path.display(), err);
                    Vec::new()
                }),
                Err(err) => {
                    eprintln!("[CronStore] Failed to read {}: {}", path.display(), err);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let count = jobs.len();
        let store = Self {
            path,
            jobs: Mutex::new(jobs),
        };

        if count > 0 {
            println!("[CronStore] Loaded {} job(s)", count);
        }

        store
    }

    /// Speichert alle Jobs auf die Festplatte.
    async fn persist(&self) -> Result<(), String> {
        let jobs = self.jobs.lock().await;
        let json = serde_json::to_string_pretty(&*jobs)
            .map_err(|e| format!("Serialization failed: {}", e))?;
        drop(jobs);

        // Verzeichnis erstellen falls nötig
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }

        std::fs::write(&self.path, json)
            .map_err(|e| format!("Failed to write file: {}", e))?;
        Ok(())
    }

    /// Neuen Job hinzufügen und persistieren.
    pub async fn add(&self, job: CronJob) -> Result<String, String> {
        let id = job.id.clone();
        {
            let mut jobs = self.jobs.lock().await;
            jobs.push(job);
        }
        self.persist().await?;
        Ok(id)
    }

    /// Job anhand der ID entfernen.
    pub async fn remove(&self, job_id: &str) -> Result<bool, String> {
        let removed = {
            let mut jobs = self.jobs.lock().await;
            let len_before = jobs.len();
            jobs.retain(|j| j.id != job_id);
            jobs.len() < len_before
        };
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    /// Alle Jobs auflisten.
    pub async fn list(&self) -> Vec<CronJob> {
        self.jobs.lock().await.clone()
    }

    /// Alle fälligen Jobs zurückgeben (und `last_run` aktualisieren).
    pub async fn take_due_jobs(&self, now: DateTime<Utc>) -> Vec<CronJob> {
        let mut due = Vec::new();
        let mut to_delete = Vec::new();

        {
            let mut jobs = self.jobs.lock().await;
            for job in jobs.iter_mut() {
                if !job.enabled {
                    continue;
                }
                if Self::is_due(job, now) {
                    due.push(job.clone());
                    job.last_run = Some(now);

                    // One-Shot-Jobs deaktivieren
                    if matches!(job.schedule, Schedule::At { .. }) {
                        if job.delete_after_run {
                            to_delete.push(job.id.clone());
                        } else {
                            job.enabled = false;
                        }
                    }
                }
            }
            // Zu löschende Jobs entfernen
            jobs.retain(|j| !to_delete.contains(&j.id));
        }

        if !due.is_empty() {
            let _ = self.persist().await;
        }

        due
    }

    /// Prüft, ob ein Job jetzt fällig ist.
    fn is_due(job: &CronJob, now: DateTime<Utc>) -> bool {
        match &job.schedule {
            Schedule::At { at } => {
                // Fällig wenn Zeitpunkt erreicht und noch nicht gelaufen
                now >= *at && job.last_run.is_none()
            }
            Schedule::Every { every_secs } => {
                match job.last_run {
                    None => true, // Noch nie gelaufen → sofort
                    Some(last) => {
                        let elapsed = (now - last).num_seconds();
                        elapsed >= *every_secs as i64
                    }
                }
            }
            Schedule::Cron { expr, tz: _ } => {
                // Cron-Ausdruck parsen und prüfen
                let Ok(cron) = croner::Cron::new(expr).parse() else {
                    return false;
                };
                let last_check = job.last_run.unwrap_or(job.created_at);
                // Nächsten Zeitpunkt nach dem letzten Check ermitteln
                match cron.find_next_occurrence(&last_check, false) {
                    Ok(next) => now >= next,
                    Err(_) => false,
                }
            }
        }
    }
}
