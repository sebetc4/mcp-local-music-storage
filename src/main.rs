// Test code legitimately uses `.unwrap()` / `.expect()` on fixtures.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::todo,
        clippy::unimplemented
    )
)]

//! MCP Server Entry Point
//!
//! This is the main entry point for the MCP server. It initializes logging,
//! loads configuration, and starts the server with the configured transport.

use anyhow::Result;
use tracing::{Level, info};
use tracing_subscriber::{EnvFilter, fmt};

use music_mcp_server::core::{Config, McpServer, TransportService};

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration from environment
    let config = Config::from_env();

    // Initialize logging
    init_logging(&config.logging.level);

    info!("Starting {} v{}", config.server.name, config.server.version);

    // Create the MCP server
    let server = McpServer::new(config.clone());

    info!("Server initialized");

    // Create and run the transport service
    let transport = TransportService::new(config.transport);
    transport.run(server).await?;

    info!("Server shutting down");

    Ok(())
}

/// Initialize the logging subsystem.
///
/// Configures tracing with the specified log level and format.
fn init_logging(level: &str) {
    let level = match level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let filter = EnvFilter::from_default_env().add_directive(level.into());

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_writer(std::io::stderr)
        .init();
}
