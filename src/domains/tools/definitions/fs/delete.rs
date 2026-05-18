//! Delete tool definition.
//!
//! A tool that deletes files and directories.

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
use crate::core::security::validate_path;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the delete tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FsDeleteParams {
    /// Path to the file or directory to delete.
    pub path: String,

    /// Recursively delete directories and their contents.
    /// Required to delete non-empty directories.
    #[serde(default)]
    pub recursive: bool,
}

// ============================================================================
// Output Structure (JSON format for AI agents)
// ============================================================================

/// Result of a delete operation
#[derive(Debug, Serialize, JsonSchema)]
struct DeleteResult {
    /// Path that was deleted
    path: String,
    /// Type of item deleted ("file", "directory", or "item")
    item_type: String,
    /// Whether the operation succeeded
    success: bool,
    /// Whether recursive deletion was used
    #[serde(skip_serializing_if = "Option::is_none")]
    recursive: Option<bool>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Delete tool - deletes files and directories.
pub struct FsDeleteTool;

impl FsDeleteTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "fs_delete";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Delete a file or directory. Use recursive=true to delete non-empty directories and their contents.";

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    #[instrument(skip_all, fields(path = %params.path))]
    pub fn execute(params: &FsDeleteParams, config: &Config) -> CallToolResult {
        info!("Delete tool called: '{}'", params.path);

        // Validate path security
        let target_path = match validate_path(&params.path, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };

        // Check if path exists
        if !target_path.exists() {
            warn!("Path does not exist: {}", params.path);
            return CallToolResult::error(vec![Content::text(format!(
                "Path does not exist: {}",
                params.path
            ))]);
        }

        // Determine item type for response (before deletion)
        let is_directory = target_path.is_dir();
        let item_type = if is_directory {
            "directory"
        } else if target_path.is_file() {
            "file"
        } else {
            "item"
        };

        // Check if directory is non-empty and recursive flag is not set
        if is_directory && !params.recursive {
            // Check if directory is empty
            match fs::read_dir(&target_path) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        warn!(
                            "Directory is not empty and recursive flag is not set: {}",
                            params.path
                        );
                        return CallToolResult::error(vec![Content::text(format!(
                            "Directory is not empty: {}. Use recursive=true to delete it and its contents.",
                            params.path
                        ))]);
                    }
                }
                Err(e) => {
                    warn!("Failed to read directory '{}': {}", params.path, e);
                    return CallToolResult::error(vec![Content::text(format!(
                        "Failed to read directory '{}': {}",
                        params.path, e
                    ))]);
                }
            }
        }

        // Perform the delete operation
        let delete_result = if is_directory {
            if params.recursive {
                fs::remove_dir_all(&target_path)
            } else {
                fs::remove_dir(&target_path)
            }
        } else {
            fs::remove_file(&target_path)
        };

        match delete_result {
            Ok(_) => {
                info!("Successfully deleted '{}' ({})", params.path, item_type);

                // Create human-readable summary
                let summary = if params.recursive && is_directory {
                    format!(
                        "Successfully deleted {} '{}' and all its contents",
                        item_type, params.path
                    )
                } else {
                    format!("Successfully deleted {} '{}'", item_type, params.path)
                };

                // Create structured result
                let result = DeleteResult {
                    path: params.path.clone(),
                    item_type: item_type.to_string(),
                    success: true,
                    recursive: if params.recursive && is_directory {
                        Some(true)
                    } else {
                        None
                    },
                };

                // Return with text summary + structured content. If serializing the
                // structured payload fails, fall back to a text-only success so the
                // caller still sees the operation succeeded.
                match serde_json::to_value(&result) {
                    Ok(structured) => CallToolResult {
                        content: vec![Content::text(summary)],
                        structured_content: Some(structured),
                        is_error: Some(false),
                        meta: None,
                    },
                    Err(e) => {
                        warn!("Failed to serialize structured content: {}", e);
                        CallToolResult::success(vec![Content::text(summary)])
                    }
                }
            }
            Err(e) => {
                warn!("Failed to delete '{}': {}", params.path, e);

                // Provide more helpful error messages
                let error_msg = if e.kind() == std::io::ErrorKind::PermissionDenied {
                    format!("Permission denied: Cannot delete '{}'", params.path)
                } else if e.kind() == std::io::ErrorKind::NotFound {
                    format!("Path not found: '{}'", params.path)
                } else {
                    format!("Failed to delete '{}': {}", params.path, e)
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
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'path' parameter".to_string())?
            .to_string();

        let recursive = arguments
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        info!("Delete tool (HTTP) called: '{}'", path);

        let params = FsDeleteParams { path, recursive };

        let result = Self::execute(&params, &config);

        // Serialize the full CallToolResult to preserve all fields including structuredContent
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<FsDeleteParams>(),
            annotations: None,
            output_schema: Some(schema_for_type::<DeleteResult>()),
            icons: None,
            meta: None,
            title: None,
        }
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
                let params: FsDeleteParams =
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
    fn test_delete_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a test file
        let test_file = temp_path.join("test.txt");
        fs::write(&test_file, "test content").unwrap();
        assert!(test_file.exists());

        let params = FsDeleteParams {
            path: test_file.to_string_lossy().to_string(),
            recursive: false,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify file no longer exists
        assert!(!test_file.exists());
    }

    #[test]
    fn test_delete_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create an empty directory
        let test_dir = temp_path.join("empty_dir");
        fs::create_dir(&test_dir).unwrap();
        assert!(test_dir.exists());

        let params = FsDeleteParams {
            path: test_dir.to_string_lossy().to_string(),
            recursive: false,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify directory no longer exists
        assert!(!test_dir.exists());
    }

    #[test]
    fn test_delete_nonempty_directory_without_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a directory with a file inside
        let test_dir = temp_path.join("nonempty_dir");
        fs::create_dir(&test_dir).unwrap();
        fs::write(test_dir.join("file.txt"), "content").unwrap();

        let params = FsDeleteParams {
            path: test_dir.to_string_lossy().to_string(),
            recursive: false,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));

        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            _ => panic!("Expected text content"),
        };
        assert!(text.contains("not empty"));
        assert!(text.contains("recursive=true"));

        // Verify directory still exists
        assert!(test_dir.exists());
    }

    #[test]
    fn test_delete_nonempty_directory_with_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a directory with files and subdirectories
        let test_dir = temp_path.join("nonempty_dir");
        let sub_dir = test_dir.join("subdir");
        fs::create_dir(&test_dir).unwrap();
        fs::create_dir(&sub_dir).unwrap();
        fs::write(test_dir.join("file1.txt"), "content1").unwrap();
        fs::write(sub_dir.join("file2.txt"), "content2").unwrap();

        let params = FsDeleteParams {
            path: test_dir.to_string_lossy().to_string(),
            recursive: true,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);
        assert!(result.is_error.is_none() || !result.is_error.unwrap());

        // Verify directory and all contents are deleted
        assert!(!test_dir.exists());
    }

    #[test]
    fn test_delete_nonexistent_path() {
        let params = FsDeleteParams {
            path: "/nonexistent/path/to/file.txt".to_string(),
            recursive: false,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));

        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(text) => &text.text,
            _ => panic!("Expected text content"),
        };
        assert!(text.contains("does not exist"));
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_delete_http_handler() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_file = temp_path.join("test.txt");
        fs::write(&test_file, "content").unwrap();

        let args = serde_json::json!({
            "path": test_file.to_string_lossy(),
            "recursive": false
        });

        let config = Arc::new(test_config());
        let result = FsDeleteTool::http_handler(args, config);
        assert!(result.is_ok());
        assert!(!test_file.exists());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_delete_http_handler_missing_param() {
        let args = serde_json::json!({
            "recursive": true
        });

        let config = Arc::new(test_config());
        let result = FsDeleteTool::http_handler(args, config);
        assert!(result.is_err());
    }

    #[test]
    fn test_structured_content_in_result() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_file = temp_path.join("test.txt");
        fs::write(&test_file, "test").unwrap();

        let params = FsDeleteParams {
            path: test_file.to_string_lossy().to_string(),
            recursive: false,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);

        // Verify structured content exists
        assert!(
            result.structured_content.is_some(),
            "structured_content should be present"
        );

        let structured = result.structured_content.unwrap();

        // Verify fields
        assert_eq!(structured["path"], params.path);
        assert_eq!(structured["item_type"], "file");
        assert_eq!(structured["success"], true);

        // Verify text summary
        let text = match &result.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("Expected text content"),
        };
        assert!(text.contains("Successfully"));
        assert!(text.contains("deleted"));
    }

    #[test]
    fn test_structured_content_with_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_dir = temp_path.join("test_dir");
        fs::create_dir(&test_dir).unwrap();
        fs::write(test_dir.join("file.txt"), "content").unwrap();

        let params = FsDeleteParams {
            path: test_dir.to_string_lossy().to_string(),
            recursive: true,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);

        let structured = result
            .structured_content
            .expect("structured_content should exist");

        // Verify recursive field is present
        assert_eq!(structured["recursive"], true);
        assert_eq!(structured["item_type"], "directory");
    }

    #[test]
    fn test_structured_content_serialization() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_file = temp_path.join("test.txt");
        fs::write(&test_file, "data").unwrap();

        let params = FsDeleteParams {
            path: test_file.to_string_lossy().to_string(),
            recursive: false,
        };

        let config = test_config();
        let result = FsDeleteTool::execute(&params, &config);

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
