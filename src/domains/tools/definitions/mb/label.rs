//! MusicBrainz Label search tool.
//!
//! This tool provides functionality to search for labels (record labels/publishers).
//! Labels represent the companies or organizations that publish music releases.

use musicbrainz_rs::{
    Search,
    entity::label::{Label, LabelSearchQuery},
};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{error, info, instrument};

use super::MbBlockingTool;
use super::common::{
    default_limit, error_result, label_type_str, structured_result, validate_limit,
};

/// Parameters for label search operations.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MbLabelParams {
    /// The search query string (label name).
    #[schemars(description = "Search query (label name)")]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Structured output for label search results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LabelSearchResult {
    pub labels: Vec<LabelInfo>,
    pub total_count: usize,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LabelInfo {
    pub name: String,
    pub mbid: String,
    pub label_type: Option<String>,
    pub country: Option<String>,
    pub disambiguation: Option<String>,
    pub label_code: Option<i32>,
}

/// MusicBrainz Label Search Tool.
pub struct MbLabelTool;

impl MbBlockingTool for MbLabelTool {
    type Params = MbLabelParams;

    const NAME: &'static str = "mb_label_search";

    const DESCRIPTION: &'static str = "Search for labels (record labels/publishers) in MusicBrainz. Labels represent the companies or organizations that publish music releases. Returns structured data with MBIDs, label types, countries, label codes, and disambiguation info.";

    // Labels rarely change — 7-day TTL keeps tag-write workflows fast.
    const TTL: std::time::Duration = crate::core::mb_request::TTL_STATIC;

    #[instrument(skip_all, fields(query = %params.query, limit = params.limit))]
    fn execute(params: &MbLabelParams) -> CallToolResult {
        Self::search_labels(&params.query, validate_limit(params.limit))
    }
}

impl MbLabelTool {
    /// Search for labels by name.
    pub fn search_labels(query: &str, limit: usize) -> CallToolResult {
        info!("Searching for labels matching: {}", query);

        let search_query = LabelSearchQuery::query_builder().label(query).build();
        let search_result = Label::search(search_query).execute();

        match search_result {
            Ok(result) => {
                let labels: Vec<_> = result.entities.into_iter().take(limit).collect();
                if labels.is_empty() {
                    return error_result(&format!("No labels found for query: {}", query));
                }

                let count = labels.len();
                let label_infos: Vec<LabelInfo> = labels
                    .into_iter()
                    .map(|l| LabelInfo {
                        name: l.name,
                        mbid: l.id,
                        label_type: l.label_type.as_ref().map(label_type_str),
                        country: l.country,
                        disambiguation: l.disambiguation.filter(|d| !d.is_empty()),
                        label_code: l.label_code.map(|c| c as i32),
                    })
                    .collect();

                let structured_data = LabelSearchResult {
                    labels: label_infos,
                    total_count: count,
                    query: query.to_string(),
                };

                let summary = format!("Found {} label(s) matching '{}'", count, query);
                structured_result(summary, structured_data)
            }
            Err(e) => {
                error!("Label search failed: {:?}", e);
                error_result(&format!("Label search failed: {}", e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::RawContent;

    #[test]
    fn test_label_params_default_limit() {
        let json = r#"{"query": "Sony Music"}"#;
        let params: MbLabelParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 10);
    }

    // Integration tests (require network, run with: cargo test -- --ignored)
    #[ignore]
    #[test]
    fn test_search_labels() {
        let result = MbLabelTool::search_labels("Sony", 5);
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );
        let content = &result.content[0];
        if let RawContent::Text(text) = &content.raw {
            assert!(
                text.text.contains("Sony") || text.text.contains("label"),
                "Expected label-related content in result"
            );
        }
    }
}
