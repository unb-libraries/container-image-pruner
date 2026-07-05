//! OCI Registry v2 manifest fetches.
//!
//! We only need to know, for a given digest, (a) which child manifests it references (the
//! `manifests[]` array of an image index — platform manifests plus embedded attestation
//! manifests) and (b) whether it carries a `subject` (a subject-linked attestation). The
//! `Accept` header lists all four common media types so single-arch docker/OCI images do
//! not 406.

use crate::error::{Error, Result};
use crate::github::client::GhClient;
use crate::github::models::Manifest;

/// All four common manifest media types; omitting docker-v2 makes single-arch docker images
/// fail content negotiation with a 406.
const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// What a fetched manifest tells us about its relationships.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestInfo {
    /// Digests of child manifests (empty for a non-index / single-arch image).
    pub children: Vec<String>,
    /// The digest this manifest attests, if it is a subject-linked artifact.
    pub subject: Option<String>,
}

/// Fetch the manifest for `digest` and extract its child + subject relationships.
pub async fn fetch_manifest(
    client: &GhClient,
    owner: &str,
    package: &str,
    token: &str,
    digest: &str,
) -> Result<ManifestInfo> {
    // Registry paths use the raw (lowercased) repository name; slashes are path separators,
    // not to be percent-encoded here.
    let owner_l = owner.to_lowercase();
    let package_l = package.to_lowercase();
    let url = format!(
        "{}/v2/{}/{}/manifests/{}",
        client.registry_base, owner_l, package_l, digest
    );

    let resp = client
        .send_with_retry(|| {
            client
                .http()
                .get(&url)
                .bearer_auth(token)
                .header("Accept", ACCEPT_MANIFESTS)
        })
        .await?;

    let body = resp.text().await?;
    let manifest: Manifest = serde_json::from_str(&body).map_err(|e| Error::Parse {
        context: format!("manifest {digest}"),
        source: e,
    })?;

    Ok(ManifestInfo {
        children: manifest.manifests.into_iter().map(|d| d.digest).collect(),
        subject: manifest.subject.map(|d| d.digest),
    })
}
