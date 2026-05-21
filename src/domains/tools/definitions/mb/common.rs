//! Common utilities shared across MusicBrainz tools.
//!
//! This module provides shared functionality like MBID validation,
//! response formatting, and error handling helpers.

use musicbrainz_rs::entity::alias::Alias;
use musicbrainz_rs::entity::label::LabelType;
use musicbrainz_rs::entity::release_group::ReleaseGroupPrimaryType;
use musicbrainz_rs::entity::tag::Tag;
use musicbrainz_rs::entity::work::WorkType;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Serialize;
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
/// Thin wrapper preserving the historical mb-tool call site signature; the
/// real implementation lives in [`crate::domains::tools::result::structured_ok`].
pub fn structured_result<T: serde::Serialize>(summary: String, data: T) -> CallToolResult {
    crate::domains::tools::result::structured_ok(summary, &data)
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

// ============================================================================
// Search-query resolution (raw Lucene escape hatch)
// ============================================================================

/// Pick which input the search call should use: the typed `query`
/// (already passed through the per-entity query builder upstream) or the
/// caller-supplied raw Lucene query string.
///
/// Contract:
/// - Exactly one of `(typed, raw)` must be non-empty.
/// - When `raw` is set, it goes straight to the MB endpoint as the
///   `query` parameter — caller takes full responsibility for syntax
///   (boolean operators, field filters, date ranges, fuzzy matches).
/// - When both are set, the caller probably has a bug (or stale code);
///   refuse rather than silently picking one.
///
/// Returns the string to pass to `Entity::search(...)`.
pub fn resolve_search_query(typed: &str, raw: Option<&str>) -> Result<String, String> {
    let typed_set = !typed.trim().is_empty();
    let raw_set = raw.map(|s| !s.trim().is_empty()).unwrap_or(false);
    match (typed_set, raw_set) {
        (false, false) => Err(
            "Missing query: provide either `query` (typed) or `raw_lucene_query` (Lucene syntax)"
                .to_string(),
        ),
        (true, true) => {
            Err("Provide exactly one of `query` or `raw_lucene_query`, not both".to_string())
        }
        (true, false) => Ok(typed.to_string()),
        (false, true) => Ok(raw.unwrap_or("").to_string()),
    }
}

// ============================================================================
// Aliases (shared across artist / label / work)
// ============================================================================

/// Stable wire-format alias summary.
///
/// `musicbrainz_rs`'s upstream [`Alias`] type carries some fields we don't
/// surface (`ended`, `type_id`) and shapes its date wrappers in a way that's
/// awkward for agents to parse. This struct pins the contract: same field
/// names across artist / label / work payloads, dates flattened to plain
/// strings, missing booleans normalised to `false`.
///
/// Populated only when the caller passes `include_aliases=true`. When the
/// flag is off, the outer entity's `aliases` field stays `None` and is
/// skipped from the JSON output entirely (via `skip_serializing_if`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AliasInfo {
    /// Display name of the alias (e.g. "Beatles", "The Beatles").
    pub name: String,
    /// Sortable form (e.g. "Beatles, The").
    pub sort_name: String,
    /// `true` when MB marks this alias as the locale's canonical form. The
    /// upstream field is `Option<bool>`; absence is treated as `false`.
    pub primary: bool,
    /// MB-defined kind: `"Legal name"`, `"Search hint"`, `"Artist name"`,
    /// etc. Absent for many entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias_type: Option<String>,
    /// Begin date in MB's `YYYY[-MM[-DD]]` form, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub begin: Option<String>,
    /// End date in MB's `YYYY[-MM[-DD]]` form, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

/// Map an upstream `Option<Vec<Alias>>` onto our stable `AliasInfo` list.
/// Returns `None` when the input is `None` (caller didn't ask for aliases
/// OR the entity has none and the upstream payload elided the field).
pub fn map_aliases(aliases: Option<&Vec<Alias>>) -> Option<Vec<AliasInfo>> {
    aliases.map(|list| {
        list.iter()
            .map(|a| AliasInfo {
                name: a.name.clone(),
                sort_name: a.sort_name.clone(),
                primary: a.primary.unwrap_or(false),
                alias_type: a.alias_type.clone(),
                begin: a.begin.as_ref().map(|d| d.0.clone()),
                end: a.end.as_ref().map(|d| d.0.clone()),
            })
            .collect()
    })
}

// ============================================================================
// Tags (shared across artist / release / release-group)
// ============================================================================

/// Stable wire-format folksonomy tag summary.
///
/// MusicBrainz's per-entity tags are upvote-style: `count` is the number
/// of users who applied the tag. `score` only appears in search responses
/// (the engine's relevance ranking, 0-100) — both are surfaced so callers
/// can decide which to sort by.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagInfo {
    /// The tag string (community-supplied; case as MB stores it).
    pub name: String,
    /// Number of users who applied this tag. `None` when MB elided it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i32>,
}

/// Map an upstream `Option<Vec<Tag>>` onto our stable `TagInfo` list.
/// Sorts by descending count (then alphabetical) so the most-voted tag
/// surfaces first — keeps output deterministic across calls regardless
/// of MB's internal ordering.
pub fn map_tags(tags: Option<&Vec<Tag>>) -> Option<Vec<TagInfo>> {
    tags.map(|list| {
        let mut mapped: Vec<TagInfo> = list
            .iter()
            .map(|t| TagInfo {
                name: t.name.clone(),
                count: t.count,
            })
            .collect();
        mapped.sort_by(|a, b| {
            b.count
                .unwrap_or(0)
                .cmp(&a.count.unwrap_or(0))
                .then(a.name.cmp(&b.name))
        });
        mapped
    })
}

// ============================================================================
// Country-code validation (ISO 3166-1 alpha-2)
// ============================================================================

/// Validate an ISO 3166-1 alpha-2 country code: exactly two ASCII uppercase
/// letters. Lowercase input is uppercased before returning so the caller
/// gets a normalised value to pass to MB's `country:` filter.
pub fn validate_country_code(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.len() != 2 || !trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(format!(
            "Invalid country code '{}': expected ISO 3166-1 alpha-2 (exactly 2 ASCII letters, e.g. 'US', 'GB', 'JP')",
            raw
        ));
    }
    Ok(trimmed.to_ascii_uppercase())
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
    fn resolve_search_query_picks_typed_when_only_typed_set() {
        assert_eq!(
            resolve_search_query("Radiohead", None).unwrap(),
            "Radiohead"
        );
        // Empty raw is treated as absent.
        assert_eq!(
            resolve_search_query("Radiohead", Some("")).unwrap(),
            "Radiohead"
        );
        assert_eq!(
            resolve_search_query("Radiohead", Some("   ")).unwrap(),
            "Radiohead"
        );
    }

    #[test]
    fn resolve_search_query_picks_raw_when_only_raw_set() {
        assert_eq!(
            resolve_search_query("", Some("artist:radiohead AND date:[1995 TO 2000]")).unwrap(),
            "artist:radiohead AND date:[1995 TO 2000]"
        );
        // Whitespace-only typed is treated as absent.
        assert_eq!(resolve_search_query("   ", Some("foo")).unwrap(), "foo");
    }

    #[test]
    fn resolve_search_query_refuses_both() {
        let err = resolve_search_query("Radiohead", Some("artist:radiohead")).unwrap_err();
        assert!(err.contains("exactly one"), "got: {}", err);
    }

    #[test]
    fn resolve_search_query_refuses_neither() {
        let err = resolve_search_query("", None).unwrap_err();
        assert!(err.contains("Missing query"), "got: {}", err);
        let err = resolve_search_query("  ", Some("")).unwrap_err();
        assert!(err.contains("Missing query"), "got: {}", err);
    }

    #[test]
    fn map_tags_sorts_by_count_then_alpha() {
        let upstream = vec![
            Tag {
                name: "rock".to_string(),
                count: Some(50),
                score: None,
            },
            Tag {
                name: "alternative".to_string(),
                count: Some(50),
                score: None,
            },
            Tag {
                name: "experimental".to_string(),
                count: Some(20),
                score: None,
            },
            Tag {
                // Missing count must not panic; treated as 0 for ordering.
                name: "obscure".to_string(),
                count: None,
                score: None,
            },
        ];
        let mapped = map_tags(Some(&upstream)).unwrap();
        // Tie on count=50 → alphabetical: "alternative" before "rock".
        assert_eq!(mapped[0].name, "alternative");
        assert_eq!(mapped[1].name, "rock");
        assert_eq!(mapped[2].name, "experimental");
        // None count sorts last.
        assert_eq!(mapped[3].name, "obscure");
    }

    #[test]
    fn map_tags_handles_none_and_empty() {
        assert!(map_tags(None).is_none());
        let empty: Vec<Tag> = Vec::new();
        let mapped = map_tags(Some(&empty)).unwrap();
        assert!(mapped.is_empty());
    }

    #[test]
    fn validate_country_code_accepts_well_formed() {
        assert_eq!(validate_country_code("US").unwrap(), "US");
        // Lowercase is uppercased.
        assert_eq!(validate_country_code("gb").unwrap(), "GB");
        // Surrounding whitespace is trimmed.
        assert_eq!(validate_country_code("  jp  ").unwrap(), "JP");
    }

    #[test]
    fn validate_country_code_rejects_malformed() {
        assert!(validate_country_code("").is_err());
        assert!(validate_country_code("U").is_err()); // too short
        assert!(validate_country_code("USA").is_err()); // too long
        assert!(validate_country_code("U1").is_err()); // digits
        assert!(validate_country_code("éé").is_err()); // non-ASCII
    }

    #[test]
    fn map_aliases_handles_none_and_empty() {
        assert!(map_aliases(None).is_none());
        let empty: Vec<Alias> = Vec::new();
        let mapped = map_aliases(Some(&empty)).unwrap();
        assert!(mapped.is_empty());
    }

    #[test]
    fn map_aliases_normalises_fields() {
        use musicbrainz_rs::entity::date_string::DateString;
        let upstream = vec![
            Alias {
                name: "The Beatles".to_string(),
                sort_name: "Beatles, The".to_string(),
                primary: Some(true),
                alias_type: Some("Artist name".to_string()),
                begin: Some(DateString("1960".to_string())),
                end: None,
                ..Default::default()
            },
            Alias {
                // Absent `primary` must surface as `false`, not panic.
                name: "Beatles".to_string(),
                sort_name: "Beatles".to_string(),
                primary: None,
                alias_type: None,
                begin: None,
                end: None,
                ..Default::default()
            },
        ];
        let mapped = map_aliases(Some(&upstream)).unwrap();
        assert_eq!(mapped.len(), 2);

        assert_eq!(mapped[0].name, "The Beatles");
        assert_eq!(mapped[0].sort_name, "Beatles, The");
        assert!(mapped[0].primary);
        assert_eq!(mapped[0].alias_type.as_deref(), Some("Artist name"));
        assert_eq!(mapped[0].begin.as_deref(), Some("1960"));
        assert!(mapped[0].end.is_none());

        assert!(!mapped[1].primary);
        assert!(mapped[1].alias_type.is_none());
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
