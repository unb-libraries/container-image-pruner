//! Tag classification.
//!
//! dockworker pushes a per-build tag `<short-sha>-<TIMESTAMP>`. The `TIMESTAMP` comes from
//! `josStorer/get-current-time` with a Moment.js format of `YYYYMMDDHHMMSS`. In Moment.js
//! tokens `MM` = month and `SS` = centiseconds, so the 14 digits are actually
//! `YYYY·MM·DD·HH·MM(month, repeated)·SS(centiseconds 00–99)` — which is why live tags show
//! "seconds" like 83 (e.g. `afef854e-20260704210783`). It is therefore NOT a valid datetime.
//!
//! Consequences baked into this module:
//!   * We never parse the embedded value as a time (age comes from the API `created_at`).
//!   * We classify by shape only, and range-check only the **stable** `YYYYMMDDHH` prefix
//!     (year 20xx / month / day / hour), which holds under both the current quirky format
//!     and any future fix. We deliberately do NOT constrain the trailing 4 digits, so a
//!     format correction upstream will not silently stop the tool from pruning.

use std::sync::LazyLock;

use regex::Regex;

/// `<7..40 hex>-<YYYY(20xx)><MM><DD><HH><4 more digits>`.
static BUILD_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-f]{7,40}-(20\d{2})(\d{2})(\d{2})(\d{2})\d{4}$")
        .expect("BUILD_TAG_RE is a valid regex")
});

/// How a package version relates to pruning, derived from its tag set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Carries at least one non-build tag (human/branch/`sha256-…`/`.sig`). Never deleted.
    Protected,
    /// Every tag is a per-build tag. Eligible for age-based pruning and for the
    /// keep-at-least minimum.
    Build,
    /// No tags at all: a multi-arch child, an attestation, or an orphan.
    Untagged,
}

/// True iff `tag` is a dockworker per-build tag (shape + stable-prefix range check).
pub fn is_build_tag(tag: &str) -> bool {
    match BUILD_TAG_RE.captures(tag) {
        // The capture groups are guaranteed by the regex to be exactly two digits each,
        // so the parses cannot fail; the fallbacks are just to avoid `unwrap`.
        Some(caps) => {
            let month: u32 = caps[2].parse().unwrap_or(0);
            let day: u32 = caps[3].parse().unwrap_or(0);
            let hour: u32 = caps[4].parse().unwrap_or(99);
            (1..=12).contains(&month) && (1..=31).contains(&day) && hour <= 23
        }
        None => false,
    }
}

/// Classify a version from its tags. Empty => `Untagged`; all-build => `Build`; otherwise
/// `Protected` (any single non-build tag is enough to protect the whole version).
pub fn classify(tags: &[String]) -> Classification {
    if tags.is_empty() {
        return Classification::Untagged;
    }
    if tags.iter().all(|t| is_build_tag(t)) {
        Classification::Build
    } else {
        Classification::Protected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn real_dockworker_tags_are_build_tags() {
        // Live tags from unb-libraries/loyalist.lib.unb.ca, incl. invalid "seconds".
        for t in [
            "afef854e-20260704210783",
            "afef854e-20260704210775",
            "afef854e-20260702140751",
            "fcb877e8-20260629110615",
        ] {
            assert!(is_build_tag(t), "{t} should classify as a build tag");
        }
    }

    #[test]
    fn short_and_long_shas() {
        assert!(is_build_tag("a1b2c3d-20250115143022")); // 7-char sha
        assert!(is_build_tag(&format!("{}-20250115143022", "a".repeat(40)))); // full sha
    }

    #[test]
    fn text_tags_are_not_build_tags() {
        for t in ["prod", "dev", "feature-ee-33", "main", "v1.2.3", "latest"] {
            assert!(!is_build_tag(t), "{t} must not be a build tag");
        }
    }

    #[test]
    fn near_misses_are_rejected() {
        assert!(!is_build_tag("afef854e-19990704210783")); // year not 20xx
        assert!(!is_build_tag("afef854e-20261304210783")); // month 13
        assert!(!is_build_tag("afef854e-20260732210783")); // day 32
        assert!(!is_build_tag("afef854e-20260704250783")); // hour 25
        assert!(!is_build_tag("afef854e-2026070421078")); // 13 digits
        assert!(!is_build_tag("afef854e-202607042107835")); // 15 digits
        assert!(!is_build_tag("abc-20260704210783")); // sha too short (<7)
        assert!(!is_build_tag("ZZZZZZZZ-20260704210783")); // non-hex
        assert!(!is_build_tag("sha256-abc.sig")); // cosign-style
    }

    #[test]
    fn classification() {
        assert_eq!(classify(&[]), Classification::Untagged);
        assert_eq!(
            classify(&s(&["afef854e-20260704210783"])),
            Classification::Build
        );
        assert_eq!(classify(&s(&["prod"])), Classification::Protected);
        // A version tagged BOTH build and prod is Protected (does not count toward keep).
        assert_eq!(
            classify(&s(&["afef854e-20260704210783", "prod"])),
            Classification::Protected
        );
    }
}
