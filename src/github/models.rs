//! Serde models for the GitHub Packages REST API and the OCI Registry v2 manifest API.
//! All are intentionally tolerant of unknown fields (no `deny_unknown_fields`).

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One container package version from the Packages API.
#[derive(Debug, Clone, Deserialize)]
pub struct PackageVersion {
    pub id: u64,
    /// The version name, which for container packages is the `sha256:…` digest.
    pub name: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

impl PackageVersion {
    pub fn tags(&self) -> Vec<String> {
        self.metadata
            .as_ref()
            .and_then(|m| m.container.as_ref())
            .map(|c| c.tags.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub container: Option<ContainerMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerMetadata {
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One package from the "list packages" endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
}

/// Minimal `/users/{owner}` shape for owner-type detection.
#[derive(Debug, Clone, Deserialize)]
pub struct OwnerInfo {
    #[serde(rename = "type")]
    pub kind: String,
}

/// A manifest as returned by the Registry v2 API. We only care about whether it is an index
/// (has a `manifests` array of children) and whether it carries a `subject` (attestation).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
    #[serde(default)]
    pub subject: Option<Descriptor>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Descriptor {
    pub digest: String,
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
}

/// Response body of the ghcr.io token exchange.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub token: String,
}
