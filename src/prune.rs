//! Orchestration: turn a [`Config`] into a [`Summary`] by planning each package (read-only)
//! and, when `--execute` is set, carrying out the deletions after a cap + confirmation gate.
//!
//! All the safety rules live here at the seams between the pure planner and the adapters:
//!   * hard-stop the untagged pass if any retained-manifest fetch failed;
//!   * conservatively protect a candidate whose subject-scan fetch failed;
//!   * fail-soft on the primary rate limit (finish the current package, then stop);
//!   * TOCTOU re-check a version's tags immediately before deleting it.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};

use crate::config::{Config, OwnerType};
use crate::domain::plan::{self, PlannedDelete, PrunePlan, VersionInput};
use crate::domain::tag::{classify, Classification};
use crate::error::Result;
use crate::github::{auth, client::GhClient, packages, registry};
use crate::report::{
    delete_reason_str, planned_report, AuditWriter, DeleteRecord, PackageReport, PackageStatus,
    Summary,
};

/// A per-package plan produced by the read-only planning phase.
pub(crate) struct PackagePlan {
    pub package: String,
    pub status: PackageStatus,
    pub message: Option<String>,
    pub plan: PrunePlan,
}

/// Entry point: plan (and optionally execute) pruning for the configured owner/package(s).
pub async fn run(config: Config) -> Result<Summary> {
    let mut client = GhClient::new(config.token.clone())?;
    if let Some(api) = &config.api_base {
        client.api_base = api.clone();
    }
    if let Some(reg) = &config.registry_base {
        client.registry_base = reg.clone();
    }
    let owner_type = match config.owner_type {
        Some(t) => t,
        None => packages::detect_owner_type(&client, &config.owner).await?,
    };
    let now = Utc::now();

    let names = match &config.package {
        Some(p) => vec![p.clone()],
        None => packages::list_container_packages(&client, &config.owner, owner_type).await?,
    };

    // Planning phase (read-only). Fail-soft on rate limit: stop after the current package.
    let mut plans: Vec<PackagePlan> = Vec::new();
    let mut rate_limited = false;
    for name in &names {
        match plan_package(&client, &config, owner_type, name, now).await {
            Ok(pp) => plans.push(pp),
            Err(e) if e.is_rate_limited() => {
                rate_limited = true;
                break;
            }
            Err(e) => plans.push(PackagePlan {
                package: name.clone(),
                status: PackageStatus::Errored,
                message: Some(e.to_string()),
                plan: PrunePlan {
                    delete: Vec::new(),
                    keep: Vec::new(),
                },
            }),
        }
    }

    if config.dry_run {
        return Ok(build_summary(true, rate_limited, &plans));
    }

    let total_planned: usize = plans.iter().map(|p| p.plan.delete.len()).sum();
    if total_planned == 0 {
        return Ok(build_summary(false, rate_limited, &plans));
    }

    // Anomaly tripwire: if the plan is unexpectedly large, abort before deleting anything and
    // report it (exit 2) so a human can review — never a silent partial run.
    if let Some(max) = config.max_delete {
        if total_planned > max {
            let mut summary = build_summary(false, rate_limited, &plans);
            summary.aborted = Some(format!(
                "plan would delete {total_planned} versions, exceeding max-delete {max}; \
                 nothing deleted — review, then re-run with a higher --max-delete"
            ));
            return Ok(summary);
        }
    }

    execute(&client, &config, owner_type, plans, rate_limited, now).await
}

/// Read-only planning for one package.
async fn plan_package(
    client: &GhClient,
    config: &Config,
    owner_type: OwnerType,
    package: &str,
    now: DateTime<Utc>,
) -> Result<PackagePlan> {
    let versions = packages::list_versions(client, &config.owner, owner_type, package).await?;
    let inputs: Vec<VersionInput> = versions
        .iter()
        .map(|v| VersionInput {
            id: v.id,
            digest: v.name.clone(),
            tags: v.tags(),
            created_at: v.created_at,
        })
        .collect();

    let selection = plan::select(&inputs, &config.policy, now);

    // No untagged versions => no children to protect => no registry calls at all.
    if !selection.has_untagged {
        let plan = plan::finalize(selection, &HashSet::new(), &HashSet::new());
        return Ok(clean(package, plan));
    }

    let token = match auth::pull_token(client, &config.owner, package).await {
        Ok(t) => t,
        Err(e) if e.is_rate_limited() => return Err(e),
        Err(e) => {
            let plan = plan::finalize_builds_only(selection);
            return Ok(degraded(
                package,
                plan,
                format!("could not obtain registry token ({e}); skipped untagged cleanup"),
            ));
        }
    };

    let retained_set = selection.retained_digests.clone();
    let retained: Vec<String> = retained_set.iter().cloned().collect();

    // Fetch retained manifests to learn which children to protect.
    let fetch_results = fetch_manifests(client, config, package, &token, retained).await;
    let mut protected: HashSet<String> = HashSet::new();
    let mut fetch_failed = false;
    for (_digest, res) in fetch_results {
        match res {
            Ok(info) => protected.extend(info.children),
            Err(e) if e.is_rate_limited() => return Err(e),
            Err(_) => fetch_failed = true,
        }
    }

    // Incomplete protected set is exactly how the #63 breakage happens: skip the untagged
    // pass (but still delete plainly build-tagged versions).
    if fetch_failed {
        let plan = plan::finalize_builds_only(selection);
        return Ok(degraded(
            package,
            plan,
            "a retained manifest fetch failed; skipped untagged cleanup for safety".to_string(),
        ));
    }

    // Optional subject-scan to protect subject-linked attestations of retained images.
    let mut subject_protected: HashSet<String> = HashSet::new();
    if config.protect_subject_attestations {
        let candidates: Vec<String> = selection
            .candidate_untagged
            .iter()
            .map(|c| c.digest.clone())
            .filter(|d| !protected.contains(d))
            .collect();
        let scan = fetch_manifests(client, config, package, &token, candidates).await;
        for (digest, res) in scan {
            match res {
                Ok(info) => {
                    if info
                        .subject
                        .as_deref()
                        .is_some_and(|s| retained_set.contains(s))
                    {
                        subject_protected.insert(digest);
                    }
                }
                Err(e) if e.is_rate_limited() => return Err(e),
                // Could not verify -> conservatively protect (never delete the unverified).
                Err(_) => {
                    subject_protected.insert(digest);
                }
            }
        }
    }

    let plan = plan::finalize(selection, &protected, &subject_protected);
    Ok(clean(package, plan))
}

/// Concurrently fetch manifests for a set of digests, pairing each result with its digest.
async fn fetch_manifests(
    client: &GhClient,
    config: &Config,
    package: &str,
    token: &str,
    digests: Vec<String>,
) -> Vec<(String, Result<registry::ManifestInfo>)> {
    stream::iter(digests)
        .map(|d| async move {
            let res = registry::fetch_manifest(client, &config.owner, package, token, &d).await;
            (d, res)
        })
        .buffer_unordered(config.read_concurrency)
        .collect()
        .await
}

/// Execute the planned deletions across packages, honoring delete concurrency, the TOCTOU
/// re-check, the audit trail, and fail-soft rate limiting.
async fn execute(
    client: &GhClient,
    config: &Config,
    owner_type: OwnerType,
    plans: Vec<PackagePlan>,
    rate_limited: bool,
    now: DateTime<Utc>,
) -> Result<Summary> {
    let mut audit = AuditWriter::new(false, config.audit_log.as_deref())?;
    let mut reports: Vec<PackageReport> = Vec::new();
    let mut stopped = rate_limited;

    for pp in &plans {
        if matches!(pp.status, PackageStatus::Errored) || pp.plan.delete.is_empty() {
            reports.push(final_report(pp, Vec::new(), 0, 0, false, stopped));
            continue;
        }
        if stopped {
            let mut r = planned_report(&pp.package, pp.status, &pp.plan, pp.message.clone());
            r.status = PackageStatus::Degraded;
            r.message = merge(r.message, "run stopped by rate limit before execution");
            reports.push(r);
            continue;
        }

        let outcomes = stream::iter(pp.plan.delete.clone())
            .map(|d| delete_one(client, config, owner_type, &pp.package, d, now))
            .buffer_unordered(config.delete_concurrency)
            .collect::<Vec<_>>()
            .await;

        let mut records = Vec::new();
        let (mut deleted, mut skipped, mut failed, mut hit_limit) = (0usize, 0usize, 0usize, false);
        for oc in outcomes {
            match oc {
                Outcome::Deleted(rec) => {
                    audit.record(&rec)?;
                    deleted += 1;
                    records.push(rec);
                }
                Outcome::Skipped => skipped += 1,
                Outcome::Failed => failed += 1,
                Outcome::RateLimited => hit_limit = true,
            }
        }
        if hit_limit {
            stopped = true;
        }
        reports.push(final_report(
            pp,
            records,
            deleted,
            skipped,
            failed > 0,
            hit_limit,
        ));
    }

    Ok(Summary {
        dry_run: false,
        rate_limited: stopped,
        aborted: None,
        packages: reports,
    })
}

enum Outcome {
    Deleted(DeleteRecord),
    Skipped,
    Failed,
    RateLimited,
}

/// Delete a single version after a TOCTOU tag re-check.
async fn delete_one(
    client: &GhClient,
    config: &Config,
    owner_type: OwnerType,
    package: &str,
    d: PlannedDelete,
    now: DateTime<Utc>,
) -> Outcome {
    let expected = match d.reason {
        plan::DeleteReason::OldBuildImage => Classification::Build,
        plan::DeleteReason::OrphanedChild => Classification::Untagged,
    };

    match packages::get_version(client, &config.owner, owner_type, package, d.id).await {
        Ok(v) => {
            if classify(&v.tags()) != expected {
                // Tags changed since planning (e.g. a text tag moved onto it) -> do not delete.
                return Outcome::Skipped;
            }
        }
        Err(e) if e.api_status() == Some(404) => {
            // Already gone: report as deleted for an accurate, idempotent audit trail.
            return Outcome::Deleted(deleted_record(package, &d, now));
        }
        Err(e) if e.is_rate_limited() => return Outcome::RateLimited,
        Err(_) => return Outcome::Skipped,
    }

    match packages::delete_version(client, &config.owner, owner_type, package, d.id).await {
        Ok(()) => Outcome::Deleted(deleted_record(package, &d, now)),
        Err(e) if e.is_rate_limited() => Outcome::RateLimited,
        Err(_) => Outcome::Failed,
    }
}

fn deleted_record(package: &str, d: &PlannedDelete, deleted_at: DateTime<Utc>) -> DeleteRecord {
    DeleteRecord {
        package: package.to_string(),
        version_id: d.id,
        digest: d.digest.clone(),
        tags: d.tags.clone(),
        created_at: d.created_at,
        reason: delete_reason_str(d.reason),
        executed: true,
        deleted_at: Some(deleted_at),
    }
}

fn clean(package: &str, plan: PrunePlan) -> PackagePlan {
    PackagePlan {
        package: package.to_string(),
        status: PackageStatus::Clean,
        message: None,
        plan,
    }
}

fn degraded(package: &str, plan: PrunePlan, message: String) -> PackagePlan {
    PackagePlan {
        package: package.to_string(),
        status: PackageStatus::Degraded,
        message: Some(message),
        plan,
    }
}

fn build_summary(dry_run: bool, rate_limited: bool, plans: &[PackagePlan]) -> Summary {
    Summary {
        dry_run,
        rate_limited,
        aborted: None,
        packages: plans
            .iter()
            .map(|pp| planned_report(&pp.package, pp.status, &pp.plan, pp.message.clone()))
            .collect(),
    }
}

fn final_report(
    pp: &PackagePlan,
    records: Vec<DeleteRecord>,
    deleted: usize,
    skipped: usize,
    failed: bool,
    hit_limit: bool,
) -> PackageReport {
    let mut status = pp.status;
    let mut msg = pp.message.clone();
    if skipped > 0 {
        msg = merge(
            msg,
            &format!("{skipped} skipped (tags changed since planning)"),
        );
    }
    if failed {
        status = PackageStatus::Degraded;
        msg = merge(msg, "one or more deletions failed");
    }
    if hit_limit {
        status = PackageStatus::Degraded;
        msg = merge(msg, "stopped by rate limit");
    }
    PackageReport {
        package: pp.package.clone(),
        status,
        kept: pp.plan.keep.len(),
        planned_delete: pp.plan.delete.len(),
        deleted,
        message: msg,
        deletes: records,
        keeps: crate::report::keep_records(&pp.plan),
    }
}

fn merge(existing: Option<String>, addition: &str) -> Option<String> {
    match existing {
        Some(e) => Some(format!("{e}; {addition}")),
        None => Some(addition.to_string()),
    }
}
