//! Core module containing shared infrastructure components.
//!
//! This module provides the foundational building blocks for the MCP server,
//! including error handling, configuration, server lifecycle management,
//! and transport layer abstractions.

pub mod config;
pub mod error;
pub mod fs_atomic;
pub mod security;
pub mod server;
pub mod transport;

pub use config::Config;
pub use error::{Error, Result};
pub use security::{PathSecurityError, is_safe_filename, validate_path};
pub use server::McpServer;
pub use transport::{TransportConfig, TransportService};
