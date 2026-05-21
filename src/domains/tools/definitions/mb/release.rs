//! MusicBrainz Release search tool.
//!
//! This tool provides functionality to search for releases (albums),
//! get tracks/recordings in a release, and find all versions of a release group.

use musicbrainz_rs::{
    Fetch, Search,
    entity::release::{Release, ReleaseSearchQuery},
    entity::release_group::{ReleaseGroup, ReleaseGroupSearchQuery},
};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, instrument};

use super::MbBlockingTool;
use super::common::{
    TagInfo, default_limit, error_result, extract_year, format_duration, get_artist_name, is_mbid,
    map_tags, release_group_primary_type_str, resolve_search_query, structured_result,
    validate_country_code, validate_limit,
};

/// Structured output for release search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseSearchResult {
    pub releases: Vec<ReleaseSearchInfo>,
    pub total_count: usize,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseSearchInfo {
    pub title: String,
    pub mbid: String,
    pub artist: String,
    pub year: Option<String>,
    pub country: Option<String>,
    pub barcode: Option<String>,
    /// Populated only when `include_tags=true`. Sorted by descending
    /// upvote count, alphabetical tiebreak.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagInfo>>,
}

/// Structured output for release recordings (track listing).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseRecordingsResult {
    pub release_title: String,
    pub release_mbid: String,
    pub artist: String,
    pub media: Vec<Medium>,
    pub total_tracks: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Medium {
    pub disc_number: usize,
    pub disc_title: Option<String>,
    pub tracks: Vec<TrackInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TrackInfo {
    pub position: usize,
    pub title: String,
    pub duration: Option<String>,
    pub recording_mbid: String,
    pub artist: Option<String>,
}

/// Structured output for release group search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseGroupSearchResult {
    pub release_groups: Vec<ReleaseGroupSearchInfo>,
    pub total_count: usize,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseGroupSearchInfo {
    pub title: String,
    pub mbid: String,
    pub artist: String,
    pub first_release_year: Option<String>,
    pub primary_type: Option<String>,
    /// Populated only when `include_tags=true`. Sorted by descending
    /// upvote count, alphabetical tiebreak. Release-group tags are
    /// usually more representative of "genre" than per-release tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagInfo>>,
}

/// Structured output for release group releases (all versions).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseGroupReleasesResult {
    pub release_group_title: String,
    pub release_group_mbid: String,
    pub artist: String,
    pub releases: Vec<ReleaseVersionInfo>,
    pub total_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseVersionInfo {
    pub title: String,
    pub mbid: String,
    pub date: Option<String>,
    pub country: Option<String>,
}

/// Parameters for release search operations.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MbReleaseParams {
    /// The type of search to perform.
    /// - "release": Search for releases by title
    /// - "release_group": Search for release groups by title
    /// - "release_recordings": Get all tracks/recordings in a release
    /// - "release_group_releases": Get all versions of a release group
    #[schemars(
        description = "Search type: 'release', 'release_group', 'release_recordings', or 'release_group_releases'"
    )]
    pub search_type: String,

    /// The search query string or MusicBrainz ID. Mutually exclusive with
    /// `raw_lucene_query`.
    #[schemars(description = r#"
        Search query (release or release-group title, or MBID). Leave empty if using raw_lucene_query.
        CRITICAL RULES FOR SEARCH BY TITLE:
        - The query MUST contain ONLY the exact album/release title, nothing else.
        - DO NOT include artist names, track titles, years, formats, countries, or any additional text.
        - DO NOT add contextual information that you think might help - it will break the search.
        - Examples of CORRECT usage:
          * "Nevermind" (✔)
          * "OK Computer" (✔)
          * "The Dark Side of the Moon" (✔)
          * "0d52c146-6e39-30d2-918e-cd9c7b3cbe07" (release MBID) (✔)
        - Examples of INCORRECT usage:
          * "Nevermind Nirvana" (✘ - contains artist name)
          * "Nevermind 1991" (✘ - contains year)
          * "OK Computer by Radiohead" (✘ - contains artist)
          * "The Dark Side of the Moon CD" (✘ - contains format)
          * "Nevermind Deluxe Edition" (✘ - unless that's the exact title)
    "#)]
    #[serde(default)]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// When `true`, enrich every returned release / release-group with
    /// its `tags` list (community folksonomy, sorted by upvote count).
    /// Most useful on `release_group` where tags tend to be more
    /// representative of "genre" than the per-release tags. Ignored by
    /// the 2-step lookups (`release_recordings`, `release_group_releases`).
    #[serde(default)]
    pub include_tags: bool,

    /// Filter releases by country (ISO 3166-1 alpha-2 — e.g. `"US"`,
    /// `"GB"`, `"JP"`). Lowercase is accepted and uppercased. Pushed
    /// into the MB query as `country:<code>`. Only valid when
    /// `search_type="release"`; refused otherwise (release-groups don't
    /// have a country, and the 2-step lookups resolve an MBID first).
    /// Also refused alongside `raw_lucene_query` — embed
    /// `country:<code>` in your raw query instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,

    /// Raw Lucene escape hatch. Only valid when `search_type="release"`
    /// or `"release_group"`; refused for the 2-step lookups
    /// (`release_recordings`, `release_group_releases`). Mutually
    /// exclusive with `query`. Example:
    /// `release:"OK Computer" AND country:US AND date:[1997 TO 1999]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_lucene_query: Option<String>,
}

/// MusicBrainz Release Search Tool.
pub struct MbReleaseTool;

impl MbBlockingTool for MbReleaseTool {
    type Params = MbReleaseParams;

    const NAME: &'static str = "mb_release_search";

    const DESCRIPTION: &'static str = "Search for releases (albums) and release groups in MusicBrainz, get track listings, and find all versions of a release group. CRITICAL: The 'query' parameter must contain ONLY the album/release title (e.g., 'OK Computer'), never include artist names, years, or formats - this will cause search failures. Returns structured data with MBIDs, artists, dates, countries, and complete tracklists.";

    #[instrument(skip_all, fields(search_type = %params.search_type, query = %params.query, limit = params.limit))]
    fn execute(params: &MbReleaseParams) -> CallToolResult {
        let limit = validate_limit(params.limit);
        let raw_provided = params.raw_lucene_query.is_some();
        let country_provided = params.country.is_some();
        match params.search_type.as_str() {
            "release" => {
                if country_provided && raw_provided {
                    return error_result(
                        "country and raw_lucene_query are mutually exclusive — embed \
                         `country:<code>` in your raw query instead.",
                    );
                }
                let resolved =
                    match resolve_search_query(&params.query, params.raw_lucene_query.as_deref()) {
                        Ok(q) => q,
                        Err(e) => return error_result(&e),
                    };
                let country = match params.country.as_deref() {
                    None => None,
                    Some(raw) => match validate_country_code(raw) {
                        Ok(c) => Some(c),
                        Err(e) => return error_result(&e),
                    },
                };
                Self::search_releases(
                    &resolved,
                    limit,
                    raw_provided,
                    country.as_deref(),
                    params.include_tags,
                )
            }
            "release_group" => {
                if country_provided {
                    return error_result(
                        "country filter is not supported for search_type='release_group' \
                         (release groups don't have a country — use search_type='release').",
                    );
                }
                let resolved =
                    match resolve_search_query(&params.query, params.raw_lucene_query.as_deref()) {
                        Ok(q) => q,
                        Err(e) => return error_result(&e),
                    };
                Self::search_release_groups(&resolved, limit, raw_provided, params.include_tags)
            }
            "release_recordings" => {
                if raw_provided {
                    return error_result(
                        "raw_lucene_query is not supported for search_type='release_recordings'; \
                         resolve the release MBID via search_type='release' first.",
                    );
                }
                if country_provided {
                    return error_result(
                        "country is not supported for search_type='release_recordings' \
                         (it's a 2-step MBID lookup, not a search).",
                    );
                }
                Self::search_release_recordings(&params.query, limit)
            }
            "release_group_releases" => {
                if raw_provided {
                    return error_result(
                        "raw_lucene_query is not supported for search_type='release_group_releases'; \
                         resolve the release-group MBID via search_type='release_group' first.",
                    );
                }
                if country_provided {
                    return error_result(
                        "country is not supported for search_type='release_group_releases' \
                         (it's a 2-step MBID lookup, not a search).",
                    );
                }
                Self::search_release_group_releases(&params.query, limit)
            }
            other => error_result(&format!(
                "Unknown search type: {}. Use 'release', 'release_group', 'release_recordings', or 'release_group_releases'",
                other
            )),
        }
    }
}

impl MbReleaseTool {
    /// Search for releases by title or fetch by MBID.
    ///
    /// - `is_raw=true` sends `query` verbatim as a Lucene search and
    ///   skips the MBID-fast-path.
    /// - `country` (when `Some`) is pushed into the typed query as a
    ///   `country:<code>` filter. The dispatcher already refuses the
    ///   combination of raw + country, so this only fires on the
    ///   typed-builder path.
    /// - `include_tags=true` adds `?inc=tags` and populates the per-result
    ///   `tags` field.
    pub fn search_releases(
        query: &str,
        limit: usize,
        is_raw: bool,
        country: Option<&str>,
        include_tags: bool,
    ) -> CallToolResult {
        info!(
            "Searching for releases matching: {} (raw={}, country={:?}, tags={})",
            query, is_raw, country, include_tags
        );

        // If query is an MBID, fetch directly (unless raw — see doc-comment).
        if !is_raw && is_mbid(query) {
            let mut fetch = Release::fetch();
            fetch.id(query);
            if include_tags {
                fetch.with_tags();
            }
            match fetch.execute() {
                Ok(release) => {
                    let tags = if include_tags {
                        map_tags(release.tags.as_ref())
                    } else {
                        None
                    };
                    let release_info = ReleaseSearchInfo {
                        title: release.title.clone(),
                        mbid: release.id.clone(),
                        artist: get_artist_name(&release.artist_credit),
                        year: release.date.as_ref().and_then(|d| extract_year(&d.0)),
                        country: release.country,
                        barcode: release.barcode.filter(|b| !b.is_empty()),
                        tags,
                    };

                    let structured_data = ReleaseSearchResult {
                        releases: vec![release_info],
                        total_count: 1,
                        query: query.to_string(),
                    };

                    let summary = format!("Found release: '{}'", release.title);
                    structured_result(summary, structured_data)
                }
                Err(e) => {
                    error!("Release fetch by MBID failed: {:?}", e);
                    error_result(&format!("Release fetch by MBID failed: {}", e))
                }
            }
        } else {
            // Search by title (or raw Lucene query when is_raw).
            let final_query = if is_raw {
                query.to_string()
            } else {
                let mut qb = ReleaseSearchQuery::query_builder();
                qb.release(query);
                if let Some(c) = country {
                    qb.country(c);
                }
                qb.build()
            };
            let mut builder = Release::search(final_query);
            if include_tags {
                builder.with_tags();
            }
            let search_result = builder.execute();

            match search_result {
                Ok(result) => {
                    let releases: Vec<_> = result.entities.into_iter().take(limit).collect();
                    if releases.is_empty() {
                        return error_result(&format!("No releases found for query: {}", query));
                    }

                    let count = releases.len();
                    let release_infos: Vec<ReleaseSearchInfo> = releases
                        .into_iter()
                        .map(|r| ReleaseSearchInfo {
                            tags: if include_tags {
                                map_tags(r.tags.as_ref())
                            } else {
                                None
                            },
                            title: r.title,
                            mbid: r.id,
                            artist: get_artist_name(&r.artist_credit),
                            year: r.date.as_ref().and_then(|d| extract_year(&d.0)),
                            country: r.country,
                            barcode: r.barcode.filter(|b| !b.is_empty()),
                        })
                        .collect();

                    let structured_data = ReleaseSearchResult {
                        releases: release_infos,
                        total_count: count,
                        query: query.to_string(),
                    };

                    let summary = format!("Found {} release(s) matching '{}'", count, query);
                    structured_result(summary, structured_data)
                }
                Err(e) => {
                    error!("Release search failed: {:?}", e);
                    error_result(&format!("Release search failed: {}", e))
                }
            }
        }
    }

    /// Search for release groups by title or fetch by MBID.
    pub fn search_release_groups(
        query: &str,
        limit: usize,
        is_raw: bool,
        include_tags: bool,
    ) -> CallToolResult {
        info!(
            "Searching for release groups matching: {} (raw={}, tags={})",
            query, is_raw, include_tags
        );

        // If query is an MBID, fetch directly (unless raw).
        if !is_raw && is_mbid(query) {
            let mut fetch = ReleaseGroup::fetch();
            fetch.id(query);
            if include_tags {
                fetch.with_tags();
            }
            match fetch.execute() {
                Ok(release_group) => {
                    let tags = if include_tags {
                        map_tags(release_group.tags.as_ref())
                    } else {
                        None
                    };
                    let group_info = ReleaseGroupSearchInfo {
                        title: release_group.title.clone(),
                        mbid: release_group.id.clone(),
                        artist: get_artist_name(&release_group.artist_credit),
                        first_release_year: release_group
                            .first_release_date
                            .as_ref()
                            .and_then(|d| extract_year(&d.0)),
                        primary_type: release_group
                            .primary_type
                            .as_ref()
                            .map(release_group_primary_type_str),
                        tags,
                    };

                    let structured_data = ReleaseGroupSearchResult {
                        release_groups: vec![group_info],
                        total_count: 1,
                        query: query.to_string(),
                    };

                    let summary = format!("Found release group: '{}'", release_group.title);
                    structured_result(summary, structured_data)
                }
                Err(e) => {
                    error!("Release group fetch by MBID failed: {:?}", e);
                    error_result(&format!("Release group fetch by MBID failed: {}", e))
                }
            }
        } else {
            // Search by title (or raw Lucene query when is_raw).
            let final_query = if is_raw {
                query.to_string()
            } else {
                ReleaseGroupSearchQuery::query_builder()
                    .release_group(query)
                    .build()
            };
            let mut builder = ReleaseGroup::search(final_query);
            if include_tags {
                builder.with_tags();
            }
            let search_result = builder.execute();

            match search_result {
                Ok(result) => {
                    let groups: Vec<_> = result.entities.into_iter().take(limit).collect();
                    if groups.is_empty() {
                        return error_result(&format!(
                            "No release groups found for query: {}",
                            query
                        ));
                    }

                    let count = groups.len();
                    let group_infos: Vec<ReleaseGroupSearchInfo> = groups
                        .into_iter()
                        .map(|rg| ReleaseGroupSearchInfo {
                            tags: if include_tags {
                                map_tags(rg.tags.as_ref())
                            } else {
                                None
                            },
                            title: rg.title,
                            mbid: rg.id,
                            artist: get_artist_name(&rg.artist_credit),
                            first_release_year: rg
                                .first_release_date
                                .as_ref()
                                .and_then(|d| extract_year(&d.0)),
                            primary_type: rg
                                .primary_type
                                .as_ref()
                                .map(release_group_primary_type_str),
                        })
                        .collect();

                    let structured_data = ReleaseGroupSearchResult {
                        release_groups: group_infos,
                        total_count: count,
                        query: query.to_string(),
                    };

                    let summary = format!("Found {} release group(s) matching '{}'", count, query);
                    structured_result(summary, structured_data)
                }
                Err(e) => {
                    error!("Release group search failed: {:?}", e);
                    error_result(&format!("Release group search failed: {}", e))
                }
            }
        }
    }

    /// Get all tracks/recordings in a release.
    pub fn search_release_recordings(query: &str, limit: usize) -> CallToolResult {
        info!("Getting recordings for release: {}", query);

        // Get the release MBID
        let release_id = if is_mbid(query) {
            query.to_string()
        } else {
            // Search for release first
            let search_query = ReleaseSearchQuery::query_builder().release(query).build();
            match Release::search(search_query).execute() {
                Ok(result) => {
                    if let Some(release) = result.entities.first() {
                        debug!("Found release: {} ({})", release.title, release.id);
                        release.id.clone()
                    } else {
                        return error_result(&format!("No release found matching: {}", query));
                    }
                }
                Err(e) => {
                    error!("Release lookup failed: {:?}", e);
                    return error_result(&format!("Release lookup failed: {}", e));
                }
            }
        };

        // Fetch release with recordings (media->tracks)
        match Release::fetch().id(&release_id).with_recordings().execute() {
            Ok(release) => {
                let artist = get_artist_name(&release.artist_credit);
                let mut total_tracks = 0usize;
                let mut media_list: Vec<Medium> = Vec::new();
                // `limit` caps the total number of tracks across the whole
                // release. Iterating discs sequentially with a shared budget
                // keeps disc/track ordering stable while applying the limit
                // globally rather than per-disc.
                let mut remaining = limit;

                if let Some(media) = &release.media {
                    for (disc_idx, medium) in media.iter().enumerate() {
                        if remaining == 0 {
                            break;
                        }
                        let mut tracks = Vec::new();
                        if let Some(medium_tracks) = &medium.tracks {
                            for track in medium_tracks {
                                if remaining == 0 {
                                    break;
                                }
                                let Some(recording) = &track.recording else {
                                    continue;
                                };
                                total_tracks += 1;
                                remaining -= 1;
                                let track_artist = get_artist_name(&recording.artist_credit);
                                tracks.push(TrackInfo {
                                    // Use the MusicBrainz-assigned position on
                                    // the medium so skipping a recording-less
                                    // track doesn't drift subsequent numbers.
                                    position: track.position as usize,
                                    title: recording.title.clone(),
                                    duration: recording.length.map(|l| format_duration(l as u64)),
                                    recording_mbid: recording.id.clone(),
                                    artist: if track_artist != artist
                                        && track_artist != "Unknown Artist"
                                    {
                                        Some(track_artist)
                                    } else {
                                        None
                                    },
                                });
                            }
                        }
                        // Only emit the disc if it ended up with at least one
                        // selected track — avoids dangling empty discs when
                        // the limit is small.
                        if !tracks.is_empty() {
                            media_list.push(Medium {
                                disc_number: disc_idx + 1,
                                disc_title: medium.title.clone(),
                                tracks,
                            });
                        }
                    }
                }

                let structured_data = ReleaseRecordingsResult {
                    release_title: release.title.clone(),
                    release_mbid: release.id.clone(),
                    artist: artist.clone(),
                    media: media_list,
                    total_tracks,
                };

                let summary = if total_tracks > 0 {
                    format!(
                        "Track listing for '{}' by {} ({} track(s))",
                        release.title, artist, total_tracks
                    )
                } else {
                    format!("No tracks available for '{}'", release.title)
                };

                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Failed to fetch release recordings: {:?}", e);
                error_result(&format!("Failed to fetch release recordings: {}", e))
            }
        }
    }

    /// Get all releases/versions of a release group.
    pub fn search_release_group_releases(query: &str, limit: usize) -> CallToolResult {
        info!("Getting all versions of release group: {}", query);

        // Get the release group MBID
        let release_group_id = if is_mbid(query) {
            query.to_string()
        } else {
            // Search for release group first
            let search_query = ReleaseGroupSearchQuery::query_builder()
                .release_group(query)
                .build();
            match ReleaseGroup::search(search_query).execute() {
                Ok(result) => {
                    if let Some(rg) = result.entities.first() {
                        debug!("Found release group: {} ({})", rg.title, rg.id);
                        rg.id.clone()
                    } else {
                        return error_result(&format!(
                            "No release group found matching: {}",
                            query
                        ));
                    }
                }
                Err(e) => {
                    error!("Release group lookup failed: {:?}", e);
                    return error_result(&format!("Release group lookup failed: {}", e));
                }
            }
        };

        // Fetch release group with releases
        match ReleaseGroup::fetch()
            .id(&release_group_id)
            .with_releases()
            .execute()
        {
            Ok(release_group) => {
                let artist = get_artist_name(&release_group.artist_credit);

                let release_versions: Vec<ReleaseVersionInfo> =
                    if let Some(releases) = &release_group.releases {
                        releases
                            .iter()
                            .take(limit)
                            .map(|r| ReleaseVersionInfo {
                                title: r.title.clone(),
                                mbid: r.id.clone(),
                                date: r.date.as_ref().map(|d| d.0.clone()),
                                country: r.country.clone(),
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                let count = release_versions.len();
                let structured_data = ReleaseGroupReleasesResult {
                    release_group_title: release_group.title.clone(),
                    release_group_mbid: release_group.id.clone(),
                    artist: artist.clone(),
                    releases: release_versions,
                    total_count: count,
                };

                let summary = if count > 0 {
                    format!(
                        "Found {} version(s) of '{}' by {}",
                        count, release_group.title, artist
                    )
                } else {
                    format!("No versions found for '{}'", release_group.title)
                };

                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Failed to fetch release group: {:?}", e);
                error_result(&format!("Failed to fetch release group: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::RawContent;

    #[test]
    fn test_release_params_default_limit() {
        let json = r#"{"search_type": "release", "query": "Nevermind"}"#;
        let params: MbReleaseParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 10);
    }

    // Integration tests (require network, run with: cargo test -- --ignored)
    #[ignore]
    #[test]
    fn test_search_releases() {
        let result = MbReleaseTool::search_releases("Nevermind", 5, false, None, false);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Nevermind"),
                "Expected 'Nevermind' in result"
            );
        }
    }

    #[ignore]
    #[test]
    fn test_search_release_recordings() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // OK Computer release MBID
        let result =
            MbReleaseTool::search_release_recordings("0d52c146-6e39-30d2-918e-cd9c7b3cbe07", 20);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("OK Computer") || text.text.contains("MBID"),
                "Expected release info in result"
            );
        }
    }

    #[ignore]
    #[test]
    fn test_search_release_group_releases() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // OK Computer release group MBID
        let result = MbReleaseTool::search_release_group_releases(
            "18079f7b-78c3-3980-b16e-c5db63cc10a5",
            10,
        );
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
    }
}
