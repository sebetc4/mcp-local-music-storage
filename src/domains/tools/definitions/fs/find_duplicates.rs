//! Duplicate-detection tool.
//!
//! Walks a tree under the configured root, hashes every matching file with
//! SHA-256 (via [`super::hash::stream_sha256`]), and returns groups whose
//! hash appears more than once. Exact-byte duplicates only — a re-encoded
//! MP3 with identical tags is *not* the same file by this measure.
//!
//! Designed for "what's eating my library" reports: one call surveys a tree
//! and reports actionable groups, rather than the agent driving a fan-out of
//! [`super::hash::FsHashTool`] calls. Hard-capped at 5000 files per call.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, instrument, warn};
use walkdir::WalkDir;

use crate::core::config::Config;
use crate::core::security::validate_path;
use crate::domains::tools::definitions::fs::hash::{MAX_HASH_BYTES, stream_sha256};

/// Hard cap on traversal depth — identical to [`super::scan_audio`]; protects
/// against pathological deep trees.
const HARD_CAP_MAX_DEPTH: usize = 16;
/// Hard cap on files considered for hashing. Past this point the response
/// flags `truncated=true` and the caller should narrow the root.
const HARD_CAP_MAX_FILES: usize = 5000;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for `find_duplicates`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindDuplicatesParams {
    /// Absolute path of the directory tree to scan.
    pub root: String,

    /// File extensions to include (case-insensitive, no leading dot).
    /// Defaults to the lofty-supported audio set; matches [`super::scan_audio`]
    /// for consistency.
    #[serde(default = "default_extensions")]
    pub extensions: Vec<String>,

    /// Maximum traversal depth from `root`. Clamped at `HARD_CAP_MAX_DEPTH`.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,

    /// Maximum files to consider in a single call. Hard-capped at
    /// `HARD_CAP_MAX_FILES`. When the cap is hit the response flags
    /// `truncated=true` — there's no resume cursor because groups don't
    /// page (splitting a group across calls would be confusing).
    #[serde(default = "default_max_files")]
    pub max_files: usize,

    /// Skip files larger than `MAX_HASH_BYTES`. When `false` (default) such
    /// files surface as warnings; when `true` they're silently ignored.
    #[serde(default)]
    pub skip_oversize_silently: bool,

    /// When `true`, descend into hidden directories (names starting with
    /// `.`). Defaults to `false`.
    #[serde(default)]
    pub include_hidden: bool,
}

fn default_extensions() -> Vec<String> {
    vec![
        "mp3".into(),
        "flac".into(),
        "m4a".into(),
        "m4b".into(),
        "mp4".into(),
        "ogg".into(),
        "opus".into(),
        "wav".into(),
        "aac".into(),
        "aiff".into(),
        "aif".into(),
        "ape".into(),
        "wv".into(),
    ]
}
fn default_max_depth() -> usize {
    HARD_CAP_MAX_DEPTH
}
fn default_max_files() -> usize {
    HARD_CAP_MAX_FILES
}

// ============================================================================
// Structured Output
// ============================================================================

/// One group of files sharing a SHA-256.
#[derive(Debug, Serialize, JsonSchema)]
pub struct DuplicateGroup {
    /// Lowercase hex SHA-256 shared by every file in `paths`.
    pub sha256: String,
    /// Byte size shared by every file in `paths` (identical bytes ⇒
    /// identical size).
    pub bytes: u64,
    /// Absolute paths of the duplicates, sorted lexicographically so the
    /// output is deterministic across calls.
    pub paths: Vec<String>,
}

/// Result of a `find_duplicates` call.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FindDuplicatesResult {
    /// Groups with two or more matching files, sorted by `paths.len()`
    /// descending then by `sha256` ascending. Single-file hashes are
    /// dropped — they're not duplicates by definition.
    pub groups: Vec<DuplicateGroup>,
    /// Count of files the walker considered (after extension and hidden-dir
    /// filtering, before hashing).
    pub files_scanned: usize,
    /// Count of files that were successfully hashed (`files_scanned` minus
    /// IO errors and oversize-skip).
    pub files_hashed: usize,
    /// Number of entries in `groups`.
    pub total_groups: usize,
    /// `true` when `max_files` cut the scan short. The reported groups are
    /// still valid; they just don't reflect a complete picture of the tree.
    pub truncated: bool,
    /// Per-entry skip reasons (oversize, IO errors, validation rejects).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Duplicate-detection tool.
pub struct FindDuplicatesTool;

impl FindDuplicatesTool {
    pub const NAME: &'static str = "find_duplicates";

    pub const DESCRIPTION: &'static str = "Walk a directory tree under the configured root, hash every matching file with \
         SHA-256, and return groups of files sharing the same hash (exact byte duplicates only — \
         re-encoded copies will not match). Filter by extension (default = lofty audio set); \
         bounded by max_depth (hard cap 16) and max_files (hard cap 5000). Oversize files (> 500 \
         MB) surface as warnings unless skip_oversize_silently=true. No resume cursor: groups \
         don't paginate cleanly; narrow the root if you hit the cap.";

    #[instrument(skip_all, fields(root = %params.root, max_files = %params.max_files))]
    pub fn execute(params: &FindDuplicatesParams, config: &Config) -> CallToolResult {
        info!(
            "find_duplicates called: root='{}' max_depth={} max_files={} include_hidden={}",
            params.root, params.max_depth, params.max_files, params.include_hidden,
        );

        let canonical_root = match validate_path(&params.root, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };
        if !canonical_root.is_dir() {
            return CallToolResult::error(vec![Content::text(format!(
                "Scan root is not a directory: {}",
                params.root
            ))]);
        }

        let effective_max_depth = params.max_depth.min(HARD_CAP_MAX_DEPTH);
        let effective_max_files = params.max_files.clamp(1, HARD_CAP_MAX_FILES);

        let allowed_extensions: Vec<String> = params
            .extensions
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();

        let include_hidden = params.include_hidden;
        let walker = WalkDir::new(&canonical_root)
            .min_depth(0)
            .max_depth(effective_max_depth)
            .follow_links(false)
            .sort_by(|a, b| a.file_name().cmp(b.file_name()))
            .into_iter()
            .filter_entry(move |entry| {
                if include_hidden || entry.depth() == 0 {
                    return true;
                }
                entry
                    .file_name()
                    .to_str()
                    .map(|s| !s.starts_with('.'))
                    .unwrap_or(true)
            });

        // (sha256, bytes) → list of paths. Bytes is part of the value so the
        // output group can echo it back without re-statting.
        let mut by_hash: HashMap<String, (u64, Vec<String>)> = HashMap::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut files_scanned: usize = 0;
        let mut files_hashed: usize = 0;
        let mut truncated = false;

        for entry_res in walker {
            let entry = match entry_res {
                Ok(e) => e,
                Err(e) => {
                    warnings.push(format!("Walk error: {}", e));
                    continue;
                }
            };
            let path = entry.path();
            let file_type = entry.file_type();
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }

            // Extension filter.
            let ext_lower = match path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
            {
                Some(e) => e,
                None => continue,
            };
            if !allowed_extensions.iter().any(|a| a == &ext_lower) {
                continue;
            }

            // Per-entry path validation (symlink policy + defence-in-depth
            // for the root containment check).
            if let Err(e) = validate_path(&path.to_string_lossy(), config) {
                warnings.push(format!("Skipped '{}': {}", path.display(), e));
                continue;
            }

            if files_scanned >= effective_max_files {
                truncated = true;
                break;
            }
            files_scanned += 1;

            // Pre-flight size check so we don't open + read a file the hasher
            // would refuse anyway. Streaming has a second guard, but this
            // gives a cleaner warning for the common "this one's too big"
            // case.
            let size = match std::fs::metadata(path) {
                Ok(m) if m.is_file() => m.len(),
                Ok(_) => continue,
                Err(e) => {
                    warnings.push(format!("Could not stat '{}': {}", path.display(), e));
                    continue;
                }
            };
            if size > MAX_HASH_BYTES {
                if !params.skip_oversize_silently {
                    warnings.push(format!(
                        "Skipped '{}': {} bytes exceeds {} byte cap",
                        path.display(),
                        size,
                        MAX_HASH_BYTES
                    ));
                }
                continue;
            }

            // Hash.
            match stream_sha256(path) {
                Ok((digest, bytes)) => {
                    let entry = by_hash.entry(digest).or_insert_with(|| (bytes, Vec::new()));
                    entry.1.push(path.display().to_string());
                    files_hashed += 1;
                }
                Err(e) => {
                    warnings.push(format!("Hash failed for '{}': {}", path.display(), e));
                }
            }
        }

        // Keep only groups with at least two members.
        let mut groups: Vec<DuplicateGroup> = by_hash
            .into_iter()
            .filter(|(_, (_, paths))| paths.len() > 1)
            .map(|(sha256, (bytes, mut paths))| {
                paths.sort();
                DuplicateGroup {
                    sha256,
                    bytes,
                    paths,
                }
            })
            .collect();

        // Deterministic ordering: largest groups first; ties broken by hash
        // so the response is byte-stable across runs.
        groups.sort_by(|a, b| {
            b.paths
                .len()
                .cmp(&a.paths.len())
                .then(a.sha256.cmp(&b.sha256))
        });

        let total_groups = groups.len();
        let summary = if total_groups == 0 {
            format!(
                "No duplicates found ({} files hashed across {} considered)",
                files_hashed, files_scanned
            )
        } else {
            let total_dup_files: usize = groups.iter().map(|g| g.paths.len()).sum();
            format!(
                "Found {} duplicate group(s) covering {} files ({} files hashed total{})",
                total_groups,
                total_dup_files,
                files_hashed,
                if truncated { ", truncated" } else { "" }
            )
        };

        let payload = FindDuplicatesResult {
            groups,
            files_scanned,
            files_hashed,
            total_groups,
            truncated,
            warnings,
        };
        info!("{}", summary);
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: FindDuplicatesParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<FindDuplicatesParams>(),
        )
        .with_raw_output_schema(schema_for_type::<FindDuplicatesResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: FindDuplicatesParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
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

    fn default_params(root: &Path) -> FindDuplicatesParams {
        FindDuplicatesParams {
            root: root.to_string_lossy().into_owned(),
            extensions: default_extensions(),
            max_depth: HARD_CAP_MAX_DEPTH,
            max_files: HARD_CAP_MAX_FILES,
            skip_oversize_silently: false,
            include_hidden: false,
        }
    }

    #[test]
    fn finds_one_group_of_three_identical_files() {
        // The roadmap's acceptance scenario.
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        let bytes = b"identical payload".repeat(100);
        std::fs::write(root.path().join("a.mp3"), &bytes).unwrap();
        std::fs::write(root.path().join("b.mp3"), &bytes).unwrap();
        std::fs::write(root.path().join("c.mp3"), &bytes).unwrap();
        // Two distinct files for noise — must not appear in the result.
        std::fs::write(root.path().join("d.mp3"), b"different 1").unwrap();
        std::fs::write(root.path().join("e.mp3"), b"different 2").unwrap();

        let r = FindDuplicatesTool::execute(&default_params(root.path()), &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["total_groups"], 1);
        assert_eq!(s["files_hashed"], 5);

        let groups = s["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group["paths"].as_array().unwrap().len(), 3);
        // Paths are sorted lexicographically.
        let paths: Vec<&str> = group["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(paths[0].ends_with("/a.mp3"));
        assert!(paths[1].ends_with("/b.mp3"));
        assert!(paths[2].ends_with("/c.mp3"));
    }

    #[test]
    fn returns_no_groups_when_every_file_is_unique() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        for i in 0..5 {
            std::fs::write(
                root.path().join(format!("f{}.mp3", i)),
                format!("payload-{}", i).as_bytes(),
            )
            .unwrap();
        }
        let r = FindDuplicatesTool::execute(&default_params(root.path()), &cfg);
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["total_groups"], 0);
        assert_eq!(s["files_hashed"], 5);
        assert!(s["groups"].as_array().unwrap().is_empty());
    }

    #[test]
    fn extension_filter_ignores_non_audio() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let bytes = b"same bytes everywhere";
        // Two duplicates inside the filter, one matching pair OUTSIDE the
        // filter — the outside pair should be invisible.
        std::fs::write(root.path().join("a.mp3"), bytes).unwrap();
        std::fs::write(root.path().join("b.mp3"), bytes).unwrap();
        std::fs::write(root.path().join("a.txt"), bytes).unwrap();
        std::fs::write(root.path().join("b.txt"), bytes).unwrap();

        let r = FindDuplicatesTool::execute(&default_params(root.path()), &cfg);
        let s = r.structured_content.unwrap();
        assert_eq!(s["total_groups"], 1);
        let group = &s["groups"][0];
        assert_eq!(group["paths"].as_array().unwrap().len(), 2);
        assert_eq!(s["files_hashed"], 2);
    }

    #[test]
    fn discovers_duplicates_across_subdirectories() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let bytes = b"album bytes";

        std::fs::create_dir_all(root.path().join("Artist A/Album")).unwrap();
        std::fs::create_dir_all(root.path().join("Artist B/Album")).unwrap();
        std::fs::write(root.path().join("Artist A/Album/01.mp3"), bytes).unwrap();
        std::fs::write(root.path().join("Artist B/Album/01.mp3"), bytes).unwrap();

        let r = FindDuplicatesTool::execute(&default_params(root.path()), &cfg);
        let s = r.structured_content.unwrap();
        assert_eq!(s["total_groups"], 1);
        assert_eq!(s["groups"][0]["paths"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn refuses_root_outside_configured_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let r = FindDuplicatesTool::execute(
            &default_params(outside.path()),
            &config_rooted_at(root.path()),
        );
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn truncates_when_max_files_exceeded() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        for i in 0..10 {
            std::fs::write(
                root.path().join(format!("f{:02}.mp3", i)),
                format!("payload-{}", i).as_bytes(),
            )
            .unwrap();
        }
        let mut params = default_params(root.path());
        params.max_files = 4;
        let r = FindDuplicatesTool::execute(&params, &cfg);
        let s = r.structured_content.unwrap();
        assert_eq!(s["truncated"], true);
        assert_eq!(s["files_scanned"], 4);
    }

    #[test]
    fn ranks_groups_by_descending_count() {
        // Two duplicate groups: triple (a/b/c) and pair (x/y).
        // Output must list the triple first.
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        let triple = b"triplet bytes";
        let pair = b"pair bytes here";
        std::fs::write(root.path().join("a.mp3"), triple).unwrap();
        std::fs::write(root.path().join("b.mp3"), triple).unwrap();
        std::fs::write(root.path().join("c.mp3"), triple).unwrap();
        std::fs::write(root.path().join("x.mp3"), pair).unwrap();
        std::fs::write(root.path().join("y.mp3"), pair).unwrap();

        let r = FindDuplicatesTool::execute(&default_params(root.path()), &cfg);
        let s = r.structured_content.unwrap();
        assert_eq!(s["total_groups"], 2);
        let groups = s["groups"].as_array().unwrap();
        assert_eq!(groups[0]["paths"].as_array().unwrap().len(), 3);
        assert_eq!(groups[1]["paths"].as_array().unwrap().len(), 2);
    }
}
