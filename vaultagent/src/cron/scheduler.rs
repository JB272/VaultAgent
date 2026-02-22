use std::sync::Arc;

use crate::cron::store::CronStore;
use crate::gateway::incoming_actions_queue::{ChronAction, IncomingAction, IncomingActionWriter};

/// Scheduler task: periodically checks the CronStore for due jobs
/// and pushes `IncomingAction::Cron` into the event queue.
pub struct CronScheduler;

impl CronScheduler {
    /// Starts the scheduler as a background task.
    /// Checks for due jobs every 5 seconds.
    pub fn start(store: Arc<CronStore>, writer: IncomingActionWriter) {
        // Log pending jobs at startup
        let store_clone = Arc::clone(&store);
        tokio::spawn(async move {
            let jobs = store_clone.list().await;
            let active: Vec<_> = jobs.iter().filter(|j| j.enabled).collect();
            if !active.is_empty() {
                println!("[Cron] {} active job(s):", active.len());
                for job in &active {
                    let schedule_desc = match &job.schedule {
                        crate::cron::store::Schedule::At { at } => {
                            format!("once at {}", at)
                        }
                        crate::cron::store::Schedule::Every { every_secs } => {
                            format!("every {}s", every_secs)
                        }
                        crate::cron::store::Schedule::Cron { expr, .. } => {
                            format!("cron '{}'", expr)
                        }
                    };
                    println!("    - \"{}\" ({})", job.name, schedule_desc);
                }
            }
        });

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now = chrono::Utc::now();
                let due_jobs = store.take_due_jobs(now).await;
                for job in due_jobs {
                    println!(
                        "[Cron] Triggered job \"{}\" for chat {} | Prompt: {}",
                        job.name,
                        job.chat_id,
                        if job.prompt.len() > 60 {
                            format!("{}…", &job.prompt[..60])
                        } else {
                            job.prompt.clone()
                        }
                    );
                    let cron_prompt = format!(
                        "[SYSTEM: This is a scheduled reminder/task named '{}', \
                         and it has just been triggered. Execute the following task NOW and respond \
                         directly to the user with the result. Do NOT create a new reminder; \
                         deliver the answer/reminder directly.]\n\n{}",
                        job.name, job.prompt
                    );
                    writer
                        .push(IncomingAction::Cron(ChronAction {
                            chat_id: job.chat_id,
                            prompt: cron_prompt,
                            job_name: job.name,
                        }))
                        .await;
                }
            }
        });
    }
}
