//! MusicBrainz Artist search tool.
//!
//! This tool provides functionality to search for artists and their releases
//! using the MusicBrainz database.

use musicbrainz_rs::{
    Fetch, Search,
    entity::artist::{Artist, ArtistSearchQuery},
    entity::release::{Release, ReleaseSearchQuery},
};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, instrument};

use super::MbBlockingTool;
use super::common::{
    default_limit, error_result, extract_year, is_mbid, structured_result, validate_limit,
};

/// Parameters for artist search operations.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MbArtistParams {
    /// The type of search to perform.
    /// - "artist": Search for artists by name
    /// - "artist_releases": Search for releases by a specific artist
    #[schemars(description = "Search type: 'artist' or 'artist_releases'")]
    pub search_type: String,

    /// The search query string or MusicBrainz ID.
    #[schemars(description = r#"
        Search query (artist name or MBID)
        IMPORTANT RULES:
        - For artist search: Use ONLY the artist name, nothing else.
        - For artist_releases search: Use ONLY the artist name or artist MBID.
        - DO NOT add release names, track titles, years, genres, or any other information.
        - Examples of CORRECT usage:
          * "Radiohead" (✔)
          * "The Beatles" (✔)
          * "a74b1b7f-71a5-4011-9441-d0b5e4122711" (artist MBID) (✔)
        - Examples of INCORRECT usage:
          * "Radiohead OK Computer" (✘ - contains album name)
          * "The Beatles 1960s" (✘ - contains period)
          * "Nirvana Smells Like Teen Spirit" (✘ - contains track name)
    "#)]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Structured output for artist search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ArtistSearchResult {
    pub artists: Vec<ArtistSearchInfo>,
    pub total_count: usize,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ArtistSearchInfo {
    pub name: String,
    pub mbid: String,
    pub country: Option<String>,
    pub area: Option<String>,
    pub disambiguation: Option<String>,
}

/// Structured output for artist releases search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ArtistReleasesResult {
    pub artist_name: String,
    pub artist_mbid: String,
    pub releases: Vec<ArtistReleaseInfo>,
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ArtistReleaseInfo {
    pub title: String,
    pub mbid: String,
    pub year: Option<String>,
    pub country: Option<String>,
}

/// MusicBrainz Artist Search Tool.
pub struct MbArtistTool;

impl MbBlockingTool for MbArtistTool {
    type Params = MbArtistParams;

    const NAME: &'static str = "mb_artist_search";

    const DESCRIPTION: &'static str = "Search for artists and their releases in the MusicBrainz database. Supports artist name search and finding all releases by an artist. IMPORTANT: The 'query' parameter must contain ONLY the artist name (e.g., 'Radiohead'), never include album names, track titles, or years. Returns structured data with MBIDs, country, area, and disambiguation.";

    #[instrument(skip_all, fields(search_type = %params.search_type, query = %params.query, limit = params.limit))]
    fn execute(params: &MbArtistParams) -> CallToolResult {
        let limit = validate_limit(params.limit);
        match params.search_type.as_str() {
            "artist" => Self::search_artists(&params.query, limit),
            "artist_releases" => Self::search_releases_by_artist(&params.query, limit),
            other => error_result(&format!(
                "Unknown search type: {}. Use 'artist' or 'artist_releases'",
                other
            )),
        }
    }
}

impl MbArtistTool {
    /// Search for artists by name or fetch by MBID.
    pub fn search_artists(query: &str, limit: usize) -> CallToolResult {
        info!("Searching for artists matching: {}", query);

        // If query is an MBID, fetch directly
        if is_mbid(query) {
            match Artist::fetch().id(query).execute() {
                Ok(artist) => {
                    let artist_info = ArtistSearchInfo {
                        name: artist.name.clone(),
                        mbid: artist.id.clone(),
                        country: artist.country.filter(|c| !c.is_empty()),
                        area: artist.area.map(|area| area.name),
                        disambiguation: if artist.disambiguation.is_empty() {
                            None
                        } else {
                            Some(artist.disambiguation)
                        },
                    };

                    let structured_data = ArtistSearchResult {
                        artists: vec![artist_info],
                        total_count: 1,
                        query: query.to_string(),
                    };

                    let summary = format!("Found artist: '{}'", artist.name);
                    structured_result(summary, structured_data)
                }
                Err(e) => {
                    error!("Artist fetch by MBID failed: {:?}", e);
                    error_result(&format!("Artist fetch by MBID failed: {}", e))
                }
            }
        } else {
            // Search by name
            let search_query = ArtistSearchQuery::query_builder().artist(query).build();
            let search_result = Artist::search(search_query).execute();

            match search_result {
                Ok(result) => {
                    let artists: Vec<_> = result.entities.into_iter().take(limit).collect();
                    if artists.is_empty() {
                        return error_result(&format!("No artists found for query: {}", query));
                    }

                    let count = artists.len();
                    let artist_infos: Vec<ArtistSearchInfo> = artists
                        .into_iter()
                        .map(|a| ArtistSearchInfo {
                            name: a.name,
                            mbid: a.id,
                            country: a.country.filter(|c| !c.is_empty()),
                            area: a.area.map(|area| area.name),
                            disambiguation: if a.disambiguation.is_empty() {
                                None
                            } else {
                                Some(a.disambiguation)
                            },
                        })
                        .collect();

                    let structured_data = ArtistSearchResult {
                        artists: artist_infos,
                        total_count: count,
                        query: query.to_string(),
                    };

                    let summary = format!("Found {} artist(s) matching '{}'", count, query);
                    structured_result(summary, structured_data)
                }
                Err(e) => {
                    error!("Artist search failed: {:?}", e);
                    error_result(&format!("Artist search failed: {}", e))
                }
            }
        }
    }

    /// Search for releases by a specific artist (using artist name or MBID).
    pub fn search_releases_by_artist(query: &str, limit: usize) -> CallToolResult {
        info!("Searching for releases by artist: {}", query);

        // Resolve the artist in a single round-trip:
        //  - MBID supplied → one `fetch` call to retrieve the display name.
        //  - Name supplied → one `search` call; the first hit already carries
        //    both id and name, so no second fetch is needed.
        let (artist_id, artist_name) = if is_mbid(query) {
            let id = query.to_string();
            let name = match Artist::fetch().id(&id).execute() {
                Ok(artist) => artist.name,
                Err(_) => "Unknown Artist".to_string(),
            };
            (id, name)
        } else {
            debug!("Looking up artist by name: {}", query);
            let search_query = ArtistSearchQuery::query_builder().artist(query).build();
            match Artist::search(search_query).execute() {
                Ok(result) => match result.entities.into_iter().next() {
                    Some(artist) => {
                        debug!("Found artist: {} ({})", artist.name, artist.id);
                        (artist.id, artist.name)
                    }
                    None => {
                        return error_result(&format!("No artist found matching: {}", query));
                    }
                },
                Err(e) => {
                    error!("Artist lookup failed: {:?}", e);
                    return error_result(&format!("Artist lookup failed: {}", e));
                }
            }
        };

        // Search for releases by this artist using arid (artist MBID)
        let search_query = ReleaseSearchQuery::query_builder().arid(&artist_id).build();
        let search_result = Release::search(search_query).execute();

        match search_result {
            Ok(result) => {
                let releases: Vec<_> = result.entities.into_iter().take(limit).collect();
                if releases.is_empty() {
                    return error_result(&format!("No releases found for artist: {}", artist_name));
                }

                let count = releases.len();
                let release_infos: Vec<ArtistReleaseInfo> = releases
                    .into_iter()
                    .map(|r| ArtistReleaseInfo {
                        title: r.title,
                        mbid: r.id,
                        year: r.date.as_ref().and_then(|d| extract_year(&d.0)),
                        country: r.country,
                    })
                    .collect();

                let structured_data = ArtistReleasesResult {
                    artist_name: artist_name.clone(),
                    artist_mbid: artist_id,
                    releases: release_infos,
                    total_count: count,
                };

                let summary = format!("Found {} release(s) by '{}'", count, artist_name);
                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Release search failed: {:?}", e);
                error_result(&format!("Release search failed: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::RawContent;

    #[test]
    fn test_artist_params_default_limit() {
        let json = r#"{"search_type": "artist", "query": "Nirvana"}"#;
        let params: MbArtistParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 10);
    }

    #[test]
    fn test_artist_params_custom_limit() {
        let json = r#"{"search_type": "artist", "query": "Nirvana", "limit": 5}"#;
        let params: MbArtistParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 5);
    }

    // Integration tests (require network, run with: cargo test -- --ignored)
    #[ignore]
    #[test]
    fn test_search_artists() {
        let result = MbArtistTool::search_artists("Nirvana", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Nirvana"),
                "Expected 'Nirvana' in result"
            );
        }
    }

    #[ignore]
    #[test]
    fn test_search_releases_by_artist() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let result = MbArtistTool::search_releases_by_artist("Radiohead", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Radiohead"),
                "Expected 'Radiohead' in result"
            );
        }
    }

    #[ignore]
    #[test]
    fn test_search_releases_by_artist_mbid() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // Radiohead MBID
        let result =
            MbArtistTool::search_releases_by_artist("a74b1b7f-71a5-4011-9441-d0b5e4122711", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
    }

    #[ignore]
    #[test]
    fn test_search_artists_by_mbid() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // Nirvana MBID
        let result = MbArtistTool::search_artists("5b11f4ce-a62d-471e-81fc-a69a8278c7da", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Nirvana"),
                "Expected 'Nirvana' in result"
            );
        }
    }
}
