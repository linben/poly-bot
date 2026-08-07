use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid upstream data: {0}")]
    InvalidData(String),
    #[error("source {source_id} failed: {message}")]
    Source { source_id: String, message: String },
    #[error("storage error: {0}")]
    Storage(String),
    #[error("AWS support is not enabled")]
    AwsDisabled,
}

pub type Result<T> = std::result::Result<T, Error>;
