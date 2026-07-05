//! Error type for the whole crate.
//!
//! `thiserror` gives us structured, matchable variants. `RateLimited` is deliberately a
//! first-class variant rather than a generic API error: the orchestrator catches it to
//! implement the "fail-soft & converge" policy (finish the current package, exit cleanly).

use chrono::{DateTime, Utc};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bad CLI input or resolved configuration.
    #[error("configuration error: {0}")]
    Config(String),

    /// The token is missing, invalid, or lacks the required scope.
    #[error("authentication/authorization error: {0}")]
    Auth(String),

    /// Transport-level failure (DNS, TLS, connect, timeout) that survived retries.
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    /// A non-success HTTP status we did not otherwise classify.
    #[error("API error {status} at {url}: {message}")]
    Api {
        status: u16,
        url: String,
        message: String,
    },

    /// A response body did not match the expected shape.
    #[error("failed to parse {context}: {source}")]
    Parse {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    /// GitHub primary rate limit exhausted. Caught by the orchestrator for fail-soft exit.
    #[error("primary rate limit exhausted; resets at {reset_at}")]
    RateLimited { reset_at: DateTime<Utc> },

    /// Local IO (audit log, stdout).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// True for the fail-soft rate-limit signal the orchestrator converts into a partial run.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Error::RateLimited { .. })
    }

    pub fn api_status(&self) -> Option<u16> {
        match self {
            Error::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}
