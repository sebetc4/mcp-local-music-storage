//! MCP Server implementation and lifecycle management.
//!
//! This module contains the main server handler that implements the MCP
//! protocol by delegating to domain-specific services.
//!
//! ## Tool Architecture
//!
//! Tools are defined in `domains/tools/definitions/` with one file per tool.
//! Each tool defines:
//! - Parameters struct (for rmcp)
//! - `execute()` method (core logic)
//! - `http_handler()` method (called via ToolRegistry for HTTP transport)
//!
//! The ToolRouter is built dynamically in `domains/tools/router.rs`.
//! **Adding a new tool does NOT require modifying this file!**

use rmcp::{handler::server::tool::ToolRouter, model::*, ServerHandler, tool_handler};
use std::sync::Arc;

use super::config::Config;
use crate::domains::tools::build_tool_router;

#[cfg(feature = "http")]
use crate::domains::tools::ToolRegistry;

/// The main MCP server handler.
///
/// This struct implements the `ServerHandler` trait from rmcp and coordinates
/// between different domain services to handle MCP protocol messages.
#[derive(Clone)]
pub struct McpServer {
    /// Server configuration.
    config: Arc<Config>,

    /// Tool router for handling tool calls.
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    /// Create a new MCP server with the given configuration.
    pub fn new(config: Config) -> Self {
        let config = Arc::new(config);

        Self {
            tool_router: build_tool_router::<Self>(config.clone()),
            config,
        }
    }

    /// Get the server name.
    pub fn name(&self) -> &str {
        &self.config.server.name
    }

    /// Get the server version.
    pub fn version(&self) -> &str {
        &self.config.server.version
    }

    /// Get the server configuration (for tool access).
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    // ========================================================================
    // HTTP Transport Support Methods
    // ========================================================================

    /// List all available tools (for HTTP transport).
    pub fn list_tools(&self) -> Vec<serde_json::Value> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema
                })
            })
            .collect()
    }

    /// Call a tool by name (for HTTP transport).
    ///
    /// This method uses the ToolRegistry to dispatch to the appropriate
    /// tool handler. Each tool's http_handler is defined in its own file
    /// under `domains/tools/definitions/`.
    #[cfg(feature = "http")]
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let registry = ToolRegistry::new(self.config.clone());
        registry.call_tool(name, arguments)
    }
}

/// ServerHandler implementation with tool_handler macro for automatic tool routing.
#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "MCP server for music library automation: filesystem, audio metadata, and MusicBrainz tooling."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
