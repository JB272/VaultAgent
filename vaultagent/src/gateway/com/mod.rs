pub mod telegram;
pub mod website;

pub(crate) fn get_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn is_token_service_enabled(token_env_name: &str) -> bool {
    get_non_empty_env(token_env_name).is_some()
}
