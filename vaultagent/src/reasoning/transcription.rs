use reqwest::Client;
use std::error::Error;

/// Transkribiert Audio-Daten über die OpenAI-kompatible Whisper API.
#[derive(Debug)]
pub struct TranscriptionService {
    client: Client,
    api_key: String,
    base_url: String,
}

impl TranscriptionService {
    /// Erstellt einen neuen Service aus Umgebungsvariablen.
    /// Nutzt denselben API-Key und Base-URL wie das LLM.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("LLM_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()?;

        if api_key.trim().is_empty() {
            return None;
        }

        let base_url = std::env::var("LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        Some(Self {
            client: Client::new(),
            api_key,
            base_url,
        })
    }

    /// Transkribiert Audio-Bytes (OGG/MP3/WAV/etc.) zu Text.
    pub async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        mime_type: Option<&str>,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let extension = match mime_type {
            Some(m) if m.contains("ogg") => "ogg",
            Some(m) if m.contains("mp3") || m.contains("mpeg") => "mp3",
            Some(m) if m.contains("wav") => "wav",
            Some(m) if m.contains("mp4") || m.contains("m4a") => "m4a",
            Some(m) if m.contains("webm") => "webm",
            _ => "ogg", // Telegram Voice Messages sind standardmäßig OGG/Opus
        };

        let filename = format!("audio.{}", extension);

        let part = reqwest::multipart::Part::bytes(audio_data)
            .file_name(filename)
            .mime_str(mime_type.unwrap_or("audio/ogg"))?;

        let form = reqwest::multipart::Form::new()
            .text("model", "whisper-1")
            .text("language", "de") // Default Deutsch, könnte konfigurierbar sein
            .part("file", part);

        let url = format!(
            "{}/audio/transcriptions",
            self.base_url.trim_end_matches('/')
        );

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Whisper API Fehler {}: {}", status, body).into());
        }

        let result: serde_json::Value = response.json().await?;
        let text = result
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(text)
    }
}
