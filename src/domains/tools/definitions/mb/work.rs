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
    default_limit, error_result, structured_result, validate_limit, work_type_str,
};

/// Parameters for work search operations.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MbWorkParams {
    /// The search query string (work title).
    #[schemars(description = "Search query (work title)")]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,
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
}

/// MusicBrainz Work Search Tool — unit struct used as a [`MbBlockingTool`] impl host.
pub struct MbWorkTool;

impl MbBlockingTool for MbWorkTool {
    type Params = MbWorkParams;

    const NAME: &'static str = "mb_work_search";

    const DESCRIPTION: &'static str = "Search for works (musical compositions) in MusicBrainz. Works represent the underlying composition independent of recordings or releases. Returns structured data with MBIDs, work types, languages, and disambiguation info.";

    #[instrument(skip_all, fields(query = %params.query, limit = params.limit))]
    fn execute(params: &MbWorkParams) -> CallToolResult {
        Self::search_works(&params.query, validate_limit(params.limit))
    }
}

impl MbWorkTool {
    /// Search for works by title.
    pub fn search_works(query: &str, limit: usize) -> CallToolResult {
        info!("Searching for works matching: {}", query);

        let search_query = WorkSearchQuery::query_builder().work(query).build();
        let search_result = Work::search(search_query).execute();

        match search_result {
            Ok(result) => {
                let works: Vec<_> = result.entities.into_iter().take(limit).collect();
                if works.is_empty() {
                    return error_result(&format!("No works found for query: {}", query));
                }

                let count = works.len();
                let work_infos: Vec<WorkInfo> = works
                    .into_iter()
                    .map(|w| WorkInfo {
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
        let result = MbWorkTool::search_works("Bohemian Rhapsody", 5);
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
