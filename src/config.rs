//! Resolved runtime configuration, produced from CLI args in `cli.rs` and consumed by the
//! orchestrator in `prune.rs`. Kept separate from `cli::Args` so the core does not depend
//! on clap.

use std::path::PathBuf;

use crate::policy::Policy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerType {
    User,
    Organization,
}

impl OwnerType {
    /// The REST path segment for this owner kind (`users` or `orgs`).
    pub fn path_segment(self) -> &'static str {
        match self {
            OwnerType::User => "users",
            OwnerType::Organization => "orgs",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Repository owner (user or org login), e.g. `unb-libraries`.
    pub owner: String,
    /// Optional single package name. `None` => process every container package under the owner.
    pub package: Option<String>,
    /// Owner kind. `None` => auto-detect via the API.
    pub owner_type: Option<OwnerType>,
    pub policy: Policy,
    /// GitHub PAT with `read:packages` (+ `delete:packages` for `--execute`).
    pub token: String,
    /// When true (the default) nothing is deleted; the plan is only reported.
    pub dry_run: bool,
    /// Abort a run whose plan would delete more than this many versions.
    pub max_delete: Option<usize>,
    /// Concurrency for read/manifest requests.
    pub read_concurrency: usize,
    /// Concurrency for DELETE requests (kept low to avoid GitHub secondary rate limits).
    pub delete_concurrency: usize,
    /// Opt in to protecting subject-linked (referrers-style) attestations of retained images.
    pub protect_subject_attestations: bool,
    /// Where to append the deletion audit trail (JSONL). `None` with `--execute` => stdout.
    pub audit_log: Option<PathBuf>,
    /// Override the GitHub REST API base (GitHub Enterprise Server, or tests). `None` =>
    /// `https://api.github.com`.
    pub api_base: Option<String>,
    /// Override the registry base (tests). `None` => `https://ghcr.io`.
    pub registry_base: Option<String>,
}
