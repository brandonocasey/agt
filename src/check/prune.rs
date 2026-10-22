//! Which recorded passes `agt cleanup` may drop.

pub(crate) use super::Entry;
use crate::core::clock;

/// Whether the pass ran `days` or more before `now`. `days` MUST be the retention of the output
/// logs, so a hit never points at a pruned log. A timestamp that cannot be read keeps the entry.
pub fn expired(entry: &Entry, now: i64, days: i64) -> bool {
    clock::parse(&entry.ran_at).is_ok_and(|ran| now - ran >= days * 86_400)
}
