//! Configuration management for the MCP server.
//!
//! This module provides a centralized configuration structure that can be
//! populated from environment variables, configuration files, or defaults.

use super::transport::TransportConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{info, warn};

/// Main configuration structure for the MCP server.
///
/// This struct contains all configurable aspects of the server, organized
/// by domain for clarity and maintainability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Server identification and metadata.
    pub server: ServerConfig,

    /// Logging configuration.
    pub logging: LoggingConfig,

    /// Transport configuration.
    pub transport: TransportConfig,

    /// External API credentials configuration.
    pub credentials: CredentialsConfig,

    /// Security and path validation configuration.
    pub security: SecurityConfig,
}

/// Server identification configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// The name of the server as reported to clients.
    pub name: String,

    /// The version of the server.
    pub version: String,
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level filter (e.g., "info", "debug", "trace").
    pub level: String,

    /// Whether to include timestamps in log output.
    pub with_timestamps: bool,
}

/// Configuration for external API credentials. Defaults to `None` for every
/// key — there is intentionally no embedded fallback (see CLAUDE.md §2.2 and
/// Phase 1.3 of the cleanup roadmap). Callers must set `MCP_ACOUSTID_API_KEY`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CredentialsConfig {
    /// AcoustID API key for audio fingerprinting.
    /// Get a free key at: https://acoustid.org/api-key
    pub acoustid_api_key: Option<String>,
}

/// Custom Debug implementation to redact secrets from logs.
impl std::fmt::Debug for CredentialsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialsConfig")
            .field(
                "acoustid_api_key",
                &self.acoustid_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Configuration for security and path validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Optional root directory for path operations.
    /// If None, no path restrictions are enforced.
    /// All file system operations will be validated against this root.
    pub root_path: Option<PathBuf>,

    /// Whether to allow symlinks in path validation.
    /// If true, symlinks are followed and their targets are validated.
    /// If false, symlinks pointing outside the root are rejected.
    pub allow_symlinks: bool,
}

/// Parse an env-style boolean. Accepts `true`/`false`, `1`/`0`, `yes`/`no`
/// (case-insensitive, surrounding whitespace ignored). Any other value emits
/// a `warn!` and falls back to `default` — silently accepting typos like
/// `MCP_ALLOW_SYMLINKS=flase` would otherwise mask misconfiguration.
pub fn parse_bool_env(name: &str, raw: &str, default: bool) -> bool {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => true,
        "false" | "0" | "no" => false,
        other => {
            warn!(
                "Invalid boolean value {:?} for {}; using default {}",
                other, name, default
            );
            default
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            // No root path restriction by default (backwards compatible)
            root_path: None,
            // Allow symlinks by default with validation
            allow_symlinks: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                name: "mcp-server".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                with_timestamps: true,
            },
            transport: TransportConfig::default(),
            credentials: CredentialsConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

impl Config {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load configuration from environment variables.
    ///
    /// Loads `.env` (if present) into the process environment, then reads
    /// `MCP_*` variables. For deterministic tests use [`Config::from_env_with`]
    /// with a closure so the developer's `.env` cannot pollute the result.
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();
        Self::from_env_with(|name| std::env::var(name).ok())
    }

    /// Build a [`Config`] from a caller-supplied env reader.
    ///
    /// The reader is the only source of `MCP_*` values — neither `.env` nor the
    /// process environment is touched directly here, which makes this the safe
    /// entry point for tests.
    pub fn from_env_with<F>(read: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut config = Self::default();

        if let Some(name) = read("MCP_SERVER_NAME") {
            config.server.name = name;
        }

        if let Some(level) = read("MCP_LOG_LEVEL") {
            config.logging.level = level;
        }

        config.transport = TransportConfig::from_env_with(&read);

        if let Some(api_key) = read("MCP_ACOUSTID_API_KEY") {
            config.credentials.acoustid_api_key = Some(api_key);
            info!("AcoustID API key loaded from environment");
        } else {
            warn!(
                "MCP_ACOUSTID_API_KEY not set — mb_identify_record will refuse \
                 to run. Get a free key at https://acoustid.org/api-key"
            );
        }

        if let Some(root_path) = read("MCP_ROOT_PATH") {
            config.security.root_path = Some(PathBuf::from(root_path));
            info!(
                "Path security enabled: root directory set to {:?}",
                config.security.root_path
            );
        } else {
            warn!(
                "MCP_ROOT_PATH not set - no path restrictions active. \
                 All filesystem paths will be allowed."
            );
        }

        if let Some(raw) = read("MCP_ALLOW_SYMLINKS") {
            config.security.allow_symlinks = parse_bool_env("MCP_ALLOW_SYMLINKS", &raw, true);
            info!("Symlinks allowed: {}", config.security.allow_symlinks);
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credentials_from_env() {
        let config = Config::from_env_with(|name| match name {
            "MCP_ACOUSTID_API_KEY" => Some("test_key_12345".to_string()),
            _ => None,
        });
        assert_eq!(
            config.credentials.acoustid_api_key.as_deref(),
            Some("test_key_12345")
        );
    }

    #[test]
    fn test_credentials_default_is_none() {
        let config = Config::from_env_with(|_| None);
        assert!(
            config.credentials.acoustid_api_key.is_none(),
            "no embedded default key must ship: callers set MCP_ACOUSTID_API_KEY"
        );
    }

    #[test]
    fn test_credentials_redacted_in_debug() {
        let creds = CredentialsConfig {
            acoustid_api_key: Some("super_secret_key".to_string()),
        };
        let debug_str = format!("{:?}", creds);
        assert!(debug_str.contains("REDACTED"));
        assert!(!debug_str.contains("super_secret_key"));
    }

    #[test]
    fn test_config_default_has_no_acoustid_key() {
        let config = Config::default();
        assert!(config.credentials.acoustid_api_key.is_none());
    }

    #[test]
    fn parse_bool_env_accepts_canonical_values() {
        for v in ["true", "TRUE", "True", "1", "yes", "YES", " yes "] {
            assert!(parse_bool_env("X", v, false), "expected true for {:?}", v);
        }
        for v in ["false", "FALSE", "0", "no", "NO"] {
            assert!(!parse_bool_env("X", v, true), "expected false for {:?}", v);
        }
    }

    #[test]
    fn parse_bool_env_falls_back_on_typo() {
        // A typo like "flase" must NOT silently flip to true — must use default.
        assert!(parse_bool_env("MCP_ALLOW_SYMLINKS", "flase", true));
        assert!(!parse_bool_env("MCP_ALLOW_SYMLINKS", "flase", false));
        assert!(parse_bool_env("X", "", true));
        assert!(parse_bool_env("X", "maybe", true));
    }

    #[test]
    fn allow_symlinks_typo_uses_default() {
        // Regression: prior behavior was `parse().unwrap_or(true)` which kept
        // `true` on any non-"true"/"false" input. The new helper must apply
        // the default (true) when the value is invalid, not silently flip.
        let config = Config::from_env_with(|name| match name {
            "MCP_ALLOW_SYMLINKS" => Some("flase".to_string()),
            _ => None,
        });
        assert!(config.security.allow_symlinks);

        // And when the user explicitly says "false", it must take effect.
        let config = Config::from_env_with(|name| match name {
            "MCP_ALLOW_SYMLINKS" => Some("false".to_string()),
            _ => None,
        });
        assert!(!config.security.allow_symlinks);
    }
}
