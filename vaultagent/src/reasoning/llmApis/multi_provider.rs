use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use crate::reasoning::llm_interface::{
    LlmChatRequest, LlmChatResponse, LlmError, LlmInterface,
};

/// Wraps multiple LLM providers and delegates to the currently active one.
/// Model switching automatically selects the correct backend.
pub struct MultiProvider {
    backends: Vec<Arc<dyn LlmInterface>>,
    active: Mutex<usize>,
}

impl MultiProvider {
    pub fn new(backends: Vec<Arc<dyn LlmInterface>>) -> Self {
        assert!(!backends.is_empty(), "MultiProvider needs at least one backend");
        Self {
            backends,
            active: Mutex::new(0),
        }
    }

    fn active_backend(&self) -> Arc<dyn LlmInterface> {
        let idx = *self.active.lock().unwrap();
        self.backends[idx].clone()
    }

    /// Detect which backend owns a model name by simple prefix heuristic.
    fn backend_index_for_model(&self, model: &str) -> Option<usize> {
        // Try exact provider match first
        for (i, b) in self.backends.iter().enumerate() {
            let prov = b.provider_name();
            if prov == "anthropic" && model.starts_with("claude") {
                return Some(i);
            }
            if prov == "openai" && !model.starts_with("claude") {
                return Some(i);
            }
        }
        None
    }
}

#[async_trait]
impl LlmInterface for MultiProvider {
    async fn chat(&self, request: LlmChatRequest) -> Result<LlmChatResponse, LlmError> {
        self.active_backend().chat(request).await
    }

    fn provider_name(&self) -> &'static str {
        self.active_backend().provider_name()
    }

    fn current_model(&self) -> String {
        self.active_backend().current_model()
    }

    fn set_model(&self, model: String) {
        if let Some(idx) = self.backend_index_for_model(&model) {
            *self.active.lock().unwrap() = idx;
            self.backends[idx].set_model(model);
        } else {
            // Fallback: set on current backend
            self.active_backend().set_model(model);
        }
    }

    async fn list_models(&self) -> Vec<String> {
        let mut all = Vec::new();
        for b in &self.backends {
            let models = b.list_models().await;
            all.extend(models);
        }
        all.sort();
        all
    }
}
