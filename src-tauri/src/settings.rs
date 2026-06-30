//! Persisted user settings — JSON file under the OS app data directory.
//!
//! `load` returns a fully-populated struct: missing fields fall back to env vars,
//! then to `LlmConfig::defaults()`. `save` writes atomically (temp + rename) so a
//! crash mid-write can't corrupt the file.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::llm_translate::LlmConfig;

/// Default global hotkey. Users can override from the settings panel.
pub const DEFAULT_HOTKEY: &str = "CommandOrControl+Shift+T";

/// Default hotkey for opening the settings panel. Separate from the capture
/// hotkey so the user always has a guaranteed way to reach the settings even
/// if the tray icon gets hidden by Win11's notification area quirks.
pub const DEFAULT_SETTINGS_HOTKEY: &str = "CommandOrControl+Shift+P";

/// What we persist to disk and round-trip to the frontend. Mirrors `LlmConfig`
/// plus the two global hotkeys. Kept separate so we can add UI-only fields
/// later without leaking them into the wire schema of the LLM client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub prompt: String,
    pub hotkey: String,
    pub settings_hotkey: String,
}

impl Settings {
    /// Build a settings struct starting from the defaults, then layered with env
    /// vars. Used as the first-run baseline before any file exists.
    pub fn bootstrap() -> Self {
        let llm = LlmConfig::from_env_with_defaults();
        Self {
            base_url: llm.base_url,
            api_key: llm.api_key,
            model: llm.model,
            prompt: llm.prompt,
            hotkey: std::env::var("RSSC_HOTKEY").unwrap_or_else(|_| DEFAULT_HOTKEY.to_string()),
            settings_hotkey: std::env::var("RSSC_SETTINGS_HOTKEY")
                .unwrap_or_else(|_| DEFAULT_SETTINGS_HOTKEY.to_string()),
        }
    }

    /// Convert into the LLM client config (drops the hotkey — that lives in the
    /// global shortcut plugin, not in the LLM request).
    pub fn into_llm_config(&self) -> LlmConfig {
        LlmConfig {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            prompt: self.prompt.clone(),
        }
    }

    /// Load from `app_data_dir/settings.json`. If the file doesn't exist or any
    /// individual field is empty, fill it from `bootstrap()`. This makes the load
    /// operation total — it always returns a usable struct.
    pub fn load(app_data_dir: &Path) -> AppResult<Self> {
        let path = settings_path(app_data_dir);
        let mut current = Self::bootstrap();

        if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|e| {
                AppError::Capture(format!("read settings file {}: {e}", path.display()))
            })?;
            match serde_json::from_str::<Settings>(&raw) {
                Ok(parsed) => {
                    // Layer parsed values on top of bootstrap; bootstrap wins for
                    // empty strings so a half-written file doesn't leave blanks.
                    if !parsed.base_url.is_empty() {
                        current.base_url = parsed.base_url;
                    }
                    // api_key is allowed to be empty (e.g. local Ollama), so we
                    // accept whatever the file has.
                    current.api_key = parsed.api_key;
                    if !parsed.model.is_empty() {
                        current.model = parsed.model;
                    }
                    if !parsed.prompt.is_empty() {
                        current.prompt = parsed.prompt;
                    }
                    if !parsed.hotkey.is_empty() {
                        current.hotkey = parsed.hotkey;
                    }
                    if !parsed.settings_hotkey.is_empty() {
                        current.settings_hotkey = parsed.settings_hotkey;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "settings file is corrupt ({e}); using bootstrap values and continuing"
                    );
                }
            }
        }

        Ok(current)
    }

    /// Persist atomically. The parent directory is created if missing. We write
    /// to `settings.json.tmp` first then rename — POSIX guarantees rename is
    /// atomic on the same filesystem, and on Windows the rename-over pattern
    /// (remove existing + rename) gives effectively the same guarantee.
    pub fn save(&self, app_data_dir: &Path) -> AppResult<()> {
        fs::create_dir_all(app_data_dir).map_err(|e| {
            AppError::Capture(format!(
                "create app data dir {}: {e}",
                app_data_dir.display()
            ))
        })?;

        let final_path = settings_path(app_data_dir);
        let tmp_path = final_path.with_extension("json.tmp");

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| AppError::Capture(format!("serialize settings: {e}")))?;

        fs::write(&tmp_path, json).map_err(|e| {
            AppError::Capture(format!("write settings tmp {}: {e}", tmp_path.display()))
        })?;

        if final_path.exists() {
            // On Windows, rename refuses to overwrite — remove the destination
            // first. The temp file is on the same volume so a crash between the
            // two operations leaves the temp around (and a future load will
            // ignore it) rather than a half-written settings.json.
            let _ = fs::remove_file(&final_path);
        }
        fs::rename(&tmp_path, &final_path).map_err(|e| {
            AppError::Capture(format!(
                "rename settings {} -> {}: {e}",
                tmp_path.display(),
                final_path.display()
            ))
        })?;
        Ok(())
    }
}

pub fn settings_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("settings.json")
}
