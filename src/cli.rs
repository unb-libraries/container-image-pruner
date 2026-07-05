//! Command-line surface (clap derive) and resolution into a [`Config`].
//!
//! This binary is the engine behind the `container-image-pruner` GitHub Action; it is
//! non-interactive by design. Settings come from flags (the Action maps its inputs to them) or,
//! for the token, the `GITHUB_TOKEN` environment variable. There is no config file and no
//! confirmation prompt — `--execute` deletes directly (guarded by `--max-delete`).

use clap::{Parser, ValueEnum};

use crate::config::{Config, OwnerType};
use crate::error::{Error, Result};
use crate::policy::Policy;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OwnerTypeArg {
    User,
    Org,
}

/// Safely prune old per-build container images from GHCR without breaking multi-arch tags.
#[derive(Debug, Parser)]
#[command(name = "container-image-pruner", version, about)]
pub struct Args {
    /// Full image reference `ghcr.io/<owner>/<package>` (alternative to the positional
    /// owner/package). Parsed into owner + package.
    #[arg(long, value_name = "REF", conflicts_with_all = ["owner", "package"])]
    pub image: Option<String>,

    /// Repository owner (user or organization login). Required unless `--image` is given.
    pub owner: Option<String>,

    /// Container package name. Omit to process every package under the owner.
    pub package: Option<String>,

    /// Delete build images strictly older than this many days.
    #[arg(long, value_name = "DAYS")]
    pub older_than_days: Option<i64>,

    /// Always keep at least this many of the most-recent build (hash-time) images, regardless
    /// of age. Text-tagged images are never counted toward this minimum.
    #[arg(long, value_name = "N")]
    pub keep_at_least: Option<usize>,

    /// GitHub token with `read:packages` (+ `delete:packages` for --execute). Normally supplied
    /// via the `GITHUB_TOKEN` environment variable.
    #[arg(long, env = "GITHUB_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Actually delete. Without this flag the tool is a dry run (reports the plan only).
    #[arg(long)]
    pub execute: bool,

    /// Abort if the plan would delete more than this many versions.
    #[arg(long, value_name = "N")]
    pub max_delete: Option<usize>,

    /// Force the owner kind instead of auto-detecting it.
    #[arg(long, value_enum)]
    pub owner_type: Option<OwnerTypeArg>,

    /// Also protect subject-linked (referrers-style) attestations of retained images.
    #[arg(long)]
    pub protect_subject_attestations: bool,

    /// Concurrency for read/manifest requests (default 8).
    #[arg(long, value_name = "N")]
    pub read_concurrency: Option<usize>,

    /// Concurrency for DELETE requests (default 3; kept low for GitHub secondary rate limits).
    #[arg(long, value_name = "N")]
    pub delete_concurrency: Option<usize>,

    /// Append a JSONL audit record of each deleted version to this file (only with --execute).
    #[arg(long, value_name = "PATH")]
    pub audit_log: Option<std::path::PathBuf>,

    /// Override the GitHub REST API base URL (e.g. a GitHub Enterprise Server instance).
    #[arg(long, value_name = "URL")]
    pub github_api_url: Option<String>,

    /// Override the container registry base URL (advanced/testing).
    #[arg(long, value_name = "URL", hide = true)]
    pub registry_url: Option<String>,
}

impl Args {
    /// Merge CLI flags and env into a validated [`Config`].
    pub fn resolve(self) -> Result<Config> {
        let (owner, package) = match &self.image {
            Some(image) => {
                let (o, p) = parse_image(image)?;
                (o, Some(p))
            }
            None => {
                let owner = self.owner.ok_or_else(|| {
                    Error::Config(
                        "a target is required: pass --image ghcr.io/<owner>/<package>, or the \
                         positional owner [package]"
                            .into(),
                    )
                })?;
                (owner, self.package)
            }
        };

        let older_than_days = self
            .older_than_days
            .ok_or_else(|| Error::Config("--older-than-days is required".into()))?;
        if older_than_days < 0 {
            return Err(Error::Config("--older-than-days must be >= 0".into()));
        }

        let keep_at_least = self
            .keep_at_least
            .ok_or_else(|| Error::Config("--keep-at-least is required".into()))?;

        let token = self.token.ok_or_else(|| {
            Error::Config("no GitHub token: set GITHUB_TOKEN (or pass --token)".into())
        })?;

        let owner_type = self.owner_type.map(|t| match t {
            OwnerTypeArg::User => OwnerType::User,
            OwnerTypeArg::Org => OwnerType::Organization,
        });

        let read_concurrency = self.read_concurrency.unwrap_or(8);
        let delete_concurrency = self.delete_concurrency.unwrap_or(3);
        if read_concurrency == 0 || delete_concurrency == 0 {
            return Err(Error::Config("concurrency values must be >= 1".into()));
        }

        Ok(Config {
            owner,
            package,
            owner_type,
            policy: Policy {
                older_than_days,
                keep_at_least,
            },
            token,
            dry_run: !self.execute,
            max_delete: self.max_delete,
            read_concurrency,
            delete_concurrency,
            protect_subject_attestations: self.protect_subject_attestations,
            audit_log: self.audit_log,
            api_base: self.github_api_url,
            registry_base: self.registry_url,
        })
    }
}

/// Parse a `ghcr.io/<owner>/<package>` reference into `(owner, package)`.
///
/// Rejects any tag/digest suffix, a non-`ghcr.io` host, or a shape that isn't exactly
/// `host/owner/package` (three non-empty segments) — a wrong parse would target the wrong
/// package, so we fail loudly instead of guessing.
pub fn parse_image(image: &str) -> Result<(String, String)> {
    let reference = image.trim();
    if reference.contains('@')
        || reference
            .rsplit('/')
            .next()
            .is_some_and(|s| s.contains(':'))
    {
        return Err(Error::Config(format!(
            "--image {image:?} must be a bare repository (no :tag or @digest)"
        )));
    }
    let parts: Vec<&str> = reference.split('/').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(Error::Config(format!(
            "--image {image:?} must be exactly ghcr.io/<owner>/<package>"
        )));
    }
    if parts[0] != "ghcr.io" {
        return Err(Error::Config(format!(
            "--image {image:?} must be on ghcr.io (got host {:?})",
            parts[0]
        )));
    }
    Ok((parts[1].to_string(), parts[2].to_string()))
}

#[cfg(test)]
mod tests {
    use super::parse_image;

    #[test]
    fn parses_valid_reference() {
        let (o, p) = parse_image("ghcr.io/unb-libraries/cogswell.lib.unb.ca").unwrap();
        assert_eq!(o, "unb-libraries");
        assert_eq!(p, "cogswell.lib.unb.ca");
    }

    #[test]
    fn rejects_missing_host() {
        assert!(parse_image("unb-libraries/foo").is_err());
    }

    #[test]
    fn rejects_extra_segments() {
        assert!(parse_image("ghcr.io/unb-libraries/group/foo").is_err());
    }

    #[test]
    fn rejects_non_ghcr_host() {
        assert!(parse_image("docker.io/unb-libraries/foo").is_err());
    }

    #[test]
    fn rejects_tag_or_digest() {
        assert!(parse_image("ghcr.io/unb-libraries/foo:dev").is_err());
        assert!(parse_image("ghcr.io/unb-libraries/foo@sha256:abc").is_err());
    }

    #[test]
    fn preserves_case_and_dots() {
        // The engine lowercases for the registry itself; parsing must not mangle the input.
        let (o, p) = parse_image("ghcr.io/UNB-Libraries/Foo.Bar").unwrap();
        assert_eq!(o, "UNB-Libraries");
        assert_eq!(p, "Foo.Bar");
    }
}
