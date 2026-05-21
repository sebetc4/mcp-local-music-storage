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
    AliasInfo, default_limit, error_result, label_type_str, map_aliases, resolve_search_query,
    structured_result, validate_limit,
};

/// Parameters for label search operations.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MbLabelParams {
    /// The search query string (label name). Mutually exclusive with
    /// `raw_lucene_query`. Leave empty when using the raw escape hatch.
    #[schemars(description = "Search query (label name) — leave empty if using raw_lucene_query")]
    #[serde(default)]
    pub query: String,

    /// Maximum number of results to return (default: 10, max: 100).
    #[schemars(description = "Maximum number of results (default: 10, max: 100)")]
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// When `true`, enrich every returned label with its `aliases` list
    /// (imprint chains and locale-specific spellings — useful when the
    /// path-stored label diverges from the MB canonical name). Off by
    /// default to keep the payload small.
    #[serde(default)]
    pub include_aliases: bool,

    /// Raw Lucene escape hatch for power queries. When set, bypasses the
    /// `label:` prefix and goes straight to the MB endpoint. Mutually
    /// exclusive with `query`. Example: `label:"sony*" AND country:US`.
    /// See [MB search docs](https://musicbrainz.org/doc/MusicBrainz_API/Search).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_lucene_query: Option<String>,
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
    /// Populated only when `include_aliases=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<AliasInfo>>,
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
        let resolved = match resolve_search_query(&params.query, params.raw_lucene_query.as_deref())
        {
            Ok(q) => q,
            Err(e) => return error_result(&e),
        };
        let is_raw = params.raw_lucene_query.is_some();
        Self::search_labels(
            &resolved,
            validate_limit(params.limit),
            params.include_aliases,
            is_raw,
        )
    }
}

impl MbLabelTool {
    /// Search for labels.
    ///
    /// `query` is the *final* Lucene string passed to MB. When `is_raw=false`,
    /// it goes through the typed `label:` builder; when `is_raw=true`, it
    /// bypasses the builder and is sent verbatim.
    pub fn search_labels(
        query: &str,
        limit: usize,
        include_aliases: bool,
        is_raw: bool,
    ) -> CallToolResult {
        info!(
            "Searching for labels matching: {} (aliases={}, raw={})",
            query, include_aliases, is_raw
        );

        let final_query = if is_raw {
            query.to_string()
        } else {
            LabelSearchQuery::query_builder().label(query).build()
        };
        let mut builder = Label::search(final_query);
        if include_aliases {
            builder.with_aliases();
        }
        match builder.execute() {
            Ok(result) => {
                let labels: Vec<_> = result.entities.into_iter().take(limit).collect();
                if labels.is_empty() {
                    return error_result(&format!("No labels found for query: {}", query));
                }

                let count = labels.len();
                let label_infos: Vec<LabelInfo> = labels
                    .into_iter()
                    .map(|l| LabelInfo {
                        aliases: if include_aliases {
                            map_aliases(l.aliases.as_ref())
                        } else {
                            None
                        },
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
        let result = MbLabelTool::search_labels("Sony", 5, false, false);
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
