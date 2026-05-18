//! Error types and handling for the MCP server.
//!
//! This module defines a unified error type that can represent errors from
//! all domains and external dependencies, providing consistent error handling
//! across the entire application.

use thiserror::Error;

/// A specialized Result type for MCP server operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Unified error type for the MCP server.
///
/// This enum captures all possible error conditions that can occur during
/// server operation, including domain-specific errors and external failures.
#[derive(Debug, Error)]
pub enum Error {
    /// Error originating from the tools domain.
    #[error("Tool error: {0}")]
    Tool(#[from] crate::domains::tools::ToolError),

    /// Configuration-related errors.
    #[error("Configuration error: {0}")]
    Config(String),

    /// I/O errors from file operations or network communication.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization errors.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Internal server errors that should not occur under normal operation.
    ///
    /// Wraps an [`anyhow::Error`] so the original cause chain (including
    /// downcasting to the underlying source via `err.downcast_ref::<T>()`) is
    /// preserved. Construct via [`Error::internal`] or `?` from any
    /// `anyhow::Error`-producing call.
    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl Error {
    /// Create a new configuration error.
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// Create a new internal error from a free-form message.
    ///
    /// For typed sources, prefer `Error::Internal(anyhow::Error::new(source))`
    /// or `?`-propagation from an `anyhow::Error`-producing context.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(anyhow::anyhow!(msg.into()))
    }
}
