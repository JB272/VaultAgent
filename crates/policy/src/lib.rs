use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub workspace_root: PathBuf,
    pub allow_network: bool,
    pub allow_shell: bool,
    pub max_file_size_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy violation: {0}")]
    Violation(String),
}

pub fn check_tool_enabled(enabled_tools: &[String], tool: &str) -> Result<(), PolicyError> {
    if enabled_tools.iter().any(|t| t == tool) {
        Ok(())
    } else {
        Err(PolicyError::Violation(format!(
            "tool '{tool}' is disabled (deny-by-default)"
        )))
    }
}

pub fn check_fs_path(cfg: &PolicyConfig, path: &Path) -> Result<PathBuf, PolicyError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cfg.workspace_root.join(path)
    };

    let normalized = normalize_path(&candidate);
    let root = normalize_path(&cfg.workspace_root);

    if !normalized.starts_with(&root) {
        return Err(PolicyError::Violation(format!(
            "filesystem access outside workspace is blocked: {}",
            path.display()
        )));
    }

    Ok(normalized)
}

pub fn check_file_size(cfg: &PolicyConfig, size: u64) -> Result<(), PolicyError> {
    if size > cfg.max_file_size_bytes {
        Err(PolicyError::Violation(format!(
            "file exceeds max size ({} > {})",
            size, cfg.max_file_size_bytes
        )))
    } else {
        Ok(())
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
