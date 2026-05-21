//! MusicBrainz Work search tool.
//!
//! This tool provides functionality to search for works (musical compositions).
//! Works represent the underlying composition, independent of recordings or releases.

use musicbrainz_rs::{
    Search,
    entity::work::{Work, WorkSearchQuery},
};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{error, info, instrument};

use super::MbBlockingTool;
use super::common::{
    AliasInfo, default_limit, error_result, map_aliases, resolve_search_query, structured_result,
    validate_limit, work_type_str,
};

/// Parameters for work search operations.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MbWorkParams {
    /// The search query string (work title). Mutually exclusive with
    /// `raw_lucene_query`.
    #[schemars(description = "Search query (work title) — leave empty if using raw_lucene_query")]
    #[serde(default)]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// When `true`, enrich every returned work with its `aliases` list
    /// (translated titles, abbreviated forms — most useful for classical
    /// repertoire where "Symphony No. 5" / "5e Symphonie" / "Symphonie
    /// Nr. 5" are the same work). Off by default.
    #[serde(default)]
    pub include_aliases: bool,

    /// Raw Lucene escape hatch. Bypasses the typed `work:` builder; sent
    /// verbatim. Mutually exclusive with `query`. Example:
    /// `type:symphony AND lang:eng`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_lucene_query: Option<String>,
}

/// Structured output for work search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkSearchResult {
    pub works: Vec<WorkInfo>,
    pub total_count: usize,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkInfo {
    pub title: String,
    pub mbid: String,
    pub work_type: Option<String>,
    pub disambiguation: Option<String>,
    pub language: Option<String>,
    /// Populated only when `include_aliases=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<AliasInfo>>,
}

/// MusicBrainz Work Search Tool — unit struct used as a [`MbBlockingTool`] impl host.
pub struct MbWorkTool;

impl MbBlockingTool for MbWorkTool {
    type Params = MbWorkParams;

    const NAME: &'static str = "mb_work_search";

    const DESCRIPTION: &'static str = "Search for works (musical compositions) in MusicBrainz. Works represent the underlying composition independent of recordings or releases. Returns structured data with MBIDs, work types, languages, and disambiguation info.";

    // Works are static composition records — rarely updated. 7-day TTL.
    const TTL: std::time::Duration = crate::core::mb_request::TTL_STATIC;

    #[instrument(skip_all, fields(query = %params.query, limit = params.limit))]
    fn execute(params: &MbWorkParams) -> CallToolResult {
        let resolved = match resolve_search_query(&params.query, params.raw_lucene_query.as_deref())
        {
            Ok(q) => q,
            Err(e) => return error_result(&e),
        };
        let is_raw = params.raw_lucene_query.is_some();
        Self::search_works(
            &resolved,
            validate_limit(params.limit),
            params.include_aliases,
            is_raw,
        )
    }
}

impl MbWorkTool {
    /// Search for works. `is_raw=true` sends the query verbatim; otherwise
    /// goes through the typed `work:` builder.
    pub fn search_works(
        query: &str,
        limit: usize,
        include_aliases: bool,
        is_raw: bool,
    ) -> CallToolResult {
        info!(
            "Searching for works matching: {} (aliases={}, raw={})",
            query, include_aliases, is_raw
        );

        let final_query = if is_raw {
            query.to_string()
        } else {
            WorkSearchQuery::query_builder().work(query).build()
        };
        let mut builder = Work::search(final_query);
        if include_aliases {
            builder.with_aliases();
        }
        match builder.execute() {
            Ok(result) => {
                let works: Vec<_> = result.entities.into_iter().take(limit).collect();
                if works.is_empty() {
                    return error_result(&format!("No works found for query: {}", query));
                }

                let count = works.len();
                let work_infos: Vec<WorkInfo> = works
                    .into_iter()
                    .map(|w| WorkInfo {
                        aliases: if include_aliases {
                            map_aliases(w.aliases.as_ref())
                        } else {
                            None
                        },
                        title: w.title,
                        mbid: w.id,
                        work_type: w.work_type.as_ref().map(work_type_str),
                        disambiguation: w.disambiguation.filter(|d| !d.is_empty()),
                        language: w.language,
                    })
                    .collect();

                let structured_data = WorkSearchResult {
                    works: work_infos,
                    total_count: count,
                    query: query.to_string(),
                };

                let summary = format!("Found {} work(s) matching '{}'", count, query);
                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Work search failed: {:?}", e);
                error_result(&format!("Work search failed: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::RawContent;

    #[test]
    fn test_work_params_default_limit() {
        let json = r#"{"query": "Bohemian Rhapsody"}"#;
        let params: MbWorkParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 10);
    }

    // Integration tests (require network, run with: cargo test -- --ignored)
    #[ignore]
    #[test]
    fn test_search_works() {
        let result = MbWorkTool::search_works("Bohemian Rhapsody", 5, false, false);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Bohemian Rhapsody"),
                "Expected 'Bohemian Rhapsody' in result"
            );
        }
    }
}
