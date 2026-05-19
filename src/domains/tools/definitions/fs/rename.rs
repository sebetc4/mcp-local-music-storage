//! Rename/move tool definition.
//!
//! A tool that renames or moves files and directories.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::validate_path;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the rename/move tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FsRenameParams {
    /// Source path (file or directory to rename/move).
    pub from: String,

    /// Destination path (new name or location).
    pub to: String,

    /// Overwrite destination if it already exists.
    #[serde(default)]
    pub overwrite: bool,
}

// ============================================================================
// Output Structure (JSON format for AI agents)
// ============================================================================

/// Result of a rename/move operation
#[derive(Debug, Serialize, JsonSchema)]
struct RenameResult {
    /// Source path (original location)
    from: String,
    /// Destination path (new location)
    to: String,
    /// Type of item renamed ("file", "directory", or "item")
    item_type: String,
    /// Type of operation performed ("renamed" or "moved")
    operation: String,
    /// Whether the operation succeeded
    success: bool,
    /// Whether an existing file was overwritten
    #[serde(skip_serializing_if = "Option::is_none")]
    overwritten: Option<bool>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Rename/move tool - renames or moves files and directories.
pub struct FsRenameTool;

impl FsRenameTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "fs_rename";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Rename or move a file or directory from one path to another. Can also be used to move items between directories.";

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    #[instrument(skip_all, fields(from = %params.from, to = %params.to))]
    pub fn execute(params: &FsRenameParams, config: &Config) -> CallToolResult {
        info!("Rename tool called: '{}' -> '{}'", params.from, params.to);

        // Validate source path security
        let from_path = match validate_path(&params.from, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Source path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Source path security validation failed: {}",
                    e
                ))]);
            }
        };

        // Validate destination path security
        // Note: For destination, we validate the parent directory since the file might not exist yet
        let to_path = Path::new(&params.to);

        // If destination exists, validate it directly
        // If it doesn't exist, validate that its parent is within bounds
        if to_path.exists() {
            match validate_path(&params.to, config) {
                Ok(_) => {}
                Err(e) => {
                    warn!("Destination path security validation failed: {}", e);
                    return CallToolResult::error(vec![Content::text(format!(
                        "Destination path security validation failed: {}",
                        e
                    ))]);
                }
            }
        } else {
            // Validate parent directory for non-existent destinations
            if let Some(parent) = to_path.parent() {
                let parent_str = parent.to_string_lossy().to_string();
                match validate_path(&parent_str, config) {
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            "Destination parent directory security validation failed: {}",
                            e
                        );
                        return CallToolResult::error(vec![Content::text(format!(
                            "Destination parent directory security validation failed: {}",
                            e
                        ))]);
                    }
                }
            }
        }

        // Check if destination already exists (track for result)
        let destination_exists = to_path.exists();
        if destination_exists && !params.overwrite {
            warn!("Destination already exists: {}", params.to);
            return CallToolResult::error(vec![Content::text(format!(
                "Destination already exists: {}. Use overwrite=true to replace it.",
                params.to
            ))]);
        }

        // Get source type for response message
        let source_type = if from_path.is_dir() {
            "directory"
        } else if from_path.is_file() {
            "file"
        } else {
            "item"
        };

        // Check if this is a move (different parent directory) or just a rename
        let is_move = from_path.parent() != to_path.parent();
        let operation = if is_move { "moved" } else { "renamed" };

        // Perform the rename/move operation
        match fs::rename(from_path, to_path) {
            Ok(_) => {
                info!(
                    "Successfully {} '{}' to '{}'",
                    operation, params.from, params.to
                );

                // Create human-readable summary
                let summary = format!(
                    "Successfully {} {} from '{}' to '{}'",
                    operation, source_type, params.from, params.to
                );

                // Create structured result
                let result = RenameResult {
                    from: params.from.clone(),
                    to: params.to.clone(),
                    item_type: source_type.to_string(),
                    operation: operation.to_string(),
                    success: true,
                    overwritten: if destination_exists && params.overwrite {
                        Some(true)
                    } else {
                        None
                    },
                };

                // Return with text summary + structured content via the
                // shared helper.
                crate::domains::tools::result::structured_ok(summary, &result)
            }
            Err(e) => {
                warn!(
                    "Failed to {} '{}' to '{}': {}",
                    operation, params.from, params.to, e
                );

                // Provide more helpful error messages
                let error_msg = if e.kind() == std::io::ErrorKind::PermissionDenied {
                    format!(
                        "Permission denied: Cannot {} '{}' to '{}'",
                        operation, params.from, params.to
                    )
                } else if e.kind() == std::io::ErrorKind::NotFound {
                    format!("Path not found: '{}'", params.from)
                } else {
                    format!(
                        "Failed to {} '{}' to '{}': {}",
                        operation, params.from, params.to, e
                    )
                };

                CallToolResult::error(vec![Content::text(error_msg)])
            }
        }
    }

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let from = arguments
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'from' parameter".to_string())?
            .to_string();

        let to = arguments
            .get("to")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'to' parameter".to_string())?
            .to_string();

        let overwrite = arguments
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        info!("Rename tool (HTTP) called: '{}' -> '{}'", from, to);

        let params = FsRenameParams {
            from,
            to,
            overwrite,
        };

        let result = Self::execute(&params, &config);

        // Serialize the full CallToolResult to preserve all fields including structuredContent
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool::new(Self::NAME, Self::DESCRIPTION, schema_for_type::<FsRenameParams>())
            .with_raw_output_schema(schema_for_type::<RenameResult>())
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
                let params: FsRenameParams =
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
    fn test_rename_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a test file
        let old_file = temp_path.join("old_name.txt");
        let new_file = temp_path.join("new_name.txt");
        fs::write(&old_file, "test content").unwrap();

        let params = FsRenameParams {
            from: old_file.to_string_lossy().to_string(),
            to: new_file.to_string_lossy().to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify old file doesn't exist and new file does
        assert!(!old_file.exists());
        assert!(new_file.exists());
        assert_eq!(fs::read_to_string(&new_file).unwrap(), "test content");
    }

    #[test]
    fn test_rename_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a test directory with a file inside
        let old_dir = temp_path.join("old_dir");
        let new_dir = temp_path.join("new_dir");
        fs::create_dir(&old_dir).unwrap();
        fs::write(old_dir.join("file.txt"), "content").unwrap();

        let params = FsRenameParams {
            from: old_dir.to_string_lossy().to_string(),
            to: new_dir.to_string_lossy().to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify old directory doesn't exist and new directory does
        assert!(!old_dir.exists());
        assert!(new_dir.exists());
        assert!(new_dir.join("file.txt").exists());
    }

    #[test]
    fn test_move_file_to_different_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create source file and destination directory
        let source_file = temp_path.join("file.txt");
        let dest_dir = temp_path.join("subdir");
        let dest_file = dest_dir.join("file.txt");

        fs::write(&source_file, "content").unwrap();
        fs::create_dir(&dest_dir).unwrap();

        let params = FsRenameParams {
            from: source_file.to_string_lossy().to_string(),
            to: dest_file.to_string_lossy().to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify file was moved
        assert!(!source_file.exists());
        assert!(dest_file.exists());
    }

    #[test]
    fn test_rename_nonexistent_source() {
        let params = FsRenameParams {
            from: "/nonexistent/file.txt".to_string(),
            to: "/some/other/path.txt".to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));

        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            _ => panic!("Expected text content"),
        };
        assert!(text.contains("does not exist"));
    }

    #[test]
    fn test_rename_destination_exists_no_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let file1 = temp_path.join("file1.txt");
        let file2 = temp_path.join("file2.txt");

        fs::write(&file1, "content1").unwrap();
        fs::write(&file2, "content2").unwrap();

        let params = FsRenameParams {
            from: file1.to_string_lossy().to_string(),
            to: file2.to_string_lossy().to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));

        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            _ => panic!("Expected text content"),
        };
        assert!(text.contains("already exists"));
    }

    #[test]
    fn test_rename_destination_exists_with_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let file1 = temp_path.join("file1.txt");
        let file2 = temp_path.join("file2.txt");

        fs::write(&file1, "content1").unwrap();
        fs::write(&file2, "content2").unwrap();

        let params = FsRenameParams {
            from: file1.to_string_lossy().to_string(),
            to: file2.to_string_lossy().to_string(),
            overwrite: true,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify file1 replaced file2
        assert!(!file1.exists());
        assert!(file2.exists());
        assert_eq!(fs::read_to_string(&file2).unwrap(), "content1");
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_rename_http_handler() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let old_file = temp_path.join("old.txt");
        let new_file = temp_path.join("new.txt");
        fs::write(&old_file, "content").unwrap();

        let args = serde_json::json!({
            "from": old_file.to_string_lossy(),
            "to": new_file.to_string_lossy(),
            "overwrite": false
        });

        let config = Arc::new(test_config());
        let result = FsRenameTool::http_handler(args, config);
        assert!(result.is_ok());
        assert!(new_file.exists());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_rename_http_handler_missing_param() {
        let args = serde_json::json!({
            "from": "/some/path.txt"
        });

        let config = Arc::new(test_config());
        let result = FsRenameTool::http_handler(args, config);
        assert!(result.is_err());
    }

    #[test]
    fn test_structured_content_in_result() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let old_file = temp_path.join("old.txt");
        let new_file = temp_path.join("new.txt");
        fs::write(&old_file, "test").unwrap();

        let params = FsRenameParams {
            from: old_file.to_string_lossy().to_string(),
            to: new_file.to_string_lossy().to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);

        // Verify structured content exists
        assert!(
            result.structured_content.is_some(),
            "structured_content should be present"
        );

        let structured = result.structured_content.unwrap();

        // Verify fields
        assert_eq!(structured["from"], params.from);
        assert_eq!(structured["to"], params.to);
        assert_eq!(structured["item_type"], "file");
        assert_eq!(structured["operation"], "renamed");
        assert_eq!(structured["success"], true);

        // Verify text summary
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("Expected text content"),
        };
        assert!(text.contains("Successfully"));
        assert!(text.contains("renamed"));
    }

    #[test]
    fn test_structured_content_with_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let old_file = temp_path.join("old.txt");
        let existing_file = temp_path.join("existing.txt");
        fs::write(&old_file, "old").unwrap();
        fs::write(&existing_file, "existing").unwrap();

        let params = FsRenameParams {
            from: old_file.to_string_lossy().to_string(),
            to: existing_file.to_string_lossy().to_string(),
            overwrite: true,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);

        let structured = result
            .structured_content
            .expect("structured_content should exist");

        // Verify overwritten field is present
        assert_eq!(structured["overwritten"], true);
    }

    #[test]
    fn test_structured_content_serialization() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let old_file = temp_path.join("source.txt");
        let new_file = temp_path.join("dest.txt");
        fs::write(&old_file, "data").unwrap();

        let params = FsRenameParams {
            from: old_file.to_string_lossy().to_string(),
            to: new_file.to_string_lossy().to_string(),
            overwrite: false,
        };

        let config = test_config();
        let result = FsRenameTool::execute(&params, &config);

        // Serialize to JSON like the MCP server does
        let serialized = serde_json::to_value(&result).unwrap();

        // Verify structuredContent exists in camelCase
        assert!(
            serialized.get("structuredContent").is_some(),
            "structuredContent field missing in serialized output"
        );

        // Verify content has text summary (not full JSON)
        let content_text = serialized["content"][0]["text"].as_str().unwrap();
        assert!(content_text.contains("Successfully"));
        assert!(
            !content_text.starts_with('{'),
            "Text should be summary, not JSON"
        );
    }
}
