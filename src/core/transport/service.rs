//! Transport service - orchestrates different transport types.
//!
//! This service provides a unified interface for starting the MCP server
//! with different transport mechanisms.

use tracing::info;

use super::{TransportConfig, TransportResult};
use crate::core::McpServer;

#[cfg(feature = "stdio")]
use super::stdio::StdioTransport;

#[cfg(feature = "tcp")]
use super::tcp::TcpTransport;

#[cfg(feature = "http")]
use super::http::HttpTransport;

/// Transport service - manages the transport layer for the MCP server.
pub struct TransportService {
    config: TransportConfig,
}

impl TransportService {
    /// Create a new transport service with the given configuration.
    pub fn new(config: TransportConfig) -> Self {
        Self { config }
    }

    /// Create a transport service from environment variables.
    pub fn from_env() -> Self {
        Self::new(TransportConfig::from_env())
    }

    /// Get the transport configuration.
    pub fn config(&self) -> &TransportConfig {
        &self.config
    }

    /// Log information about the configured transport.
    pub fn log_info(&self) {
        info!("Starting transport: {}", self.config.description());
    }

    /// Start the transport with the given MCP server.
    ///
    /// This method blocks until the transport is shut down.
    pub async fn run(self, server: McpServer) -> TransportResult<()> {
        self.log_info();

        match self.config {
            #[cfg(feature = "stdio")]
            TransportConfig::Stdio => StdioTransport::run(server).await,
            #[cfg(feature = "tcp")]
            TransportConfig::Tcp(cfg) => TcpTransport::new(cfg).run(server).await,
            #[cfg(feature = "http")]
            TransportConfig::Http(cfg) => HttpTransport::new(cfg).run(server).await,
        }
    }
}
