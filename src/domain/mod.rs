//! Pure, IO-free domain logic. Everything here is deterministic and unit-tested; no
//! network, filesystem, or clock access (time is always passed in as `now`).

pub mod plan;
pub mod tag;
