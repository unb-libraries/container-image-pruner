//! Output and audit: the machine-/human-readable run summary, and the append-only audit
//! trail written as deletions happen (so a crash mid-run still leaves an accurate record).

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::plan::{DeleteReason, KeepReason, PrunePlan};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageStatus {
    /// Fully processed as planned.
    Clean,
    /// Processed, but the untagged pass was skipped (incomplete manifest fetch) or the run
    /// was cut short by rate limiting.
    Degraded,
    /// Could not be processed (e.g. auth/parse error); nothing was deleted for it.
    Errored,
}

/// One deleted-or-planned version, used both in the summary and the audit trail.
#[derive(Debug, Clone, Serialize)]
pub struct DeleteRecord {
    pub package: String,
    pub version_id: u64,
    pub digest: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub reason: &'static str,
    /// True once actually deleted (false in dry-run).
    pub executed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
}

/// One kept version and why it was kept.
#[derive(Debug, Clone, Serialize)]
pub struct KeepRecord {
    pub version_id: u64,
    pub digest: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageReport {
    pub package: String,
    pub status: PackageStatus,
    pub kept: usize,
    /// Number of versions in the delete plan (executed or not).
    pub planned_delete: usize,
    /// Number actually deleted (equals `planned_delete` in a clean `--execute`, 0 in dry-run).
    pub deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub deletes: Vec<DeleteRecord>,
    pub keeps: Vec<KeepRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub dry_run: bool,
    /// True if any package was cut short by the primary rate limit.
    pub rate_limited: bool,
    /// Set when the run was aborted before executing (e.g. the plan exceeded `--max-delete`).
    /// Nothing was deleted; the reported `planned_delete` counts show what *would* have been.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aborted: Option<String>,
    pub packages: Vec<PackageReport>,
}

impl Summary {
    pub fn worst_status(&self) -> PackageStatus {
        let mut worst = PackageStatus::Clean;
        for p in &self.packages {
            worst = match (worst, p.status) {
                (_, PackageStatus::Errored) => PackageStatus::Errored,
                (PackageStatus::Errored, _) => PackageStatus::Errored,
                (_, PackageStatus::Degraded) | (PackageStatus::Degraded, _) => {
                    PackageStatus::Degraded
                }
                _ => PackageStatus::Clean,
            };
        }
        worst
    }

    /// Process exit code: 0 clean; 2 if the run aborted, was rate-limited, or any package
    /// degraded/errored (all "safe to re-run").
    pub fn exit_code(&self) -> i32 {
        if self.aborted.is_some()
            || self.rate_limited
            || self.worst_status() != PackageStatus::Clean
        {
            2
        } else {
            0
        }
    }

    /// A single-word run status for the Action output.
    pub fn status_word(&self) -> &'static str {
        if self.aborted.is_some() {
            "aborted"
        } else {
            match self.worst_status() {
                PackageStatus::Clean if self.rate_limited => "degraded",
                PackageStatus::Clean => "clean",
                PackageStatus::Degraded => "degraded",
                PackageStatus::Errored => "errored",
            }
        }
    }

    pub fn total_planned_delete(&self) -> usize {
        self.packages.iter().map(|p| p.planned_delete).sum()
    }

    pub fn total_deleted(&self) -> usize {
        self.packages.iter().map(|p| p.deleted).sum()
    }

    pub fn total_kept(&self) -> usize {
        self.packages.iter().map(|p| p.kept).sum()
    }
}

/// Build a `PackageReport` from a plan where nothing has been executed yet: `deletes` lists
/// the planned deletions with `executed = false`. Used for dry-run, the execute preview, and
/// the aborted/rate-limited cases.
pub fn planned_report(
    package: &str,
    status: PackageStatus,
    plan: &PrunePlan,
    message: Option<String>,
) -> PackageReport {
    let deletes = plan
        .delete
        .iter()
        .map(|d| DeleteRecord {
            package: package.to_string(),
            version_id: d.id,
            digest: d.digest.clone(),
            tags: d.tags.clone(),
            created_at: d.created_at,
            reason: delete_reason_str(d.reason),
            executed: false,
            deleted_at: None,
        })
        .collect();
    PackageReport {
        package: package.to_string(),
        status,
        kept: plan.keep.len(),
        planned_delete: plan.delete.len(),
        deleted: 0,
        message,
        deletes,
        keeps: keep_records(plan),
    }
}

/// Build `KeepRecord`s from a plan's kept versions.
pub fn keep_records(plan: &PrunePlan) -> Vec<KeepRecord> {
    plan.keep
        .iter()
        .map(|k| KeepRecord {
            version_id: k.id,
            digest: k.digest.clone(),
            tags: k.tags.clone(),
            created_at: k.created_at,
            reason: keep_reason_str(k.reason),
        })
        .collect()
}

pub fn delete_reason_str(r: DeleteReason) -> &'static str {
    match r {
        DeleteReason::OldBuildImage => "old-build-image",
        DeleteReason::OrphanedChild => "orphaned-child",
    }
}

pub fn keep_reason_str(r: KeepReason) -> &'static str {
    match r {
        KeepReason::TextTagged => "text-tagged",
        KeepReason::WithinKeepAtLeast => "within-keep-at-least",
        KeepReason::NotOldEnough => "not-old-enough",
        KeepReason::ReferencedByRetained => "referenced-by-retained",
        KeepReason::AttestsRetained => "attests-retained",
        KeepReason::UntaggedPassSkipped => "untagged-pass-skipped",
    }
}

/// A **compact** Markdown summary for `$GITHUB_STEP_SUMMARY`: one row per package (counts +
/// status), totals, and any notes — deliberately *not* the full per-version lists (those live in
/// the JSON stdout / audit artifact).
pub fn step_summary_markdown(summary: &Summary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "### container-image-pruner\n");
    let mode = if summary.dry_run {
        "Dry run — nothing was deleted (plan only)"
    } else {
        "Execute"
    };
    let _ = writeln!(out, "**Mode:** {mode}  ");
    if let Some(reason) = &summary.aborted {
        let _ = writeln!(out, "**Aborted:** {reason}  ");
    }
    if summary.rate_limited {
        let _ = writeln!(
            out,
            "**Note:** cut short by the GitHub rate limit — safe to re-run (converges).  "
        );
    }
    let verb = if summary.dry_run {
        "Planned"
    } else {
        "Deleted"
    };
    let _ = writeln!(
        out,
        "\n| Package | Status | Kept | Planned | {verb} |\n|---|---|---:|---:|---:|"
    );
    for p in &summary.packages {
        let _ = writeln!(
            out,
            "| {pkg} | {status} | {kept} | {planned} | {deleted} |",
            pkg = p.package,
            status = status_word(p.status),
            kept = p.kept,
            planned = p.planned_delete,
            deleted = p.deleted,
        );
    }
    let _ = writeln!(
        out,
        "\n**Total:** kept {kept}, planned {planned}, deleted {deleted} across {n} package(s).",
        kept = summary.total_kept(),
        planned = summary.total_planned_delete(),
        deleted = summary.total_deleted(),
        n = summary.packages.len(),
    );
    let notes: Vec<String> = summary
        .packages
        .iter()
        .filter_map(|p| {
            p.message
                .as_ref()
                .map(|m| format!("- **{}**: {m}", p.package))
        })
        .collect();
    if !notes.is_empty() {
        let _ = writeln!(out, "\n**Notes:**\n{}", notes.join("\n"));
    }
    out
}

/// The `key=value` lines for `$GITHUB_OUTPUT`.
pub fn github_outputs(summary: &Summary) -> String {
    format!(
        "deleted={}\nplanned-delete={}\nstatus={}\n",
        summary.total_deleted(),
        summary.total_planned_delete(),
        summary.status_word(),
    )
}

/// Emit GitHub workflow annotations (to stderr, so stdout stays pure JSON) for anything a human
/// should notice: an abort, and each degraded/errored package.
pub fn emit_annotations(summary: &Summary) {
    if let Some(reason) = &summary.aborted {
        eprintln!("::warning title=container-image-pruner aborted::{reason}");
    }
    for p in &summary.packages {
        match p.status {
            PackageStatus::Errored => eprintln!(
                "::error title=prune failed for {}::{}",
                p.package,
                p.message.as_deref().unwrap_or("errored")
            ),
            PackageStatus::Degraded => eprintln!(
                "::warning title=prune degraded for {}::{}",
                p.package,
                p.message.as_deref().unwrap_or("degraded")
            ),
            PackageStatus::Clean => {}
        }
    }
}

fn status_word(s: PackageStatus) -> &'static str {
    match s {
        PackageStatus::Clean => "ok",
        PackageStatus::Degraded => "DEGRADED",
        PackageStatus::Errored => "ERROR",
    }
}

/// Append-only JSONL audit trail. Disabled in dry-run or when no path is configured.
pub struct AuditWriter {
    inner: Option<std::fs::File>,
}

impl AuditWriter {
    pub fn new(dry_run: bool, path: Option<&Path>) -> Result<Self> {
        let inner = match (dry_run, path) {
            (false, Some(p)) => Some(OpenOptions::new().create(true).append(true).open(p)?),
            _ => None,
        };
        Ok(Self { inner })
    }

    /// Write one record as a JSON line and flush immediately.
    pub fn record(&mut self, rec: &DeleteRecord) -> Result<()> {
        if let Some(f) = self.inner.as_mut() {
            let line = serde_json::to_string(rec).map_err(|e| crate::error::Error::Parse {
                context: "audit record".to_string(),
                source: e,
            })?;
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        Ok(())
    }
}
