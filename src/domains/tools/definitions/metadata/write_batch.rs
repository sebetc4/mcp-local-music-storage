//! Batch metadata write tool.
//!
//! Writes tags to many audio files in a single MCP round-trip. Mirrors
//! `read_metadata_batch` but for the write side. Each per-item write reuses
//! the singleton `WriteMetadataTool::execute` so the atomic-save chain
//! (copy → save_to_path → rename) applies uniformly to every entry.
//!
//! Failure model: by default every item is attempted, and per-item failures
//! land in `entry.error` without aborting the batch. With `stop_on_error =
//! true`, the loop halts at the first failure; subsequent writes are *not*
//! attempted (no rollback of earlier successes — filesystem rollback is
//! unsafe in general).

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
use crate::domains::tools::definitions::metadata::read_batch::MAX_BATCH;
use crate::domains::tools::definitions::metadata::write::{
    WriteMetadataParams, WriteMetadataTool,
};

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the batch metadata write tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WriteMetadataBatchParams {
    /// Per-file write requests. Each entry uses the same shape as the
    /// singleton `write_metadata` params. Hard-capped at `MAX_BATCH`.
    pub writes: Vec<WriteMetadataParams>,

    /// When `true`, halt the loop at the first failure. Already-committed
    /// writes are not rolled back (filesystem rollback can't be guaranteed
    /// safely), and remaining items are not attempted — the response's
    /// `stopped_early` flag signals this, and `results.len() <
    /// writes.len()` lets the caller count the gap.
    #[serde(default)]
    pub stop_on_error: bool,
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of a batch metadata write.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WriteMetadataBatchResult {
    /// Per-attempted-write outcomes, in input order. When
    /// `stopped_early == true`, this is shorter than the input `writes`.
    pub results: Vec<BatchWriteEntry>,
    /// Number of writes that completed successfully (atomic save landed).
    pub ok_count: usize,
    /// Number of writes that errored.
    pub error_count: usize,
    /// `true` when `stop_on_error` triggered the loop to bail before
    /// processing every input.
    pub stopped_early: bool,
    /// Number of input writes that were never attempted because the loop
    /// bailed early. Zero when `stopped_early == false`.
    pub skipped: usize,
}

/// One per-file outcome inside a batch write.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BatchWriteEntry {
    /// The path that was targeted, copied from the request entry.
    pub path: String,
    /// Number of metadata fields that landed on the file. Zero when the
    /// write errored.
    pub fields_updated: usize,
    /// Human-readable failure description, or `None` on success.
    pub error: Option<String>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Batch metadata write — runs `write_metadata` per input, aggregates results.
pub struct WriteMetadataBatchTool;

impl WriteMetadataBatchTool {
    pub const NAME: &'static str = "write_metadata_batch";

    pub const DESCRIPTION: &'static str =
        "Write or update tags across many audio files in a single MCP call. Each entry uses \
         the same shape as write_metadata; per-item failures land in entry.error without \
         aborting the batch. Set stop_on_error=true to halt at the first failure (no \
         rollback of earlier writes). Hard-capped at 500 files per call.";

    #[instrument(skip_all, fields(count = %params.writes.len(), stop_on_error = %params.stop_on_error))]
    pub fn execute(params: &WriteMetadataBatchParams, config: &Config) -> CallToolResult {
        info!(
            "Write metadata batch called: {} writes (stop_on_error={})",
            params.writes.len(),
            params.stop_on_error
        );

        if params.writes.len() > MAX_BATCH {
            return CallToolResult::error(vec![Content::text(format!(
                "Batch too large: {} writes exceeds the {}-item hard cap. Split the call.",
                params.writes.len(),
                MAX_BATCH
            ))]);
        }

        let total = params.writes.len();
        let mut results: Vec<BatchWriteEntry> = Vec::with_capacity(total);
        let mut ok_count = 0usize;
        let mut error_count = 0usize;
        let mut stopped_early = false;

        for write in &params.writes {
            let single = WriteMetadataTool::execute(write, config);
            let errored = single.is_error.unwrap_or(false);

            let entry = if errored {
                error_count += 1;
                BatchWriteEntry {
                    path: write.path.clone(),
                    fields_updated: 0,
                    error: Some(extract_error_text(&single)),
                }
            } else {
                ok_count += 1;
                // The singleton's structured output exposes `fields_updated`
                // as a u64. Treat absence as zero — preserves forward-compat
                // if the singleton's schema ever drops the field.
                let fields_updated = single
                    .structured_content
                    .as_ref()
                    .and_then(|v| v.get("fields_updated"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                BatchWriteEntry {
                    path: write.path.clone(),
                    fields_updated,
                    error: None,
                }
            };
            results.push(entry);

            if errored && params.stop_on_error {
                stopped_early = true;
                break;
            }
        }

        let skipped = total - results.len();
        let summary = if stopped_early {
            format!(
                "Wrote {}/{} files; stopped at first failure ({} skipped)",
                ok_count, total, skipped
            )
        } else if error_count > 0 {
            format!(
                "Wrote {}/{} files ({} error(s))",
                ok_count, total, error_count
            )
        } else {
            format!("Wrote {}/{} files", ok_count, total)
        };

        let payload = WriteMetadataBatchResult {
            results,
            ok_count,
            error_count,
            stopped_early,
            skipped,
        };
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: WriteMetadataBatchParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<WriteMetadataBatchParams>(),
        )
        .with_raw_output_schema(schema_for_type::<WriteMetadataBatchResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: WriteMetadataBatchParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

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

    fn write_for(path: &str, title: &str) -> WriteMetadataParams {
        WriteMetadataParams {
            path: path.to_string(),
            title: Some(title.to_string()),
            artist: None,
            album: None,
            album_artist: None,
            year: None,
            track: None,
            track_total: None,
            genre: None,
            comment: None,
            clear_existing: false,
        }
    }

    #[test]
    fn rejects_batches_above_the_hard_cap() {
        let writes: Vec<WriteMetadataParams> = (0..MAX_BATCH + 1)
            .map(|i| write_for(&format!("/tmp/x{}.mp3", i), "t"))
            .collect();
        let p = WriteMetadataBatchParams {
            writes,
            stop_on_error: false,
        };
        let r = WriteMetadataBatchTool::execute(&p, &test_config());
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn empty_writes_returns_empty_result() {
        let p = WriteMetadataBatchParams {
            writes: vec![],
            stop_on_error: false,
        };
        let r = WriteMetadataBatchTool::execute(&p, &test_config());
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["results"].as_array().unwrap().len(), 0);
        assert_eq!(s["ok_count"], 0);
        assert_eq!(s["error_count"], 0);
        assert_eq!(s["stopped_early"], false);
    }

    #[test]
    fn stop_on_error_halts_at_first_failure() {
        // All three target non-existent files, so the first write errors.
        // With stop_on_error=true, we expect a single result + 2 skipped.
        let p = WriteMetadataBatchParams {
            writes: vec![
                write_for("/nonexistent/a.mp3", "A"),
                write_for("/nonexistent/b.mp3", "B"),
                write_for("/nonexistent/c.mp3", "C"),
            ],
            stop_on_error: true,
        };
        let r = WriteMetadataBatchTool::execute(&p, &test_config());
        // Overall call succeeds — per-item failure is captured in payload.
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["results"].as_array().unwrap().len(), 1);
        assert_eq!(s["ok_count"], 0);
        assert_eq!(s["error_count"], 1);
        assert_eq!(s["stopped_early"], true);
        assert_eq!(s["skipped"], 2);
    }

    #[test]
    fn default_continues_through_per_item_failures() {
        let p = WriteMetadataBatchParams {
            writes: vec![
                write_for("/nonexistent/a.mp3", "A"),
                write_for("/nonexistent/b.mp3", "B"),
            ],
            stop_on_error: false,
        };
        let r = WriteMetadataBatchTool::execute(&p, &test_config());
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["results"].as_array().unwrap().len(), 2);
        assert_eq!(s["ok_count"], 0);
        assert_eq!(s["error_count"], 2);
        assert_eq!(s["stopped_early"], false);
    }
}
