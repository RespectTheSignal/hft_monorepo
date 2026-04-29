use thiserror::Error;

#[derive(Debug, Error)]
pub enum GateError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    #[error("Auth error: session expired or invalid")]
    Auth,

    #[error("Rate limited: {0}")]
    RateLimited(String),

    #[error("Non-JSON response (status {status}): {body}")]
    NonJson { status: u16, body: String },
}
