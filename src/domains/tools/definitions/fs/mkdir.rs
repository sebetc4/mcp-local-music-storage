//! Mkdir tool definition.
//!
//! Creates directories under the configured root. Mirrors `mkdir -p` when
//! `recursive=true`, otherwise behaves like plain `mkdir` (the immediate
//! parent must already exist).

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::validate_unborn_path;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the mkdir tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FsMkdirParams {
    /// Absolute path of the directory to create.
    pub path: String,

    /// When `true` (default), create every missing parent (`mkdir -p`).
    /// When `false`, only the leaf is created and the immediate parent must
    /// already exist.
    #[serde(default = "default_recursive")]
    pub recursive: bool,

    /// When `true`, validate the request and report the would-be created
    /// directories without touching the filesystem.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_recursive() -> bool {
    true
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of a mkdir operation.
#[derive(Debug, Serialize, JsonSchema)]
struct MkdirResult {
    /// The validated path the operation targets.
    path: String,
    /// Whether `recursive` mode was used.
    recursive: bool,
    /// Whether the directory already existed before this call (no-op).
    already_existed: bool,
    /// Directories created by this call, in creation order. Empty when
    /// `already_existed=true`.
    created: Vec<String>,
    /// `true` when `dry_run=true` — no filesystem mutation occurred.
    dry_run: bool,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Mkdir tool — creates a directory under the configured root.
pub struct FsMkdirTool;

impl FsMkdirTool {
    pub const NAME: &'static str = "fs_mkdir";

    pub const DESCRIPTION: &'static str = "Create a directory under the configured root. \
         With recursive=true (default), creates all missing parents (mkdir -p). \
         Idempotent: succeeds with no-op if the directory already exists. \
         Refuses when the target exists as a file. Supports dry_run.";

    #[instrument(skip_all, fields(path = %params.path))]
    pub fn execute(params: &FsMkdirParams, config: &Config) -> CallToolResult {
        info!(
            "Mkdir tool called: '{}' (recursive={}, dry_run={})",
            params.path, params.recursive, params.dry_run
        );

        let target = match validate_unborn_path(&params.path, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };

        // Idempotent fast path: directory already exists.
        if target.exists() {
            if target.is_dir() {
                let result = MkdirResult {
                    path: target.display().to_string(),
                    recursive: params.recursive,
                    already_existed: true,
                    created: Vec::new(),
                    dry_run: params.dry_run,
                };
                let summary = format!("Directory already exists: '{}'", target.display());
                return structured_ok(summary, result);
            }
            warn!("Target exists but is not a directory: {}", params.path);
            return CallToolResult::error(vec![Content::text(format!(
                "Target exists and is not a directory: {}",
                target.display()
            ))]);
        }

        // Build the ordered list of components that would be created. Walk up
        // from the validated target until we hit an existing ancestor, then
        // reverse so callers see the creation order (top-down).
        let mut planned: Vec<std::path::PathBuf> = Vec::new();
        {
            let mut cursor = target.as_path();
            while !cursor.exists() {
                planned.push(cursor.to_path_buf());
                match cursor.parent() {
                    Some(parent) => cursor = parent,
                    None => break,
                }
            }
        }
        planned.reverse();

        // Non-recursive mode: only the leaf is allowed to be missing. If we'd
        // need to create more than one directory, refuse with a clear error.
        if !params.recursive && planned.len() > 1 {
            warn!(
                "Refusing non-recursive mkdir because the parent does not exist: {}",
                params.path
            );
            return CallToolResult::error(vec![Content::text(format!(
                "Parent directory does not exist for '{}'. Set recursive=true to create intermediate directories.",
                target.display()
            ))]);
        }

        if params.dry_run {
            let result = MkdirResult {
                path: target.display().to_string(),
                recursive: params.recursive,
                already_existed: false,
                created: planned.iter().map(|p| p.display().to_string()).collect(),
                dry_run: true,
            };
            let summary = format!(
                "Dry-run: would create {} directory/directories under '{}'",
                planned.len(),
                target.display()
            );
            return structured_ok(summary, result);
        }

        // Commit. `create_dir_all` is idempotent and short-circuits when each
        // directory already exists, so it stays safe under race with other
        // mkdir callers.
        let outcome = if params.recursive {
            fs::create_dir_all(&target)
        } else {
            fs::create_dir(&target)
        };

        if let Err(e) = outcome {
            warn!("Failed to create directory '{}': {}", target.display(), e);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to create directory '{}': {}",
                target.display(),
                e
            ))]);
        }

        let result = MkdirResult {
            path: target.display().to_string(),
            recursive: params.recursive,
            already_existed: false,
            created: planned.iter().map(|p| p.display().to_string()).collect(),
            dry_run: false,
        };
        let summary = format!(
            "Created {} directory/directories under '{}'",
            planned.len(),
            target.display()
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
        let params: FsMkdirParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;

        let result = Self::execute(&params, &config);

        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<FsMkdirParams>(),
        )
        .with_raw_output_schema(schema_for_type::<MkdirResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: FsMkdirParams = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

/// Local alias preserving the call-site signature; delegates to the shared
/// helper introduced by the rmcp 1.7 migration (struct literals are now
/// blocked by `#[non_exhaustive]`).
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

    fn config_rooted_at(root: &std::path::Path) -> Config {
        let mut cfg = Config::default();
        cfg.security = SecurityConfig {
            root_path: Some(root.to_path_buf()),
            allow_symlinks: true,
        };
        cfg
    }

    #[test]
    fn creates_single_directory() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("album");
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: target.to_string_lossy().into_owned(),
                recursive: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        assert!(target.is_dir());

        let structured = r.structured_content.unwrap();
        assert_eq!(structured["already_existed"], false);
        assert_eq!(structured["dry_run"], false);
        assert_eq!(structured["created"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn creates_full_tree_with_recursive() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("a").join("b").join("c");
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: target.to_string_lossy().into_owned(),
                recursive: true,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        assert!(target.is_dir());
        assert!(root.path().join("a/b").is_dir());

        let structured = r.structured_content.unwrap();
        assert_eq!(structured["created"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn rejects_non_recursive_with_missing_parent() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("a").join("b"); // a/ doesn't exist
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: target.to_string_lossy().into_owned(),
                recursive: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
        // Nothing was created.
        assert!(!root.path().join("a").exists());
    }

    #[test]
    fn idempotent_on_existing_directory() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("existing");
        fs::create_dir(&target).unwrap();
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: target.to_string_lossy().into_owned(),
                recursive: true,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        let structured = r.structured_content.unwrap();
        assert_eq!(structured["already_existed"], true);
        assert!(structured["created"].as_array().unwrap().is_empty());
    }

    #[test]
    fn refuses_when_target_is_a_file() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("file.txt");
        fs::write(&target, b"hi").unwrap();
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: target.to_string_lossy().into_owned(),
                recursive: true,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
        // File untouched.
        assert!(target.is_file());
    }

    #[test]
    fn dry_run_makes_no_changes() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("a").join("b").join("c");
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: target.to_string_lossy().into_owned(),
                recursive: true,
                dry_run: true,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        let structured = r.structured_content.unwrap();
        assert_eq!(structured["dry_run"], true);
        assert_eq!(structured["created"].as_array().unwrap().len(), 3);
        // No directory was actually created.
        assert!(!root.path().join("a").exists());
    }

    #[test]
    fn rejects_path_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: outside.path().join("album").to_string_lossy().into_owned(),
                recursive: true,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn rejects_traversal_via_dotdot() {
        let root = TempDir::new().unwrap();
        let cfg = config_rooted_at(root.path());

        let traversal = root.path().join("foo").join("..").join("..").join("escape");
        let r = FsMkdirTool::execute(
            &FsMkdirParams {
                path: traversal.to_string_lossy().into_owned(),
                recursive: true,
                dry_run: false,
            },
            &cfg,
        );
        assert!(r.is_error.unwrap_or(false));
    }
}
