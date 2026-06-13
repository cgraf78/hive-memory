//! Currentness checks for indexed memory validity windows.

use crate::index::IndexEntry;
use time::OffsetDateTime;

/// Return whether an indexed record is currently valid for injection.
pub(crate) fn allows_current(entry: &IndexEntry) -> bool {
    allows(entry, false)
}

/// Return whether an indexed record is valid for search.
///
/// Future records are always hidden. Expired records are included only when the
/// query explicitly asks for historical memory.
pub(crate) fn allows_search(entry: &IndexEntry, include_expired: bool) -> bool {
    allows(entry, include_expired)
}

fn allows(entry: &IndexEntry, include_expired: bool) -> bool {
    let now = OffsetDateTime::now_utc();
    if let Some(valid_from) = entry.valid_from.as_deref()
        && let Some(valid_from) = parse_time(valid_from)
        && valid_from > now
    {
        return false;
    }
    if !include_expired
        && let Some(valid_to) = entry.valid_to.as_deref()
        && let Some(valid_to) = parse_time(valid_to)
        && valid_to <= now
    {
        return false;
    }
    true
}

fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
}
