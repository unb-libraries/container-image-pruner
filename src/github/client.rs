//! Shared HTTP client and the single retry/rate-limit wrapper every request flows through.
//!
//! Concentrating the fiddly logic here keeps it testable and prevents it drifting across
//! call sites:
//!   * Primary limit (`x-ratelimit-remaining: 0`) => fail-soft `Error::RateLimited`, which
//!     the orchestrator turns into a clean partial run.
//!   * Secondary limit (403/429 with `retry-after` or the known body message) => honor
//!     `Retry-After`, bounded retries.
//!   * 5xx / transport errors => exponential backoff with mild jitter, bounded retries.
//!   * All other non-2xx => `Error::Api` (callers may special-case, e.g. 404-on-delete).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use reqwest::{RequestBuilder, Response, StatusCode};

use crate::error::{Error, Result};

// Bounded so a persistently-failing endpoint gives up in a few seconds (~0.5+1+2+4 ≈ 7.5s)
// rather than stalling the run; the tool is re-runnable, so giving up early and converging
// on the next run is preferable to long blocking.
const MAX_RETRIES: u32 = 4;
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 30_000;

/// The GitHub REST API version header value we pin to.
const GITHUB_API_VERSION: &str = "2022-11-28";

#[derive(Debug, Clone)]
pub struct GhClient {
    http: reqwest::Client,
    /// GitHub REST API base, e.g. `https://api.github.com` (overridable for tests).
    pub api_base: String,
    /// Registry base, e.g. `https://ghcr.io` (overridable for tests).
    pub registry_base: String,
    token: String,
}

impl GhClient {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "container-image-pruner/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;
        Ok(Self {
            http,
            api_base: "https://api.github.com".to_string(),
            registry_base: "https://ghcr.io".to_string(),
            token,
        })
    }

    /// Override the API and registry base URLs (used by wiremock-based tests).
    pub fn with_bases(mut self, api_base: String, registry_base: String) -> Self {
        self.api_base = api_base;
        self.registry_base = registry_base;
        self
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// A GET against the REST API with the standard GitHub headers and bearer auth.
    pub fn api_get(&self, url: &str) -> RequestBuilder {
        self.http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
    }

    /// A DELETE against the REST API.
    pub fn api_delete(&self, url: &str) -> RequestBuilder {
        self.http
            .delete(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
    }

    /// Send a request built by `build`, retrying and classifying per the module docs.
    /// `build` is a closure so the request can be reconstructed on each retry.
    pub async fn send_with_retry<F>(&self, build: F) -> Result<Response>
    where
        F: Fn() -> RequestBuilder,
    {
        let mut attempt: u32 = 0;
        loop {
            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp);
                    }
                    match self.classify_failure(resp, status, attempt).await {
                        Retry::Return(err) => return Err(err),
                        Retry::After(delay) => {
                            if attempt >= MAX_RETRIES {
                                return Err(Error::Api {
                                    status: status.as_u16(),
                                    url: String::new(),
                                    message: "exhausted retries".to_string(),
                                });
                            }
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                        }
                    }
                }
                Err(e) => {
                    if (e.is_timeout() || e.is_connect() || e.is_request()) && attempt < MAX_RETRIES
                    {
                        tokio::time::sleep(backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(Error::Http(e));
                }
            }
        }
    }

    async fn classify_failure(&self, resp: Response, status: StatusCode, attempt: u32) -> Retry {
        let url = resp.url().to_string();
        let retry_after = header_u64(&resp, "retry-after");
        let remaining = header_u64(&resp, "x-ratelimit-remaining");
        let reset = header_u64(&resp, "x-ratelimit-reset");

        // Primary rate limit exhausted -> fail-soft.
        if remaining == Some(0) && matches!(status.as_u16(), 403 | 429) {
            let reset_at = reset
                .and_then(|s| DateTime::<Utc>::from_timestamp(s as i64, 0))
                .unwrap_or_else(Utc::now);
            return Retry::Return(Error::RateLimited { reset_at });
        }

        // Secondary rate limit: retry-after header, or the documented body message.
        if matches!(status.as_u16(), 403 | 429) {
            if let Some(secs) = retry_after {
                return Retry::After(Duration::from_secs(secs.min(MAX_BACKOFF_MS / 1000)));
            }
            let body = resp.text().await.unwrap_or_default();
            if body.to_lowercase().contains("secondary rate limit") {
                return Retry::After(backoff(attempt));
            }
            // A genuine permission problem.
            return Retry::Return(Error::Api {
                status: status.as_u16(),
                url,
                message: truncate(&body),
            });
        }

        if status.is_server_error() {
            return Retry::After(backoff(attempt));
        }

        let body = resp.text().await.unwrap_or_default();
        Retry::Return(Error::Api {
            status: status.as_u16(),
            url,
            message: truncate(&body),
        })
    }
}

enum Retry {
    Return(Error),
    After(Duration),
}

fn header_u64(resp: &Response, name: &str) -> Option<u64> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

/// Exponential backoff with mild jitter derived from the wall clock (cheap, dependency-free).
fn backoff(attempt: u32) -> Duration {
    let exp = BASE_BACKOFF_MS.saturating_mul(1u64 << attempt.min(6));
    let capped = exp.min(MAX_BACKOFF_MS);
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() as u64) % (capped / 4 + 1))
        .unwrap_or(0);
    Duration::from_millis(capped + jitter)
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 300 {
        s.to_string()
    } else {
        format!("{}…", &s[..300])
    }
}
