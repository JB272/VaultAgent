use std::sync::Arc;

use crate::cron::store::CronStore;
use crate::gateway::incoming_actions_queue::{ChronAction, IncomingAction, IncomingActionWriter};

/// Scheduler-Task: prüft periodisch den CronStore auf fällige Jobs
/// und pushed `IncomingAction::Cron` in die Event-Queue.
pub struct CronScheduler;

impl CronScheduler {
    /// Startet den Scheduler als Background-Task.
    /// Prüft alle 5 Sekunden auf fällige Jobs.
    pub fn start(store: Arc<CronStore>, writer: IncomingActionWriter) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now = chrono::Utc::now();
                let due_jobs = store.take_due_jobs(now).await;
                for job in due_jobs {
                    println!(
                        "  Cron feuert: \"{}\" → Chat {} | Prompt: {}",
                        job.name,
                        job.chat_id,
                        if job.prompt.len() > 60 {
                            format!("{}…", &job.prompt[..60])
                        } else {
                            job.prompt.clone()
                        }
                    );
                    writer
                        .push(IncomingAction::Cron(ChronAction {
                            chat_id: job.chat_id,
                            prompt: job.prompt,
                            job_name: job.name,
                        }))
                        .await;
                }
            }
        });
    }
}
