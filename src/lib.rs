// Test code legitimately uses `.unwrap()` / `.expect()` on fixtures and
// builders; only production code is held to the strict rule.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

//! MCP Server Library
//!
//! This crate provides a scalable Model Context Protocol (MCP) server template
//! with a modular architecture organized by domains.
//!
//! # Architecture
//!
//! The server is organized into the following modules:
//!
//! - **core**: Core infrastructure including configuration, error handling, and the main server
//! - **domains**: Business logic organized by bounded contexts
//!   - **tools**: MCP tools that can be executed by clients (12 tools across `fs`, `metadata`, `mb`)
//!
//! # Example
//!
//! ```rust,no_run
//! use music_mcp_server::{core::McpServer, core::Config};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = Config::from_env();
//!     let server = McpServer::new(config);
//!     // Start the server...
//!     Ok(())
//! }
//! ```

pub mod core;
pub mod domains;

// Re-export commonly used types for convenience
pub use core::{Config, Error, McpServer, Result};
