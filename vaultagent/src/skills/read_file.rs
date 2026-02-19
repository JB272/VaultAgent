use serde_json::json;
use std::path::{Component, Path, PathBuf};

pub async fn execute(path: &str) -> String {
    match sanitize_relative_path(path) {
        Ok(safe_path) => match tokio::fs::read_to_string(&safe_path).await {
            Ok(content) => json!({
                "ok": true,
                "path": safe_path.to_string_lossy(),
                "content": content,
            })
            .to_string(),
            Err(err) => json!({
                "ok": false,
                "error": format!("Datei konnte nicht gelesen werden: {}", err),
            })
            .to_string(),
        },
        Err(err) => json!({
            "ok": false,
            "error": err,
        })
        .to_string(),
    }
}

fn sanitize_relative_path(path: &str) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("Pfad darf nicht leer sein.".to_string());
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err("Nur relative Pfade im Workspace sind erlaubt.".to_string());
    }

    if candidate.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err("Pfad enthält unzulässige Segmente (.. oder Root).".to_string());
    }

    Ok(PathBuf::from(path))
}
