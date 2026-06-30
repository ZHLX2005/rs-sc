//! Multimodal LLM client — speaks the OpenAI `/chat/completions` schema.
//!
//! Compatible with OpenAI itself, Azure OpenAI (set base_url to the deployment URL),
//! DeepSeek, Ollama (`http://127.0.0.1:11434/v1`), and any other provider that follows
//! the same JSON schema. The user passes the image as a `data:image/png;base64,...` URL.
//!
//! The active configuration lives behind an `RwLock` so the settings panel can hot-swap
//! it at runtime — the next `translate_png` call picks up the new values without a
//! process restart.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{AppError, AppResult};

/// User-tunable configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub prompt: String,
}

impl LlmConfig {
    /// Hard-coded fallbacks used when neither the settings file nor environment
    /// variables provide a value. Defaults to a local Ollama instance so the app
    /// runs out of the box with `ollama serve`.
    pub fn defaults() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434/v1/chat/completions".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            prompt:
                "你是一名专业翻译。请把图片里的文字翻译成简体中文，仅输出译文本身，不要解释、不要加引号。"
                    .to_string(),
        }
    }

    /// Layer environment variables on top of the defaults. Used as the bootstrap
    /// configuration before the user has saved anything to disk.
    pub fn from_env_with_defaults() -> Self {
        let mut cfg = Self::defaults();
        if let Ok(v) = std::env::var("RSSC_API_BASE") {
            cfg.base_url = v;
        }
        if let Ok(v) = std::env::var("RSSC_API_KEY") {
            cfg.api_key = v;
        }
        if let Ok(v) = std::env::var("RSSC_MODEL") {
            cfg.model = v;
        }
        if let Ok(v) = std::env::var("RSSC_PROMPT") {
            cfg.prompt = v;
        }
        cfg
    }
}

#[derive(Clone)]
pub struct LlmTranslateClient {
    http: Arc<reqwest::Client>,
    config: Arc<RwLock<LlmConfig>>,
}

impl LlmTranslateClient {
    pub fn new(http: Arc<reqwest::Client>, config: LlmConfig) -> Self {
        Self {
            http,
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// Replace the live configuration. Subsequent `translate_png` calls use the new
    /// values. The previous config is dropped only after all in-flight reads finish,
    /// so this is safe to call concurrently with translation.
    pub async fn set_config(&self, new_config: LlmConfig) {
        *self.config.write().await = new_config;
    }

    /// Send a PNG to the multimodal model and return the translated text.
    pub async fn translate_png(&self, png_bytes: &[u8]) -> AppResult<String> {
        // Take a snapshot of the config so we don't hold the read lock across the
        // network round-trip.
        let cfg = self.config.read().await.clone();

        let b64 = BASE64.encode(png_bytes);
        let data_url = format!("data:image/png;base64,{}", b64);

        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: vec![
                    ContentPart::Text { text: cfg.prompt.clone() },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl { url: data_url },
                    },
                ],
            }],
            temperature: 0.2,
        };

        let mut req = self
            .http
            .post(&cfg.base_url)
            .header("Content-Type", "application/json")
            .json(&request);
        if !cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Ai(format!("translate request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Ai(format!("read body failed: {e}")))?;

        if !status.is_success() {
            let detail: String = body.chars().take(500).collect();
            return Err(AppError::Ai(format!(
                "AI returned HTTP {}: {detail}",
                status.as_u16()
            )));
        }

        let body_preview: String = body.chars().take(200).collect();
        let parsed: ChatResponse = serde_json::from_str(&body).map_err(|e| {
            AppError::Ai(format!(
                "parse response failed: {e}; body={body_preview}"
            ))
        })?;

        let text = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content.trim().to_string())
            .unwrap_or_default();

        if text.is_empty() {
            return Err(AppError::Ai("model returned empty content".into()));
        }
        Ok(text)
    }

    /// Send a minimal probe (text only) to verify the API is reachable and returns
    /// a valid response. Used by the "Test connection" button in the settings panel.
    pub async fn probe(&self) -> AppResult<String> {
        let cfg = self.config.read().await.clone();

        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: vec![ContentPart::Text {
                    text: "ping".to_string(),
                }],
            }],
            temperature: 0.0,
        };

        let mut req = self
            .http
            .post(&cfg.base_url)
            .header("Content-Type", "application/json")
            .json(&request);
        if !cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Ai(format!("connect failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let detail: String = body.chars().take(200).collect();
            return Err(AppError::Ai(format!(
                "HTTP {}: {detail}",
                status.as_u16()
            )));
        }
        Ok(format!("HTTP {}", status.as_u16()))
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────
// We only model the shape we actually use; serde will ignore unknown fields.

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: Vec<ContentPart>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
struct ImageUrl {
    url: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}
