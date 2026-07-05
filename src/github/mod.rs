//! IO adapters for the GitHub Packages REST API and the OCI Registry v2 API. All network
//! access lives here, behind small functions the orchestrator composes.

pub mod auth;
pub mod client;
pub mod models;
pub mod packages;
pub mod registry;
