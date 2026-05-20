//! Recursive audio scan tool.
//!
//! Walks a directory tree under the configured root and returns the audio
//! files it contains. Designed for autonomous "process this library" runs:
//! one call replaces a quadratic chain of [`super::list_dir::FsListDirTool`]
//! invocations. Bounded depth, bounded result count, opaque resume cursor.
//!
//! Pagination contract: traversal is deterministic (depth-first pre-order,
//! each directory's children sorted by filename). When the per-call cap is
//! hit the tool returns an opaque `next_cursor` encoding the last emitted
//! path; passing it back skips exactly the files already seen.

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, instrument, warn};
use walkdir::WalkDir;

use crate::core::config::Config;
use crate::core::security::validate_path;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the recursive audio scan tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FsScanAudioParams {
    /// Absolute path of the directory to scan.
    pub root: String,

    /// Audio file extensions to match (case-insensitive, no leading dot).
    /// Defaults to the lofty-supported set: mp3, flac, m4a, m4b, mp4, ogg,
    /// opus, wav, aac, aiff, aif, ape, wv.
    #[serde(default = "default_extensions")]
    pub extensions: Vec<String>,

    /// Maximum traversal depth from `root`. The scan never descends below
    /// `HARD_CAP_MAX_DEPTH` regardless of the requested value.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,

    /// Maximum audio files returned in this call. Hard-capped at
    /// `HARD_CAP_MAX_RESULTS`. When the cap is hit, the response carries a
    /// `next_cursor` that resumes the scan from the next file.
    #[serde(default = "default_max_results")]
    pub max_results: usize,

    /// Opaque cursor returned by a previous truncated call. When present,
    /// the scan resumes from the first file lexicographically greater than
    /// the cursor's recorded path.
    #[serde(default)]
    pub cursor: Option<String>,

    /// When `true`, descend into hidden directories (names starting with
    /// `.`) and emit hidden files. Defaults to `false`.
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

fn default_max_results() -> usize {
    HARD_CAP_MAX_RESULTS
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of a recursive audio scan.
#[derive(Debug, Serialize, JsonSchema)]
struct ScanResult {
    /// Files matched in this call, in traversal order.
    files: Vec<ScanEntry>,
    /// Audio files emitted across the whole scan so far (this call + every
    /// prior page resumed via cursor).
    total_seen: usize,
    /// Cursor for the next page, or `null` when the scan is complete.
    next_cursor: Option<String>,
    /// `true` when the per-call cap (`max_results`) cut the response short.
    truncated: bool,
    /// `true` when the requested `max_depth` exceeded the hard cap and was
    /// silently clamped down. The client should consider deepening only via
    /// a re-scan from a sub-root.
    depth_clamped: bool,
    /// Per-entry warnings (symlinks rejected by policy, metadata read
    /// failures, etc.). Empty in the happy path.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// A single audio file matched by the scan.
#[derive(Debug, Serialize, JsonSchema)]
struct ScanEntry {
    /// Absolute path of the audio file.
    path: String,
    /// File size in bytes.
    size: u64,
    /// Lower-case extension (without leading dot).
    extension: String,
}

/// Cursor payload — serialized as JSON then base64-encoded so the agent
/// treats it as opaque.
#[derive(Debug, Serialize, Deserialize)]
struct ScanCursor {
    /// Absolute path of the last emitted file. The next call skips every
    /// entry less than or equal to this path.
    last_path: String,
    /// Cumulative count of audio files emitted before this cursor was
    /// issued. Used to populate `total_seen` on resume.
    scanned_count: usize,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Recursive audio scan — walks a tree and returns matching audio files.
pub struct FsScanAudioTool;

impl FsScanAudioTool {
    pub const NAME: &'static str = "fs_scan_audio";

    pub const DESCRIPTION: &'static str = "Recursively scan a directory under the configured root and return audio files. \
         Filters by extension (case-insensitive), bounded by max_depth (hard cap 16) and \
         max_results (hard cap 5000). Symlinks are rejected per the server symlink policy \
         and reported as warnings instead of aborting the scan. When the result count cap \
         is reached, the response carries an opaque next_cursor that resumes from the next \
         file. Designed so an autonomous workflow inspects a library in a small number of \
         round-trips rather than one per directory.";

    /// Hard caps — independent of user input. The `max_depth` clamp protects
    /// against pathologically deep trees; the `max_results` clamp keeps a
    /// single tool call from monopolising the server.
    const HARD_CAP_MAX_DEPTH: usize = HARD_CAP_MAX_DEPTH;
    const HARD_CAP_MAX_RESULTS: usize = HARD_CAP_MAX_RESULTS;

    #[instrument(skip_all, fields(root = %params.root, max_depth = %params.max_depth, max_results = %params.max_results))]
    pub fn execute(params: &FsScanAudioParams, config: &Config) -> CallToolResult {
        info!(
            "Scan audio tool called: root='{}' max_depth={} max_results={} include_hidden={} cursor={}",
            params.root,
            params.max_depth,
            params.max_results,
            params.include_hidden,
            params.cursor.is_some(),
        );

        // Root must exist and live under the configured root.
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
            warn!("Scan root is not a directory: {}", params.root);
            return CallToolResult::error(vec![Content::text(format!(
                "Scan root is not a directory: {}",
                params.root
            ))]);
        }

        // Clamp parameters defensively. We accept user values up to the hard
        // caps and quietly reduce above them; `depth_clamped` surfaces the
        // depth clamp so the caller can decide whether to re-scan deeper.
        let depth_clamped = params.max_depth > Self::HARD_CAP_MAX_DEPTH;
        let effective_max_depth = params.max_depth.min(Self::HARD_CAP_MAX_DEPTH);
        let effective_max_results = params.max_results.clamp(1, Self::HARD_CAP_MAX_RESULTS);

        // Lower-case the extension whitelist once. Strip any leading dot
        // ("." prefix users often paste from filenames) so both forms match.
        let allowed_extensions: Vec<String> = params
            .extensions
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();

        // Decode the resume cursor. A malformed cursor is a client error —
        // refusing it is safer than silently starting from scratch.
        let (resume_last_path, prior_count) = match params.cursor.as_deref() {
            Some(raw) => match decode_cursor(raw) {
                Ok(c) => (Some(c.last_path), c.scanned_count),
                Err(e) => {
                    warn!("Invalid cursor: {}", e);
                    return CallToolResult::error(vec![Content::text(format!(
                        "Invalid cursor: {}",
                        e
                    ))]);
                }
            },
            None => (None, 0),
        };

        let include_hidden = params.include_hidden;

        // Pre-order DFS with per-directory sort so traversal is deterministic
        // and cursor resume by lex-comparison is well-defined. follow_links
        // is intentionally false: symlinks are surfaced as entries and the
        // per-entry validation rejects them per the server policy.
        let walker = WalkDir::new(&canonical_root)
            .min_depth(0)
            .max_depth(effective_max_depth)
            .follow_links(false)
            .sort_by(|a, b| a.file_name().cmp(b.file_name()))
            .into_iter();

        let mut files: Vec<ScanEntry> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut next_cursor: Option<String> = None;
        let mut total_emitted = prior_count;
        let mut last_emitted_path: Option<String> = None;

        // Filter: prune hidden directories early so we don't descend at all.
        // The root itself is exempt — a user-supplied "/library/.private" is
        // an explicit choice the caller made.
        let walker = walker.filter_entry(move |entry| {
            if include_hidden {
                return true;
            }
            if entry.depth() == 0 {
                return true;
            }
            entry
                .file_name()
                .to_str()
                .map(|s| !s.starts_with('.'))
                .unwrap_or(true)
        });

        for entry_res in walker {
            let entry = match entry_res {
                Ok(e) => e,
                Err(e) => {
                    warnings.push(format!("Walk error: {}", e));
                    continue;
                }
            };

            // We only emit files; directories drive traversal but aren't
            // results. Symlinks aren't files (we set follow_links=false), so
            // they'll show up here as a non-file entry and fall through to
            // the validation step below.
            let path = entry.path();

            // Cursor skip: pre-order traversal with sorted children yields
            // entries in lex order, so a single comparison suffices.
            if let Some(ref resume) = resume_last_path
                && path.as_os_str().to_string_lossy().as_ref() <= resume.as_str()
            {
                continue;
            }

            // Directories drive traversal but never appear in the result
            // set. Symlinks are kept around for one more step so we can
            // surface a per-entry warning when the policy rejects them
            // (otherwise a `link.mp3` would just vanish silently).
            let file_type = entry.file_type();
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }

            // Extension match. Entries without a matching extension are
            // skipped silently — they're not "rejected", they just don't
            // fit the audio filter.
            let ext_lower = match path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
            {
                Some(e) => e,
                None => continue,
            };
            if !allowed_extensions
                .iter()
                .any(|allowed| allowed == &ext_lower)
            {
                continue;
            }

            // Per-entry security validation. Rejects symlinks under the
            // strict policy, and any path that drifted outside the root
            // (defence-in-depth — `walkdir` shouldn't escape the root we
            // gave it, but a symlinked ancestor could). When validation
            // refuses an entry that looked like audio, emit a warning so
            // the agent can see what was dropped.
            if let Err(e) = validate_path(&path.to_string_lossy(), config) {
                warnings.push(format!("Skipped '{}': {}", path.display(), e));
                continue;
            }

            // Past this point the entry either is a regular file, or is a
            // policy-allowed symlink. Resolve the target metadata with
            // `fs::metadata` (follows symlinks) so a symlink to a regular
            // file still gives the underlying byte size; if the link points
            // at a directory or a broken target, drop it.
            let size = match std::fs::metadata(path) {
                Ok(m) if m.is_file() => m.len(),
                Ok(_) => continue,
                Err(e) => {
                    warnings.push(format!(
                        "Could not read metadata for '{}': {}",
                        path.display(),
                        e
                    ));
                    continue;
                }
            };

            // If we've already filled this page, the current eligible entry
            // proves there is at least one more file past the cap, so the
            // cursor is meaningful. Setting truncated only here (rather than
            // immediately at `files.len() == max_results`) avoids a spurious
            // "truncated" flag when the cap coincides with the last file of
            // the traversal.
            if files.len() >= effective_max_results {
                truncated = true;
                if let Some(ref last) = last_emitted_path {
                    next_cursor = Some(encode_cursor(&ScanCursor {
                        last_path: last.clone(),
                        scanned_count: total_emitted,
                    }));
                }
                break;
            }

            let path_str = path.display().to_string();
            files.push(ScanEntry {
                path: path_str.clone(),
                size,
                extension: ext_lower,
            });
            total_emitted += 1;
            last_emitted_path = Some(path_str);
        }

        let summary = if truncated {
            format!(
                "Scanned {} files (truncated at max_results={}); use next_cursor to continue",
                files.len(),
                effective_max_results
            )
        } else {
            format!("Scanned {} files; traversal complete", files.len())
        };

        let result = ScanResult {
            files,
            total_seen: total_emitted,
            next_cursor,
            truncated,
            depth_clamped,
            warnings,
        };

        info!("{}", summary);
        crate::domains::tools::result::structured_ok(summary, &result)
    }

    /// HTTP handler for this tool.
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: FsScanAudioParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;

        let result = Self::execute(&params, &config);
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<FsScanAudioParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ScanResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: FsScanAudioParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

const HARD_CAP_MAX_DEPTH: usize = 16;
const HARD_CAP_MAX_RESULTS: usize = 5000;

fn encode_cursor(cursor: &ScanCursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_default();
    STANDARD.encode(json)
}

fn decode_cursor(raw: &str) -> Result<ScanCursor, String> {
    let bytes = STANDARD
        .decode(raw.as_bytes())
        .map_err(|e| format!("base64 decode failed: {}", e))?;
    serde_json::from_slice::<ScanCursor>(&bytes)
        .map_err(|e| format!("cursor payload not valid JSON: {}", e))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::SecurityConfig;
    use std::fs;
    use tempfile::TempDir;

    fn config_rooted_at(root: &std::path::Path) -> Config {
        let mut cfg = Config::default();
        cfg.security = SecurityConfig {
            root_path: Some(root.to_path_buf()),
            allow_symlinks: true,
        };
        cfg
    }

    fn config_strict_symlinks(root: &std::path::Path) -> Config {
        let mut cfg = Config::default();
        cfg.security = SecurityConfig {
            root_path: Some(root.to_path_buf()),
            allow_symlinks: false,
        };
        cfg
    }

    fn defaults_for(root: &std::path::Path) -> FsScanAudioParams {
        FsScanAudioParams {
            root: root.to_string_lossy().into_owned(),
            extensions: default_extensions(),
            max_depth: HARD_CAP_MAX_DEPTH,
            max_results: HARD_CAP_MAX_RESULTS,
            cursor: None,
            include_hidden: false,
        }
    }

    #[test]
    fn finds_audio_files_recursively() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        // Two audio files at different depths + one non-audio file.
        fs::create_dir_all(root.path().join("Artist/Album")).unwrap();
        fs::write(root.path().join("Artist/Album/01.mp3"), b"a").unwrap();
        fs::write(root.path().join("Artist/Album/02.flac"), b"bb").unwrap();
        fs::write(root.path().join("notes.txt"), b"ignore me").unwrap();

        let r = FsScanAudioTool::execute(&defaults_for(root.path()), &cfg);
        assert!(!r.is_error.unwrap_or(false));

        let s = r.structured_content.unwrap();
        let files = s["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(s["truncated"], false);
        assert!(s["next_cursor"].is_null());
        assert_eq!(s["total_seen"], 2);
    }

    #[test]
    fn extension_filter_is_case_insensitive_and_dot_tolerant() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        fs::write(root.path().join("a.MP3"), b"x").unwrap();
        fs::write(root.path().join("b.FLAC"), b"x").unwrap();
        fs::write(root.path().join("c.txt"), b"x").unwrap();

        let mut p = defaults_for(root.path());
        // Mixed: lower-case, upper-case, with-dot prefix.
        p.extensions = vec!["mp3".into(), ".FLAC".into()];

        let r = FsScanAudioTool::execute(&p, &cfg);
        let s = r.structured_content.unwrap();
        let files = s["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn pagination_splits_results_across_calls() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        // 6 audio files, max_results=4 → first call returns 4 + cursor,
        // second call returns the remaining 2.
        for i in 0..6 {
            fs::write(root.path().join(format!("track_{:02}.mp3", i)), b"x").unwrap();
        }

        let mut p = defaults_for(root.path());
        p.max_results = 4;

        let r1 = FsScanAudioTool::execute(&p, &cfg);
        let s1 = r1.structured_content.unwrap();
        assert_eq!(s1["files"].as_array().unwrap().len(), 4);
        assert_eq!(s1["truncated"], true);
        let cursor = s1["next_cursor"].as_str().unwrap().to_string();
        assert!(!cursor.is_empty());
        assert_eq!(s1["total_seen"], 4);

        p.cursor = Some(cursor);
        let r2 = FsScanAudioTool::execute(&p, &cfg);
        let s2 = r2.structured_content.unwrap();
        assert_eq!(s2["files"].as_array().unwrap().len(), 2);
        assert_eq!(s2["truncated"], false);
        assert!(s2["next_cursor"].is_null());
        assert_eq!(s2["total_seen"], 6);
    }

    #[test]
    fn pagination_yields_no_duplicates_and_no_gaps() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        fs::create_dir_all(root.path().join("a")).unwrap();
        fs::create_dir_all(root.path().join("b")).unwrap();
        for (dir, n) in [("a", 5), ("b", 5)] {
            for i in 0..n {
                fs::write(root.path().join(dir).join(format!("t_{:02}.mp3", i)), b"x").unwrap();
            }
        }

        let mut all_paths: Vec<String> = Vec::new();
        let mut p = defaults_for(root.path());
        p.max_results = 3;

        loop {
            let r = FsScanAudioTool::execute(&p, &cfg);
            let s = r.structured_content.unwrap();
            for f in s["files"].as_array().unwrap() {
                all_paths.push(f["path"].as_str().unwrap().to_string());
            }
            match s["next_cursor"].as_str() {
                Some(c) => p.cursor = Some(c.to_string()),
                None => break,
            }
        }

        // 10 files total; no duplicates.
        assert_eq!(all_paths.len(), 10);
        let mut sorted = all_paths.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 10);
    }

    #[test]
    fn max_depth_zero_returns_only_root_level_files() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        fs::write(root.path().join("top.mp3"), b"x").unwrap();
        fs::create_dir(root.path().join("sub")).unwrap();
        fs::write(root.path().join("sub/inner.mp3"), b"x").unwrap();

        let mut p = defaults_for(root.path());
        p.max_depth = 1;

        let r = FsScanAudioTool::execute(&p, &cfg);
        let s = r.structured_content.unwrap();
        let files = s["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0]["path"].as_str().unwrap().ends_with("top.mp3"));
    }

    #[test]
    fn hidden_files_skipped_by_default_included_when_requested() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        // Hidden directory + hidden file at root level.
        fs::create_dir(root.path().join(".cache")).unwrap();
        fs::write(root.path().join(".cache/x.mp3"), b"x").unwrap();
        fs::write(root.path().join(".secret.mp3"), b"x").unwrap();
        fs::write(root.path().join("visible.mp3"), b"x").unwrap();

        let p_default = defaults_for(root.path());
        let r1 = FsScanAudioTool::execute(&p_default, &cfg);
        let s1 = r1.structured_content.unwrap();
        // Only visible.mp3 (hidden dir + hidden file pruned).
        assert_eq!(s1["files"].as_array().unwrap().len(), 1);

        let mut p_hidden = defaults_for(root.path());
        p_hidden.include_hidden = true;
        let r2 = FsScanAudioTool::execute(&p_hidden, &cfg);
        let s2 = r2.structured_content.unwrap();
        // visible + .secret + .cache/x = 3
        assert_eq!(s2["files"].as_array().unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_with_warning_when_policy_strict() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let cfg = config_strict_symlinks(root.path());

        // One real audio file + one symlink to another real audio file.
        fs::write(root.path().join("real.mp3"), b"x").unwrap();
        let target = root.path().join("target.mp3");
        fs::write(&target, b"x").unwrap();
        symlink(&target, root.path().join("link.mp3")).unwrap();

        let r = FsScanAudioTool::execute(&defaults_for(root.path()), &cfg);
        let s = r.structured_content.unwrap();
        let files = s["files"].as_array().unwrap();
        // real.mp3 and target.mp3 are kept; link.mp3 is rejected by policy.
        let paths: Vec<String> = files
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 2);
        assert!(paths.iter().any(|p| p.ends_with("real.mp3")));
        assert!(paths.iter().any(|p| p.ends_with("target.mp3")));
        // Warning surfaced for the rejected symlink.
        let warnings = s["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap_or("").contains("link.mp3")),
            "expected a warning for link.mp3, got: {:?}",
            warnings
        );
    }

    #[test]
    fn rejects_invalid_cursor() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let mut p = defaults_for(root.path());
        p.cursor = Some("not-base64!!!".into());

        let r = FsScanAudioTool::execute(&p, &cfg);
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn rejects_path_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        let mut p = defaults_for(outside.path());
        p.root = outside.path().to_string_lossy().into_owned();

        let r = FsScanAudioTool::execute(&p, &cfg);
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn depth_clamped_flag_set_when_above_hard_cap() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        fs::write(root.path().join("a.mp3"), b"x").unwrap();

        let mut p = defaults_for(root.path());
        p.max_depth = 100;

        let r = FsScanAudioTool::execute(&p, &cfg);
        let s = r.structured_content.unwrap();
        assert_eq!(s["depth_clamped"], true);
    }

    #[test]
    fn cursor_roundtrip_preserves_payload() {
        let original = ScanCursor {
            last_path: "/library/Artist/Album/01 Track.mp3".to_string(),
            scanned_count: 42,
        };
        let encoded = encode_cursor(&original);
        let decoded = decode_cursor(&encoded).unwrap();
        assert_eq!(decoded.last_path, original.last_path);
        assert_eq!(decoded.scanned_count, original.scanned_count);
    }
}
