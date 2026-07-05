//! container-image-pruner: safely prune old per-build container images from GitHub Container
//! Registry (GHCR) without breaking multi-arch tagged images.
//!
//! Layering: `main.rs` wires only; this crate root exposes the public API; `domain::*` is
//! pure/IO-free (the correctness core); `github::*` are the IO adapters at the edges.

pub mod cli;
pub mod config;
pub mod domain;
pub mod error;
pub mod github;
pub mod policy;
pub mod prune;
pub mod report;

pub use config::Config;
pub use error::{Error, Result};
pub use report::Summary;

/// Plan (and, when `config.dry_run` is false, execute) pruning for the configured target.
pub async fn run(config: Config) -> Result<Summary> {
    prune::run(config).await
}
