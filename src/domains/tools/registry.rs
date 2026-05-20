//! Tool Registry - central registration and dispatch for all tools.
//!
//! This module provides:
//! - A registry of all available tools
//! - HTTP dispatch for tool calls (when http feature is enabled)
//! - Tool metadata for listing
//!
//! The four lists (names, metadata, HTTP dispatch, router) are all derived
//! from the single [`crate::foreach_tool!`] X-macro in `definitions/mod.rs`.

use std::sync::Arc;
#[cfg(feature = "http")]
use tracing::warn;

use rmcp::model::Tool;

use crate::core::config::Config;
// Bring the blocking-tool trait into scope so `<NoConfigTool>::NAME`,
// `to_tool()`, `http_handler()` etc. resolve via the trait impls.
use crate::domains::tools::definitions::mb::MbBlockingTool;

/// Tool registry - manages all available tools.
pub struct ToolRegistry {
    config: Arc<Config>,
}

impl ToolRegistry {
    /// Create a new tool registry.
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    /// Get all tool names. Derived from [`crate::foreach_tool!`].
    pub fn tool_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = Vec::new();
        macro_rules! push_name {
            ($t:ty, $_kind:ident) => {
                names.push(<$t>::NAME);
            };
        }
        crate::foreach_tool!(push_name);
        names
    }

    /// Get all tools as [`Tool`] models (metadata). Derived from
    /// [`crate::foreach_tool!`].
    pub fn get_all_tools() -> Vec<Tool> {
        let mut tools: Vec<Tool> = Vec::new();
        macro_rules! push_tool {
            ($t:ty, $_kind:ident) => {
                tools.push(<$t>::to_tool());
            };
        }
        crate::foreach_tool!(push_tool);
        tools
    }

    /// Dispatch an HTTP tool call to the appropriate handler. Derived from
    /// [`crate::foreach_tool!`].
    #[cfg(feature = "http")]
    pub fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let config = &self.config;
        // The `if … { return … }` chain consumes `arguments` at most once,
        // because the matching arm always early-returns. The borrow checker
        // tolerates this pattern even though `arguments` is moved into the
        // chosen `http_handler` call.
        macro_rules! try_dispatch {
            ($t:ty, with_config) => {
                if name == <$t>::NAME {
                    return <$t>::http_handler(arguments, config.clone());
                }
            };
            ($t:ty, no_config) => {
                if name == <$t>::NAME {
                    return <$t>::http_handler(arguments);
                }
            };
        }
        crate::foreach_tool!(try_dispatch);

        warn!("Unknown tool requested: {}", name);
        Err(format!("Unknown tool: {}", name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    #[test]
    fn test_registry_tool_names() {
        let registry = ToolRegistry::new(test_config());
        let names = registry.tool_names();
        assert_eq!(names.len(), 20);
        assert!(names.contains(&"apply_naming_scheme"));
        assert!(names.contains(&"apply_plan"));
        assert!(names.contains(&"embed_cover"));
        assert!(names.contains(&"fs_delete"));
        assert!(names.contains(&"fs_list_dir"));
        assert!(names.contains(&"fs_mkdir"));
        assert!(names.contains(&"fs_move"));
        assert!(names.contains(&"fs_rename"));
        assert!(names.contains(&"fs_scan_audio"));
        assert!(names.contains(&"read_metadata_batch"));
        assert!(names.contains(&"write_metadata_batch"));
        assert!(names.contains(&"mb_artist_search"));
        assert!(names.contains(&"mb_cover_download"));
        assert!(names.contains(&"mb_identify_record"));
        assert!(names.contains(&"mb_label_search"));
        assert!(names.contains(&"mb_recording_search"));
        assert!(names.contains(&"mb_release_search"));
        assert!(names.contains(&"mb_work_search"));
        assert!(names.contains(&"read_metadata"));
        assert!(names.contains(&"write_metadata"));
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_registry_call_echo() {
        let registry = ToolRegistry::new(test_config());
        let result = registry.call_tool("fs_list_dir", serde_json::json!({ "path": "test" }));
        assert!(result.is_ok());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_registry_call_unknown() {
        let registry = ToolRegistry::new(test_config());
        let result = registry.call_tool("unknown", serde_json::json!({}));
        assert!(result.is_err());
    }
}
