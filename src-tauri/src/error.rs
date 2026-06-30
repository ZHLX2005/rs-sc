use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("capture error: {0}")]
    Capture(String),

    #[error("image error: {0}")]
    Image(#[from] image::ImageError),

    #[error("AI error: {0}")]
    Ai(String),

    #[error("Tauri error: {0}")]
    Tauri(#[from] tauri::Error),
}

// Tauri commands need to serialize the error back to the JS side. We collapse
// the variant to a single string — enough for a "show in the status bar" UX,
// and it sidesteps the question of which fields each variant should expose.
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
