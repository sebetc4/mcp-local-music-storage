//! Agent-owned manifests.
//!
//! Three thin persistence tools — `manifest_write`, `manifest_read`,
//! `manifest_list` — for the resumability story documented in Phase 4.2 of
//! the autonomy roadmap. The server only persists the JSON blob the agent
//! gives it; the schema of the content is entirely agent-owned.
//!
//! ### Storage layout
//!
//! Files land at `<dir>/<id>.json` where `<dir>` resolves in this order:
//!
//! 1. `$MCP_MANIFEST_DIR` (override — useful for tests and operator
//!    relocation).
//! 2. `$XDG_CACHE_HOME/music-mcp/manifests`.
//! 3. `$HOME/.cache/music-mcp/manifests`.
//!
//! If none of those resolves, the call errors out — manifests need a real
//! filesystem to be useful for resume.
//!
//! ### Atomicity & safety
//!
//! Writes go through [`crate::core::fs_atomic::write_atomic`] so a crash
//! mid-write leaves the prior version untouched. IDs are validated against
//! a strict allowlist (`[A-Za-z0-9._-]{1,128}`, no leading `.`) so an id
//! can never escape the manifest directory.
//!
//! ### No delete tool
//!
//! Deliberate. Removing the file is a `rm` away; adding a deletion tool
//! invites accidents (e.g. an agent retrying a failed write by "cleaning
//! up first"). The user owns destructive maintenance.

use chrono::{DateTime, Utc};
use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::fs_atomic::write_atomic;
use crate::core::mb_cache::default_cache_dir;

/// 10 MB cap on serialised manifest bytes — manifests carry intent, not
/// payload; an agent that needs to dump GBs of state should rethink the
/// model rather than blow past this.
pub const MAX_MANIFEST_BYTES: usize = 10 * 1024 * 1024;

/// Maximum manifests listed in a single `manifest_list` call.
pub const MAX_LIST: usize = 100;

/// Cap on manifest-id length — `is_valid_manifest_id` enforces this plus
/// the character allowlist.
pub const MAX_ID_LEN: usize = 128;

// ============================================================================
// ID validation + path resolution
// ============================================================================

/// Strict allowlist for manifest IDs: `[A-Za-z0-9._-]{1,128}` with no
/// leading `.`. Tighter than [`crate::core::security::is_safe_filename`]
/// because manifest IDs go into URLs / logs / hand-written inputs where
/// non-ASCII or whitespace is more friction than features.
fn is_valid_manifest_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return false;
    }
    if id.starts_with('.') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Resolve the directory where manifest files live.
fn manifest_dir() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var("MCP_MANIFEST_DIR")
        && !override_dir.is_empty()
    {
        return Some(PathBuf::from(override_dir));
    }
    default_cache_dir().map(|p| p.join("manifests"))
}

fn manifest_path(id: &str) -> Option<PathBuf> {
    manifest_dir().map(|dir| dir.join(format!("{}.json", id)))
}

fn format_mtime(mtime: std::time::SystemTime) -> String {
    let dt: DateTime<Utc> = mtime.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// ============================================================================
// manifest_write
// ============================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ManifestWriteParams {
    /// Manifest identifier. Must match `[A-Za-z0-9._-]{1,128}` with no
    /// leading `.`. Becomes the filename (`<id>.json`) — invalid IDs are
    /// refused at the validation step so the path can never escape the
    /// manifests directory.
    pub id: String,
    /// Arbitrary JSON content. Schema is agent-owned; the server only
    /// serialises and persists. Hard cap at 10 MB serialised.
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ManifestWriteResult {
    /// Absolute path the manifest landed at.
    pub path: String,
    /// Serialised byte count (post-JSON encode).
    pub bytes: usize,
    /// RFC3339 UTC timestamp captured immediately after the write.
    pub written_at: String,
}

pub struct ManifestWriteTool;

impl ManifestWriteTool {
    pub const NAME: &'static str = "manifest_write";

    pub const DESCRIPTION: &'static str = "Persist a JSON blob keyed by `id` under the manifests directory. Atomic write \
         (crash-safe via temp + rename). Overwrites any existing manifest with the same id. \
         ID must match `[A-Za-z0-9._-]{1,128}` with no leading '.'. Hard cap at 10 MB. The \
         server owns the file location; the agent owns the JSON schema.";

    #[instrument(skip_all, fields(id = %params.id))]
    pub fn execute(params: &ManifestWriteParams, _config: &Config) -> CallToolResult {
        info!("manifest_write called: id='{}'", params.id);

        if !is_valid_manifest_id(&params.id) {
            return CallToolResult::error(vec![Content::text(format!(
                "Invalid manifest id '{}': must match [A-Za-z0-9._-]{{1,{}}}, no leading '.'",
                params.id, MAX_ID_LEN
            ))]);
        }

        let path = match manifest_path(&params.id) {
            Some(p) => p,
            None => {
                return CallToolResult::error(vec![Content::text(
                    "Could not resolve manifest dir: set $MCP_MANIFEST_DIR, $XDG_CACHE_HOME, or $HOME".to_string(),
                )]);
            }
        };

        let body = match serde_json::to_vec_pretty(&params.content) {
            Ok(b) => b,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to serialise content: {}",
                    e
                ))]);
            }
        };
        if body.len() > MAX_MANIFEST_BYTES {
            return CallToolResult::error(vec![Content::text(format!(
                "Manifest too large: {} bytes exceeds the {} byte cap",
                body.len(),
                MAX_MANIFEST_BYTES
            ))]);
        }

        // Create the parent dir on first use. Subsequent writes are no-op
        // because create_dir_all is idempotent.
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to create manifest dir '{}': {}",
                parent.display(),
                e
            ))]);
        }

        if let Err(e) = write_atomic(&path, &body) {
            warn!("Failed to write manifest '{}': {}", params.id, e);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to write manifest: {}",
                e
            ))]);
        }

        let written_at = format_mtime(std::time::SystemTime::now());
        let bytes = body.len();
        let payload = ManifestWriteResult {
            path: path.display().to_string(),
            bytes,
            written_at: written_at.clone(),
        };
        let summary = format!(
            "Wrote manifest '{}' ({} bytes) at {}",
            params.id, bytes, written_at
        );
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: ManifestWriteParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<ManifestWriteParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ManifestWriteResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: ManifestWriteParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// manifest_read
// ============================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ManifestReadParams {
    /// Manifest identifier. Same validation as [`ManifestWriteParams`].
    pub id: String,
}

/// Read response. When the manifest doesn't exist, the call still succeeds
/// at the MCP level — `error = Some("NotFound")` so the agent can branch
/// on "first run vs resume" cleanly. Tool errors are reserved for
/// validation failures and IO errors that the agent can't recover from.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ManifestReadResult {
    /// The id that was requested, echoed back for correlation.
    pub id: String,
    /// Parsed JSON content. `null` when `error == "NotFound"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    /// RFC3339 UTC timestamp of the manifest file's modification time.
    /// `null` when not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_at: Option<String>,
    /// File size in bytes. `null` when not found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// `"NotFound"` when the file doesn't exist; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct ManifestReadTool;

impl ManifestReadTool {
    pub const NAME: &'static str = "manifest_read";

    pub const DESCRIPTION: &'static str = "Read a previously persisted manifest by id. Returns the parsed JSON content with \
         its mtime and byte size. A missing manifest returns a structured `error: \"NotFound\"` \
         (NOT a tool error) so the agent can distinguish 'first run' from 'malformed call'.";

    #[instrument(skip_all, fields(id = %params.id))]
    pub fn execute(params: &ManifestReadParams, _config: &Config) -> CallToolResult {
        info!("manifest_read called: id='{}'", params.id);

        if !is_valid_manifest_id(&params.id) {
            return CallToolResult::error(vec![Content::text(format!(
                "Invalid manifest id '{}': must match [A-Za-z0-9._-]{{1,{}}}, no leading '.'",
                params.id, MAX_ID_LEN
            ))]);
        }

        let path = match manifest_path(&params.id) {
            Some(p) => p,
            None => {
                return CallToolResult::error(vec![Content::text(
                    "Could not resolve manifest dir".to_string(),
                )]);
            }
        };

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let payload = ManifestReadResult {
                    id: params.id.clone(),
                    content: None,
                    written_at: None,
                    bytes: None,
                    error: Some("NotFound".to_string()),
                };
                let summary = format!("Manifest '{}' not found", params.id);
                return crate::domains::tools::result::structured_ok(summary, &payload);
            }
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to read manifest '{}': {}",
                    params.id, e
                ))]);
            }
        };

        let content: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Manifest '{}' is not valid JSON: {}",
                    params.id, e
                ))]);
            }
        };

        let (bytes_len, mtime) = match std::fs::metadata(&path) {
            Ok(m) => (
                m.len(),
                m.modified()
                    .map(format_mtime)
                    .unwrap_or_else(|_| "unknown".to_string()),
            ),
            Err(_) => (bytes.len() as u64, "unknown".to_string()),
        };

        let payload = ManifestReadResult {
            id: params.id.clone(),
            content: Some(content),
            written_at: Some(mtime.clone()),
            bytes: Some(bytes_len),
            error: None,
        };
        let summary = format!(
            "Read manifest '{}' ({} bytes, written {})",
            params.id, bytes_len, mtime
        );
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: ManifestReadParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<ManifestReadParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ManifestReadResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: ManifestReadParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// manifest_list
// ============================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
pub struct ManifestListParams {}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ManifestSummary {
    /// Manifest id (filename without the `.json` extension).
    pub id: String,
    /// RFC3339 UTC mtime.
    pub written_at: String,
    /// File size in bytes.
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ManifestListResult {
    /// Manifests sorted by `written_at` descending. Capped at 100 entries —
    /// older manifests fall off the list.
    pub manifests: Vec<ManifestSummary>,
    /// Absolute path to the directory the listing came from. Echoed so
    /// the caller can `rm` directly if cleanup is needed.
    pub dir: String,
    /// Total `.json` files in the directory before the cap was applied.
    pub total: usize,
    /// `true` when the listing was capped at `MAX_LIST`.
    pub truncated: bool,
}

pub struct ManifestListTool;

impl ManifestListTool {
    pub const NAME: &'static str = "manifest_list";

    pub const DESCRIPTION: &'static str = "List manifests in the configured directory, sorted by mtime descending. Capped at \
         100 entries; older manifests are dropped from the listing. Returns `dir` so the agent (or \
         the operator) knows where the files live for direct `rm` cleanup if needed.";

    #[instrument(skip_all)]
    pub fn execute(_params: &ManifestListParams, _config: &Config) -> CallToolResult {
        let dir = match manifest_dir() {
            Some(d) => d,
            None => {
                return CallToolResult::error(vec![Content::text(
                    "Could not resolve manifest dir".to_string(),
                )]);
            }
        };

        // Missing directory is a legitimate "no manifests yet" state —
        // return an empty list rather than erroring.
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let payload = ManifestListResult {
                    manifests: Vec::new(),
                    dir: dir.display().to_string(),
                    total: 0,
                    truncated: false,
                };
                let summary = format!("No manifests yet (directory '{}' is absent)", dir.display());
                return crate::domains::tools::result::structured_ok(summary, &payload);
            }
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to list manifests in '{}': {}",
                    dir.display(),
                    e
                ))]);
            }
        };

        let mut summaries: Vec<ManifestSummary> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Only `.json` files; everything else is the agent's business
            // (e.g. operator-left notes).
            let id = match path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            {
                Some(s) => s,
                None => continue,
            };
            let ext_ok = path
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("json"));
            if !ext_ok {
                continue;
            }
            if !is_valid_manifest_id(&id) {
                // Skip stray files whose name doesn't fit the contract —
                // never produced by our own writer.
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let bytes = metadata.len();
            let written_at = metadata
                .modified()
                .map(format_mtime)
                .unwrap_or_else(|_| "unknown".to_string());
            summaries.push(ManifestSummary {
                id,
                written_at,
                bytes,
            });
        }

        // Sort by RFC3339 string descending — strings compare lex-correctly
        // when all entries are in the same UTC offset (we always emit `Z`).
        summaries.sort_by(|a, b| b.written_at.cmp(&a.written_at));

        let total = summaries.len();
        let truncated = total > MAX_LIST;
        if truncated {
            summaries.truncate(MAX_LIST);
        }

        let summary = if total == 0 {
            format!("No manifests in '{}'", dir.display())
        } else if truncated {
            format!(
                "Listed {} most-recent manifests of {} in '{}'",
                MAX_LIST,
                total,
                dir.display()
            )
        } else {
            format!("Listed {} manifest(s) in '{}'", total, dir.display())
        };

        let payload = ManifestListResult {
            manifests: summaries,
            dir: dir.display().to_string(),
            total,
            truncated,
        };
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: ManifestListParams = serde_json::from_value(arguments).unwrap_or_default();
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<ManifestListParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ManifestListResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                // `manifest_list` is parameter-less; tolerate any input
                // shape rather than erroring on an empty `{}`.
                let params: ManifestListParams =
                    serde_json::from_value(serde_json::Value::Object(args)).unwrap_or_default();
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validator_accepts_normal_ids() {
        assert!(is_valid_manifest_id("harmonize-2026-05-17"));
        assert!(is_valid_manifest_id("run_42"));
        assert!(is_valid_manifest_id("v1.2.3"));
        assert!(is_valid_manifest_id("a"));
        assert!(is_valid_manifest_id(&"a".repeat(MAX_ID_LEN)));
    }

    #[test]
    fn id_validator_refuses_traversal_and_separators() {
        assert!(!is_valid_manifest_id(""));
        assert!(!is_valid_manifest_id(".."));
        assert!(!is_valid_manifest_id(".hidden"));
        assert!(!is_valid_manifest_id("a/b"));
        assert!(!is_valid_manifest_id("a\\b"));
        assert!(!is_valid_manifest_id("a b"));
        assert!(!is_valid_manifest_id("é"));
        assert!(!is_valid_manifest_id("a:b"));
        assert!(!is_valid_manifest_id(&"a".repeat(MAX_ID_LEN + 1)));
    }
}
