//! MusicBrainz Work search tool.
//!
//! This tool provides functionality to search for works (musical compositions).
//! Works represent the underlying composition, independent of recordings or releases.

use futures::FutureExt;
use musicbrainz_rs::{
    Search,
    entity::work::{Work, WorkSearchQuery},
};
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::{error, info};

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

/// MusicBrainz Work Search Tool implementation.
#[derive(Debug, Clone)]
pub struct MbWorkTool;

impl MbWorkTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "mb_work_search";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Search for works (musical compositions) in MusicBrainz. Works represent the underlying composition independent of recordings or releases. Returns structured data with MBIDs, work types, languages, and disambiguation info.";

    pub fn new() -> Self {
        Self
    }

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    pub fn execute(params: &MbWorkParams) -> CallToolResult {
        let query = params.query.clone();
        let limit = validate_limit(params.limit);

        Self::search_works(&query, limit)
    }

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(arguments: serde_json::Value) -> Result<serde_json::Value, String> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'query' parameter".to_string())?
            .to_string();

        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;

        let params = MbWorkParams {
            query,
            limit,
        };

        // Use std::thread::spawn to avoid nested runtime panic.
        // musicbrainz_rs uses reqwest::blocking which creates its own runtime.
        let handle = std::thread::spawn(move || Self::execute(&params));

        let result = handle
            .join()
            .map_err(|_| "Thread panicked during work search".to_string())?;

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<MbWorkParams>(),
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
                let params: MbWorkParams =
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

impl Default for MbWorkTool {
    fn default() -> Self {
        Self::new()
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
