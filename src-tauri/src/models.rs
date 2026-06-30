use serde::{Deserialize, Serialize};

/// Selection rectangle in screenshot pixel coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Translation result delivered to the result window.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslateResult {
    pub text: String,
    /// Base64 PNG of the cropped region, so the UI can show "what you translated".
    pub image_base64: String,
}
