//! File-hash tool.
//!
//! Streams a file through SHA-256 in 64 KiB chunks and returns the lowercase
//! hex digest plus the byte count. Cheap to call per-file (no whole-file
//! buffer); the 500 MB cap is a sanity gate, not a correctness limit — beyond
//! that, the caller should slice the work or rethink why they're hashing
//! anything that large.
//!
//! Catches *exact* duplicates only: a re-encoded MP3 with identical tags will
//! not match its source. That's by design — perceptual matching belongs in a
//! separate tool (decoded audio frames → Chromaprint), out of scope here.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::validate_path;

/// Hard cap on the file size we'll hash. Streaming makes the memory cost
/// trivial, but spending CPU on multi-gigabyte files in a single tool call
/// is a footgun — let the caller decide explicitly to override (deferred to
/// a flag if it ever becomes a real need).
pub const MAX_HASH_BYTES: u64 = 500 * 1024 * 1024;

/// 64 KiB streaming buffer — the sha2 crate is happiest with chunks in this
/// range (small enough to stay in L2, large enough to amortise syscall cost).
const HASH_CHUNK_BYTES: usize = 64 * 1024;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for `fs_hash`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FsHashParams {
    /// Absolute path to the file to hash. Must live under the configured
    /// root and resolve to a regular file (directories are refused; symlinks
    /// are rejected per the server's symlink policy).
    pub path: String,
}

// ============================================================================
// Structured Output
// ============================================================================

/// Result of an `fs_hash` call.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FsHashResult {
    /// The validated absolute path.
    pub path: String,
    /// `sha256` — kept as the algorithm tag for forward-compatibility (if
    /// we ever add `blake3` or similar, the same response shape carries
    /// across).
    pub algorithm: &'static str,
    /// Lower-case hex digest (64 characters for SHA-256).
    pub sha256: String,
    /// Byte count actually streamed through the hasher.
    pub bytes: u64,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// File-hash tool — SHA-256 of a single file.
pub struct FsHashTool;

impl FsHashTool {
    pub const NAME: &'static str = "fs_hash";

    pub const DESCRIPTION: &'static str = "Compute the SHA-256 of a file under the configured root. \
         Streams in 64 KiB chunks (no whole-file buffer). Capped at 500 MB — beyond that the call \
         is refused. Catches exact byte-for-byte duplicates only; a re-encoded copy will not match \
         its source.";

    #[instrument(skip_all, fields(path = %params.path))]
    pub fn execute(params: &FsHashParams, config: &Config) -> CallToolResult {
        info!("fs_hash called: '{}'", params.path);

        let path = match validate_path(&params.path, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };

        if !path.is_file() {
            warn!("Path is not a regular file: {}", params.path);
            return CallToolResult::error(vec![Content::text(format!(
                "Path is not a regular file: {}",
                params.path
            ))]);
        }

        // Cap check up front so we don't burn IO on something the caller
        // shouldn't be hashing in one shot. We re-check the streamed byte
        // count below as defence against a TOCTOU race (file grew between
        // metadata() and read).
        match std::fs::metadata(&path) {
            Ok(m) if m.len() > MAX_HASH_BYTES => {
                return CallToolResult::error(vec![Content::text(format!(
                    "File too large to hash in one call: {} bytes (max {} bytes)",
                    m.len(),
                    MAX_HASH_BYTES
                ))]);
            }
            Ok(_) => {}
            Err(e) => {
                return CallToolResult::error(vec![Content::text(format!(
                    "Could not stat '{}': {}",
                    params.path, e
                ))]);
            }
        }

        let (digest, bytes) = match stream_sha256(&path) {
            Ok(out) => out,
            Err(e) => {
                warn!("Hash failed for '{}': {}", params.path, e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to hash '{}': {}",
                    params.path, e
                ))]);
            }
        };

        let payload = FsHashResult {
            path: path.display().to_string(),
            algorithm: "sha256",
            sha256: digest,
            bytes,
        };
        let summary = format!(
            "sha256({}) = {} ({} bytes)",
            params.path, payload.sha256, bytes
        );
        crate::domains::tools::result::structured_ok(summary, &payload)
    }

    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: FsHashParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;
        let result = Self::execute(&params, &config);
        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<FsHashParams>(),
        )
        .with_raw_output_schema(schema_for_type::<FsHashResult>())
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: FsHashParams = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

/// Stream a file through SHA-256 and return `(lowercase_hex_digest,
/// total_bytes_read)`. Caller already validated the path; this is the pure
/// IO core, reusable by `find_duplicates`.
pub(crate) fn stream_sha256(path: &Path) -> std::io::Result<(String, u64)> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(HASH_CHUNK_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        // Defence against the file growing past the cap between metadata()
        // and the streaming read: refuse the hash rather than truncate
        // silently or burn unbounded IO.
        total = total.saturating_add(n as u64);
        if total > MAX_HASH_BYTES {
            return Err(std::io::Error::other(format!(
                "File grew past {} byte cap during read",
                MAX_HASH_BYTES
            )));
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        // `write!` into a String never fails — the result is dropped.
        let _ = write!(hex, "{:02x}", byte);
    }
    Ok((hex, total))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::SecurityConfig;
    use tempfile::TempDir;

    fn config_rooted_at(root: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.security = SecurityConfig {
            root_path: Some(root.to_path_buf()),
            allow_symlinks: true,
        };
        cfg
    }

    /// Known SHA-256 of the empty string (matches `printf '' | sha256sum`).
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Known SHA-256 of "abc" — the NIST FIPS 180-4 worked example.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn empty_file_hashes_to_known_digest() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();

        let r = FsHashTool::execute(
            &FsHashParams {
                path: path.to_string_lossy().into_owned(),
            },
            &config_rooted_at(root.path()),
        );
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["sha256"], EMPTY_SHA256);
        assert_eq!(s["bytes"], 0);
        assert_eq!(s["algorithm"], "sha256");
    }

    #[test]
    fn abc_file_hashes_to_fips_180_4_digest() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();

        let r = FsHashTool::execute(
            &FsHashParams {
                path: path.to_string_lossy().into_owned(),
            },
            &config_rooted_at(root.path()),
        );
        assert!(!r.is_error.unwrap_or(false));
        let s = r.structured_content.unwrap();
        assert_eq!(s["sha256"], ABC_SHA256);
        assert_eq!(s["bytes"], 3);
    }

    #[test]
    fn identical_bytes_produce_identical_digests() {
        let root = TempDir::new().unwrap();
        let a = root.path().join("a.bin");
        let b = root.path().join("b.bin");
        // Choose a payload that spans multiple chunks so the streaming
        // accumulator is actually exercised.
        let payload = vec![0x42u8; HASH_CHUNK_BYTES * 3 + 17];
        std::fs::write(&a, &payload).unwrap();
        std::fs::write(&b, &payload).unwrap();

        let cfg = config_rooted_at(root.path());
        let ra = FsHashTool::execute(
            &FsHashParams {
                path: a.to_string_lossy().into_owned(),
            },
            &cfg,
        );
        let rb = FsHashTool::execute(
            &FsHashParams {
                path: b.to_string_lossy().into_owned(),
            },
            &cfg,
        );
        assert_eq!(
            ra.structured_content.unwrap()["sha256"],
            rb.structured_content.unwrap()["sha256"]
        );
    }

    #[test]
    fn one_byte_difference_changes_digest() {
        let root = TempDir::new().unwrap();
        let a = root.path().join("a.bin");
        let b = root.path().join("b.bin");
        std::fs::write(&a, b"hello world").unwrap();
        std::fs::write(&b, b"hello worle").unwrap();
        let cfg = config_rooted_at(root.path());
        let ra = FsHashTool::execute(
            &FsHashParams {
                path: a.to_string_lossy().into_owned(),
            },
            &cfg,
        );
        let rb = FsHashTool::execute(
            &FsHashParams {
                path: b.to_string_lossy().into_owned(),
            },
            &cfg,
        );
        assert_ne!(
            ra.structured_content.unwrap()["sha256"],
            rb.structured_content.unwrap()["sha256"]
        );
    }

    #[test]
    fn refuses_directory() {
        let root = TempDir::new().unwrap();
        let r = FsHashTool::execute(
            &FsHashParams {
                path: root.path().to_string_lossy().into_owned(),
            },
            &config_rooted_at(root.path()),
        );
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn refuses_path_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let f = outside.path().join("a.bin");
        std::fs::write(&f, b"x").unwrap();
        let r = FsHashTool::execute(
            &FsHashParams {
                path: f.to_string_lossy().into_owned(),
            },
            &config_rooted_at(root.path()),
        );
        assert!(r.is_error.unwrap_or(false));
    }

    #[test]
    fn refuses_nonexistent_path() {
        let root = TempDir::new().unwrap();
        let r = FsHashTool::execute(
            &FsHashParams {
                path: root
                    .path()
                    .join("missing.bin")
                    .to_string_lossy()
                    .into_owned(),
            },
            &config_rooted_at(root.path()),
        );
        assert!(r.is_error.unwrap_or(false));
    }
}
