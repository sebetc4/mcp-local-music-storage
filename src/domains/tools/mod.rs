//! Tools domain module.
//!
//! This module handles all tool-related functionality for the MCP server.
//! Tools are executable functions that can be called by MCP clients to perform
//! specific actions or computations.
//!
//! ## Architecture
//!
//! - `definitions/` - Individual tool implementations (one file per tool)
//! - `router.rs` - Dynamic ToolRouter builder for STDIO/TCP transport
//! - `registry.rs` - Central tool registry and HTTP dispatch
//! - `error.rs` - Tool-specific error types
//!
//! ## Adding a New Tool
//!
//! See DEVELOPMENT.md for the complete guide.
//! 1. Create a new file in `definitions/` (e.g., `my_tool.rs`)
//! 2. Define params, execute(), and http_handler()
//! 3. Export in `definitions/mod.rs`
//! 4. Add route in `router.rs` using `with_route()`
//! 5. Register in `registry.rs` for HTTP support
//!
//! **No need to modify `server.rs`!** The router is built dynamically.

pub mod definitions;
mod error;
#[cfg(feature = "http")]
pub mod http_response;
mod registry;
pub mod result;
pub mod router;

pub use error::ToolError;
pub use registry::ToolRegistry;
pub use result::structured_ok;
pub use router::build_tool_router;
