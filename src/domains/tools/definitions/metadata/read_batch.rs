//! Batch metadata read tool.
//!
//! Reads metadata from many audio files in a single MCP round-trip. Designed
//! for autonomous "process this library" runs where the agent has just
//! discovered N files via `fs_scan_audio` and would otherwise need N
//! separate `read_metadata` calls.
//!
//! Each item carries its own `error: Option<String>`. The call itself
//! succeeds even if individual files fail — the agent decides what to do
//! per item by reading the per-item status.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, instrument};

use crate::core::config::Config;
use crate::domains::tools::definitions::metadata::read::{
    MetadataReadResult, ReadMetadataParams, ReadMetadataTool,
};

/// Hard cap on the number of files processed in a single call. Keeps one
/// tool invocation from monopolising the server for minutes at a time on
/// huge libraries — the agent paginates by re-calling instead.
pub const MAX_BATCH: usize = 500;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the batch metadata read tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadMetadataBatchParams {
    /// Paths of the audio files to read. Hard-capped at `MAX_BATCH`.
    pub paths: Vec<String>,

    /// When `true`, include technical audio properties (bitrate, sample
    /// rate, duration) for every entry — applied uniformly across the
    /// batch.
    #[serde(default)]
    pub include_properties: bool,
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of a batch metadata read.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadMetadataBatchResult {
    /// Per-input results, same length and order as `paths`. An item with
    /// `error: null` carries a populated `metadata`; an item with
    /// `error: Some(_)` carries `metadata: null`.
    pub results: Vec<BatchReadEntry>,
    /// Number of items that produced metadata successfully.
    pub ok_count: usize,
    /// Number of items that errored. `ok_count + error_count == results.len()`.
    pub error_count: usize,
}

/// One per-file outcome inside a batch read.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BatchReadEntry {
    /// The path that was read, copied verbatim from the request so the
    /// agent can match results to inputs without index gymnastics.
    pub path: String,
    /// The metadata payload that would have been returned by a singleton
    /// `read_metadata` call. `None` when the read failed (see `error`).
    pub metadata: Option<MetadataReadResult>,
    /// Human-readable failure description. Mutually exclusive with
    /// `metadata` being populated.
    pub error: Option<String>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Batch metadata read — runs `read_metadata` per path, aggregates results.
pub struct ReadMetadataBatchTool;

impl ReadMetadataBatchTool {
    pub const NAME: &'static str = "read_metadata_batch";

    pub const DESCRIPTION: &'static str =
        "Read metadata from many audio files in a single MCP call. \
         Returns per-item results; each item carries its own error field so a single \
         bad file doesn't fail the whole batch. Hard-capped at 500 files per call — \
         paginate above that. Uniform include_properties applied to every entry.";

    #[instrument(skip_all, fields(count = %params.paths.len(), include_properties = %params.include_properties))]
    pub fn execute(params: &ReadMetadataBatchParams, config: &Config) -> CallToolResult {
        info!(
            "Read metadata batch called: {} paths (include_properties={})",
            params.paths.len(),
            params.include_properties
        );

        if params.paths.len() > MAX_BATCH {
            return CallToolResult::error(vec![Content::text(format!(
                "Batch too large: {} paths exceeds the {}-item hard cap. Split the call.",
                params.paths.len(),
                MAX_BATCH
            ))]);
        }

        let mut results: Vec<BatchReadEntry> = Vec::with_capacity(params.paths.len());
        let mut ok_count = 0usize;
        let mut error_count = 0usize;

        for path in &params.paths {
            // Reuse the singleton's execute() verbatim so the batch path and
            // the per-file path share parsing, validation, and structured
            // output. Per-item failures land in `entry.error`; the overall
            // call still succeeds.
            let single = ReadMetadataTool::execute(
                &ReadMetadataParams {
                    path: path.clone(),
                    include_properties: params.include_properties,
                },
                config,
            );

            let entry = if single.is_error.unwrap_or(false) {
                error_count += 1;
                BatchReadEntry {
                    path: path.clone(),
                    metadata: None,
                    error: Some(extract_error_text(&single)),
                }
            } else {
                ok_count += 1;
                BatchReadEntry {
                    path: path.clone(),
                    metadata: single
                        .structured_content
                        .as_ref()
                        .and_then(|v| serde_json::from_value(v.clone()).ok()),
                    error: None,
                }
            };
            results.push(entry);
        }

        let summary = format!(
            "Read metadata for {}/{} files ({} error(s))",
            ok_count,
            results.len(),
            error_count
        );

        let payload = ReadMetadataBatchResult {
            results,
            ok_count,
            error_count,
        };
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: ReadMetadataBatchParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<ReadMetadataBatchParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ReadMetadataBatchResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: ReadMetadataBatchParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

/// Pull the human-readable error message out of a singleton `execute`
/// failure. CallToolResult uses `Content::text` for both summaries and
/// errors, so we just concatenate every text fragment — usually one.
fn extract_error_text(result: &CallToolResult) -> String {
    use rmcp::model::RawContent;
    let mut parts: Vec<String> = Vec::new();
    for c in &result.content {
        if let RawContent::Text(t) = &c.raw {
            parts.push(t.text.clone());
        }
    }
    if parts.is_empty() {
        "unknown error".to_string()
    } else {
        parts.join(" | ")
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn rejects_batches_above_the_hard_cap() {
        let paths: Vec<String> = (0..MAX_BATCH + 1).map(|i| format!("/tmp/x{}.mp3", i)).collect();
        let p = ReadMetadataBatchParams {
            paths,
            include_properties: false,
        };
        let r = ReadMetadataBatchTool::execute(&p, &test_config());
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn empty_paths_returns_empty_result() {
        let p = ReadMetadataBatchParams {
            paths: vec![],
            include_properties: false,
        };
        let r = ReadMetadataBatchTool::execute(&p, &test_config());
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["results"].as_array().unwrap().len(), 0);
        assert_eq!(s["ok_count"], 0);
        assert_eq!(s["error_count"], 0);
    }

    #[test]
    fn missing_files_land_as_per_item_errors() {
        let p = ReadMetadataBatchParams {
            paths: vec![
                "/nonexistent/a.mp3".into(),
                "/nonexistent/b.mp3".into(),
            ],
            include_properties: false,
        };
        let r = ReadMetadataBatchTool::execute(&p, &test_config());
        // Overall call succeeds — failures are per-item.
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        let results = s["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(s["ok_count"], 0);
        assert_eq!(s["error_count"], 2);
        for entry in results {
            assert!(entry["metadata"].is_null());
            assert!(entry["error"].as_str().is_some());
        }
    }
}
