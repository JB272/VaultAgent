//! Policy engine: deny-by-default, fail-closed checks.

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy violation: {0}")]
    Violation(String),
}
