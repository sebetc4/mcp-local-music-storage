//! Move tool definition.
//!
//! Moves a file or directory from one path to another, optionally creating
//! intermediate parent directories. Same-filesystem moves go through
//! `std::fs::rename` (atomic). Cross-filesystem moves fall back to a
//! recursive copy followed by a remove of the source. Where `fs_rename`
//! stays narrow (same-directory renames), `fs_move` is the explicit
//! "traverse the tree" tool.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::{validate_path, validate_unborn_path};

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the move tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FsMoveParams {
    /// Source path. Must exist and resolve inside the configured root.
    pub from: String,

    /// Destination path. May not yet exist; its parents will be created if
    /// `mkdir_parents=true`.
    pub to: String,

    /// When `true`, create every missing parent of the destination
    /// (`mkdir -p`) before the move.
    #[serde(default)]
    pub mkdir_parents: bool,

    /// Replace an existing destination. When `false` (default) and the
    /// destination already exists, the call refuses.
    #[serde(default)]
    pub overwrite: bool,

    /// When `true`, validate the request and report the planned actions
    /// without touching the filesystem.
    #[serde(default)]
    pub dry_run: bool,
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of a move operation.
#[derive(Debug, Serialize, JsonSchema)]
struct MoveResult {
    /// Source path that was moved.
    from: String,
    /// Validated destination path.
    to: String,
    /// `"file"` or `"directory"`.
    item_type: String,
    /// Parent directories that were created by `mkdir_parents`. Empty when
    /// the parent already existed or `mkdir_parents=false`.
    created_parents: Vec<String>,
    /// `"rename"` for the in-process atomic rename, `"copy_then_delete"`
    /// when the move crossed filesystems.
    strategy: String,
    /// Whether an existing destination was replaced.
    overwritten: bool,
    /// `true` when `dry_run=true` — no filesystem mutation occurred.
    dry_run: bool,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Move tool — moves a file or directory across directories under the root.
pub struct FsMoveTool;

impl FsMoveTool {
    pub const NAME: &'static str = "fs_move";

    pub const DESCRIPTION: &'static str = "Move a file or directory across directories under the configured root. \
         With mkdir_parents=true, missing parent directories of the destination are created first (mkdir -p). \
         Atomic rename on the same filesystem; falls back to recursive copy + delete on cross-filesystem moves. \
         Use fs_rename for in-place same-directory renames; fs_move is the explicit 'traverse the tree' variant.";

    #[instrument(skip_all, fields(from = %params.from, to = %params.to))]
    pub fn execute(params: &FsMoveParams, config: &Config) -> CallToolResult {
        info!(
            "Move tool called: '{}' -> '{}' (mkdir_parents={}, overwrite={}, dry_run={})",
            params.from, params.to, params.mkdir_parents, params.overwrite, params.dry_run
        );

        // 1. Source must exist and live under the root.
        let from = match validate_path(&params.from, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Source path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Source path security validation failed: {}",
                    e
                ))]);
            }
        };

        let item_type = if from.is_dir() {
            "directory"
        } else if from.is_file() {
            "file"
        } else {
            "item"
        };

        // 2. Destination: may not yet exist. `validate_unborn_path` walks up
        // to the deepest existing ancestor and validates it against root,
        // returning the would-be path under the canonical ancestor.
        let to = match validate_unborn_path(&params.to, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Destination path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Destination path security validation failed: {}",
                    e
                ))]);
            }
        };

        // 3. Loop check: refuse a move whose destination lives inside the
        // source tree (would relocate a directory into itself).
        if to == from || to.starts_with(&from) {
            warn!(
                "Refusing move: destination '{}' lies inside source '{}'",
                to.display(),
                from.display()
            );
            return CallToolResult::error(vec![Content::text(format!(
                "Destination '{}' lies inside source '{}'; would create a cycle",
                to.display(),
                from.display()
            ))]);
        }

        // 4. Overwrite gate.
        let dest_exists = to.exists();
        if dest_exists && !params.overwrite {
            warn!("Destination already exists: {}", to.display());
            return CallToolResult::error(vec![Content::text(format!(
                "Destination already exists: '{}'. Use overwrite=true to replace it.",
                to.display()
            ))]);
        }

        // 5. Plan parent directories to create (if any).
        let parent_for_dest = match to.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                warn!(
                    "Destination has no parent directory: {}",
                    to.display()
                );
                return CallToolResult::error(vec![Content::text(format!(
                    "Destination has no parent directory: {}",
                    to.display()
                ))]);
            }
        };
        let mut planned_parents: Vec<PathBuf> = Vec::new();
        if !parent_for_dest.exists() {
            if !params.mkdir_parents {
                warn!(
                    "Parent directory does not exist: {} (set mkdir_parents=true)",
                    parent_for_dest.display()
                );
                return CallToolResult::error(vec![Content::text(format!(
                    "Parent directory does not exist: '{}'. Set mkdir_parents=true to create it.",
                    parent_for_dest.display()
                ))]);
            }
            // Walk up to collect every missing ancestor for the report.
            let mut cursor = parent_for_dest.as_path();
            while !cursor.exists() {
                planned_parents.push(cursor.to_path_buf());
                match cursor.parent() {
                    Some(p) => cursor = p,
                    None => break,
                }
            }
            planned_parents.reverse();
        }

        // 6. Dry-run short-circuits before any side effect.
        if params.dry_run {
            let result = MoveResult {
                from: from.display().to_string(),
                to: to.display().to_string(),
                item_type: item_type.to_string(),
                created_parents: planned_parents
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect(),
                strategy: "rename".to_string(),
                overwritten: dest_exists,
                dry_run: true,
            };
            let summary = format!(
                "Dry-run: would move {} '{}' -> '{}'",
                item_type,
                from.display(),
                to.display()
            );
            return structured_ok(summary, result);
        }

        // 7. Create missing parents.
        if !planned_parents.is_empty()
            && let Err(e) = fs::create_dir_all(&parent_for_dest)
        {
            warn!(
                "Failed to create parent '{}': {}",
                parent_for_dest.display(),
                e
            );
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to create parent directory '{}': {}",
                parent_for_dest.display(),
                e
            ))]);
        }

        // 8. Try same-filesystem atomic rename; on cross-fs, fall back to
        //    recursive copy + delete.
        let strategy = match fs::rename(&from, &to) {
            Ok(_) => "rename",
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                match move_across_filesystems(&from, &to) {
                    Ok(_) => "copy_then_delete",
                    Err(err) => {
                        warn!(
                            "Cross-filesystem move failed: '{}' -> '{}': {}",
                            from.display(),
                            to.display(),
                            err
                        );
                        return CallToolResult::error(vec![Content::text(format!(
                            "Cross-filesystem move failed: {}",
                            err
                        ))]);
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to move '{}' -> '{}': {}",
                    from.display(),
                    to.display(),
                    e
                );
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to move '{}' -> '{}': {}",
                    from.display(),
                    to.display(),
                    e
                ))]);
            }
        };

        let result = MoveResult {
            from: from.display().to_string(),
            to: to.display().to_string(),
            item_type: item_type.to_string(),
            created_parents: planned_parents
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            strategy: strategy.to_string(),
            overwritten: dest_exists,
            dry_run: false,
        };
        let summary = format!(
            "Moved {} '{}' -> '{}' (strategy: {})",
            item_type,
            from.display(),
            to.display(),
            strategy
        );
        info!("{}", summary);
        structured_ok(summary, result)
    }

    /// HTTP handler for this tool.
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: FsMoveParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;

        let result = Self::execute(&params, &config);

        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    pub fn to_tool() -> Tool {
        Tool::new(Self::NAME, Self::DESCRIPTION, schema_for_type::<FsMoveParams>())
            .with_raw_output_schema(schema_for_type::<MoveResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: FsMoveParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

/// Recursive copy of file or directory followed by removal of the source.
/// Used as the cross-filesystem fallback when `rename(2)` returns `EXDEV`.
fn move_across_filesystems(from: &Path, to: &Path) -> io::Result<()> {
    if from.is_dir() {
        copy_dir_recursive(from, to)?;
        fs::remove_dir_all(from)
    } else {
        fs::copy(from, to)?;
        fs::remove_file(from)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path)?;
        } else {
            // Symlinks and other entry types are deliberately skipped: the
            // server's symlink policy is owned by `validate_path` on the
            // input side, so we refuse rather than silently dereference here.
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Refusing to copy non-regular file entry: {}",
                    src_path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Local alias delegating to the shared helper (rmcp 1.7 made `CallToolResult`
/// non-exhaustive, so the inline struct literal is no longer possible).
fn structured_ok<T: Serialize>(summary: String, data: T) -> CallToolResult {
    crate::domains::tools::result::structured_ok(summary, &data)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::SecurityConfig;
    use tempfile::TempDir;

    fn config_rooted_at(root: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.security = SecurityConfig {
            root_path: Some(root.to_path_buf()),
            allow_symlinks: true,
        };
        cfg
    }

    #[test]
    fn moves_file_within_root_same_dir() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("a.txt");
        let dst = root.path().join("b.txt");
        fs::write(&src, b"content").unwrap();

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: false,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"content");

        let s = r.structured_content.unwrap();
        assert_eq!(s["strategy"], "rename");
        assert_eq!(s["item_type"], "file");
        assert_eq!(s["overwritten"], false);
        assert!(s["created_parents"].as_array().unwrap().is_empty());
    }

    #[test]
    fn moves_file_into_new_directory_with_mkdir_parents() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("inbox").join("track.mp3");
        fs::create_dir_all(src.parent().unwrap()).unwrap();
        fs::write(&src, b"audio bytes").unwrap();

        // Destination's parents A/, A/B/ don't exist yet.
        let dst = root.path().join("A").join("B").join("track.mp3");

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: true,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(
            !r.is_error.unwrap_or(false),
            "expected success, got error result"
        );
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"audio bytes");
        assert!(root.path().join("A").is_dir());
        assert!(root.path().join("A/B").is_dir());

        let s = r.structured_content.unwrap();
        assert_eq!(s["created_parents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn refuses_move_when_parent_missing_and_no_mkdir() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("a.txt");
        fs::write(&src, b"x").unwrap();
        let dst = root.path().join("missing_dir").join("a.txt");

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: false,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
        // Source still intact, no parents created.
        assert!(src.exists());
        assert!(!root.path().join("missing_dir").exists());
    }

    #[test]
    fn refuses_destination_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("a.txt");
        fs::write(&src, b"x").unwrap();
        let dst = outside.path().join("escape.txt");

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: true,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
        assert!(src.exists());
        assert!(!dst.exists());
    }

    #[test]
    fn refuses_destination_inside_source() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src_dir = root.path().join("Artist");
        fs::create_dir(&src_dir).unwrap();
        // Trying to move /root/Artist into /root/Artist/sub would create a cycle.
        let dst = src_dir.join("sub");

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src_dir.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: true,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
        assert!(src_dir.is_dir());
    }

    #[test]
    fn refuses_existing_destination_without_overwrite() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("a.txt");
        let dst = root.path().join("b.txt");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: false,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
        // Both files survive untouched.
        assert_eq!(fs::read(&src).unwrap(), b"new");
        assert_eq!(fs::read(&dst).unwrap(), b"old");
    }

    #[test]
    fn overwrite_replaces_destination() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("a.txt");
        let dst = root.path().join("b.txt");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: false,
                overwrite: true,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"new");

        let s = r.structured_content.unwrap();
        assert_eq!(s["overwritten"], true);
    }

    #[test]
    fn dry_run_reports_plan_without_side_effects() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src = root.path().join("a.txt");
        fs::write(&src, b"x").unwrap();
        let dst = root.path().join("A").join("B").join("a.txt");

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src.to_string_lossy().into_owned(),
                to: dst.to_string_lossy().into_owned(),
                mkdir_parents: true,
                overwrite: false,
                dry_run: true,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        // Nothing actually happened.
        assert!(src.exists());
        assert!(!root.path().join("A").exists());
        let s = r.structured_content.unwrap();
        assert_eq!(s["dry_run"], true);
        assert_eq!(s["created_parents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn moves_directory_with_children() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());
        let src_dir = root.path().join("inbox").join("Album");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("01.mp3"), b"a").unwrap();
        fs::write(src_dir.join("02.mp3"), b"b").unwrap();

        let dst_dir = root.path().join("library").join("Album");

        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: src_dir.to_string_lossy().into_owned(),
                to: dst_dir.to_string_lossy().into_owned(),
                mkdir_parents: true,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        assert!(!src_dir.exists());
        assert_eq!(fs::read(dst_dir.join("01.mp3")).unwrap(), b"a");
        assert_eq!(fs::read(dst_dir.join("02.mp3")).unwrap(), b"b");

        let s = r.structured_content.unwrap();
        assert_eq!(s["item_type"], "directory");
    }
}
