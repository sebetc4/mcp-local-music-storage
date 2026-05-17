//! MusicBrainz Recording search tool.
//!
//! This tool provides functionality to search for recordings (tracks/songs)
//! and find which releases contain a specific recording.

use futures::FutureExt;
use musicbrainz_rs::{
    Fetch, Search,
    entity::recording::{Recording, RecordingSearchQuery},
};
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use super::common::{
    default_limit, error_result, extract_year, format_duration, get_artist_name, is_mbid,
    structured_result, validate_limit,
};

/// Parameters for recording search operations.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MbRecordingParams {
    /// The type of search to perform.
    /// - "recording": Search for recordings by title
    /// - "recording_releases": Find all releases containing a specific recording
    #[schemars(description = "Search type: 'recording' or 'recording_releases'")]
    pub search_type: String,

    /// The search query string or MusicBrainz ID.
    #[schemars(description = r#"
        Search query (recording title or MBID)
        CRITICAL RULES FOR SEARCH BY TITLE:
        - The query MUST contain ONLY the exact recording/track title, nothing else.
        - DO NOT include artist names, album names, years, formats, or any additional text.
        - DO NOT add contextual information that you think might help - it will break the search.
        - Examples of CORRECT usage:
          * "Imagine" (✔)
          * "Smells Like Teen Spirit" (✔)
          * "Bohemian Rhapsody" (✔)
          * "3a909079-a42a-4642-b06f-398bf91f34f4" (recording MBID) (✔)
        - Examples of INCORRECT usage:
          * "Imagine John Lennon" (✘ - contains artist name)
          * "Imagine 1971" (✘ - contains year)
          * "Smells Like Teen Spirit by Nirvana" (✘ - contains artist)
          * "Bohemian Rhapsody from A Night at the Opera" (✘ - contains album)
    "#)]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Structured output for recording search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingSearchResult {
    pub recordings: Vec<RecordingSearchInfo>,
    pub total_count: usize,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingSearchInfo {
    pub title: String,
    pub mbid: String,
    pub artist: String,
    pub duration: Option<String>,
    pub disambiguation: Option<String>,
}

/// Structured output for single recording details (by MBID).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingDetails {
    pub title: String,
    pub mbid: String,
    pub artist: String,
    pub duration: Option<String>,
    pub disambiguation: Option<String>,
    pub artist_mbids: Vec<ArtistMbid>,
    pub releases: Vec<RecordingReleaseInfo>,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ArtistMbid {
    pub name: String,
    pub mbid: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingReleaseInfo {
    pub title: String,
    pub mbid: String,
    pub country: Option<String>,
    pub year: Option<String>,
}

/// Structured output for recording releases search.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingReleasesResult {
    pub recording_title: String,
    pub recording_mbid: String,
    pub recording_artist: String,
    pub duration: Option<String>,
    pub releases: Vec<ReleaseWithArtist>,
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseWithArtist {
    pub title: String,
    pub mbid: String,
    pub artist: String,
    pub date: Option<String>,
    pub country: Option<String>,
}

/// MusicBrainz Recording Search Tool implementation.
#[derive(Debug, Clone)]
pub struct MbRecordingTool;

impl MbRecordingTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "mb_recording_search";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str =
        "Search for recordings (tracks/songs) in MusicBrainz and find which releases contain them. CRITICAL: The 'query' parameter must contain ONLY the track title (e.g., 'Imagine'), never include artist names, album names, or years - this will cause search failures. Returns structured data with MBIDs, artists, durations, and release information.";

    pub fn new() -> Self {
        Self
    }

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    pub fn execute(params: &MbRecordingParams) -> CallToolResult {
        let search_type = params.search_type.clone();
        let query = params.query.clone();
        let limit = validate_limit(params.limit);

        match search_type.as_str() {
            "recording" => Self::search_recordings(&query, limit),
            "recording_releases" => Self::search_recording_releases(&query, limit),
            _ => error_result(&format!(
                "Unknown search type: {}. Use 'recording' or 'recording_releases'",
                search_type
            )),
        }
    }

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(arguments: serde_json::Value) -> Result<serde_json::Value, String> {
        let search_type = arguments
            .get("search_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'search_type' parameter".to_string())?
            .to_string();

        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'query' parameter".to_string())?
            .to_string();

        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        let params = MbRecordingParams {
            search_type,
            query,
            limit,
        };

        // Use std::thread::spawn to avoid nested runtime panic.
        // musicbrainz_rs uses reqwest::blocking which creates its own runtime.
        let handle = std::thread::spawn(move || Self::execute(&params));

        let result = handle
            .join()
            .map_err(|_| "Thread panicked during recording search".to_string())?;

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<MbRecordingParams>(),
            annotations: None,
            output_schema: None,
            icons: None,
            meta: None,
            title: None,
        }
    }

    /// Create a ToolRoute for STDIO/TCP transport.
    pub fn create_route<S>() -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            async move {
                let params: MbRecordingParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

                // Use std::thread::spawn to avoid nested runtime panic.
                // musicbrainz_rs uses reqwest::blocking which creates its own runtime,
                // so we need a completely separate OS thread.
                let handle = std::thread::spawn(move || Self::execute(&params));

                let result = handle
                    .join()
                    .map_err(|_| McpError::internal_error("Thread panicked".to_string(), None))?;

                Ok(result)
            }
            .boxed()
        })
    }

    /// Search for recordings by title or MBID.
    pub fn search_recordings(query: &str, limit: usize) -> CallToolResult {
        info!("Searching for recordings matching: {}", query);

        // If the query is a MusicBrainz ID (MBID), fetch the recording directly.
        if is_mbid(query) {
            Self::fetch_recording_by_id(query)
        } else {
            Self::search_recordings_by_title(query, limit)
        }
    }

    /// Fetch a recording by its MBID with full details.
    fn fetch_recording_by_id(mbid: &str) -> CallToolResult {
        match Recording::fetch()
            .id(mbid)
            .with_artists()
            .with_releases()
            .with_genres()
            .execute()
        {
            Ok(recording) => {
                let artist = get_artist_name(&recording.artist_credit);
                let duration = recording.length.map(|l| format_duration(l as u64));

                // Build artist MBIDs
                let artist_mbids: Vec<ArtistMbid> = recording
                    .artist_credit
                    .as_ref()
                    .map(|artists| {
                        artists
                            .iter()
                            .map(|a| ArtistMbid {
                                name: a.name.clone(),
                                mbid: a.artist.id.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Build releases info
                let releases: Vec<RecordingReleaseInfo> = recording
                    .releases
                    .as_ref()
                    .map(|rels| {
                        rels.iter()
                            .map(|r| RecordingReleaseInfo {
                                title: r.title.clone(),
                                mbid: r.id.clone(),
                                country: r.country.clone(),
                                year: r.date.as_ref().and_then(|d| extract_year(&d.0)),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Build genres list
                let genres: Vec<String> = recording
                    .genres
                    .as_ref()
                    .map(|gs| gs.iter().map(|g| g.name.clone()).collect())
                    .unwrap_or_default();

                let structured_data = RecordingDetails {
                    title: recording.title.clone(),
                    mbid: recording.id,
                    artist: artist.clone(),
                    duration: duration.clone(),
                    disambiguation: recording
                        .disambiguation
                        .filter(|d| !d.is_empty()),
                    artist_mbids,
                    releases: releases.clone(),
                    genres: genres.clone(),
                };

                // Build summary
                let summary = if releases.is_empty() {
                    format!("'{}' by {} ({})", recording.title, artist, duration.unwrap_or_else(|| "unknown duration".to_string()))
                } else {
                    format!(
                        "'{}' by {} ({}) - found on {} release(s)",
                        recording.title,
                        artist,
                        duration.unwrap_or_else(|| "unknown duration".to_string()),
                        releases.len()
                    )
                };

                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Failed to fetch recording by MBID: {:?}", e);
                error_result(&format!("Failed to fetch recording: {}", e))
            }
        }
    }

    /// Search for recordings by title.
    fn search_recordings_by_title(query: &str, limit: usize) -> CallToolResult {
        let search_query = RecordingSearchQuery::query_builder()
            .recording(query)
            .build();

        let search_result = Recording::search(search_query).execute();

        match search_result {
            Ok(result) => {
                let recordings: Vec<_> = result.entities.into_iter().take(limit).collect();
                if recordings.is_empty() {
                    return error_result(&format!("No recordings found for query: {}", query));
                }

                let count = recordings.len();
                let recording_infos: Vec<RecordingSearchInfo> = recordings
                    .into_iter()
                    .map(|r| RecordingSearchInfo {
                        title: r.title,
                        mbid: r.id,
                        artist: get_artist_name(&r.artist_credit),
                        duration: r.length.map(|l| format_duration(l as u64)),
                        disambiguation: r.disambiguation.filter(|d| !d.is_empty()),
                    })
                    .collect();

                let structured_data = RecordingSearchResult {
                    recordings: recording_infos,
                    total_count: count,
                    query: query.to_string(),
                };

                let summary = format!("Found {} recording(s) matching '{}'", count, query);
                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Recording search failed: {:?}", e);
                error_result(&format!("Recording search failed: {}", e))
            }
        }
    }

    /// Find all releases containing a specific recording.
    pub fn search_recording_releases(query: &str, limit: usize) -> CallToolResult {
        info!("Finding releases containing recording: {}", query);

        // Get the recording MBID
        let recording_id = if is_mbid(query) {
            query.to_string()
        } else {
            // Search for recording first
            let search_query = RecordingSearchQuery::query_builder()
                .recording(query)
                .build();
            match Recording::search(search_query).execute() {
                Ok(result) => {
                    if let Some(recording) = result.entities.first() {
                        debug!("Found recording: {} ({})", recording.title, recording.id);
                        recording.id.clone()
                    } else {
                        return error_result(&format!("No recording found matching: {}", query));
                    }
                }
                Err(e) => {
                    error!("Recording lookup failed: {:?}", e);
                    return error_result(&format!("Recording lookup failed: {}", e));
                }
            }
        };

        // Fetch recording with releases and artists
        match Recording::fetch()
            .id(&recording_id)
            .with_releases()
            .with_artists()
            .execute()
        {
            Ok(recording) => {
                let artist = get_artist_name(&recording.artist_credit);
                let duration = recording.length.map(|l| format_duration(l as u64));

                let releases: Vec<ReleaseWithArtist> = recording
                    .releases
                    .as_ref()
                    .map(|rels| {
                        rels.iter()
                            .take(limit)
                            .map(|r| ReleaseWithArtist {
                                title: r.title.clone(),
                                mbid: r.id.clone(),
                                artist: get_artist_name(&r.artist_credit),
                                date: r.date.as_ref().and_then(|d| extract_year(&d.0)),
                                country: r.country.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let count = releases.len();

                let structured_data = RecordingReleasesResult {
                    recording_title: recording.title.clone(),
                    recording_mbid: recording.id,
                    recording_artist: artist.clone(),
                    duration: duration.clone(),
                    releases,
                    total_count: count,
                };

                let summary = if count == 0 {
                    format!("'{}' by {} - no releases found", recording.title, artist)
                } else {
                    format!(
                        "'{}' by {} - found on {} release(s)",
                        recording.title, artist, count
                    )
                };

                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Failed to fetch recording releases: {:?}", e);
                error_result(&format!("Failed to fetch recording releases: {}", e))
            }
        }
    }
}

impl Default for MbRecordingTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::RawContent;

    #[test]
    fn test_recording_params_default_limit() {
        let json = r#"{"search_type": "recording", "query": "Smells Like Teen Spirit"}"#;
        let params: MbRecordingParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 10);
    }

    // Integration tests (require network, run with: cargo test -- --ignored)
    #[ignore]
    #[test]
    fn test_search_recordings() {
        let result = MbRecordingTool::search_recordings("Paranoid Android", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Paranoid Android"),
                "Expected 'Paranoid Android' in result"
            );
        }
    }

    #[ignore]
    #[test]
    fn test_search_recordings_by_id() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // Specific recording MBID
        let result = MbRecordingTool::search_recordings("3a909079-a42a-4642-b06f-398bf91f34f4", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("3a909079-a42a-4642-b06f-398bf91f34f4") || text.text.len() > 0,
                "Expected non-empty result for MBID"
            );
        }
    }

    #[ignore]
    #[test]
    fn test_search_recording_releases_() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // Paranoid Android recording MBID
        // Also test searching releases by recording name
        let result = MbRecordingTool::search_recording_releases("Paranoid Android", 10);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
    }

    #[ignore]
    #[test]
    fn test_search_recording_releases_by_id() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // Paranoid Android recording MBID
        let result =
            MbRecordingTool::search_recording_releases("8b8a07f6-53a6-4025-acb7-d30c7e29fce6", 10);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
    }
}
