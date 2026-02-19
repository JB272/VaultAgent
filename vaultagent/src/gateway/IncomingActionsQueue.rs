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
		queue.push_back(action);
		drop(queue);
		self.inner.notify.notify_one();
	}
}

#[derive(Debug, Clone)]
pub enum IncomingAction {
	Chat(ChatAction),
	Agent(AgentAction),
	Chron(ChronAction),
}

#[derive(Debug, Clone)]
pub struct ChatAction {
	pub chat_id: i64,
	pub text: String,
}

#[derive(Debug, Clone)]
pub struct AgentAction;

#[derive(Debug, Clone)]
pub struct ChronAction;
