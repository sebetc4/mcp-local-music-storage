//! Apply-plan tool definition.
//!
//! Executes a heterogeneous list of operations (mkdir / move / write_metadata /
//! embed_cover) in input order, with explicit `dry_run` and `stop_on_error`
//! semantics. Each operation reuses its singleton's `execute` so all
//! validation, atomicity, and structured-output guarantees apply uniformly.
//!
//! ### Non-rollback policy
//!
//! Filesystem rollback can't be guaranteed safely: a half-applied plan might
//! leave a `write_metadata` succeeded on a file that has since been moved, or
//! a `move` that landed on top of a destination we no longer remember the
//! prior bytes of. Rather than fake atomicity, this tool documents what ran
//! and surfaces the index where it stopped — the caller (agent or human) is
//! responsible for resuming, retrying, or reconciling.
//!
//! ### Dry-run propagation
//!
//! - `mkdir` and `move` have native `dry_run` flags; the plan forces them on
//!   when its own `dry_run=true`. Per-op `dry_run=true` is also respected,
//!   so a caller can mix "validate this one" with "commit the rest".
//! - `write_metadata` and `embed_cover` don't have native `dry_run`; under
//!   `dry_run=true` the plan runs a minimal validation pass (path + file
//!   existence + image-source arity for embed_cover) without touching tags.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, RawContent, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, instrument};

use crate::core::config::Config;
use crate::core::security::validate_path;
use crate::domains::tools::definitions::fs::{
    mkdir::{FsMkdirParams, FsMkdirTool},
    mv::{FsMoveParams, FsMoveTool},
};
use crate::domains::tools::definitions::metadata::{
    embed_cover::{EmbedCoverParams, EmbedCoverTool},
    write::{WriteMetadataParams, WriteMetadataTool},
};

/// Hard cap on the number of operations a single plan can carry. Sized larger
/// than `MAX_BATCH` (500, metadata batch) because plans are heterogeneous —
/// organising one file usually chains 3-4 ops, so 1000 ops ≈ ~250 files of
/// end-to-end work in a single call.
pub const MAX_OPERATIONS: usize = 1000;

// ============================================================================
// Operation variants
// ============================================================================

/// One operation in a plan. Wire format is the singleton's params object
/// augmented with an `"op"` discriminator, e.g.
/// `{"op": "mkdir", "path": "/lib/A/B", "recursive": true}`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Create a directory. Mirrors `fs_mkdir` params.
    Mkdir(FsMkdirParams),
    /// Move a file or directory. Mirrors `fs_move` params.
    Move(FsMoveParams),
    /// Write or update tags. Mirrors `write_metadata` params.
    WriteMetadata(WriteMetadataParams),
    /// Embed a cover image. Mirrors `embed_cover` params.
    EmbedCover(EmbedCoverParams),
}

impl Operation {
    fn kind(&self) -> &'static str {
        match self {
            Operation::Mkdir(_) => "mkdir",
            Operation::Move(_) => "move",
            Operation::WriteMetadata(_) => "write_metadata",
            Operation::EmbedCover(_) => "embed_cover",
        }
    }
}

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for `apply_plan`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ApplyPlanParams {
    /// Ordered list of operations to execute. Hard-capped at
    /// `MAX_OPERATIONS`.
    pub operations: Vec<Operation>,

    /// When `true`, stop at the first failure. Operations already committed
    /// are NOT rolled back; the response's `stopped_early` flag plus
    /// `skipped` count let the caller see where the plan halted.
    #[serde(default)]
    pub stop_on_error: bool,

    /// When `true`, validate every operation without touching the
    /// filesystem. Per-op `dry_run=true` (where supported by the singleton)
    /// is also respected, so callers can mix "validate this one" with
    /// "commit the rest".
    #[serde(default)]
    pub dry_run: bool,
}

// ============================================================================
// Structured Output
// ============================================================================

/// Per-operation outcome.
#[derive(Debug, Serialize, JsonSchema)]
pub struct OperationResult {
    /// Zero-based position in the input `operations` array.
    pub op_index: usize,
    /// `"mkdir" | "move" | "write_metadata" | "embed_cover"`.
    pub op_kind: &'static str,
    /// `"ok"` or `"error"`. Skipped ops are not represented (see the plan
    /// `skipped` counter for those).
    pub status: &'static str,
    /// Singleton's structured output on success, or a small validation
    /// summary for validate-only dry-runs of tag-writing ops. `Null` on
    /// failure.
    pub detail: serde_json::Value,
    /// Concatenated text content from the inner tool on failure, `None` on
    /// success.
    pub error: Option<String>,
    /// Mirrors the effective `dry_run` flag for this particular op (plan
    /// dry-run OR per-op dry-run for the ops that support it).
    pub dry_run: bool,
}

/// Result of an `apply_plan` call.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ApplyPlanResult {
    /// Per-op outcomes, in input order. When `stopped_early == true`, this
    /// is shorter than the input `operations`.
    pub results: Vec<OperationResult>,
    /// Number of ops attempted (counts both ok and error). When
    /// `stopped_early`, `executed == results.len()`.
    pub executed: usize,
    /// Number of ops that returned a successful structured payload.
    pub ok_count: usize,
    /// Number of ops that errored.
    pub error_count: usize,
    /// Number of input ops never attempted because the loop bailed early.
    pub skipped: usize,
    /// `true` when `stop_on_error` triggered the loop to bail.
    pub stopped_early: bool,
    /// Echoes the plan-level `dry_run` flag.
    pub dry_run: bool,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Apply-plan tool — runs a heterogeneous sequence of operations.
pub struct ApplyPlanTool;

impl ApplyPlanTool {
    pub const NAME: &'static str = "apply_plan";

    pub const DESCRIPTION: &'static str = "Execute an ordered list of operations (mkdir, move, write_metadata, embed_cover) \
         in a single MCP call. Each entry carries the same shape as its singleton tool, \
         plus an 'op' discriminator. With dry_run=true the plan validates every op without \
         touching state. With stop_on_error=true the loop halts at the first failure; \
         already-committed ops are NOT rolled back (filesystem rollback cannot be \
         guaranteed safely). Hard-capped at 1000 operations per call.";

    #[instrument(skip_all, fields(count = %params.operations.len(), dry_run = %params.dry_run, stop_on_error = %params.stop_on_error))]
    pub fn execute(params: &ApplyPlanParams, config: &Config) -> CallToolResult {
        info!(
            "Apply plan called: {} operations (dry_run={}, stop_on_error={})",
            params.operations.len(),
            params.dry_run,
            params.stop_on_error
        );

        if params.operations.len() > MAX_OPERATIONS {
            return CallToolResult::error(vec![Content::text(format!(
                "Plan too large: {} operations exceeds the {}-op hard cap. Split the call.",
                params.operations.len(),
                MAX_OPERATIONS
            ))]);
        }

        let total = params.operations.len();
        let mut results: Vec<OperationResult> = Vec::with_capacity(total);
        let mut ok_count = 0usize;
        let mut error_count = 0usize;
        let mut stopped_early = false;

        for (idx, op) in params.operations.iter().enumerate() {
            let op_kind = op.kind();
            let (effective_dry_run, single) = dispatch_operation(op, params.dry_run, config);
            let errored = single.is_error.unwrap_or(false);

            let entry = if errored {
                error_count += 1;
                OperationResult {
                    op_index: idx,
                    op_kind,
                    status: "error",
                    detail: serde_json::Value::Null,
                    error: Some(extract_error_text(&single)),
                    dry_run: effective_dry_run,
                }
            } else {
                ok_count += 1;
                OperationResult {
                    op_index: idx,
                    op_kind,
                    status: "ok",
                    detail: single
                        .structured_content
                        .clone()
                        .unwrap_or(serde_json::Value::Null),
                    error: None,
                    dry_run: effective_dry_run,
                }
            };
            results.push(entry);

            if errored && params.stop_on_error {
                stopped_early = true;
                break;
            }
        }

        let executed = results.len();
        let skipped = total - executed;
        let payload = ApplyPlanResult {
            results,
            executed,
            ok_count,
            error_count,
            skipped,
            stopped_early,
            dry_run: params.dry_run,
        };

        let summary = if stopped_early {
            format!(
                "Plan halted at first failure: {}/{} ops ran ({} skipped, {} error)",
                executed, total, skipped, error_count
            )
        } else if error_count > 0 {
            format!(
                "Plan complete: {}/{} ops ok, {} error(s)",
                ok_count, total, error_count
            )
        } else if params.dry_run {
            format!("Plan validated (dry-run): {}/{} ops ok", ok_count, total)
        } else {
            format!("Plan complete: {}/{} ops ok", ok_count, total)
        };

        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: ApplyPlanParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<ApplyPlanParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ApplyPlanResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: ApplyPlanParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// Dispatch + validation-only paths
// ============================================================================

/// Dispatch one operation to its singleton (or to a validation-only stub for
/// tag-writing ops under dry-run). Returns the effective dry-run flag used
/// and the `CallToolResult` to surface back.
fn dispatch_operation(
    op: &Operation,
    plan_dry_run: bool,
    config: &Config,
) -> (bool, CallToolResult) {
    match op {
        Operation::Mkdir(p) => {
            let mut overridden = p.clone();
            overridden.dry_run = overridden.dry_run || plan_dry_run;
            let effective = overridden.dry_run;
            (effective, FsMkdirTool::execute(&overridden, config))
        }
        Operation::Move(p) => {
            let mut overridden = p.clone();
            overridden.dry_run = overridden.dry_run || plan_dry_run;
            let effective = overridden.dry_run;
            (effective, FsMoveTool::execute(&overridden, config))
        }
        Operation::WriteMetadata(p) => {
            if plan_dry_run {
                (true, validate_write_metadata(p, config))
            } else {
                (false, WriteMetadataTool::execute(p, config))
            }
        }
        Operation::EmbedCover(p) => {
            if plan_dry_run {
                (true, validate_embed_cover(p, config))
            } else {
                (false, EmbedCoverTool::execute(p, config))
            }
        }
    }
}

/// Validation-only stub for `write_metadata` under dry-run. Checks the same
/// pre-conditions the singleton checks (path under root + exists as a file)
/// without invoking lofty's writer. Surfaces the same error shape on failure.
fn validate_write_metadata(params: &WriteMetadataParams, config: &Config) -> CallToolResult {
    let path = match validate_path(&params.path, config) {
        Ok(p) => p,
        Err(e) => {
            return CallToolResult::error(vec![Content::text(format!(
                "Path security validation failed: {}",
                e
            ))]);
        }
    };
    if !path.is_file() {
        return CallToolResult::error(vec![Content::text(format!(
            "Path is not a file: {}",
            params.path
        ))]);
    }
    let summary = format!("Validated write_metadata on '{}'", params.path);
    let payload = serde_json::json!({
        "file": params.path,
        "validated": true,
    });
    crate::domains::tools::result::structured_ok(summary, &payload)
}

/// Validation-only stub for `embed_cover` under dry-run. Mirrors the
/// singleton's preconditions: audio path resolves to a real file, exactly one
/// image source is provided, and (when `image_path` is the source) it
/// resolves and is a regular file. The image is NOT read into memory.
fn validate_embed_cover(params: &EmbedCoverParams, config: &Config) -> CallToolResult {
    let path = match validate_path(&params.path, config) {
        Ok(p) => p,
        Err(e) => {
            return CallToolResult::error(vec![Content::text(format!(
                "Path security validation failed: {}",
                e
            ))]);
        }
    };
    if !path.is_file() {
        return CallToolResult::error(vec![Content::text(format!(
            "Path is not a file: {}",
            params.path
        ))]);
    }
    match (&params.image_path, &params.image_bytes_base64) {
        (Some(_), Some(_)) => {
            return CallToolResult::error(vec![Content::text(
                "Provide exactly one of 'image_path' or 'image_bytes_base64', not both".to_string(),
            )]);
        }
        (None, None) => {
            return CallToolResult::error(vec![Content::text(
                "Missing image source: provide 'image_path' or 'image_bytes_base64'".to_string(),
            )]);
        }
        (Some(image_path), None) => {
            let resolved = match validate_path(image_path, config) {
                Ok(p) => p,
                Err(e) => {
                    return CallToolResult::error(vec![Content::text(format!(
                        "Image path security validation failed: {}",
                        e
                    ))]);
                }
            };
            if !resolved.is_file() {
                return CallToolResult::error(vec![Content::text(format!(
                    "Image path is not a file: {}",
                    image_path
                ))]);
            }
        }
        (None, Some(_)) => {
            // Base64 decoding is deferred to the real execute call; in
            // validation-only mode we accept the literal without parsing.
        }
    }
    let summary = format!("Validated embed_cover on '{}'", params.path);
    let payload = serde_json::json!({
        "file": params.path,
        "validated": true,
    });
    crate::domains::tools::result::structured_ok(summary, &payload)
}

/// Concatenate every text content block from a tool result.
fn extract_error_text(result: &CallToolResult) -> String {
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
    use crate::core::config::SecurityConfig;
    use std::path::Path;
    use tempfile::TempDir;

    fn config_rooted_at(root: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.security = SecurityConfig {
            root_path: Some(root.to_path_buf()),
            allow_symlinks: true,
        };
        cfg
    }

    fn empty_write(path: &str) -> WriteMetadataParams {
        WriteMetadataParams {
            path: path.to_string(),
            title: Some("X".to_string()),
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
    fn rejects_plan_above_the_hard_cap() {
        let ops: Vec<Operation> = (0..MAX_OPERATIONS + 1)
            .map(|i| {
                Operation::Mkdir(FsMkdirParams {
                    path: format!("/tmp/x{}", i),
                    recursive: true,
                    dry_run: true,
                })
            })
            .collect();
        let p = ApplyPlanParams {
            operations: ops,
            stop_on_error: false,
            dry_run: true,
        };
        let r = ApplyPlanTool::execute(&p, &Config::default());
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn empty_plan_returns_empty_result() {
        let p = ApplyPlanParams {
            operations: vec![],
            stop_on_error: false,
            dry_run: false,
        };
        let r = ApplyPlanTool::execute(&p, &Config::default());
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["executed"], 0);
        assert_eq!(s["skipped"], 0);
        assert_eq!(s["stopped_early"], false);
    }

    #[test]
    fn dry_run_forces_mkdir_and_move_into_validation() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        // Source for the move must exist (move validates `from`).
        std::fs::write(root.path().join("src.txt"), b"x").unwrap();

        let mkdir_path = root.path().join("A").join("B");
        let move_dst = root.path().join("A").join("B").join("src.txt");

        let p = ApplyPlanParams {
            operations: vec![
                Operation::Mkdir(FsMkdirParams {
                    path: mkdir_path.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false, // plan-level dry_run should still force validation
                }),
                Operation::Move(FsMoveParams {
                    from: root.path().join("src.txt").to_string_lossy().into_owned(),
                    to: move_dst.to_string_lossy().into_owned(),
                    mkdir_parents: true,
                    overwrite: false,
                    dry_run: false,
                }),
            ],
            stop_on_error: false,
            dry_run: true,
        };

        let r = ApplyPlanTool::execute(&p, &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["executed"], 2);
        assert_eq!(s["ok_count"], 2);
        assert_eq!(s["error_count"], 0);
        assert_eq!(s["dry_run"], true);
        // Nothing actually happened.
        assert!(!mkdir_path.exists());
        assert!(root.path().join("src.txt").exists());
        // Each per-op result records its effective dry_run flag.
        let results = s["results"].as_array().unwrap();
        assert_eq!(results[0]["op_kind"], "mkdir");
        assert_eq!(results[0]["dry_run"], true);
        assert_eq!(results[1]["op_kind"], "move");
        assert_eq!(results[1]["dry_run"], true);
    }

    #[test]
    fn dry_run_validates_write_metadata_without_touching_file() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let f = root.path().join("track.mp3");
        // We don't need a real audio file: validation-only just checks path
        // resolves to a file under the root.
        std::fs::write(&f, b"not really audio").unwrap();
        let before = std::fs::read(&f).unwrap();

        let p = ApplyPlanParams {
            operations: vec![Operation::WriteMetadata(empty_write(&f.to_string_lossy()))],
            stop_on_error: false,
            dry_run: true,
        };
        let r = ApplyPlanTool::execute(&p, &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["ok_count"], 1);
        assert_eq!(s["results"][0]["op_kind"], "write_metadata");
        assert_eq!(s["results"][0]["detail"]["validated"], true);
        // Bytes untouched — singleton would have rewritten the file.
        assert_eq!(std::fs::read(&f).unwrap(), before);
    }

    #[test]
    fn dry_run_validates_embed_cover_arity() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let audio = root.path().join("track.mp3");
        std::fs::write(&audio, b"x").unwrap();

        // Both sources provided → validation should refuse.
        let p = ApplyPlanParams {
            operations: vec![Operation::EmbedCover(EmbedCoverParams {
                path: audio.to_string_lossy().into_owned(),
                image_path: Some("/tmp/whatever.jpg".to_string()),
                image_bytes_base64: Some("AAAA".to_string()),
                picture_type: "CoverFront".to_string(),
                description: None,
                replace_existing: false,
            })],
            stop_on_error: false,
            dry_run: true,
        };
        let r = ApplyPlanTool::execute(&p, &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["ok_count"], 0);
        assert_eq!(s["error_count"], 1);
        assert!(
            s["results"][0]["error"]
                .as_str()
                .unwrap()
                .contains("exactly one")
        );
    }

    #[test]
    fn stop_on_error_halts_at_first_failure() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        // Three mkdir ops, the second one targets a path outside the root and
        // will refuse, halting the plan.
        let inside1 = root.path().join("A");
        let inside2 = root.path().join("B");
        let outside = TempDir::new().unwrap();
        let escape = outside.path().join("escape");

        let p = ApplyPlanParams {
            operations: vec![
                Operation::Mkdir(FsMkdirParams {
                    path: inside1.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false,
                }),
                Operation::Mkdir(FsMkdirParams {
                    path: escape.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false,
                }),
                Operation::Mkdir(FsMkdirParams {
                    path: inside2.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false,
                }),
            ],
            stop_on_error: true,
            dry_run: false,
        };
        let r = ApplyPlanTool::execute(&p, &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["executed"], 2);
        assert_eq!(s["ok_count"], 1);
        assert_eq!(s["error_count"], 1);
        assert_eq!(s["skipped"], 1);
        assert_eq!(s["stopped_early"], true);
        // First op committed, second errored, third never ran.
        assert!(inside1.is_dir());
        assert!(!escape.exists());
        assert!(!inside2.exists());
    }

    #[test]
    fn best_effort_continues_through_failures() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let outside = TempDir::new().unwrap();
        let escape = outside.path().join("escape");
        let inside = root.path().join("ok");

        let p = ApplyPlanParams {
            operations: vec![
                Operation::Mkdir(FsMkdirParams {
                    path: escape.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false,
                }),
                Operation::Mkdir(FsMkdirParams {
                    path: inside.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false,
                }),
            ],
            stop_on_error: false,
            dry_run: false,
        };
        let r = ApplyPlanTool::execute(&p, &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["executed"], 2);
        assert_eq!(s["ok_count"], 1);
        assert_eq!(s["error_count"], 1);
        assert_eq!(s["skipped"], 0);
        assert_eq!(s["stopped_early"], false);
        // The second op still ran even though the first failed.
        assert!(inside.is_dir());
    }

    #[test]
    fn per_op_dry_run_respected_under_committing_plan() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let a = root.path().join("A");
        let b = root.path().join("B");

        // Plan committing, but the first op was authored as a dry-run.
        let p = ApplyPlanParams {
            operations: vec![
                Operation::Mkdir(FsMkdirParams {
                    path: a.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: true,
                }),
                Operation::Mkdir(FsMkdirParams {
                    path: b.to_string_lossy().into_owned(),
                    recursive: true,
                    dry_run: false,
                }),
            ],
            stop_on_error: false,
            dry_run: false,
        };
        let r = ApplyPlanTool::execute(&p, &cfg);
        assert!(!r.is_error.unwrap_or(false));
        // First was dry-run, second committed.
        assert!(!a.exists());
        assert!(b.is_dir());
        let s = r.structured_content.unwrap();
        assert_eq!(s["results"][0]["dry_run"], true);
        assert_eq!(s["results"][1]["dry_run"], false);
    }

    #[test]
    fn deserializes_wire_format_with_op_discriminator() {
        // Confirms the JSON shape the roadmap documents is parseable.
        let v = serde_json::json!({
            "operations": [
                {"op": "mkdir", "path": "/lib/A/B", "recursive": true},
                {"op": "move", "from": "/a", "to": "/b", "mkdir_parents": true},
                {"op": "write_metadata", "path": "/a/x.mp3", "title": "T"},
                {"op": "embed_cover", "path": "/a/x.mp3", "image_path": "/c.jpg"},
            ],
            "stop_on_error": true,
            "dry_run": true,
        });
        let parsed: ApplyPlanParams = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.operations.len(), 4);
        assert!(matches!(parsed.operations[0], Operation::Mkdir(_)));
        assert!(matches!(parsed.operations[1], Operation::Move(_)));
        assert!(matches!(parsed.operations[2], Operation::WriteMetadata(_)));
        assert!(matches!(parsed.operations[3], Operation::EmbedCover(_)));
    }
}
