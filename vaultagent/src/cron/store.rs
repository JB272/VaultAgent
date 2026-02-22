use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// A scheduled job — persisted in `cron/jobs.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Unique job ID (UUID).
    pub id: String,
    /// Human-readable name (e.g. "Morning weather report").
    pub name: String,
    /// When the job should run.
    pub schedule: Schedule,
    /// The prompt sent to the LLM when the job fires.
    pub prompt: String,
    /// Chat ID where the response will be sent.
    pub chat_id: i64,
    /// Whether the job is active.
    pub enabled: bool,
    /// For one-shot jobs: automatically delete after execution?
    pub delete_after_run: bool,
    /// Timestamp of the last execution (for duplicate protection).
    pub last_run: Option<DateTime<Utc>>,
    /// Created at.
    pub created_at: DateTime<Utc>,
}

/// Schedule types, inspired by OpenClaw.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Schedule {
    /// One-time timestamp (ISO 8601).
    #[serde(rename = "at")]
    At { at: DateTime<Utc> },
    /// Recurring interval in seconds.
    #[serde(rename = "every")]
    Every { every_secs: u64 },
    /// Cron expression (5-field: "30 6 * * *") with optional timezone.
    #[serde(rename = "cron")]
    Cron {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
}

/// Persistent store for cron jobs.
/// Reads/writes `jobs.json` in the specified directory.
pub struct CronStore {
    path: PathBuf,
    jobs: Mutex<Vec<CronJob>>,
}

impl CronStore {
    /// Loads existing jobs from `dir/jobs.json` or starts empty.
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

    /// Saves all jobs to disk.
    async fn persist(&self) -> Result<(), String> {
        let jobs = self.jobs.lock().await;
        let json = serde_json::to_string_pretty(&*jobs)
            .map_err(|e| format!("Serialization failed: {}", e))?;
        drop(jobs);

        // Create directory if needed
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }

        std::fs::write(&self.path, json).map_err(|e| format!("Failed to write file: {}", e))?;
        Ok(())
    }

    /// Add a new job and persist.
    pub async fn add(&self, job: CronJob) -> Result<String, String> {
        let id = job.id.clone();
        {
            let mut jobs = self.jobs.lock().await;
            jobs.push(job);
        }
        self.persist().await?;
        Ok(id)
    }

    /// Remove a job by its ID.
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

    /// List all jobs.
    pub async fn list(&self) -> Vec<CronJob> {
        self.jobs.lock().await.clone()
    }

    /// Return all due jobs (and update `last_run`).
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

                    // Deactivate one-shot jobs
                    if matches!(job.schedule, Schedule::At { .. }) {
                        if job.delete_after_run {
                            to_delete.push(job.id.clone());
                        } else {
                            job.enabled = false;
                        }
                    }
                }
            }
            // Remove jobs marked for deletion
            jobs.retain(|j| !to_delete.contains(&j.id));
        }

        if !due.is_empty() {
            let _ = self.persist().await;
        }

        due
    }

    /// Checks whether a job is due now.
    fn is_due(job: &CronJob, now: DateTime<Utc>) -> bool {
        match &job.schedule {
            Schedule::At { at } => {
                // Due when timestamp is reached and has not run yet
                now >= *at && job.last_run.is_none()
            }
            Schedule::Every { every_secs } => {
                match job.last_run {
                    None => true, // Never run yet → run immediately
                    Some(last) => {
                        let elapsed = (now - last).num_seconds();
                        elapsed >= *every_secs as i64
                    }
                }
            }
            Schedule::Cron { expr, tz: _ } => {
                // Parse cron expression and check
                let Ok(cron) = croner::Cron::new(expr).parse() else {
                    return false;
                };
                let last_check = job.last_run.unwrap_or(job.created_at);
                // Find next occurrence after the last check
                match cron.find_next_occurrence(&last_check, false) {
                    Ok(next) => now >= next,
                    Err(_) => false,
                }
            }
        }
    }
}
