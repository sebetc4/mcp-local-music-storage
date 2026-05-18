//! Transport configuration types.

use serde::{Deserialize, Serialize};

/// Transport configuration options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportConfig {
    /// Standard input/output transport (default for MCP).
    #[cfg(feature = "stdio")]
    Stdio,

    /// TCP socket transport with JSON-RPC messages.
    #[cfg(feature = "tcp")]
    Tcp(TcpConfig),

    /// HTTP transport with JSON-RPC over POST.
    #[cfg(feature = "http")]
    Http(HttpConfig),
}

/// TCP transport configuration.
#[cfg(feature = "tcp")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpConfig {
    /// Port number to listen on.
    pub port: u16,

    /// Host address to bind to.
    #[serde(default = "default_host")]
    pub host: String,
}

/// HTTP transport configuration.
#[cfg(feature = "http")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Port number to listen on.
    pub port: u16,

    /// Host address to bind to.
    #[serde(default = "default_host")]
    pub host: String,

    /// Path for JSON-RPC endpoint.
    #[serde(default = "default_rpc_path")]
    pub rpc_path: String,

    /// Enable CORS for browser clients.
    #[serde(default = "default_cors")]
    pub enable_cors: bool,

    /// Explicit allow-list of CORS origins. When empty, `Any` is only granted
    /// on loopback hosts; binding to a non-loopback address without an explicit
    /// origin list refuses startup.
    #[serde(default)]
    pub cors_allow_origins: Vec<String>,
}

#[cfg(any(feature = "tcp", feature = "http"))]
fn default_host() -> String {
    "127.0.0.1".to_string()
}

#[cfg(feature = "http")]
fn default_rpc_path() -> String {
    "/mcp".to_string()
}

#[cfg(feature = "http")]
fn default_cors() -> bool {
    true
}

impl Default for TransportConfig {
    // Explicit `return` keeps each cfg-gated branch self-contained and
    // readable; the alternative (trailing expressions) is brittle when
    // sibling cfg blocks are added or removed.
    #[allow(clippy::needless_return)]
    fn default() -> Self {
        #[cfg(feature = "stdio")]
        {
            return Self::Stdio;
        }

        #[cfg(all(not(feature = "stdio"), feature = "tcp"))]
        {
            return Self::Tcp(TcpConfig::default());
        }

        #[cfg(all(not(feature = "stdio"), not(feature = "tcp"), feature = "http"))]
        {
            return Self::Http(HttpConfig::default());
        }

        #[cfg(not(any(feature = "stdio", feature = "tcp", feature = "http")))]
        {
            compile_error!("At least one transport feature must be enabled: stdio, tcp, or http");
        }
    }
}

#[cfg(feature = "tcp")]
impl Default for TcpConfig {
    fn default() -> Self {
        Self {
            port: 3000,
            host: default_host(),
        }
    }
}

#[cfg(feature = "http")]
impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            host: default_host(),
            rpc_path: default_rpc_path(),
            enable_cors: default_cors(),
            cors_allow_origins: Vec::new(),
        }
    }
}

impl TransportConfig {
    /// Create a STDIO transport config.
    #[cfg(feature = "stdio")]
    pub fn stdio() -> Self {
        Self::Stdio
    }

    /// Create a TCP transport config.
    #[cfg(feature = "tcp")]
    pub fn tcp(port: u16, host: impl Into<String>) -> Self {
        Self::Tcp(TcpConfig {
            port,
            host: host.into(),
        })
    }

    /// Create an HTTP transport config.
    #[cfg(feature = "http")]
    pub fn http(port: u16, host: impl Into<String>) -> Self {
        Self::Http(HttpConfig {
            port,
            host: host.into(),
            ..Default::default()
        })
    }

    /// Load transport config from the process environment.
    pub fn from_env() -> Self {
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    /// Load transport config using a caller-supplied env reader.
    pub fn from_env_with<F>(read: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let transport = read("MCP_TRANSPORT").unwrap_or_default().to_lowercase();

        match transport.as_str() {
            #[cfg(feature = "tcp")]
            "tcp" => {
                let port = read("MCP_TCP_PORT")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(3000);
                let host = read("MCP_TCP_HOST").unwrap_or_else(default_host);
                Self::Tcp(TcpConfig { port, host })
            }
            #[cfg(feature = "http")]
            "http" => {
                let port = read("MCP_HTTP_PORT")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(8080);
                let host = read("MCP_HTTP_HOST").unwrap_or_else(default_host);
                let rpc_path = read("MCP_HTTP_PATH").unwrap_or_else(default_rpc_path);
                let enable_cors = read("MCP_HTTP_CORS")
                    .map(|raw| crate::core::config::parse_bool_env("MCP_HTTP_CORS", &raw, true))
                    .unwrap_or(true);
                let cors_allow_origins = read("MCP_HTTP_CORS_ORIGINS")
                    .map(|raw| {
                        raw.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Self::Http(HttpConfig {
                    port,
                    host,
                    rpc_path,
                    enable_cors,
                    cors_allow_origins,
                })
            }
            #[cfg(feature = "stdio")]
            _ => Self::Stdio,
            #[cfg(all(not(feature = "stdio"), feature = "tcp"))]
            _ => Self::Tcp(TcpConfig::default()),
            #[cfg(all(not(feature = "stdio"), not(feature = "tcp"), feature = "http"))]
            _ => Self::Http(HttpConfig::default()),
        }
    }

    /// Get a description of this transport for logging.
    pub fn description(&self) -> String {
        match self {
            #[cfg(feature = "stdio")]
            Self::Stdio => "STDIO (standard MCP mode)".to_string(),
            #[cfg(feature = "tcp")]
            Self::Tcp(cfg) => format!("TCP on {}:{}", cfg.host, cfg.port),
            #[cfg(feature = "http")]
            Self::Http(cfg) => format!("HTTP on {}:{}{}", cfg.host, cfg.port, cfg.rpc_path),
        }
    }

    /// Check if this transport is the standard STDIO mode.
    pub fn is_stdio(&self) -> bool {
        #[cfg(feature = "stdio")]
        {
            matches!(self, Self::Stdio)
        }
        #[cfg(not(feature = "stdio"))]
        {
            false
        }
    }
}
