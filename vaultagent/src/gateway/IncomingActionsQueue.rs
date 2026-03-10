use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
pub struct IncomingActionQueue {
    inner: Arc<IncomingActionQueueInner>,
}

impl IncomingActionQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(IncomingActionQueueInner {
                queue: Mutex::new(VecDeque::new()),
                notify: Notify::new(),
            }),
        }
    }

    pub fn writer(&self) -> IncomingActionWriter {
        IncomingActionWriter {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn register_service(&self) -> IncomingActionWriter {
        self.writer()
    }

    pub async fn pop(&self) -> IncomingAction {
        loop {
            if let Some(action) = {
                let mut queue = self.inner.queue.lock().await;
                queue.pop_front()
            } {
                return action;
            }

            self.inner.notify.notified().await;
        }
    }
}

struct IncomingActionQueueInner {
    queue: Mutex<VecDeque<IncomingAction>>,
    notify: Notify,
}

#[derive(Clone)]
pub struct IncomingActionWriter {
    inner: Arc<IncomingActionQueueInner>,
}

impl IncomingActionWriter {
    pub async fn push(&self, action: IncomingAction) {
        let mut queue = self.inner.queue.lock().await;

        // Burst-coalescing: if multiple plain text messages from the same chat
        // arrive quickly, merge them into one queue item. This reduces stale
        // responses when users send several follow-ups in a row.
        if let IncomingAction::Chat(new_chat) = action {
            if let Some(IncomingAction::Chat(existing_chat)) = queue.back_mut() {
                if existing_chat.chat_id == new_chat.chat_id
                    && existing_chat.image_url.is_none()
                    && new_chat.image_url.is_none()
                {
                    if !existing_chat.text.trim().is_empty() {
                        existing_chat.text.push_str("\n\n[Follow-up message]\n");
                    }
                    existing_chat.text.push_str(new_chat.text.trim());
                    drop(queue);
                    self.inner.notify.notify_one();
                    return;
                }
            }

            queue.push_back(IncomingAction::Chat(new_chat));
        } else {
            queue.push_back(action);
        }
        drop(queue);
        self.inner.notify.notify_one();
    }
}

#[derive(Debug, Clone)]
pub enum IncomingAction {
    Chat(ChatAction),
    Agent(AgentAction),
    Cron(ChronAction),
}

#[derive(Debug, Clone)]
pub struct ChatAction {
    pub chat_id: i64,
    pub text: String,
    /// Optional base64 data-URL of an image (e.g. from a Telegram photo).
    pub image_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentAction;

#[derive(Debug, Clone)]
pub struct ChronAction {
    pub chat_id: i64,
    pub prompt: String,
    pub job_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merges_followups_for_same_chat_without_images() {
        let queue = IncomingActionQueue::new();
        let writer = queue.writer();

        writer
            .push(IncomingAction::Chat(ChatAction {
                chat_id: 42,
                text: "erste nachricht".to_string(),
                image_url: None,
            }))
            .await;
        writer
            .push(IncomingAction::Chat(ChatAction {
                chat_id: 42,
                text: "zweite nachricht".to_string(),
                image_url: None,
            }))
            .await;

        let inner = queue.inner.queue.lock().await;
        assert_eq!(inner.len(), 1);
        let merged = match inner.front().cloned().unwrap() {
            IncomingAction::Chat(c) => c.text,
            _ => panic!("expected merged chat action"),
        };
        assert!(merged.contains("erste nachricht"));
        assert!(merged.contains("zweite nachricht"));
    }

    #[tokio::test]
    async fn does_not_merge_when_images_are_present() {
        let queue = IncomingActionQueue::new();
        let writer = queue.writer();

        writer
            .push(IncomingAction::Chat(ChatAction {
                chat_id: 42,
                text: "mit bild".to_string(),
                image_url: Some("data:image/jpeg;base64,abc".to_string()),
            }))
            .await;
        writer
            .push(IncomingAction::Chat(ChatAction {
                chat_id: 42,
                text: "follow up".to_string(),
                image_url: None,
            }))
            .await;

        let inner = queue.inner.queue.lock().await;
        assert_eq!(inner.len(), 2);
    }
}
