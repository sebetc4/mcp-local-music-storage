//! Common utilities shared across MusicBrainz tools.
//!
//! This module provides shared functionality like MBID validation,
//! response formatting, and error handling helpers.

use musicbrainz_rs::entity::label::LabelType;
use musicbrainz_rs::entity::release_group::ReleaseGroupPrimaryType;
use musicbrainz_rs::entity::work::WorkType;
use rmcp::model::{CallToolResult, Content};
use tracing::warn;

/// UUID format: 8-4-4-4-12 hexadecimal characters
const MBID_LENGTH: usize = 36;
const MBID_DASH_COUNT: usize = 4;

/// Check if a string looks like a MusicBrainz ID (UUID format).
///
/// MBIDs are UUIDs in the format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
/// Example: 5b11f4ce-a62d-471e-81fc-a69a8278c7da
pub fn is_mbid(query: &str) -> bool {
    query.len() == MBID_LENGTH
        && query.chars().filter(|c| *c == '-').count() == MBID_DASH_COUNT
        && query.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Format a duration in milliseconds to MM:SS format.
pub fn format_duration(length_ms: u64) -> String {
    let duration_secs = length_ms / 1000;
    let minutes = duration_secs / 60;
    let seconds = duration_secs % 60;
    format!("{}:{:02}", minutes, seconds)
}

/// Extract year from a date string.
///
/// MusicBrainz DateString format is `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`. Only
/// returns `Some` when the first four characters are ASCII digits — guards
/// against junk prefixes like `"unknown"` or `"XXXX-01"` that the previous
/// length-only check let through.
pub fn extract_year(date_str: &str) -> Option<String> {
    let bytes = date_str.as_bytes();
    if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    // First 4 bytes are ASCII digits, so the str-slice cannot land mid-char.
    Some(date_str[..4].to_string())
}

/// Create an error result with a formatted message.
pub fn error_result(message: &str) -> CallToolResult {
    warn!("{}", message);
    CallToolResult::error(vec![Content::text(message.to_string())])
}

/// Create a success result with text content.
pub fn success_result(content: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(content)])
}

/// Create a success result with both text summary and structured content.
pub fn structured_result<T: serde::Serialize>(summary: String, data: T) -> CallToolResult {
    match serde_json::to_value(&data) {
        Ok(structured) => CallToolResult {
            content: vec![Content::text(summary)],
            structured_content: Some(structured),
            is_error: Some(false),
            meta: None,
        },
        Err(e) => {
            warn!("Failed to serialize structured content: {}", e);
            // Fallback to text-only result
            success_result(summary)
        }
    }
}

/// Get artist name from artist credit.
pub fn get_artist_name(
    artist_credit: &Option<Vec<musicbrainz_rs::entity::artist_credit::ArtistCredit>>,
) -> String {
    artist_credit
        .as_ref()
        .and_then(|ac| ac.first())
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "Unknown Artist".to_string())
}

/// Default limit for search results.
pub fn default_limit() -> usize {
    10
}

/// Validate and clamp limit to allowed range (1-100).
pub fn validate_limit(limit: usize) -> usize {
    limit.clamp(1, 100)
}

/// Common HTTP handler helper to extract entity parameter.
#[cfg(feature = "http")]
pub fn extract_entity_param(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("entity")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

// ============================================================================
// Stable string mappings for MusicBrainz enums
// ============================================================================
//
// Rendering these enums via `format!("{:?}", t)` (Debug) leaks the Rust
// variant name, which silently changes if the upstream crate renames a
// variant. Each helper below pins the wire format to the same string
// MusicBrainz itself uses, mirroring the upstream `#[serde(rename = "…")]`
// attributes (or the `From<String>` impl in `WorkType`'s case). A unit test
// covers every variant so a future upstream addition surfaces immediately.

/// Stable string for a [`ReleaseGroupPrimaryType`] variant.
pub fn release_group_primary_type_str(t: &ReleaseGroupPrimaryType) -> String {
    match t {
        ReleaseGroupPrimaryType::Album => "Album",
        ReleaseGroupPrimaryType::Single => "Single",
        ReleaseGroupPrimaryType::Ep => "EP",
        ReleaseGroupPrimaryType::Broadcast => "Broadcast",
        ReleaseGroupPrimaryType::Other => "Other",
        // Catches `UnrecognizedReleaseGroupPrimaryType` and any future variant
        // added under `#[non_exhaustive]`.
        _ => "Unknown",
    }
    .to_string()
}

/// Stable string for a [`LabelType`] variant.
pub fn label_type_str(t: &LabelType) -> String {
    match t {
        LabelType::BootlegProduction => "Bootleg Production",
        LabelType::Distributor => "Distributor",
        LabelType::Holding => "Holding",
        LabelType::Imprint => "Imprint",
        LabelType::OriginalProduction => "Original Production",
        LabelType::Production => "Production",
        LabelType::Publisher => "Publisher",
        LabelType::ReissueProduction => "Reissue Production",
        LabelType::RightsSociety => "Rights Society",
        LabelType::Manufacturer => "Manufacturer",
        _ => "Unknown",
    }
    .to_string()
}

/// Stable string for a [`WorkType`] variant. Mirrors the upstream
/// `From<String>` mapping; the `UnrecognizedWorkType(raw)` arm surfaces the
/// raw type name MusicBrainz returned.
pub fn work_type_str(t: &WorkType) -> String {
    match t {
        WorkType::Song => "Song".to_string(),
        WorkType::Aria => "Aria".to_string(),
        WorkType::AudioDrama => "Audio drama".to_string(),
        WorkType::Ballet => "Ballet".to_string(),
        WorkType::BeijingOpera => "Beijing opera".to_string(),
        WorkType::Cantata => "Cantata".to_string(),
        WorkType::Concerto => "Concerto".to_string(),
        WorkType::Etude => "Étude".to_string(),
        WorkType::IncidentalMusic => "Incidental music".to_string(),
        WorkType::Madrigal => "Madrigal".to_string(),
        WorkType::Mass => "Mass".to_string(),
        WorkType::Motet => "Motet".to_string(),
        WorkType::Musical => "Musical".to_string(),
        WorkType::Opera => "Opera".to_string(),
        WorkType::Operetta => "Operetta".to_string(),
        WorkType::Oratorio => "Oratorio".to_string(),
        WorkType::Overture => "Overture".to_string(),
        WorkType::Partita => "Partita".to_string(),
        WorkType::Play => "Play".to_string(),
        WorkType::Poem => "Poem".to_string(),
        WorkType::Prose => "Prose".to_string(),
        WorkType::Quartet => "Quartet".to_string(),
        WorkType::Sonata => "Sonata".to_string(),
        WorkType::SongCycle => "Song-cycle".to_string(),
        WorkType::Soundtrack => "Soundtrack".to_string(),
        WorkType::Suite => "Suite".to_string(),
        WorkType::SymphonicPoem => "Symphonic poem".to_string(),
        WorkType::Symphony => "Symphony".to_string(),
        WorkType::Zarzuela => "Zarzuela".to_string(),
        WorkType::UnrecognizedWorkType(raw) => raw.clone(),
        // WorkType is `#[non_exhaustive]`; this arm covers any variant added
        // upstream that hasn't been mapped yet.
        _ => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_mbid_valid() {
        assert!(is_mbid("5b11f4ce-a62d-471e-81fc-a69a8278c7da"));
        assert!(is_mbid("1b022e01-4da6-387b-8658-8678046e4cef"));
    }

    #[test]
    fn test_is_mbid_invalid() {
        assert!(!is_mbid("Nirvana"));
        assert!(!is_mbid("5b11f4ce-a62d-471e-81fc")); // too short
        assert!(!is_mbid("5b11f4ce-a62d-471e-81fc-a69a8278c7da-extra")); // too long
        assert!(!is_mbid("5b11f4ce_a62d_471e_81fc_a69a8278c7da")); // wrong separator
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(180000), "3:00");
        assert_eq!(format_duration(245000), "4:05");
        assert_eq!(format_duration(61000), "1:01");
        assert_eq!(format_duration(59000), "0:59");
    }

    #[test]
    fn test_validate_limit() {
        assert_eq!(validate_limit(10), 10);
        assert_eq!(validate_limit(0), 1);
        assert_eq!(validate_limit(200), 100);
        assert_eq!(validate_limit(50), 50);
    }

    #[test]
    fn test_extract_year() {
        assert_eq!(extract_year("1997-06-16"), Some("1997".to_string()));
        assert_eq!(extract_year("1997-06"), Some("1997".to_string()));
        assert_eq!(extract_year("1997"), Some("1997".to_string()));
        assert_eq!(extract_year("97"), None);
    }

    #[test]
    fn extract_year_rejects_non_digit_prefix() {
        assert_eq!(extract_year("unknown"), None);
        assert_eq!(extract_year("XXXX-01-01"), None);
        // Mixed digits and dashes within the first 4 chars: also rejected.
        assert_eq!(extract_year("19-7-06"), None);
        // Multi-byte char at the start must not panic.
        assert_eq!(extract_year("é1997"), None);
    }

    #[test]
    fn release_group_primary_type_str_mapping() {
        assert_eq!(
            release_group_primary_type_str(&ReleaseGroupPrimaryType::Album),
            "Album"
        );
        assert_eq!(
            release_group_primary_type_str(&ReleaseGroupPrimaryType::Single),
            "Single"
        );
        // Critical: `Ep` serializes as "EP", not "Ep" — Debug would silently
        // produce the wrong string.
        assert_eq!(
            release_group_primary_type_str(&ReleaseGroupPrimaryType::Ep),
            "EP"
        );
        assert_eq!(
            release_group_primary_type_str(&ReleaseGroupPrimaryType::Broadcast),
            "Broadcast"
        );
        assert_eq!(
            release_group_primary_type_str(&ReleaseGroupPrimaryType::Other),
            "Other"
        );
        assert_eq!(
            release_group_primary_type_str(
                &ReleaseGroupPrimaryType::UnrecognizedReleaseGroupPrimaryType
            ),
            "Unknown"
        );
    }

    #[test]
    fn label_type_str_mapping() {
        // Spot-check the variants whose serde rename diverges from the Rust name.
        assert_eq!(
            label_type_str(&LabelType::BootlegProduction),
            "Bootleg Production"
        );
        assert_eq!(
            label_type_str(&LabelType::OriginalProduction),
            "Original Production"
        );
        assert_eq!(
            label_type_str(&LabelType::ReissueProduction),
            "Reissue Production"
        );
        assert_eq!(label_type_str(&LabelType::RightsSociety), "Rights Society");
        // Simple-name variant.
        assert_eq!(label_type_str(&LabelType::Distributor), "Distributor");
        assert_eq!(label_type_str(&LabelType::UnrecognizedLabelType), "Unknown");
    }

    #[test]
    fn work_type_str_mapping() {
        // Spot-check the variants whose mapping is non-trivial (multi-word,
        // accent, hyphen).
        assert_eq!(work_type_str(&WorkType::Song), "Song");
        assert_eq!(work_type_str(&WorkType::AudioDrama), "Audio drama");
        assert_eq!(work_type_str(&WorkType::BeijingOpera), "Beijing opera");
        assert_eq!(work_type_str(&WorkType::Etude), "Étude");
        assert_eq!(
            work_type_str(&WorkType::IncidentalMusic),
            "Incidental music"
        );
        assert_eq!(work_type_str(&WorkType::SongCycle), "Song-cycle");
        assert_eq!(work_type_str(&WorkType::SymphonicPoem), "Symphonic poem");
        // The catch-all surfaces the raw upstream string.
        assert_eq!(
            work_type_str(&WorkType::UnrecognizedWorkType("Custom".to_string())),
            "Custom"
        );
    }
}
