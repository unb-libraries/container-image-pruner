//! The pure pruning planner.
//!
//! Two deterministic functions with an IO step sandwiched between them (performed by the
//! orchestrator in `prune.rs`):
//!
//! 1. [`select`] classifies versions and decides the build-tag deletions purely from the
//!    version list + policy. It reports which retained digests must have their manifests
//!    fetched (to protect multi-arch children) and which untagged versions are old enough
//!    to be deletion *candidates*.
//! 2. The orchestrator fetches manifests for the retained set → `protected_digests`, and
//!    (optionally) subject-scans candidates → `subject_protected`.
//! 3. [`finalize`] turns candidates into keep/delete decisions using those two sets.
//!    [`finalize_builds_only`] is the degraded path used when the retained-manifest fetch
//!    was incomplete: it deletes only the build versions and keeps every untagged candidate
//!    (never deletes an untagged manifest we could not prove safe).

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::domain::tag::{classify, Classification};
use crate::policy::Policy;

pub type Digest = String;

/// A package version, decoupled from the GitHub API model so the planner stays IO-free.
#[derive(Debug, Clone)]
pub struct VersionInput {
    pub id: u64,
    /// The version `name` from the Packages API, i.e. the `sha256:…` digest.
    pub digest: Digest,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// Carries a non-build tag (human/branch/etc.).
    TextTagged,
    /// One of the `keep_at_least` most-recent build versions.
    WithinKeepAtLeast,
    /// Newer than the age cutoff.
    NotOldEnough,
    /// Referenced (as a child) by a manifest index we are keeping.
    ReferencedByRetained,
    /// Its `subject` attests an image we are keeping.
    AttestsRetained,
    /// The retained-manifest fetch was incomplete, so the untagged pass was skipped.
    UntaggedPassSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// An old build (hash-time) version, beyond the keep-at-least minimum.
    OldBuildImage,
    /// An old untagged manifest not referenced by anything we keep.
    OrphanedChild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedDelete {
    pub id: u64,
    pub digest: Digest,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub reason: DeleteReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptVersion {
    pub id: u64,
    pub digest: Digest,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub reason: KeepReason,
}

/// A too-new-to-delete-yet untagged version whose fate depends on the reference sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateUntagged {
    pub id: u64,
    pub digest: Digest,
    pub created_at: DateTime<Utc>,
}

/// Output of [`select`]: everything the orchestrator needs to run the IO passes and call
/// [`finalize`].
#[derive(Debug, Clone)]
pub struct Selection {
    /// Digests of versions we keep that may be manifest indexes — fetch these to discover
    /// (and thereby protect) their multi-arch children. All are tagged (build or text).
    pub retained_digests: HashSet<Digest>,
    /// Build versions selected for deletion (every tag is a build tag).
    pub deletable_build: Vec<PlannedDelete>,
    /// Untagged versions old enough to be candidates; decided in [`finalize`].
    pub candidate_untagged: Vec<CandidateUntagged>,
    /// Versions kept with a decided reason (text-tagged, within-keep, or not-old-enough).
    pub kept: Vec<KeptVersion>,
    /// Whether the package has any untagged versions at all (governs whether the
    /// orchestrator needs to touch the registry).
    pub has_untagged: bool,
}

/// The final decision set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunePlan {
    pub delete: Vec<PlannedDelete>,
    pub keep: Vec<KeptVersion>,
}

/// Classify versions and decide build-tag deletions. See module docs for the flow.
pub fn select(versions: &[VersionInput], policy: &Policy, now: DateTime<Utc>) -> Selection {
    let mut retained_digests = HashSet::new();
    let mut kept = Vec::new();
    let mut deletable_build = Vec::new();
    let mut candidate_untagged = Vec::new();

    let mut builds: Vec<&VersionInput> = Vec::new();
    let mut has_untagged = false;

    for v in versions {
        match classify(&v.tags) {
            Classification::Protected => {
                retained_digests.insert(v.digest.clone());
                kept.push(KeptVersion {
                    id: v.id,
                    digest: v.digest.clone(),
                    tags: v.tags.clone(),
                    created_at: v.created_at,
                    reason: KeepReason::TextTagged,
                });
            }
            Classification::Build => builds.push(v),
            Classification::Untagged => {
                has_untagged = true;
                if policy.is_older_than_cutoff(v.created_at, now) {
                    candidate_untagged.push(CandidateUntagged {
                        id: v.id,
                        digest: v.digest.clone(),
                        created_at: v.created_at,
                    });
                } else {
                    kept.push(KeptVersion {
                        id: v.id,
                        digest: v.digest.clone(),
                        tags: v.tags.clone(),
                        created_at: v.created_at,
                        reason: KeepReason::NotOldEnough,
                    });
                }
            }
        }
    }

    // Newest first; break ties by id (descending) for a deterministic keep-at-least frontier.
    builds.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));

    for (i, v) in builds.iter().enumerate() {
        if i < policy.keep_at_least {
            // Protected purely by the minimum-keep count, even if old.
            retained_digests.insert(v.digest.clone());
            kept.push(KeptVersion {
                id: v.id,
                digest: v.digest.clone(),
                tags: v.tags.clone(),
                created_at: v.created_at,
                reason: KeepReason::WithinKeepAtLeast,
            });
        } else if policy.is_older_than_cutoff(v.created_at, now) {
            deletable_build.push(PlannedDelete {
                id: v.id,
                digest: v.digest.clone(),
                tags: v.tags.clone(),
                created_at: v.created_at,
                reason: DeleteReason::OldBuildImage,
            });
        } else {
            retained_digests.insert(v.digest.clone());
            kept.push(KeptVersion {
                id: v.id,
                digest: v.digest.clone(),
                tags: v.tags.clone(),
                created_at: v.created_at,
                reason: KeepReason::NotOldEnough,
            });
        }
    }

    Selection {
        retained_digests,
        deletable_build,
        candidate_untagged,
        kept,
        has_untagged,
    }
}

/// Resolve untagged candidates against the reference sets to produce the final plan.
///
/// A candidate is kept if it is referenced by a retained index (`protected_digests`) or, when
/// subject-scanning is enabled, if it attests a retained image (`subject_protected`);
/// otherwise it is deleted as an orphaned child.
pub fn finalize(
    selection: Selection,
    protected_digests: &HashSet<Digest>,
    subject_protected: &HashSet<Digest>,
) -> PrunePlan {
    let Selection {
        deletable_build,
        candidate_untagged,
        mut kept,
        ..
    } = selection;

    let mut delete = deletable_build;

    for c in candidate_untagged {
        if protected_digests.contains(&c.digest) {
            kept.push(KeptVersion {
                id: c.id,
                digest: c.digest,
                tags: Vec::new(),
                created_at: c.created_at,
                reason: KeepReason::ReferencedByRetained,
            });
        } else if subject_protected.contains(&c.digest) {
            kept.push(KeptVersion {
                id: c.id,
                digest: c.digest,
                tags: Vec::new(),
                created_at: c.created_at,
                reason: KeepReason::AttestsRetained,
            });
        } else {
            delete.push(PlannedDelete {
                id: c.id,
                digest: c.digest,
                tags: Vec::new(),
                created_at: c.created_at,
                reason: DeleteReason::OrphanedChild,
            });
        }
    }

    PrunePlan { delete, keep: kept }
}

/// Degraded path: the retained-manifest fetch was incomplete, so `protected_digests` cannot
/// be trusted. Delete only the build versions (safe — deleting an index does not delete its
/// children) and keep every untagged candidate.
pub fn finalize_builds_only(selection: Selection) -> PrunePlan {
    let Selection {
        deletable_build,
        candidate_untagged,
        mut kept,
        ..
    } = selection;

    for c in candidate_untagged {
        kept.push(KeptVersion {
            id: c.id,
            digest: c.digest,
            tags: Vec::new(),
            created_at: c.created_at,
            reason: KeepReason::UntaggedPassSkipped,
        });
    }

    PrunePlan {
        delete: deletable_build,
        keep: kept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(days_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::days(days_ago)
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 5, 12, 0, 0).unwrap()
    }

    fn v(id: u64, digest: &str, tags: &[&str], days_ago: i64) -> VersionInput {
        VersionInput {
            id,
            digest: digest.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            created_at: ts(days_ago, now()),
        }
    }

    fn digests(items: &[PlannedDelete]) -> HashSet<Digest> {
        items.iter().map(|d| d.digest.clone()).collect()
    }

    fn set(items: &[&str]) -> HashSet<Digest> {
        items.iter().map(|s| s.to_string()).collect()
    }

    const POLICY: Policy = Policy {
        older_than_days: 30,
        keep_at_least: 2,
    };

    #[test]
    fn text_tagged_never_deleted_and_not_counted_in_keep() {
        // Two build images (both old) plus a prod tag. keep_at_least=2 keeps both builds by
        // count; prod is protected separately and must NOT consume a keep slot.
        let versions = vec![
            v(1, "sha256:prod", &["prod"], 100),
            v(2, "sha256:b1", &["aaaaaaa-20260101120000"], 100),
            v(3, "sha256:b2", &["bbbbbbb-20260201120000"], 90),
            v(4, "sha256:b3", &["ccccccc-20260301120000"], 80),
        ];
        let sel = select(&versions, &POLICY, now());
        // b3 (newest) and b2 kept by keep-at-least; b1 (oldest, 3rd) deleted.
        let plan = finalize(sel, &HashSet::new(), &HashSet::new());
        assert_eq!(digests(&plan.delete), set(&["sha256:b1"]));
        // prod is kept as TextTagged.
        assert!(plan
            .keep
            .iter()
            .any(|k| k.digest == "sha256:prod" && k.reason == KeepReason::TextTagged));
    }

    #[test]
    fn dual_tagged_build_plus_prod_is_protected() {
        let versions = vec![
            v(1, "sha256:dual", &["aaaaaaa-20260101120000", "prod"], 100),
            v(2, "sha256:b1", &["bbbbbbb-20260101120000"], 100),
        ];
        // keep_at_least=0 so nothing is kept purely by count.
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 0,
        };
        let sel = select(&versions, &policy, now());
        let plan = finalize(sel, &HashSet::new(), &HashSet::new());
        // Only the pure build image is deleted; the dual-tagged one is protected.
        assert_eq!(digests(&plan.delete), set(&["sha256:b1"]));
    }

    #[test]
    fn keep_at_least_protects_recent_builds_even_if_old() {
        let versions = vec![
            v(1, "sha256:b1", &["aaaaaaa-20260101120000"], 100),
            v(2, "sha256:b2", &["bbbbbbb-20260201120000"], 90),
            v(3, "sha256:b3", &["ccccccc-20260301120000"], 80),
        ];
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 5, // more than we have -> keep all
        };
        let sel = select(&versions, &policy, now());
        assert!(sel.deletable_build.is_empty());
    }

    #[test]
    fn not_old_enough_builds_are_kept_and_retained() {
        let versions = vec![
            v(1, "sha256:new", &["aaaaaaa-20260701120000"], 3), // within 30d
            v(2, "sha256:old", &["bbbbbbb-20260101120000"], 100),
        ];
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 0,
        };
        let sel = select(&versions, &policy, now());
        assert_eq!(digests(&sel.deletable_build), set(&["sha256:old"]));
        assert!(sel.retained_digests.contains("sha256:new"));
    }

    #[test]
    fn untagged_orphan_deleted_only_when_unreferenced() {
        let versions = vec![
            v(1, "sha256:idx", &["aaaaaaa-20260701120000"], 3), // kept (new) -> retained
            v(2, "sha256:child_ref", &[], 3),                   // too new -> kept anyway
            v(3, "sha256:child_old_ref", &[], 100),             // old, referenced -> keep
            v(4, "sha256:orphan", &[], 100),                    // old, unreferenced -> delete
        ];
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 0,
        };
        let sel = select(&versions, &policy, now());
        // Simulate: idx references child_old_ref (shared/dedup old child).
        let protected = set(&["sha256:child_old_ref"]);
        let plan = finalize(sel, &protected, &HashSet::new());
        assert_eq!(digests(&plan.delete), set(&["sha256:orphan"]));
        assert!(plan
            .keep
            .iter()
            .any(|k| k.digest == "sha256:child_old_ref"
                && k.reason == KeepReason::ReferencedByRetained));
    }

    #[test]
    fn subject_linked_attestation_of_retained_is_protected() {
        let versions = vec![
            v(1, "sha256:att_old", &[], 100), // old untagged attestation, subject=retained
            v(2, "sha256:orphan", &[], 100),
        ];
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 0,
        };
        let sel = select(&versions, &policy, now());
        let subject_protected = set(&["sha256:att_old"]);
        let plan = finalize(sel, &HashSet::new(), &subject_protected);
        assert_eq!(digests(&plan.delete), set(&["sha256:orphan"]));
        assert!(plan
            .keep
            .iter()
            .any(|k| k.digest == "sha256:att_old" && k.reason == KeepReason::AttestsRetained));
    }

    #[test]
    fn degraded_path_never_deletes_untagged() {
        let versions = vec![
            v(1, "sha256:b1", &["aaaaaaa-20260101120000"], 100),
            v(2, "sha256:orphan", &[], 100),
        ];
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 0,
        };
        let sel = select(&versions, &policy, now());
        let plan = finalize_builds_only(sel);
        // Build image still deleted; untagged candidate kept as UntaggedPassSkipped.
        assert_eq!(digests(&plan.delete), set(&["sha256:b1"]));
        assert!(plan
            .keep
            .iter()
            .any(|k| k.digest == "sha256:orphan" && k.reason == KeepReason::UntaggedPassSkipped));
    }

    #[test]
    fn centisecond_collision_duplicate_tags_are_keyed_by_id() {
        // Two distinct versions can (rarely) share the same tag string. We key on id/digest,
        // never the tag, so both are handled independently.
        let versions = vec![
            v(1, "sha256:dupA", &["afef854e-20260704210783"], 100),
            v(2, "sha256:dupB", &["afef854e-20260704210783"], 99),
        ];
        let policy = Policy {
            older_than_days: 30,
            keep_at_least: 1,
        };
        let sel = select(&versions, &policy, now());
        // Newest (id 2, 99 days) kept by keep-at-least; older (id 1) deleted.
        let plan = finalize(sel, &HashSet::new(), &HashSet::new());
        assert_eq!(digests(&plan.delete), set(&["sha256:dupA"]));
    }

    #[test]
    fn has_untagged_flag() {
        let with = vec![v(1, "sha256:x", &[], 1)];
        let without = vec![v(1, "sha256:x", &["prod"], 1)];
        assert!(select(&with, &POLICY, now()).has_untagged);
        assert!(!select(&without, &POLICY, now()).has_untagged);
    }
}
