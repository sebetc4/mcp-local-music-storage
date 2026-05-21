//! Inventory-divergences tool.
//!
//! Pure-read survey of a library against a path template. For every audio
//! file under `root`, the tool:
//!
//! 1. Matches the file's path against `path_template`, capturing the named
//!    fields (genre, artist, album, title, …).
//! 2. Reads its tag values via lofty.
//! 3. Compares the path-inferred values against the tag values per file.
//!
//! Files are grouped by **leaf directory** (the file's parent), so the agent
//! can scan the per-directory histograms in `field_value_counts` to spot
//! "13 tags say 'Beatles', 4 say 'The Beatles'" and pick a canonical form.
//!
//! ### Template DSL
//!
//! Reuses the parser from [`crate::domains::tools::definitions::naming`] —
//! same `{name}`, `{name|fallback}`, `{name:0Nd}` placeholders, parsed once
//! by `parse_template`. The reverse direction (path → captures) walks each
//! `/`-delimited slot **left-to-right** using `find` for every literal,
//! except the literal immediately preceding a trailing placeholder, which
//! uses `rfind`. That single asymmetry handles the extension-detection case
//! cleanly: `{title}.{ext}` on `Mr. Brightside.mp3` captures
//! `title="Mr. Brightside"` / `ext="mp3"` (rfind on the last dot), while
//! multi-capture slots like `{disc}-{track} {title}.{ext}` still bind
//! `disc=01` and `track=05` on the leftmost separators.
//!
//! ### Pagination
//!
//! Capped at `MAX_FILES = 5000` per call. Cursor encodes the last
//! *completed* directory's path plus the cumulative `files_scanned`. On
//! resume, directories ≤ the cursor's path are skipped. Directories are
//! never split across pages — the histogram + divergence list per directory
//! only makes sense over the complete file set.

use base64::{Engine, engine::general_purpose::STANDARD};
use futures::FutureExt;
use lofty::prelude::*;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, instrument, warn};
use walkdir::WalkDir;

use crate::core::config::Config;
use crate::core::security::validate_path;
use crate::domains::tools::definitions::naming::apply_scheme::{Segment, parse_template};

/// Maximum traversal depth — matches the rest of the fs walkers.
const HARD_CAP_MAX_DEPTH: usize = 16;
/// Cap on files emitted in a single call. Aligned with `MAX_BATCH` (Phase
/// 2.2) so the agent's "process this library" runs use the same scale
/// everywhere.
const HARD_CAP_MAX_FILES: usize = 5000;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for `inventory_divergences`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct InventoryDivergencesParams {
    /// Absolute path of the directory tree to scan.
    pub root: String,

    /// Path template, relative to `root`. Uses the same `{name}` DSL as
    /// `apply_naming_scheme`. Example: `"{genre}/{artist}/{album}/{title}.{ext}"`.
    pub path_template: String,

    /// Field names to compare. When omitted, defaults to every named
    /// capture in `path_template` that has a known tag mapping (title,
    /// artist, album, album_artist, genre, year, track, disc, comment).
    /// `ext` is a path-only field with no tag mapping; including it never
    /// produces a divergence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields_to_compare: Option<Vec<String>>,

    /// When `false` (default), string comparisons are case-insensitive
    /// (so "Beatles" matches "BEATLES"). Whitespace is always trimmed
    /// before comparison regardless of this flag.
    #[serde(default)]
    pub case_sensitive: bool,

    /// Maximum files scanned per call (hard-capped at 5000). Directories
    /// are never split across pages, so the effective cap can be smaller
    /// when the cap falls in the middle of a directory's files.
    #[serde(default = "default_max_files")]
    pub max_files: usize,

    /// Opaque cursor returned by a previous truncated call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,

    /// When `true`, descend into hidden directories. Defaults to `false`.
    #[serde(default)]
    pub include_hidden: bool,
}

fn default_max_files() -> usize {
    HARD_CAP_MAX_FILES
}

// ============================================================================
// Structured Output
// ============================================================================

/// Inventory of one leaf directory (typically an album folder).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DirectoryInventory {
    /// Absolute path to this directory.
    pub path: String,
    /// Directory-level captures from the template (the fields that fall
    /// above the file's `/`-segment — typically genre/artist/album).
    pub path_inferred: BTreeMap<String, String>,
    /// Per-field value histogram across this directory's files. Each map
    /// is `value → count`, sorted with `BTreeMap` so output is
    /// deterministic. The agent uses this to pick canonical spellings.
    pub field_value_counts: BTreeMap<String, BTreeMap<String, usize>>,
    /// Per-file entries inside this directory, sorted by filename.
    pub files: Vec<FileEntry>,
}

/// One audio file under a directory.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileEntry {
    /// File name (no path).
    pub name: String,
    /// File-level captures from the template (typically title + ext +
    /// disc/track if present in the template's file segment).
    pub path_inferred: BTreeMap<String, String>,
    /// Tag values read for the comparison fields. `None` when the file
    /// has no tag for that field (vs. an empty-string tag, which is rare
    /// but kept distinct).
    pub tags: BTreeMap<String, Option<String>>,
    /// Field names whose path-inferred value diverges from the tag value
    /// (both non-empty after trim; numeric values compared numerically
    /// when both parse as integers; case sensitivity controlled by the
    /// `case_sensitive` param). Empty when the file is consistent.
    pub divergences: Vec<String>,
}

/// Result of an `inventory_divergences` call.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InventoryDivergencesResult {
    /// Directories surveyed in this call, sorted by path.
    pub directories: Vec<DirectoryInventory>,
    /// Cumulative count of files matched + processed across this call
    /// and every prior page resumed via cursor.
    pub files_scanned: usize,
    /// Of the files in `directories`, how many have at least one entry
    /// in `divergences` (across the whole call, not just this page).
    pub files_with_divergences: usize,
    /// Cursor for the next page, or `null` when the scan is complete.
    pub next_cursor: Option<String>,
    /// `true` when the per-call cap cut the scan short.
    pub truncated: bool,
    /// Files that didn't match the template (with a short reason) or
    /// errored during tag-read. Surfacing them as warnings rather than
    /// dropping them silently lets the agent decide whether to widen
    /// the template or investigate the file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InventoryCursor {
    /// Last *completed* directory path. On resume, every directory ≤ this
    /// path is skipped.
    last_directory_path: String,
    /// Total files scanned across all prior pages, propagated through the
    /// `files_scanned` counter.
    files_scanned: usize,
    /// Cumulative count of files with divergences across all prior pages.
    files_with_divergences: usize,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Inventory-divergences tool.
pub struct InventoryDivergencesTool;

impl InventoryDivergencesTool {
    pub const NAME: &'static str = "inventory_divergences";

    pub const DESCRIPTION: &'static str = "Survey a library tree against a path template and report, per leaf directory, \
         the histogram of tag values found and which files diverge from what the path implies. \
         Reuses the apply_naming_scheme template DSL in reverse direction. Output is grouped by \
         leaf directory (typically the album folder) so the agent can spot 'Beatles vs The \
         Beatles' inconsistencies without a per-file scan. Capped at 5000 files per call with an \
         opaque cursor for resume; directories are never split across pages.";

    #[instrument(skip_all, fields(root = %params.root, template = %params.path_template))]
    pub fn execute(params: &InventoryDivergencesParams, config: &Config) -> CallToolResult {
        info!(
            "inventory_divergences called: root='{}' template='{}' case_sensitive={} max_files={} cursor={}",
            params.root,
            params.path_template,
            params.case_sensitive,
            params.max_files,
            params.cursor.is_some(),
        );

        let canonical_root = match validate_path(&params.root, config) {
            Ok(p) => p,
            Err(e) => {
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

        let segments = match parse_template(&params.path_template) {
            Ok(s) => s,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Template parse error: {}",
                    e
                ))]);
            }
        };
        let template_slots = match split_template_into_slots(&segments) {
            Ok(s) => s,
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Template not usable for path matching: {}",
                    e
                ))]);
            }
        };
        if template_slots.is_empty() {
            return CallToolResult::error(vec![Content::text(
                "Template has no slots — cannot match any path".to_string(),
            )]);
        }

        // Compute the effective fields_to_compare list.
        let template_field_names = collect_template_fields(&segments);
        let fields_to_compare: Vec<String> = match &params.fields_to_compare {
            Some(list) => list.clone(),
            None => template_field_names
                .iter()
                .filter(|f| tag_mapping_known(f))
                .cloned()
                .collect(),
        };

        let effective_max_files = params.max_files.clamp(1, HARD_CAP_MAX_FILES);

        let (resume_path, prior_scanned, prior_diverging) = match params.cursor.as_deref() {
            None => (None, 0usize, 0usize),
            Some(raw) => match decode_cursor(raw) {
                Ok(c) => (
                    Some(c.last_directory_path),
                    c.files_scanned,
                    c.files_with_divergences,
                ),
                Err(e) => {
                    return CallToolResult::error(vec![Content::text(format!(
                        "Invalid cursor: {}",
                        e
                    ))]);
                }
            },
        };

        let include_hidden = params.include_hidden;
        let walker = WalkDir::new(&canonical_root)
            .min_depth(1)
            .max_depth(HARD_CAP_MAX_DEPTH)
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

        // Streaming accumulator: directories are emitted in batches as the
        // walker leaves them. `pending` collects the current directory's
        // files until we encounter a file under a different parent (or the
        // walk ends), at which point we finalise.
        let mut directories: Vec<DirectoryInventory> = Vec::new();
        let mut pending_dir: Option<PathBuf> = None;
        let mut pending_dir_captures: BTreeMap<String, String> = BTreeMap::new();
        let mut pending_files: Vec<FileEntry> = Vec::new();
        let mut pending_histogram: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut files_scanned: usize = prior_scanned;
        let mut files_with_divergences: usize = prior_diverging;
        let mut truncated = false;
        let mut next_cursor: Option<String> = None;
        let mut last_completed_dir: Option<String> = None;

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

            // Defence-in-depth: re-validate every emitted path against the
            // root + symlink policy.
            if let Err(e) = validate_path(&path.to_string_lossy(), config) {
                warnings.push(format!("Skipped '{}': {}", path.display(), e));
                continue;
            }

            // Try to match against the template. A file whose path doesn't
            // fit the template is just "not in scope" — surface it as a
            // warning rather than aborting the whole scan.
            let relative = match path.strip_prefix(&canonical_root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let relative_str = relative.to_string_lossy().replace('\\', "/");
            let captures = match match_path(&template_slots, &relative_str) {
                Some(c) => c,
                None => {
                    warnings.push(format!("Path doesn't match template: {}", path.display()));
                    continue;
                }
            };

            let parent_dir = match path.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };

            // Cursor skip — we only emit data for directories *strictly
            // greater* than the resume point. The comparison uses the same
            // lex string ordering walkdir's sort produces.
            if let Some(ref resume) = resume_path
                && parent_dir.to_string_lossy().as_ref() <= resume.as_str()
            {
                continue;
            }

            // Directory boundary: finalise the previous one (or check the
            // cap before starting a new one).
            if pending_dir.as_ref() != Some(&parent_dir) {
                if let Some(prev_dir) = pending_dir.take() {
                    finalise_pending(
                        &mut directories,
                        &mut last_completed_dir,
                        prev_dir,
                        std::mem::take(&mut pending_dir_captures),
                        std::mem::take(&mut pending_files),
                        std::mem::take(&mut pending_histogram),
                    );
                }

                // We're about to start a new directory. If the cap is
                // already reached AND we have at least one finished
                // directory to return, stop here and point the cursor at
                // the last one we completed. Without "at least one"
                // directories, we'd be unable to make progress on a tree
                // whose first directory is itself larger than `max_files`.
                if files_scanned - prior_scanned >= effective_max_files && !directories.is_empty() {
                    truncated = true;
                    break;
                }
                pending_dir = Some(parent_dir.clone());
                pending_dir_captures = directory_level_captures(&captures, &template_slots);
            }

            // Read tags for the comparison fields. A read failure isn't
            // fatal — we keep the file in the directory with empty tags
            // and a warning.
            let tags = match read_tags_for_fields(path, &fields_to_compare) {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("Tag read failed for '{}': {}", path.display(), e));
                    fields_to_compare
                        .iter()
                        .map(|f| (f.clone(), None))
                        .collect()
                }
            };

            let file_inferred = file_level_captures(&captures, &template_slots);
            let divergences =
                compute_divergences(&captures, &tags, &fields_to_compare, params.case_sensitive);

            // Histogram update: every non-empty tag value contributes one
            // count to its field's histogram.
            for (field, value) in &tags {
                if let Some(v) = value.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    *pending_histogram
                        .entry(field.clone())
                        .or_default()
                        .entry(v.to_string())
                        .or_default() += 1;
                }
            }

            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !divergences.is_empty() {
                files_with_divergences += 1;
            }
            pending_files.push(FileEntry {
                name,
                path_inferred: file_inferred,
                tags,
                divergences,
            });
            files_scanned += 1;
        }

        // Tail: finalise the last in-flight directory if we didn't bail
        // early. When we bailed because of the cap we deliberately *don't*
        // finalise it — it goes into the next page so we don't emit a
        // partial histogram.
        if !truncated && let Some(prev_dir) = pending_dir.take() {
            finalise_pending(
                &mut directories,
                &mut last_completed_dir,
                prev_dir,
                pending_dir_captures,
                pending_files,
                pending_histogram,
            );
        }

        if truncated && let Some(ref last) = last_completed_dir {
            next_cursor = Some(encode_cursor(&InventoryCursor {
                last_directory_path: last.clone(),
                files_scanned,
                files_with_divergences,
            }));
        }

        let summary = if directories.is_empty() {
            format!(
                "No directories matched template '{}' under '{}'",
                params.path_template, params.root
            )
        } else if truncated {
            format!(
                "Surveyed {} directories ({} files; {} with divergences); truncated, use next_cursor",
                directories.len(),
                files_scanned,
                files_with_divergences
            )
        } else {
            format!(
                "Surveyed {} directories ({} files; {} with divergences); scan complete",
                directories.len(),
                files_scanned,
                files_with_divergences
            )
        };

        let payload = InventoryDivergencesResult {
            directories,
            files_scanned,
            files_with_divergences,
            next_cursor,
            truncated,
            warnings,
        };
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: InventoryDivergencesParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<InventoryDivergencesParams>(),
        )
        .with_raw_output_schema(schema_for_type::<InventoryDivergencesResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: InventoryDivergencesParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// Template → slot splitter + reverse matcher
// ============================================================================

/// One `/`-delimited slot from the template. Its segments are always either
/// `Literal` (without `/`) or `Placeholder`.
type TemplateSlot = Vec<Segment>;

/// Split a parsed template into `/`-separated slots. Returns an error if any
/// slot contains two adjacent placeholders with no literal between them —
/// that pattern is unambiguous in the forward direction but undecidable in
/// reverse (`{a}{b}` against `"xy"` could split at every offset).
fn split_template_into_slots(segments: &[Segment]) -> Result<Vec<TemplateSlot>, String> {
    let mut slots: Vec<TemplateSlot> = Vec::new();
    let mut current: TemplateSlot = Vec::new();

    for seg in segments {
        match seg {
            Segment::Literal(s) => {
                let mut buf = String::new();
                for ch in s.chars() {
                    if ch == '/' {
                        if !buf.is_empty() {
                            current.push(Segment::Literal(std::mem::take(&mut buf)));
                        }
                        slots.push(std::mem::take(&mut current));
                    } else {
                        buf.push(ch);
                    }
                }
                if !buf.is_empty() {
                    current.push(Segment::Literal(buf));
                }
            }
            Segment::Placeholder {
                name,
                fallback,
                format,
            } => {
                if matches!(current.last(), Some(Segment::Placeholder { .. })) {
                    return Err(format!(
                        "Adjacent placeholders without a literal between them ('{}' follows another placeholder) — cannot match deterministically in reverse",
                        name
                    ));
                }
                current.push(Segment::Placeholder {
                    name: name.clone(),
                    fallback: fallback.clone(),
                    format: format.as_ref().map(clone_format),
                });
            }
        }
    }
    // Flush the trailing slot (the loop only flushes on a `/`).
    slots.push(current);

    // Drop empty leading/trailing slots so templates starting or ending
    // with `/` don't require a phantom empty path segment.
    while slots.first().is_some_and(Vec::is_empty) {
        slots.remove(0);
    }
    while slots.last().is_some_and(Vec::is_empty) {
        slots.pop();
    }
    Ok(slots)
}

fn clone_format(
    f: &crate::domains::tools::definitions::naming::apply_scheme::FormatSpec,
) -> crate::domains::tools::definitions::naming::apply_scheme::FormatSpec {
    use crate::domains::tools::definitions::naming::apply_scheme::FormatSpec;
    match f {
        FormatSpec::ZeroPadInt(w) => FormatSpec::ZeroPadInt(*w),
    }
}

/// Match a relative path string (with `/` separators) against the template
/// slots. Returns the captured name → value map on success, `None` on any
/// mismatch.
fn match_path(slots: &[TemplateSlot], rel_path: &str) -> Option<HashMap<String, String>> {
    let components: Vec<&str> = rel_path.split('/').filter(|c| !c.is_empty()).collect();
    if components.len() != slots.len() {
        return None;
    }
    let mut captures: HashMap<String, String> = HashMap::new();
    for (slot, comp) in slots.iter().zip(components.iter()) {
        let slot_captures = match_slot(slot, comp)?;
        for (k, v) in slot_captures {
            captures.insert(k, v);
        }
    }
    Some(captures)
}

/// Match one `/`-free component against one slot's segments.
///
/// Walks **left to right** with `find` for every literal, except the
/// "critical" literal — the one immediately preceding a *trailing*
/// placeholder — where it uses `rfind` instead. That single asymmetry is
/// what gives the extension-detection behaviour for `{title}.{ext}` on
/// `Mr. Brightside.mp3` (binds the rightmost `.`) while keeping `find`
/// elsewhere so multi-capture slots like `{disc}-{track} {title}.{ext}`
/// match the leftmost separator for `disc`/`track`.
///
/// When the slot ends with a literal instead of a placeholder, no critical
/// literal exists — the trailing literal pins the match itself and `find`
/// suffices everywhere.
fn match_slot(slot: &TemplateSlot, component: &str) -> Option<Vec<(String, String)>> {
    if slot.is_empty() {
        return if component.is_empty() {
            Some(Vec::new())
        } else {
            None
        };
    }

    // Identify the critical literal index: only set when the slot ENDS
    // with a placeholder AND there's a literal immediately before it.
    let critical_idx = match slot.last() {
        Some(Segment::Placeholder { .. }) if slot.len() >= 2 => Some(slot.len() - 2),
        _ => None,
    };

    let mut captures: Vec<(String, String)> = Vec::new();
    let mut cursor: usize = 0;
    let mut pending: Option<&str> = None;

    for (idx, seg) in slot.iter().enumerate() {
        match seg {
            Segment::Placeholder { name, .. } => {
                if pending.is_some() {
                    // Adjacent placeholders are rejected at slot-split
                    // time. Defence-in-depth.
                    return None;
                }
                pending = Some(name.as_str());
            }
            Segment::Literal(s) => {
                let tail = component.get(cursor..)?;
                let rel_pos = if Some(idx) == critical_idx {
                    tail.rfind(s.as_str())?
                } else {
                    tail.find(s.as_str())?
                };
                let pos = cursor + rel_pos;
                if let Some(name) = pending.take() {
                    let captured = &component[cursor..pos];
                    if captured.is_empty() {
                        return None;
                    }
                    captures.push((name.to_string(), captured.to_string()));
                } else if pos != cursor {
                    // Literal didn't sit flush against the cursor; leading
                    // bytes have no segment to claim them.
                    return None;
                }
                cursor = pos + s.len();
            }
        }
    }

    if let Some(name) = pending {
        let captured = &component[cursor..];
        if captured.is_empty() {
            return None;
        }
        captures.push((name.to_string(), captured.to_string()));
    } else if cursor != component.len() {
        // No trailing placeholder, but bytes remain past the last literal.
        return None;
    }

    Some(captures)
}

/// Names of every placeholder in the template, in declaration order. Used
/// to default `fields_to_compare` when the caller doesn't provide one.
fn collect_template_fields(segments: &[Segment]) -> Vec<String> {
    let mut names = Vec::new();
    for seg in segments {
        if let Segment::Placeholder { name, .. } = seg {
            names.push(name.clone());
        }
    }
    names
}

/// Which captures live above the file's slot (everything except the last
/// `/`-separated slot). These are the "directory-level" captures — the
/// fields the agent uses to identify the directory itself.
fn directory_level_captures(
    captures: &HashMap<String, String>,
    slots: &[TemplateSlot],
) -> BTreeMap<String, String> {
    if slots.len() < 2 {
        return BTreeMap::new();
    }
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for slot in &slots[..slots.len() - 1] {
        for seg in slot {
            if let Segment::Placeholder { name, .. } = seg {
                names.insert(name.clone());
            }
        }
    }
    captures
        .iter()
        .filter(|(k, _)| names.contains(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Captures from the file's slot — typically title + ext (+ disc/track when
/// the template carries them).
fn file_level_captures(
    captures: &HashMap<String, String>,
    slots: &[TemplateSlot],
) -> BTreeMap<String, String> {
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(last) = slots.last() {
        for seg in last {
            if let Segment::Placeholder { name, .. } = seg {
                names.insert(name.clone());
            }
        }
    }
    captures
        .iter()
        .filter(|(k, _)| names.contains(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// ============================================================================
// Tag reading + divergence
// ============================================================================

/// Whether a field name has a known mapping in `read_tags_for_fields`. Used
/// to filter the default `fields_to_compare` so we never include path-only
/// fields (like `ext`) that can't have a tag-side value.
fn tag_mapping_known(field: &str) -> bool {
    matches!(
        field,
        "title"
            | "artist"
            | "album"
            | "album_artist"
            | "genre"
            | "year"
            | "track"
            | "disc"
            | "comment"
    )
}

fn read_tags_for_fields(
    path: &Path,
    fields: &[String],
) -> Result<BTreeMap<String, Option<String>>, String> {
    let tagged = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let tag = tagged.primary_tag();
    let mut out = BTreeMap::new();
    for field in fields {
        let value = tag.and_then(|t| extract_field_from_tag(t, field));
        out.insert(field.clone(), value);
    }
    Ok(out)
}

fn extract_field_from_tag(tag: &lofty::tag::Tag, field: &str) -> Option<String> {
    match field {
        "title" => tag.title().map(|s| s.to_string()),
        "artist" => tag.artist().map(|s| s.to_string()),
        "album" => tag.album().map(|s| s.to_string()),
        "album_artist" => tag
            .get_string(lofty::tag::ItemKey::AlbumArtist)
            .map(|s| s.to_string()),
        "genre" => tag.genre().map(|s| s.to_string()),
        "year" => tag.date().map(|d| u32::from(d.year).to_string()),
        "track" => tag.track().map(|t| t.to_string()),
        "disc" => tag.disk().map(|d| d.to_string()),
        "comment" => tag.comment().map(|s| s.to_string()),
        _ => None,
    }
}

fn compute_divergences(
    captures: &HashMap<String, String>,
    tags: &BTreeMap<String, Option<String>>,
    fields_to_compare: &[String],
    case_sensitive: bool,
) -> Vec<String> {
    let mut diff: Vec<String> = Vec::new();
    for field in fields_to_compare {
        let inferred = match captures.get(field) {
            Some(v) => v.trim(),
            None => continue,
        };
        let tag_val = match tags.get(field).and_then(|o| o.as_ref()) {
            Some(v) => v.trim(),
            None => continue,
        };
        if inferred.is_empty() || tag_val.is_empty() {
            continue;
        }
        if is_divergent(inferred, tag_val, case_sensitive) {
            diff.push(field.clone());
        }
    }
    diff.sort();
    diff
}

fn is_divergent(inferred: &str, tag_val: &str, case_sensitive: bool) -> bool {
    // Numeric path: `"01"` vs `"1"` should NOT be flagged when both parse
    // to the same integer, since the difference is purely formatting that
    // the template's `:0Nd` already accounts for in the forward direction.
    if let (Ok(a), Ok(b)) = (inferred.parse::<i64>(), tag_val.parse::<i64>()) {
        return a != b;
    }
    if case_sensitive {
        inferred != tag_val
    } else {
        !inferred.eq_ignore_ascii_case(tag_val)
    }
}

// ============================================================================
// Pending-directory finalisation
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn finalise_pending(
    directories: &mut Vec<DirectoryInventory>,
    last_completed_dir: &mut Option<String>,
    dir_path: PathBuf,
    captures: BTreeMap<String, String>,
    files: Vec<FileEntry>,
    histogram: BTreeMap<String, BTreeMap<String, usize>>,
) {
    let path_str = dir_path.display().to_string();
    directories.push(DirectoryInventory {
        path: path_str.clone(),
        path_inferred: captures,
        field_value_counts: histogram,
        files,
    });
    *last_completed_dir = Some(path_str);
}

// ============================================================================
// Cursor helpers
// ============================================================================

fn encode_cursor(cursor: &InventoryCursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_default();
    STANDARD.encode(json)
}

fn decode_cursor(raw: &str) -> Result<InventoryCursor, String> {
    let bytes = STANDARD
        .decode(raw.as_bytes())
        .map_err(|e| format!("base64 decode failed: {}", e))?;
    serde_json::from_slice::<InventoryCursor>(&bytes)
        .map_err(|e| format!("cursor payload not valid JSON: {}", e))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(template: &str) -> Vec<TemplateSlot> {
        let segs = parse_template(template).unwrap();
        split_template_into_slots(&segs).unwrap()
    }

    #[test]
    fn slot_match_extracts_single_placeholder() {
        let slots = parse("{album}");
        let caps = match_slot(&slots[0], "Abbey Road").unwrap();
        assert_eq!(caps, vec![("album".to_string(), "Abbey Road".to_string())]);
    }

    #[test]
    fn slot_match_two_placeholders_with_literal_between() {
        // {title}.{ext} against a normal filename.
        let slots = parse("{title}.{ext}");
        let caps = match_slot(&slots[0], "Hells Bells.mp3").unwrap();
        let map: HashMap<_, _> = caps.into_iter().collect();
        assert_eq!(map.get("title").map(String::as_str), Some("Hells Bells"));
        assert_eq!(map.get("ext").map(String::as_str), Some("mp3"));
    }

    #[test]
    fn slot_match_right_to_left_handles_dot_in_title() {
        // The whole point of the right-to-left algorithm: a literal dot in
        // the title must not steal the extension.
        let slots = parse("{title}.{ext}");
        let caps = match_slot(&slots[0], "Mr. Brightside.mp3").unwrap();
        let map: HashMap<_, _> = caps.into_iter().collect();
        assert_eq!(map.get("title").map(String::as_str), Some("Mr. Brightside"));
        assert_eq!(map.get("ext").map(String::as_str), Some("mp3"));
    }

    #[test]
    fn slot_match_three_captures_with_literal_separators() {
        let slots = parse("{disc}-{track} {title}.{ext}");
        let caps = match_slot(&slots[0], "01-05 Hells Bells.mp3").unwrap();
        let map: HashMap<_, _> = caps.into_iter().collect();
        assert_eq!(map.get("disc").map(String::as_str), Some("01"));
        assert_eq!(map.get("track").map(String::as_str), Some("05"));
        assert_eq!(map.get("title").map(String::as_str), Some("Hells Bells"));
        assert_eq!(map.get("ext").map(String::as_str), Some("mp3"));
    }

    #[test]
    fn slot_match_returns_none_on_missing_literal() {
        // Template expects a dot, file has none.
        let slots = parse("{title}.{ext}");
        assert!(match_slot(&slots[0], "no_dot_here").is_none());
    }

    #[test]
    fn slot_match_returns_none_on_empty_capture() {
        // `.mp3` would force title to be empty — refused.
        let slots = parse("{title}.{ext}");
        assert!(match_slot(&slots[0], ".mp3").is_none());
    }

    #[test]
    fn adjacent_placeholders_are_refused() {
        let segs = parse_template("{a}{b}").unwrap();
        assert!(split_template_into_slots(&segs).is_err());
    }

    #[test]
    fn path_match_distributes_captures_across_slots() {
        let slots = parse("{genre}/{artist}/{album}/{title}.{ext}");
        let caps = match_path(&slots, "Rock/The Beatles/Abbey Road/01 Come Together.mp3").unwrap();
        assert_eq!(caps.get("genre").map(String::as_str), Some("Rock"));
        assert_eq!(caps.get("artist").map(String::as_str), Some("The Beatles"));
        assert_eq!(caps.get("album").map(String::as_str), Some("Abbey Road"));
        assert_eq!(
            caps.get("title").map(String::as_str),
            Some("01 Come Together")
        );
        assert_eq!(caps.get("ext").map(String::as_str), Some("mp3"));
    }

    #[test]
    fn path_match_rejects_wrong_component_count() {
        // Template has 4 slots; path has 5 components.
        let slots = parse("{genre}/{artist}/{album}/{title}.{ext}");
        assert!(
            match_path(
                &slots,
                "Rock/The Beatles/Abbey Road/Side A/01 Come Together.mp3"
            )
            .is_none()
        );
    }

    #[test]
    fn directory_level_captures_drops_file_level() {
        let slots = parse("{genre}/{artist}/{album}/{title}.{ext}");
        let mut caps = HashMap::new();
        caps.insert("genre".to_string(), "Rock".to_string());
        caps.insert("artist".to_string(), "Beatles".to_string());
        caps.insert("album".to_string(), "Abbey Road".to_string());
        caps.insert("title".to_string(), "Come Together".to_string());
        caps.insert("ext".to_string(), "mp3".to_string());
        let dir = directory_level_captures(&caps, &slots);
        assert!(dir.contains_key("genre"));
        assert!(dir.contains_key("artist"));
        assert!(dir.contains_key("album"));
        assert!(!dir.contains_key("title"));
        assert!(!dir.contains_key("ext"));
    }

    #[test]
    fn divergent_strings_detected_case_insensitive_by_default() {
        assert!(is_divergent("The Beatles", "Beatles", false));
        // Same string with different casing → not divergent.
        assert!(!is_divergent("The Beatles", "the beatles", false));
        // Case-sensitive flag flips it.
        assert!(is_divergent("The Beatles", "the beatles", true));
    }

    #[test]
    fn divergent_handles_numeric_padding() {
        // Path says "01", tag says "1" — same integer, no divergence.
        assert!(!is_divergent("01", "1", false));
        // Different integers → divergence.
        assert!(is_divergent("01", "02", false));
        // One numeric, one not → fall back to string comparison.
        assert!(is_divergent("01", "first", false));
    }

    #[test]
    fn divergent_trims_whitespace_implicitly() {
        // Caller always passes trimmed strings (compute_divergences does),
        // but verify the predicate is robust to either side coming in
        // pre-trimmed.
        assert!(!is_divergent("Beatles", "Beatles", false));
    }

    #[test]
    fn compute_divergences_lists_only_diverging_fields() {
        let mut caps = HashMap::new();
        caps.insert("artist".to_string(), "Beatles".to_string());
        caps.insert("album".to_string(), "Abbey Road".to_string());
        caps.insert("genre".to_string(), "Rock".to_string());

        let mut tags = BTreeMap::new();
        tags.insert("artist".to_string(), Some("The Beatles".to_string()));
        tags.insert("album".to_string(), Some("Abbey Road".to_string()));
        tags.insert("genre".to_string(), Some("Pop Rock".to_string()));

        let fields = vec![
            "artist".to_string(),
            "album".to_string(),
            "genre".to_string(),
        ];
        let div = compute_divergences(&caps, &tags, &fields, false);
        assert_eq!(div, vec!["artist".to_string(), "genre".to_string()]);
    }

    #[test]
    fn collect_template_fields_returns_declaration_order() {
        let segs = parse_template("{genre}/{artist}/{album}/{title}.{ext}").unwrap();
        let fields = collect_template_fields(&segs);
        assert_eq!(fields, vec!["genre", "artist", "album", "title", "ext"]);
    }

    #[test]
    fn tag_mapping_filters_path_only_fields() {
        assert!(tag_mapping_known("artist"));
        assert!(tag_mapping_known("title"));
        assert!(!tag_mapping_known("ext"));
        assert!(!tag_mapping_known("genre_alias"));
    }

    #[test]
    fn cursor_roundtrip() {
        let cursor = InventoryCursor {
            last_directory_path: "/library/Rock/Beatles/Abbey Road".to_string(),
            files_scanned: 123,
            files_with_divergences: 4,
        };
        let encoded = encode_cursor(&cursor);
        let decoded = decode_cursor(&encoded).unwrap();
        assert_eq!(decoded.last_directory_path, cursor.last_directory_path);
        assert_eq!(decoded.files_scanned, cursor.files_scanned);
        assert_eq!(
            decoded.files_with_divergences,
            cursor.files_with_divergences
        );
    }
}
