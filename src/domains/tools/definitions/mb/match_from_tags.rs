//! Tag-based MusicBrainz identification.
//!
//! Fallback for `mb_identify_record` when `fpcalc` / AcoustID aren't available
//! (first-time setup, restricted environments, or simply when the existing
//! tags are already informative enough that an MB query resolves the track
//! deterministically).
//!
//! Driven by `(title, artist, duration_seconds, album)` rather than acoustic
//! fingerprints. Returns the same `RecordingMatch` shape `mb_identify_record`
//! uses, wrapped in a thin `TagMatch { confidence, score_breakdown, recording
//! }` envelope — so an agent can swap one tool for the other without
//! re-learning the response shape.
//!
//! ### Scoring
//!
//! Each candidate gets three component scores in `[0.0, 1.0]`:
//!
//! - **title** — normalised string-match against the candidate's recording
//!   title (case-insensitive, punctuation-stripped, whitespace-collapsed).
//!   Exact = 1.0, prefix = 0.85, substring = 0.7, else 0.0.
//! - **artist** — same string matcher against the candidate's combined artist
//!   credit. When the caller omits `artist`, this component is 0.5 (neutral,
//!   so the confidence floor still gates noise).
//! - **duration** — `|query_sec - candidate_sec|`: ≤2s = 1.0, ≤5s = 0.85,
//!   ≤10s = 0.6, ≤30s = 0.3, else 0.0. Missing query duration is 0.5; missing
//!   candidate duration is 0.3 (small penalty — MB usually has this).
//!
//! The combined confidence is `0.5·title + 0.3·artist + 0.2·duration`, so a
//! match driven purely by an exact title with no other info caps at 0.75
//! (= 0.5 + 0.15 + 0.10), comfortably above the default 0.6 floor. Add a
//! correct artist and the score reaches 0.9; add a ±2s duration too and it
//! hits 1.0.
//!
//! ### Album
//!
//! `album` is passed to MusicBrainz as a `release:` filter to narrow the
//! candidate set, but is deliberately **not** part of the score: a search
//! response doesn't include the candidate's full release list (would need a
//! second per-candidate fetch), and scoring on a sometimes-missing field
//! produces uneven confidence. The query-side filter is enough to bias
//! results.

use musicbrainz_rs::{
    Search,
    entity::recording::{Recording, RecordingSearchQuery},
};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{error, info, instrument};

use crate::domains::tools::definitions::mb::MbBlockingTool;
use crate::domains::tools::definitions::mb::common::{
    error_result, get_artist_name, structured_result,
};
use crate::domains::tools::definitions::mb::identify_record::RecordingMatch;

/// Default cap on returned matches.
const DEFAULT_LIMIT: usize = 5;
/// Upper bound on returned matches (the MB search itself already pages — 25
/// is plenty for a tag-driven query that's usually expected to land within
/// the top 3).
const MAX_LIMIT: usize = 25;
/// Below this confidence the agent should treat the candidate as
/// inconclusive; the default exists so callers don't have to pick a number.
const DEFAULT_CONFIDENCE_FLOOR: f64 = 0.6;

/// Score-component weights (sum to 1.0).
const W_TITLE: f64 = 0.5;
const W_ARTIST: f64 = 0.3;
const W_DURATION: f64 = 0.2;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for `mb_match_from_tags`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MbMatchFromTagsParams {
    /// Track title. The only required field — without it the query has no
    /// anchor.
    pub title: String,

    /// Credited artist (combined credit, e.g. "Daft Punk feat. Pharrell").
    /// Optional but materially improves the score breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,

    /// Recording duration in **seconds**. Optional. When present, candidates
    /// within ±2s score 1.0 on the duration component; ±10s scores 0.6;
    /// anything further is filtered down hard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u32>,

    /// Album hint passed to MB as a `release:` filter. Narrows the candidate
    /// set; does not contribute to the confidence score (see module docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,

    /// Maximum number of matches to return. Default 5, hard-capped at 25.
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Discard candidates whose final confidence falls below this floor.
    /// Default 0.6 — below that, the caller should fall back to fingerprinting.
    #[serde(default = "default_confidence_floor")]
    pub confidence_floor: f64,
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}
fn default_confidence_floor() -> f64 {
    DEFAULT_CONFIDENCE_FLOOR
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of a tag-driven match.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagMatchResult {
    /// Echoes back the query so the agent can correlate with its own state.
    pub query: TagQueryEcho,
    /// Matches whose confidence cleared `confidence_floor`, sorted by
    /// confidence descending.
    pub matches: Vec<TagMatch>,
    /// Total candidates MusicBrainz returned, before applying the floor and
    /// the `limit` cap. Useful when the agent wants to know "did MB find
    /// *anything*?" separately from "did anything pass the bar?".
    pub total_candidates: usize,
    /// Number of entries in `matches` (always `<= limit`).
    pub returned: usize,
    /// The floor used for this call (echoed so the agent doesn't have to
    /// remember which default was in play).
    pub confidence_floor: f64,
}

/// Echo of the query parameters, for correlation in the response.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagQueryEcho {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
}

/// One scored match. The `recording` field mirrors `mb_identify_record`'s
/// `RecordingMatch`, so downstream code that already handles fingerprint
/// matches can consume tag matches with no extra wiring.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagMatch {
    /// Combined score in `[0.0, 1.0]`. See `score_breakdown` for the per-
    /// component values that produced it.
    pub confidence: f64,
    /// Per-component scores (title / artist / duration), so a low overall
    /// confidence can be diagnosed without re-running the call.
    pub score_breakdown: TagMatchScores,
    /// The candidate recording, in the same shape `mb_identify_record` uses.
    pub recording: RecordingMatch,
}

/// Per-component scores.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagMatchScores {
    pub title: f64,
    pub artist: f64,
    pub duration: f64,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Tag-based MusicBrainz recording matcher.
pub struct MbMatchFromTagsTool;

impl MbBlockingTool for MbMatchFromTagsTool {
    type Params = MbMatchFromTagsParams;

    const NAME: &'static str = "mb_match_from_tags";

    const DESCRIPTION: &'static str = "Identify a MusicBrainz recording from existing tag values (title + optional artist, \
         duration, album) rather than an acoustic fingerprint. Use as a fallback when fpcalc/AcoustID \
         aren't available, or when the local tags are already trustworthy enough to resolve the \
         track without audio decode. Returns the same RecordingMatch shape as mb_identify_record, \
         wrapped with a confidence score in [0.0, 1.0] and a per-component breakdown. Candidates \
         below `confidence_floor` (default 0.6) are dropped — at that point the agent should switch \
         to fingerprinting.";

    #[instrument(skip_all, fields(title = %params.title, has_artist = params.artist.is_some(), has_duration = params.duration_seconds.is_some()))]
    fn execute(params: &MbMatchFromTagsParams) -> CallToolResult {
        if params.title.trim().is_empty() {
            return error_result("Missing required 'title' (cannot run a tag match with no title)");
        }

        let limit = params.limit.clamp(1, MAX_LIMIT);
        // Clamp once and use the clamped value going forward; an out-of-range
        // floor would silently let through every candidate (or none).
        let floor = params.confidence_floor.clamp(0.0, 1.0);

        info!(
            "mb_match_from_tags: title='{}' artist={:?} dur={:?}s album={:?} (limit={}, floor={:.2})",
            params.title, params.artist, params.duration_seconds, params.album, limit, floor
        );

        // 1. Build the MB query. We feed `recording`, `artist`, `release` as
        //    Lucene-style hints; duration goes into scoring, not into the
        //    query, because MB's `dur:` filter expects milliseconds with no
        //    fuzziness and a tight match would defeat the whole point of
        //    scoring with tolerance.
        let mut builder = RecordingSearchQuery::query_builder();
        builder.recording(&params.title);
        if let Some(a) = params.artist.as_deref().filter(|s| !s.trim().is_empty()) {
            builder.artist(a);
        }
        if let Some(r) = params.album.as_deref().filter(|s| !s.trim().is_empty()) {
            builder.release(r);
        }
        let search_query = builder.build();

        // 2. Execute. Search results don't include releases by default; we
        //    map only what `RecordingMatch` needs (id, title, duration,
        //    artists) — the release_groups field stays `None`.
        let recordings = match Recording::search(search_query).execute() {
            Ok(result) => result.entities,
            Err(e) => {
                error!("MusicBrainz search failed: {:?}", e);
                return error_result(&format!("MusicBrainz search failed: {}", e));
            }
        };

        let total_candidates = recordings.len();

        // 3. Score every candidate.
        let mut scored: Vec<TagMatch> = recordings
            .into_iter()
            .map(|r| {
                let candidate_artist = get_artist_name(&r.artist_credit);
                let breakdown = TagMatchScores {
                    title: string_match_score(&params.title, &r.title),
                    artist: match params.artist.as_deref() {
                        None => 0.5,
                        Some(q) => string_match_score(q, &candidate_artist),
                    },
                    duration: duration_score(params.duration_seconds, r.length),
                };
                let confidence = W_TITLE * breakdown.title
                    + W_ARTIST * breakdown.artist
                    + W_DURATION * breakdown.duration;

                // Pull every credited artist's display name (the MB search
                // response lists them all when there's a multi-credit join).
                let artists: Option<Vec<String>> = r
                    .artist_credit
                    .as_ref()
                    .map(|acs| acs.iter().map(|ac| ac.name.clone()).collect::<Vec<_>>());

                let recording = RecordingMatch {
                    id: r.id.clone(),
                    title: Some(r.title.clone()),
                    duration: r.length.map(|ms| ms / 1000),
                    artists,
                    release_groups: None,
                };

                TagMatch {
                    confidence,
                    score_breakdown: breakdown,
                    recording,
                }
            })
            .collect();

        // 4. Filter by floor, sort by confidence desc, truncate to limit.
        scored.retain(|m| m.confidence >= floor);
        scored.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);

        let returned = scored.len();
        let summary = if returned == 0 {
            format!(
                "No tag matches above confidence {:.2} for '{}' ({} candidate(s) considered)",
                floor, params.title, total_candidates
            )
        } else {
            let top = &scored[0];
            format!(
                "Top match: '{}' (confidence {:.2}, {} of {} candidates above floor)",
                top.recording
                    .title
                    .as_deref()
                    .unwrap_or(top.recording.id.as_str()),
                top.confidence,
                returned,
                total_candidates
            )
        };

        let payload = TagMatchResult {
            query: TagQueryEcho {
                title: params.title.clone(),
                artist: params.artist.clone(),
                duration_seconds: params.duration_seconds,
                album: params.album.clone(),
            },
            matches: scored,
            total_candidates,
            returned,
            confidence_floor: floor,
        };

        structured_result(summary, payload)
    }
}

// ============================================================================
// Scoring helpers (pure, unit-tested below)
// ============================================================================

/// Normalise a string for cross-source comparison: lowercase, treat
/// non-alphanumerics as word separators (so "AC/DC" splits the same way as
/// "AC DC"), collapse the resulting whitespace runs.
///
/// Apostrophes are elided rather than separated, so "Don't" → "dont" (one
/// token, what a user expects) instead of "don t" (two tokens, which breaks
/// substring matching against tag values stored without the apostrophe).
fn normalize_for_match(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        } else if matches!(ch, '\'' | '\u{2019}' | '\u{02BC}') {
            // Apostrophes / typographic right-single-quote / modifier
            // apostrophe: elide. Anything else falls through to the
            // separator case below.
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Score a query string against a candidate string. Returns a value in
/// `[0.0, 1.0]`. Both inputs are normalised first (see [`normalize_for_match`]);
/// an empty normalised form on either side scores 0.0.
fn string_match_score(query: &str, candidate: &str) -> f64 {
    let q = normalize_for_match(query);
    let c = normalize_for_match(candidate);
    if q.is_empty() || c.is_empty() {
        return 0.0;
    }
    if q == c {
        return 1.0;
    }
    // Either side starts with the other → "prefix": handles "Hells Bells" vs
    // "Hells Bells (live)" or vice versa.
    if c.starts_with(&q) || q.starts_with(&c) {
        return 0.85;
    }
    if c.contains(&q) || q.contains(&c) {
        return 0.7;
    }
    0.0
}

/// Score a `(query_seconds, candidate_milliseconds)` pair. Both optional.
fn duration_score(query_sec: Option<u32>, candidate_ms: Option<u32>) -> f64 {
    match (query_sec, candidate_ms) {
        // No query side: neutral score (we don't penalise the candidate for
        // an absence the caller introduced).
        (None, _) => 0.5,
        // We have a query but the candidate is missing duration. Small
        // penalty: MB usually has this, but releases without media info do
        // exist (live recordings without timings, etc.).
        (Some(_), None) => 0.3,
        (Some(q), Some(cms)) => {
            let c = cms / 1000;
            let diff = q.abs_diff(c);
            if diff <= 2 {
                1.0
            } else if diff <= 5 {
                0.85
            } else if diff <= 10 {
                0.6
            } else if diff <= 30 {
                0.3
            } else {
                0.0
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected ~{}, got {}", b, a);
    }

    #[test]
    fn normalize_lowercases_and_strips_punctuation() {
        assert_eq!(normalize_for_match("Hells Bells"), "hells bells");
        assert_eq!(normalize_for_match("AC/DC"), "ac dc");
        assert_eq!(
            normalize_for_match("  Multiple   Spaces  "),
            "multiple spaces"
        );
        assert_eq!(normalize_for_match("Don't Stop Me Now"), "dont stop me now");
        assert_eq!(normalize_for_match("Beyoncé"), "beyoncé");
    }

    #[test]
    fn string_score_exact_match() {
        assert_close(string_match_score("Hells Bells", "Hells Bells"), 1.0);
        // Case + punctuation differences still count as exact after normalisation.
        assert_close(string_match_score("AC/DC", "ac dc"), 1.0);
    }

    #[test]
    fn string_score_prefix_and_substring() {
        // Candidate has extra suffix ("(Live)") — counts as prefix match.
        assert_close(
            string_match_score("Hells Bells", "Hells Bells (Live)"),
            0.85,
        );
        // Query is a strict substring inside a longer candidate.
        assert_close(
            string_match_score("Bells", "I Heard the Bells on Christmas Day"),
            0.7,
        );
    }

    #[test]
    fn string_score_no_match() {
        assert_close(string_match_score("Hells Bells", "Bohemian Rhapsody"), 0.0);
        // Empty inputs always score 0.0.
        assert_close(string_match_score("", "anything"), 0.0);
        assert_close(string_match_score("anything", ""), 0.0);
    }

    #[test]
    fn duration_score_within_two_seconds() {
        assert_close(duration_score(Some(312), Some(312_000)), 1.0);
        assert_close(duration_score(Some(312), Some(314_000)), 1.0);
        assert_close(duration_score(Some(312), Some(310_000)), 1.0);
    }

    #[test]
    fn duration_score_buckets() {
        // ±5s
        assert_close(duration_score(Some(300), Some(305_000)), 0.85);
        // ±10s
        assert_close(duration_score(Some(300), Some(310_000)), 0.6);
        // ±30s
        assert_close(duration_score(Some(300), Some(330_000)), 0.3);
        // Beyond → 0.0
        assert_close(duration_score(Some(300), Some(360_000)), 0.0);
    }

    #[test]
    fn duration_score_handles_missing_sides() {
        // No query duration → neutral 0.5 regardless of candidate.
        assert_close(duration_score(None, Some(300_000)), 0.5);
        assert_close(duration_score(None, None), 0.5);
        // Query but no candidate → small penalty 0.3.
        assert_close(duration_score(Some(300), None), 0.3);
    }

    #[test]
    fn confidence_for_perfect_match() {
        // What the integration test will look for: exact title + artist +
        // ±2s duration → 1.0.
        let confidence = W_TITLE * string_match_score("Hells Bells", "Hells Bells")
            + W_ARTIST * string_match_score("AC/DC", "AC/DC")
            + W_DURATION * duration_score(Some(312), Some(312_000));
        assert_close(confidence, 1.0);
    }

    #[test]
    fn confidence_for_title_only_query() {
        // Exact title, no artist or duration provided → baseline of 0.75
        // (well above the default 0.6 floor — title alone is enough).
        let confidence = W_TITLE * string_match_score("Hells Bells", "Hells Bells")
            + W_ARTIST * 0.5     // missing artist query: neutral
            + W_DURATION * 0.5; // missing duration query: neutral
        assert_close(confidence, 0.75);
    }

    #[test]
    fn prefix_title_alone_clears_default_floor() {
        // "Hells Bells" vs "Hells Bells (live)" with everything else absent:
        // 0.5·0.85 + 0.3·0.5 + 0.2·0.5 = 0.425 + 0.15 + 0.10 = 0.675.
        let confidence = W_TITLE * string_match_score("Hells Bells", "Hells Bells (live)")
            + W_ARTIST * 0.5
            + W_DURATION * 0.5;
        assert!(confidence > DEFAULT_CONFIDENCE_FLOOR);
        assert_close(confidence, 0.675);
    }

    #[test]
    fn limit_clamping_respects_max() {
        // Sanity check via direct constants — the code path that uses them
        // is the same in `execute`.
        assert_eq!(0usize.clamp(1, MAX_LIMIT), 1);
        assert_eq!(100usize.clamp(1, MAX_LIMIT), MAX_LIMIT);
        assert_eq!(5usize.clamp(1, MAX_LIMIT), 5);
    }

    #[test]
    fn params_defaults_parse_correctly() {
        let v = serde_json::json!({ "title": "Hells Bells" });
        let p: MbMatchFromTagsParams = serde_json::from_value(v).unwrap();
        assert_eq!(p.title, "Hells Bells");
        assert!(p.artist.is_none());
        assert!(p.duration_seconds.is_none());
        assert!(p.album.is_none());
        assert_eq!(p.limit, DEFAULT_LIMIT);
        assert!((p.confidence_floor - DEFAULT_CONFIDENCE_FLOOR).abs() < 1e-9);
    }

    // ------------------------------------------------------------------------
    // Network-bound integration test (ignored by default).
    // Run with:
    //   cargo test --features all -- --ignored --test-threads=1 tag_match
    // ------------------------------------------------------------------------

    #[ignore]
    #[test]
    fn live_query_returns_correct_mbid_with_high_confidence() {
        // AC/DC — Hells Bells (from Back in Black, 1980). Track length 5:12.
        let params = MbMatchFromTagsParams {
            title: "Hells Bells".to_string(),
            artist: Some("AC/DC".to_string()),
            duration_seconds: Some(312),
            album: Some("Back in Black".to_string()),
            limit: 5,
            confidence_floor: 0.6,
        };
        let result = MbMatchFromTagsTool::execute(&params);
        assert!(
            !result.is_error.unwrap_or(true),
            "expected success, got error: {:?}",
            result.content
        );

        let structured = result.structured_content.expect("structured content");
        let matches = structured["matches"].as_array().expect("matches array");
        assert!(!matches.is_empty(), "expected at least one match");
        let top = &matches[0];
        let confidence = top["confidence"].as_f64().expect("confidence f64");
        assert!(
            confidence > 0.85,
            "expected top confidence > 0.85, got {}",
            confidence
        );
        let title = top["recording"]["title"].as_str().unwrap_or("");
        assert_eq!(title.to_lowercase(), "hells bells");
    }
}
