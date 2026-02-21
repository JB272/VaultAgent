use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::Mutex;

/// Tracks LLM token usage per day. Resets automatically at midnight.
pub struct UsageCounter {
    date: Mutex<String>,
    pub requests: AtomicU32,
    pub prompt_tokens: AtomicU32,
    pub completion_tokens: AtomicU32,
}

impl UsageCounter {
    pub fn new() -> Self {
        Self {
            date: Mutex::new(today()),
            requests: AtomicU32::new(0),
            prompt_tokens: AtomicU32::new(0),
            completion_tokens: AtomicU32::new(0),
        }
    }

    /// Record one LLM call. Resets counters if the date has changed since last call.
    pub async fn record(
        &self,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    ) {
        let now = today();
        let mut stored = self.date.lock().await;
        if *stored != now {
            *stored = now;
            self.requests.store(0, Ordering::Relaxed);
            self.prompt_tokens.store(0, Ordering::Relaxed);
            self.completion_tokens.store(0, Ordering::Relaxed);
        }
        drop(stored);

        self.requests.fetch_add(1, Ordering::Relaxed);
        if let Some(p) = prompt_tokens {
            self.prompt_tokens.fetch_add(p, Ordering::Relaxed);
        }
        if let Some(c) = completion_tokens {
            self.completion_tokens.fetch_add(c, Ordering::Relaxed);
        }
    }

    /// Returns a formatted stats string for display.
    pub async fn stats_message(&self) -> String {
        let date = self.date.lock().await.clone();
        let req = self.requests.load(Ordering::Relaxed);
        let prompt = self.prompt_tokens.load(Ordering::Relaxed);
        let completion = self.completion_tokens.load(Ordering::Relaxed);
        let total = prompt + completion;

        format!(
            "📊 <b>LLM Usage — {date}</b>\n\
             • Requests: <b>{req}</b>\n\
             • Prompt tokens: <b>{prompt}</b>\n\
             • Completion tokens: <b>{completion}</b>\n\
             • Total tokens: <b>{total}</b>",
        )
    }
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}
