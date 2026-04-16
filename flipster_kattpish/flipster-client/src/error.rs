use thiserror::Error;

#[derive(Debug, Error)]
pub enum FlipsterError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("Auth error: session expired or invalid")]
    Auth,
}
