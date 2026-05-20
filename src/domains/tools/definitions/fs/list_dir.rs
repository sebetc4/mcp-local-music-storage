//! List directory tool definition.
//!
//! A tool that lists files and directories in a given path with optional recursive traversal.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::validate_path;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the list directory tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FSListDirParams {
    /// Path to the directory to list.
    pub path: String,

    /// Include hidden files (starting with '.')
    #[serde(default)]
    pub include_hidden: bool,

    /// Show additional details (size, type, permissions)
    #[serde(default)]
    pub detailed: bool,

    /// Recursion depth: 0 = no recursion (default), positive = levels deep, -1 = unlimited
    #[serde(default)]
    pub recursive_depth: i32,
}

// ============================================================================
// Output Structures (JSON format for AI agents)
// ============================================================================

/// Result of listing a directory
#[derive(Debug, Serialize, JsonSchema)]
struct ListResult {
    /// Path that was listed
    path: String,
    /// List of entries found
    entries: Vec<EntryInfo>,
    /// Total count of directories
    dir_count: usize,
    /// Total count of files
    file_count: usize,
    /// Warnings encountered during traversal
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// Information about a single file/directory entry (hierarchical structure)
#[derive(Debug, Serialize, Clone, JsonSchema)]
struct EntryInfo {
    /// Name of the entry (just the filename, not full path)
    name: String,
    /// Type of entry: "file", "directory", or "symlink"
    #[serde(rename = "type")]
    entry_type: String,
    /// Size in bytes (only for files in detailed mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    /// Child entries (only for directories when recursing)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<EntryInfo>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// List directory tool - lists files and directories in a given path with optional recursion.
pub struct FsListDirTool;

impl FsListDirTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "fs_list_dir";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "List files and directories in a given path. Supports recursive traversal with configurable depth. Returns JSON format optimized for AI agents.";

    /// Safety limits
    const MAX_DEPTH_LIMIT: usize = 10;
    const MAX_ENTRIES_LIMIT: usize = 1000;

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    #[instrument(skip_all, fields(path = %params.path, depth = %params.recursive_depth))]
    pub fn execute(params: &FSListDirParams, config: &Config) -> CallToolResult {
        info!(
            "List directory tool called for path: {} with recursive_depth: {}",
            params.path, params.recursive_depth
        );

        // Validate path security first
        let path = match validate_path(&params.path, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };

        // Validate it's a directory
        if !path.is_dir() {
            warn!("Path is not a directory: {}", params.path);
            return CallToolResult::error(vec![Content::text(format!(
                "Path is not a directory: {}",
                params.path
            ))]);
        }

        // Determine effective max depth
        let max_depth = if params.recursive_depth < 0 {
            // -1 or "full" means unlimited, but we cap it at safety limit
            Self::MAX_DEPTH_LIMIT
        } else {
            params.recursive_depth as usize
        };

        // Traverse directory with hierarchical structure
        let mut warnings = Vec::new();
        let mut visited_inodes = HashSet::new();
        let mut total_count = 0;
        let mut truncated = false;

        let entries = Self::traverse_directory_hierarchical(
            &path,
            0,
            max_depth,
            params.include_hidden,
            params.detailed,
            config,
            &mut warnings,
            &mut visited_inodes,
            &mut total_count,
            &mut truncated,
        );

        // Add truncation warning if needed
        if truncated {
            warnings.push(format!(
                "Results truncated: exceeded maximum of {} entries. Consider reducing recursive_depth.",
                Self::MAX_ENTRIES_LIMIT
            ));
        }

        if params.recursive_depth < 0 && max_depth == Self::MAX_DEPTH_LIMIT {
            warnings.push(format!(
                "Depth limited to {} levels for safety (requested unlimited).",
                Self::MAX_DEPTH_LIMIT
            ));
        }

        // Count directories and files recursively
        let (dir_count, file_count) = Self::count_entries(&entries);

        // Build result
        let result = ListResult {
            path: params.path.clone(),
            entries,
            dir_count,
            file_count,
            warnings,
        };

        info!(
            "Listed {} total entries in {} (recursive_depth: {})",
            result.dir_count + result.file_count,
            params.path,
            params.recursive_depth
        );

        // Create human-readable text summary
        let summary = if result.warnings.is_empty() {
            format!(
                "Found {} directories and {} files in '{}'",
                result.dir_count, result.file_count, params.path
            )
        } else {
            format!(
                "Found {} directories and {} files in '{}' ({} warnings)",
                result.dir_count,
                result.file_count,
                params.path,
                result.warnings.len()
            )
        };

        // Return with text summary + structured content via the shared helper.
        crate::domains::tools::result::structured_ok(summary, &result)
    }

    /// Recursively traverse a directory and build hierarchical structure
    #[allow(clippy::too_many_arguments)]
    fn traverse_directory_hierarchical(
        current: &Path,
        current_depth: usize,
        max_depth: usize,
        include_hidden: bool,
        detailed: bool,
        config: &Config,
        warnings: &mut Vec<String>,
        visited_inodes: &mut HashSet<u64>,
        total_count: &mut usize,
        truncated: &mut bool,
    ) -> Vec<EntryInfo> {
        // Check if we've hit the entry limit
        if *total_count >= Self::MAX_ENTRIES_LIMIT {
            *truncated = true;
            return Vec::new();
        }

        // Check if we've exceeded max depth
        if current_depth > max_depth {
            return Vec::new();
        }

        // Read directory entries
        let dir_entries = match fs::read_dir(current) {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to read directory {:?}: {}", current, e);
                warnings.push(format!(
                    "Could not read directory '{}': {}",
                    current.display(),
                    e
                ));
                return Vec::new();
            }
        };

        // Collect and sort entries
        let mut sorted_entries: Vec<_> = dir_entries
            .filter_map(|entry_result| entry_result.ok())
            .collect();
        sorted_entries.sort_by_key(|e| e.file_name());

        let mut results = Vec::new();

        for entry in sorted_entries {
            // Check entry limit again
            if *total_count >= Self::MAX_ENTRIES_LIMIT {
                *truncated = true;
                return results;
            }

            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().to_string();

            // Skip hidden files if not requested
            if !include_hidden && name.starts_with('.') {
                continue;
            }

            let entry_path = entry.path();

            // Validate path security for each entry
            if let Err(e) = validate_path(&entry_path.to_string_lossy(), config) {
                warn!("Path validation failed for {:?}: {}", entry_path, e);
                warnings.push(format!(
                    "Skipped '{}': security validation failed",
                    entry_path.display()
                ));
                continue;
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to get metadata for {:?}: {}", entry_path, e);
                    warnings.push(format!(
                        "Could not read metadata for '{}': {}",
                        entry_path.display(),
                        e
                    ));
                    continue;
                }
            };

            // Check for symlink loops using inodes (Unix-like systems)
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let inode = metadata.ino();
                if metadata.is_dir() && !visited_inodes.insert(inode) {
                    warnings.push(format!(
                        "Skipped '{}': symlink loop detected",
                        entry_path.display()
                    ));
                    continue;
                }
            }

            // Determine entry type
            let entry_type = if metadata.is_dir() {
                "directory"
            } else if metadata.is_symlink() {
                "symlink"
            } else {
                "file"
            };

            // Get size only for files in detailed mode
            let size = if detailed && metadata.is_file() {
                Some(metadata.len())
            } else {
                None
            };

            // Increment total count
            *total_count += 1;

            // Recursively get children if it's a directory and within depth limit
            let children = if metadata.is_dir() && current_depth < max_depth {
                Self::traverse_directory_hierarchical(
                    &entry_path,
                    current_depth + 1,
                    max_depth,
                    include_hidden,
                    detailed,
                    config,
                    warnings,
                    visited_inodes,
                    total_count,
                    truncated,
                )
            } else {
                Vec::new()
            };

            // Add entry to results with its children
            results.push(EntryInfo {
                name,
                entry_type: entry_type.to_string(),
                size,
                children,
            });
        }

        results
    }

    /// Recursively count directories and files in hierarchical structure
    fn count_entries(entries: &[EntryInfo]) -> (usize, usize) {
        let mut dir_count = 0;
        let mut file_count = 0;

        for entry in entries {
            match entry.entry_type.as_str() {
                "directory" => {
                    dir_count += 1;
                    // Recursively count children
                    let (child_dirs, child_files) = Self::count_entries(&entry.children);
                    dir_count += child_dirs;
                    file_count += child_files;
                }
                "file" => file_count += 1,
                _ => {} // symlinks don't count as either
            }
        }

        (dir_count, file_count)
    }

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'path' parameter".to_string())?
            .to_string();

        let include_hidden = arguments
            .get("include_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let detailed = arguments
            .get("detailed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let recursive_depth = arguments
            .get("recursive_depth")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;

        info!(
            "List directory tool (HTTP) called for path: {} with recursive_depth: {}",
            path, recursive_depth
        );

        let params = FSListDirParams {
            path,
            include_hidden,
            detailed,
            recursive_depth,
        };

        let result = Self::execute(&params, &config);

        // Serialize the full CallToolResult to preserve all fields including structuredContent
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<FSListDirParams>(),
        )
        .with_raw_output_schema(schema_for_type::<ListResult>())
    }

    /// Create a ToolRoute for STDIO/TCP transport.
    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: FSListDirParams =
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
    use std::fs;
    use tempfile::TempDir;

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_list_dir_execute() {
        // Create a temporary directory with test files
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create test files
        fs::write(temp_path.join("file1.txt"), "content").unwrap();
        fs::write(temp_path.join("file2.txt"), "content").unwrap();
        fs::create_dir(temp_path.join("subdir")).unwrap();

        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: false,
            detailed: false,
            recursive_depth: 0,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Check text summary exists (human-readable)
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            _ => panic!("Expected text content"),
        };
        // The text should be a human-readable summary
        assert!(text.contains("Found"));
        assert!(text.contains("directories"));
        assert!(text.contains("files"));

        // Extract and parse structured content
        let json = result
            .structured_content
            .expect("Expected structured content");
        assert_eq!(json["dir_count"], 1);
        assert_eq!(json["file_count"], 2);

        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn test_list_dir_nonexistent() {
        let params = FSListDirParams {
            path: "/nonexistent/path/12345".to_string(),
            include_hidden: false,
            detailed: false,
            recursive_depth: 0,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));
    }

    #[test]
    fn test_list_dir_detailed() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        fs::write(temp_path.join("test.txt"), "hello world").unwrap();

        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: false,
            detailed: true,
            recursive_depth: 0,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Extract structured content
        let json = result
            .structured_content
            .expect("Expected structured content");
        let entries = json["entries"].as_array().unwrap();

        // Find the file entry
        let file_entry = entries
            .iter()
            .find(|e| e["name"].as_str().unwrap() == "test.txt")
            .expect("test.txt not found");

        assert_eq!(file_entry["type"], "file");
        assert_eq!(file_entry["size"], 11); // "hello world" = 11 bytes
    }

    #[test]
    fn test_list_dir_recursive_depth_1() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create nested structure
        fs::create_dir(temp_path.join("dir1")).unwrap();
        fs::write(temp_path.join("dir1/file_in_dir1.txt"), "content").unwrap();
        fs::write(temp_path.join("root_file.txt"), "content").unwrap();

        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: false,
            detailed: false,
            recursive_depth: 1,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        let json = result
            .structured_content
            .expect("Expected structured content");
        assert_eq!(json["dir_count"], 1);
        assert_eq!(json["file_count"], 2); // root_file.txt + file_in_dir1.txt

        // Verify hierarchical structure
        let entries = json["entries"].as_array().unwrap();
        let dir_entry = entries
            .iter()
            .find(|e| e["name"].as_str().unwrap() == "dir1")
            .expect("dir1 not found");
        assert_eq!(dir_entry["type"], "directory");
        assert!(dir_entry["children"].as_array().unwrap().len() > 0);
    }

    #[test]
    fn test_list_dir_recursive_depth_2() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create nested structure: root/dir1/dir2/file.txt
        fs::create_dir(temp_path.join("dir1")).unwrap();
        fs::create_dir(temp_path.join("dir1/dir2")).unwrap();
        fs::write(temp_path.join("dir1/dir2/deep_file.txt"), "content").unwrap();
        fs::write(temp_path.join("root_file.txt"), "content").unwrap();

        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: false,
            detailed: false,
            recursive_depth: 2,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        let json = result
            .structured_content
            .expect("Expected structured content");
        assert_eq!(json["dir_count"], 2); // dir1, dir2
        assert_eq!(json["file_count"], 2); // root_file.txt, deep_file.txt

        // Verify nested structure
        let entries = json["entries"].as_array().unwrap();
        let dir1 = entries
            .iter()
            .find(|e| e["name"].as_str().unwrap() == "dir1")
            .expect("dir1 not found");
        let dir1_children = dir1["children"].as_array().unwrap();
        let dir2 = dir1_children
            .iter()
            .find(|e| e["name"].as_str().unwrap() == "dir2")
            .expect("dir2 not found");
        assert!(dir2["children"].as_array().unwrap().len() > 0);
    }

    #[test]
    fn test_list_dir_hidden_files() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        fs::write(temp_path.join(".hidden"), "content").unwrap();
        fs::write(temp_path.join("visible.txt"), "content").unwrap();

        // Without include_hidden
        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: false,
            detailed: false,
            recursive_depth: 0,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);
        let json = result
            .structured_content
            .expect("Expected structured content");
        assert_eq!(json["file_count"], 1); // Only visible.txt

        // With include_hidden
        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: true,
            detailed: false,
            recursive_depth: 0,
        };

        let result = FsListDirTool::execute(&params, &config);
        let json = result
            .structured_content
            .expect("Expected structured content");
        assert_eq!(json["file_count"], 2); // Both .hidden and visible.txt
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_list_dir_http_handler() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        fs::write(temp_path.join("test.txt"), "content").unwrap();

        let args = serde_json::json!({
            "path": temp_path.to_string_lossy(),
            "include_hidden": false,
            "detailed": false,
            "recursive_depth": 0
        });

        let config = Arc::new(test_config());
        let result = FsListDirTool::http_handler(args, config);
        assert!(result.is_ok());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_list_dir_http_handler_missing_param() {
        let args = serde_json::json!({
            "include_hidden": true
        });

        let config = Arc::new(test_config());
        let result = FsListDirTool::http_handler(args, config);
        assert!(result.is_err());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_list_dir_http_handler_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        fs::create_dir(temp_path.join("subdir")).unwrap();
        fs::write(temp_path.join("subdir/nested.txt"), "content").unwrap();

        let args = serde_json::json!({
            "path": temp_path.to_string_lossy(),
            "include_hidden": false,
            "detailed": false,
            "recursive_depth": 1
        });

        let config = Arc::new(test_config());
        let result = FsListDirTool::http_handler(args, config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_structured_content_serialization() {
        // Test that CallToolResult::structured() produces JSON with structuredContent field
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();
        fs::write(temp_path.join("file.txt"), "test").unwrap();

        let params = FSListDirParams {
            path: temp_path.to_string_lossy().to_string(),
            include_hidden: false,
            detailed: false,
            recursive_depth: 0,
        };

        let config = test_config();
        let result = FsListDirTool::execute(&params, &config);

        // Serialize the CallToolResult as JSON (like the MCP server does)
        let serialized = serde_json::to_value(&result).unwrap();

        println!("Serialized CallToolResult:");
        println!("{}", serde_json::to_string_pretty(&serialized).unwrap());

        // Verify structuredContent field exists with camelCase
        assert!(
            serialized.get("structuredContent").is_some(),
            "structuredContent field is missing in serialized output! Got keys: {:?}",
            serialized.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );

        // Verify the structured content has the expected fields
        let structured = &serialized["structuredContent"];
        assert!(structured.get("path").is_some());
        assert!(structured.get("entries").is_some());
        assert!(structured.get("dir_count").is_some());
        assert!(structured.get("file_count").is_some());
    }
}
