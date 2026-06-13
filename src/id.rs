//! Sortable write identifiers for notes and JSON event sidecars.
//!
//! IDs are part of the storage contract: they become filenames, pair Markdown
//! notes with JSON events, and let cloud-sync systems handle concurrent writes
//! without a shared append-only hot file.

use time::OffsetDateTime;
use uuid::Uuid;

/// Context used to generate a write id.
///
/// The values are sanitized before becoming filename components, so callers may
/// pass host or agent ids from config/env without pre-validating them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteIdContext {
    /// Host component embedded in the filename.
    pub host_id: String,
    /// Agent component embedded in the filename.
    pub agent_id: String,
}

/// Generate a sortable, extensionless write id.
///
/// The format is `YYYYMMDDTHHMMSS.ffffffZ_<host>_<pid>_<agent>_<random>`.
/// Randomness comes from UUIDv7, which is available without a coordinator and
/// preserves rough timestamp ordering for human browsing.
pub fn new_write_id(context: &WriteIdContext) -> String {
    write_id_with_parts(
        OffsetDateTime::now_utc(),
        std::process::id(),
        &context.host_id,
        &context.agent_id,
        &uuid_random_suffix(Uuid::now_v7()),
    )
}

/// Build a write id from explicit parts.
///
/// This is public so tests and import tools can make deterministic paths while
/// still using the same sanitization and formatting contract as production ids.
pub fn write_id_with_parts(
    timestamp: OffsetDateTime,
    pid: u32,
    host_id: &str,
    agent_id: &str,
    random: &str,
) -> String {
    format!(
        "{}_{}_{}_{}_{}",
        timestamp_prefix(timestamp),
        sanitize_component(host_id),
        pid,
        sanitize_component(agent_id),
        sanitize_component(random).to_ascii_lowercase()
    )
}

/// Sanitize one filename component to `[a-zA-Z0-9_-]`.
pub fn sanitize_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn timestamp_prefix(timestamp: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}.{:06}Z",
        timestamp.year(),
        u8::from(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second(),
        timestamp.nanosecond() / 1_000
    )
}

fn uuid_random_suffix(uuid: Uuid) -> String {
    let simple = uuid.simple().to_string();
    simple[simple.len() - 12..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    #[test]
    fn write_id_is_sortable_and_sanitized() {
        let timestamp = OffsetDateTime::from_unix_timestamp(1_778_946_153)
            .expect("timestamp")
            .replace_nanosecond(184_921_000)
            .expect("nanos");

        let id = write_id_with_parts(timestamp, 12345, "tay lor.local", "codex/agent", "A8F31C!!");

        assert_eq!(
            id,
            "20260516T154233.184921Z_tay-lor-local_12345_codex-agent_a8f31c--"
        );
    }

    #[test]
    fn empty_component_becomes_unknown() {
        assert_eq!(sanitize_component(""), "unknown");
    }

    #[test]
    fn timestamp_prefix_preserves_calendar_fields() {
        let timestamp = OffsetDateTime::now_utc()
            .replace_year(2026)
            .expect("year")
            .replace_month(Month::May)
            .expect("month")
            .replace_day(16)
            .expect("day")
            .replace_hour(1)
            .expect("hour")
            .replace_minute(2)
            .expect("minute")
            .replace_second(3)
            .expect("second")
            .replace_nanosecond(4_000)
            .expect("nanos");

        let id = write_id_with_parts(timestamp, 1, "h", "a", "abcdef12");
        assert!(id.starts_with("20260516T010203.000004Z_"));
    }

    #[test]
    fn uuid_suffix_uses_random_tail_bits() {
        let uuid = Uuid::parse_str("018f5f57-bd9b-7d33-9e21-1f44f0c5a013").expect("uuid");

        assert_eq!(uuid_random_suffix(uuid), "1f44f0c5a013");
    }
}
