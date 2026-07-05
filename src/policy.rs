//! The retention policy: the two knobs the user sets, plus the age predicate.
//!
//! `now` is always passed in (never read from the clock here) so the pure planner is
//! deterministic and unit-testable.

use chrono::{DateTime, Duration, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Delete build images strictly older than this many days.
    pub older_than_days: i64,
    /// Always keep at least this many of the most-recent **build** (hash-time) versions,
    /// regardless of age. Text-tagged versions do NOT count toward this minimum.
    pub keep_at_least: usize,
}

impl Policy {
    /// True when `created_at` is strictly older than the cutoff measured from `now`.
    pub fn is_older_than_cutoff(&self, created_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        now - created_at > Duration::days(self.older_than_days)
    }
}
