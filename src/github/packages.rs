//! GitHub Packages REST API: owner-type detection, listing packages and versions
//! (paginated), fetching a single version (for the pre-delete TOCTOU re-check), and
//! deleting a version (404 treated as success for idempotent re-runs).

use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;

use crate::config::OwnerType;
use crate::error::{Error, Result};
use crate::github::client::GhClient;
use crate::github::models::{OwnerInfo, Package, PackageVersion};

/// Detect whether `owner` is a User or an Organization via `GET /users/{owner}`.
pub async fn detect_owner_type(client: &GhClient, owner: &str) -> Result<OwnerType> {
    let url = format!("{}/users/{}", client.api_base, owner);
    let resp = client.send_with_retry(|| client.api_get(&url)).await?;
    let body = resp.text().await?;
    let info: OwnerInfo = serde_json::from_str(&body).map_err(|e| Error::Parse {
        context: format!("owner info for {owner}"),
        source: e,
    })?;
    match info.kind.as_str() {
        "Organization" => Ok(OwnerType::Organization),
        "User" => Ok(OwnerType::User),
        other => Err(Error::Config(format!(
            "unexpected owner type {other:?} for {owner}"
        ))),
    }
}

/// List every container package name under the owner.
pub async fn list_container_packages(
    client: &GhClient,
    owner: &str,
    owner_type: OwnerType,
) -> Result<Vec<String>> {
    let url = format!(
        "{}/{}/{}/packages?package_type=container&per_page=100",
        client.api_base,
        owner_type.path_segment(),
        owner
    );
    let packages: Vec<Package> = get_all(client, url, &format!("packages for {owner}")).await?;
    Ok(packages.into_iter().map(|p| p.name).collect())
}

/// List all versions of a container package (following pagination).
pub async fn list_versions(
    client: &GhClient,
    owner: &str,
    owner_type: OwnerType,
    package: &str,
) -> Result<Vec<PackageVersion>> {
    let url = format!(
        "{}/{}/{}/packages/container/{}/versions?per_page=100",
        client.api_base,
        owner_type.path_segment(),
        owner,
        encode_segment(package)
    );
    get_all(client, url, &format!("versions for {owner}/{package}")).await
}

/// Fetch a single version (used to re-check tags immediately before deleting).
pub async fn get_version(
    client: &GhClient,
    owner: &str,
    owner_type: OwnerType,
    package: &str,
    id: u64,
) -> Result<PackageVersion> {
    let url = format!(
        "{}/{}/{}/packages/container/{}/versions/{}",
        client.api_base,
        owner_type.path_segment(),
        owner,
        encode_segment(package),
        id
    );
    let resp = client.send_with_retry(|| client.api_get(&url)).await?;
    let body = resp.text().await?;
    serde_json::from_str(&body).map_err(|e| Error::Parse {
        context: format!("version {id} for {owner}/{package}"),
        source: e,
    })
}

/// Delete a package version by id. A 404 means it is already gone, which we treat as
/// success so retried and re-run deletions are idempotent.
pub async fn delete_version(
    client: &GhClient,
    owner: &str,
    owner_type: OwnerType,
    package: &str,
    id: u64,
) -> Result<()> {
    let url = format!(
        "{}/{}/{}/packages/container/{}/versions/{}",
        client.api_base,
        owner_type.path_segment(),
        owner,
        encode_segment(package),
        id
    );
    match client.send_with_retry(|| client.api_delete(&url)).await {
        Ok(_) => Ok(()),
        Err(e) if e.api_status() == Some(404) => Ok(()),
        Err(e) => Err(e),
    }
}

/// GET every page, following the `Link: …; rel="next"` header.
async fn get_all<T: DeserializeOwned>(
    client: &GhClient,
    first_url: String,
    context: &str,
) -> Result<Vec<T>> {
    let mut next = Some(first_url);
    let mut out = Vec::new();
    while let Some(url) = next {
        let resp = client.send_with_retry(|| client.api_get(&url)).await?;
        next = next_page_url(resp.headers());
        let body = resp.text().await?;
        let page: Vec<T> = serde_json::from_str(&body).map_err(|e| Error::Parse {
            context: context.to_string(),
            source: e,
        })?;
        out.extend(page);
    }
    Ok(out)
}

/// Extract the `rel="next"` URL from a `Link` header, if any.
fn next_page_url(headers: &HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in link.split(',') {
        let mut segs = part.split(';');
        let url_seg = segs.next()?.trim();
        let is_next = segs.any(|s| s.trim() == "rel=\"next\"");
        if is_next {
            let url = url_seg.trim_start_matches('<').trim_end_matches('>');
            return Some(url.to_string());
        }
    }
    None
}

/// Percent-encode a REST path segment (package names may contain `/` and other characters).
/// Unreserved characters (RFC 3986) pass through unchanged.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_next_link() {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::LINK,
            "<https://api.github.com/x?page=2>; rel=\"next\", \
             <https://api.github.com/x?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            next_page_url(&h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
    }

    #[test]
    fn no_next_link() {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::LINK,
            "<https://api.github.com/x?page=1>; rel=\"prev\""
                .parse()
                .unwrap(),
        );
        assert_eq!(next_page_url(&h), None);
        assert_eq!(next_page_url(&HeaderMap::new()), None);
    }

    #[test]
    fn encodes_slashes_and_keeps_dots() {
        assert_eq!(encode_segment("loyalist.lib.unb.ca"), "loyalist.lib.unb.ca");
        assert_eq!(encode_segment("team/app"), "team%2Fapp");
    }
}
