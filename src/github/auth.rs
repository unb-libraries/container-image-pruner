//! ghcr.io token exchange for pulling manifests from the Registry v2 API.
//!
//! We exchange the PAT for a short-lived, repository-scoped pull token via the registry
//! token endpoint. The token is fetched once per package (its scope is per-repository) and
//! reused for all of that package's manifest fetches.

use crate::error::{Error, Result};
use crate::github::client::GhClient;
use crate::github::models::TokenResponse;

/// Obtain a `pull`-scoped token for `owner/package`. Owner and package are lowercased, as
/// the registry requires.
pub async fn pull_token(client: &GhClient, owner: &str, package: &str) -> Result<String> {
    let owner_l = owner.to_lowercase();
    let package_l = package.to_lowercase();
    let url = format!(
        "{}/token?service=ghcr.io&scope=repository:{}/{}:pull",
        client.registry_base, owner_l, package_l
    );

    let resp = client
        .send_with_retry(|| {
            client
                .http()
                .get(&url)
                .basic_auth(&owner_l, Some(client.token()))
        })
        .await?;

    let body = resp.text().await?;
    let parsed: TokenResponse = serde_json::from_str(&body).map_err(|e| Error::Parse {
        context: format!("ghcr token for {owner_l}/{package_l}"),
        source: e,
    })?;
    Ok(parsed.token)
}
