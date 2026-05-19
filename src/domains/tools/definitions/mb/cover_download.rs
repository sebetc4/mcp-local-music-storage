//! MusicBrainz Cover Art download tool.
//!
//! This tool downloads cover art images for music releases from the Cover Art Archive.
//! Supports multiple thumbnail sizes with intelligent fallback strategies.

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::core::config::Config;
use crate::core::fs_atomic::write_atomic;
use crate::core::security::{is_safe_filename, validate_path};

use super::common::{error_result, is_mbid, structured_result};

/// Hard cap on cover-art download size. A misbehaving or malicious server could
/// otherwise stream unbounded bytes into memory.
const MAX_COVER_BYTES: u64 = 50 * 1024 * 1024;

/// Read up to `cap` bytes from `reader`. Returns an error if more bytes are
/// available — i.e. the source exceeded the cap.
fn read_with_cap<R: Read>(reader: &mut R, cap: u64) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    // Take cap+1 so a single extra byte beyond the cap surfaces as overflow.
    let mut limited = reader.take(cap.saturating_add(1));
    limited
        .read_to_end(&mut buf)
        .map_err(|e| format!("Failed to read response: {}", e))?;
    if buf.len() as u64 > cap {
        return Err(format!("cover too large: > {} bytes, max {}", cap, cap));
    }
    Ok(buf)
}

// ============================================================================
// Cover Art Archive JSON structures
// ============================================================================

/// Thumbnail structure that handles both string and numeric IDs.
/// Supports both legacy format (small/large) and new format (250/500/1200).
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Thumbnail {
    #[serde(default)]
    pub small: Option<String>,
    #[serde(default)]
    pub large: Option<String>,
    #[serde(rename = "250", default)]
    pub res_250: Option<String>,
    #[serde(rename = "500", default)]
    pub res_500: Option<String>,
    #[serde(rename = "1200", default)]
    pub res_1200: Option<String>,
}

/// Cover art image structure with flexible ID handling.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct CoverartImage {
    pub approved: bool,
    pub back: bool,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub edit: u64,
    pub front: bool,
    #[serde(deserialize_with = "deserialize_string_or_number")]
    pub id: String,
    pub image: String,
    pub thumbnails: Thumbnail,
    #[serde(default)]
    pub types: Vec<String>,
}

/// Cover Art Archive response structure.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Coverart {
    pub images: Vec<CoverartImage>,
    #[serde(default)]
    pub release: Option<String>,
}

/// Deserializer that accepts both string and number for ID field.
fn deserialize_string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct StringOrNumber;

    impl<'de> Visitor<'de> for StringOrNumber {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or number")
        }

        fn visit_str<E>(self, value: &str) -> Result<String, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_u64<E>(self, value: u64) -> Result<String, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_i64<E>(self, value: i64) -> Result<String, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }
    }

    deserializer.deserialize_any(StringOrNumber)
}

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for cover art download operations.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MbCoverDownloadParams {
    /// MusicBrainz Release ID (UUID format).
    #[schemars(description = "MusicBrainz Release ID (MBID) in UUID format")]
    pub mbid: String,

    /// Directory path where cover will be saved.
    #[schemars(description = "Target directory path (must be within allowed root)")]
    pub path: String,

    /// Filename without extension (default: "cover").
    #[serde(default = "default_filename")]
    #[schemars(description = "Output filename without extension (default: 'cover')")]
    pub filename: String,

    /// Preferred thumbnail size: "250", "500", "1200", or "original".
    #[serde(default = "default_thumbnail_size")]
    #[schemars(description = "Thumbnail size: 250, 500, 1200, or original (default: 500)")]
    pub thumbnail_size: String,

    /// Whether to overwrite existing file.
    #[serde(default)]
    #[schemars(description = "Overwrite existing file if present (default: false)")]
    pub overwrite: bool,
}

fn default_filename() -> String {
    "cover".to_string()
}

fn default_thumbnail_size() -> String {
    "500".to_string()
}

// ============================================================================
// Structured Output
// ============================================================================

/// Structured output for cover download results.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CoverDownloadResult {
    pub success: bool,
    pub file_path: String,
    pub file_size_bytes: u64,
    pub image_type: String,
    pub thumbnail_size: String,
    pub source_url: String,
}

// ============================================================================
// Tool Implementation
// ============================================================================

/// MusicBrainz Cover Art Download Tool implementation.
#[derive(Debug, Clone)]
pub struct MbCoverDownloadTool;

impl MbCoverDownloadTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "mb_cover_download";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Download cover art images for music releases from the Cover Art Archive. \
         Supports multiple thumbnail sizes (250, 500, 1200, or original) with intelligent fallback. \
         Prioritizes Front cover but falls back to other available images. \
         Returns structured data with file path, size, and image metadata.";

    pub fn new() -> Self {
        Self
    }

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    pub fn execute(params: &MbCoverDownloadParams, config: &Config) -> CallToolResult {
        info!(
            "Cover download tool called for MBID: {}, path: {}",
            params.mbid, params.path
        );

        // 1. Validate MBID format
        if !is_mbid(&params.mbid) {
            warn!("Invalid MBID format: {}", params.mbid);
            return error_result("Invalid MBID format (expected UUID)");
        }

        // 2. Reject unsafe filenames before any I/O. Without this check a value like
        // `"../escape"` would let `dir_path.join(...)` write outside the validated root.
        if !is_safe_filename(&params.filename) {
            warn!("Rejected unsafe filename: {:?}", params.filename);
            return error_result(
                "Invalid filename: must not be empty, start with '.', or contain path separators",
            );
        }

        // 3. Validate path with security
        let dir_path = match validate_path(&params.path, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };

        // 3. Verify it's a directory
        if !dir_path.is_dir() {
            warn!("Path is not a directory: {}", params.path);
            return error_result(&format!("Path is not a directory: {}", params.path));
        }

        // 4. Validate thumbnail_size
        if !matches!(
            params.thumbnail_size.as_str(),
            "250" | "500" | "1200" | "original"
        ) {
            warn!("Invalid thumbnail size: {}", params.thumbnail_size);
            return error_result("Invalid thumbnail size (use 250, 500, 1200, or original)");
        }

        // 5. Fetch coverart metadata from Cover Art Archive
        info!("Fetching cover art metadata for MBID: {}", params.mbid);
        let coverart = match Self::fetch_coverart(&params.mbid) {
            Ok(data) => data,
            Err(e) => {
                error!("Failed to fetch cover art: {}", e);
                return error_result(&format!("Failed to fetch cover art: {}", e));
            }
        };

        // 6. Select the best image (Front prioritized)
        let selected_image = match Self::select_best_image(&coverart) {
            Ok(img) => img,
            Err(e) => {
                warn!("No suitable image found: {}", e);
                return error_result(&format!("No suitable image found: {}", e));
            }
        };

        // 7. Get URL for requested size with fallback
        let (image_url, actual_size) = Self::get_image_url(selected_image, &params.thumbnail_size);

        // Validate URL
        if image_url.is_empty() {
            error!("Empty image URL received from API");
            return error_result("Invalid image URL received from Cover Art Archive");
        }

        info!(
            "Selected image URL ({}): {}",
            actual_size,
            image_url.chars().take(60).collect::<String>()
        );

        // 8. Download the image with proper HTTP client configuration
        let client = match reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to build HTTP client: {:?}", e);
                return error_result(&format!("Failed to build HTTP client: {}", e));
            }
        };

        // Convert HTTP URLs to HTTPS for Cover Art Archive
        let secure_url = if image_url.starts_with("http://coverartarchive.org") {
            image_url.replace("http://", "https://")
        } else {
            image_url.clone()
        };

        info!("Downloading from: {}", secure_url);

        let image_bytes = match client.get(&secure_url).send() {
            Ok(mut response) => {
                let status = response.status();
                if !status.is_success() {
                    error!(
                        "HTTP request failed with status: {} for URL: {}",
                        status, secure_url
                    );
                    return error_result(&format!(
                        "Failed to download image: HTTP {} - URL: {}",
                        status, secure_url
                    ));
                }

                // Reject upfront when the server declares an oversized body.
                if let Some(declared) = response.content_length()
                    && declared > MAX_COVER_BYTES
                {
                    error!(
                        "Cover Content-Length {} exceeds cap {} for URL: {}",
                        declared, MAX_COVER_BYTES, secure_url
                    );
                    return error_result(&format!(
                        "cover too large: {} bytes, max {}",
                        declared, MAX_COVER_BYTES
                    ));
                }

                match read_with_cap(&mut response, MAX_COVER_BYTES) {
                    Ok(bytes) => {
                        if bytes.is_empty() {
                            error!("Received empty response from: {}", secure_url);
                            return error_result("Failed to download image: Empty response");
                        }
                        bytes
                    }
                    Err(e) => {
                        error!("Cover download aborted: {} (URL: {})", e, secure_url);
                        return error_result(&e);
                    }
                }
            }
            Err(e) => {
                error!("HTTP request failed for URL {}: {:?}", secure_url, e);
                return error_result(&format!(
                    "Failed to download image from {}: {}",
                    secure_url, e
                ));
            }
        };

        // 9. Determine file extension from URL
        let extension = Self::detect_extension(&image_url);
        let full_filename = format!("{}.{}", params.filename, extension);
        let file_path = dir_path.join(&full_filename);

        // Defensive re-check: even with a sanitized filename, refuse to proceed if the
        // resulting path somehow escaped the validated directory.
        if !file_path.starts_with(&dir_path) {
            error!(
                "Resolved cover path '{}' escapes validated directory '{}'",
                file_path.display(),
                dir_path.display()
            );
            return error_result("Resolved cover path escapes the validated directory");
        }

        // 10. Check if file exists
        if file_path.exists() && !params.overwrite {
            warn!("File already exists: {}", file_path.display());
            return error_result(&format!(
                "File already exists: {}. Use overwrite=true to replace",
                file_path.display()
            ));
        }

        // 11. Write the file atomically: a partial download or process kill
        //     leaves the existing target (if any) untouched.
        if let Err(e) = write_atomic(&file_path, &image_bytes) {
            error!("Failed to write file: {:?}", e);
            return error_result(&format!("Failed to write file: {}", e));
        }

        // 12. Build result
        let image_type = if selected_image.front {
            "Front".to_string()
        } else if selected_image.back {
            "Back".to_string()
        } else {
            selected_image
                .types
                .first()
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string())
        };

        let result = CoverDownloadResult {
            success: true,
            file_path: file_path.display().to_string(),
            file_size_bytes: image_bytes.len() as u64,
            image_type: image_type.clone(),
            thumbnail_size: actual_size.clone(),
            source_url: secure_url,
        };

        let summary = format!(
            "Downloaded {} cover ({}) to {} ({} bytes)",
            image_type,
            actual_size,
            file_path.display(),
            result.file_size_bytes
        );

        info!("{}", summary);

        structured_result(summary, result)
    }

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let mbid = arguments
            .get("mbid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'mbid' parameter".to_string())?
            .to_string();

        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'path' parameter".to_string())?
            .to_string();

        let filename = arguments
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("cover")
            .to_string();

        let thumbnail_size = arguments
            .get("thumbnail_size")
            .and_then(|v| v.as_str())
            .unwrap_or("500")
            .to_string();

        let overwrite = arguments
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let params = MbCoverDownloadParams {
            mbid,
            path,
            filename,
            thumbnail_size,
            overwrite,
        };

        // Use std::thread::spawn to avoid nested runtime panic.
        // musicbrainz_rs and reqwest::blocking both create their own runtime.
        let handle = std::thread::spawn(move || Self::execute(&params, &config));

        let result = handle
            .join()
            .map_err(|_| "Thread panicked during cover download".to_string())?;

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<MbCoverDownloadParams>(),
        )
    }

    /// Create a ToolRoute for STDIO/TCP transport.
    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: MbCoverDownloadParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

                // `reqwest::blocking` issues blocking HTTP from its own internal
                // mini-runtime. `spawn_blocking` moves that work onto tokio's
                // blocking thread pool so the async runtime stays responsive.
                let result = tokio::task::spawn_blocking(move || Self::execute(&params, &config))
                    .await
                    .map_err(|e| {
                        McpError::internal_error(format!("Task execution failed: {}", e), None)
                    })?;

                Ok(result)
            }
            .boxed()
        })
    }

    // ========================================================================
    // Helper Functions
    // ========================================================================

    /// Fetch coverart metadata from Cover Art Archive API.
    fn fetch_coverart(mbid: &str) -> Result<Coverart, String> {
        let url = format!("https://coverartarchive.org/release/{}", mbid);

        info!("Fetching cover art from: {}", url);

        let client = reqwest::blocking::Client::builder()
            .user_agent("MusicMCPServer/0.1.0")
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        let response = client
            .get(&url)
            .send()
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = response.status();
        if !status.is_success() {
            // Provide more helpful error message for 404
            if status.as_u16() == 404 {
                return Err(format!(
                    "No cover art available for this release (MBID: {}). The release may not have any uploaded cover art in the Cover Art Archive.",
                    mbid
                ));
            }
            return Err(format!(
                "HTTP {} - {}",
                status,
                status.canonical_reason().unwrap_or("Unknown error")
            ));
        }

        let json_text = response
            .text()
            .map_err(|e| format!("Failed to read response text: {}", e))?;

        info!("Received JSON response ({} bytes)", json_text.len());

        serde_json::from_str(&json_text).map_err(|e| {
            format!(
                "Failed to parse JSON: {} - Response: {}",
                e,
                &json_text.chars().take(200).collect::<String>()
            )
        })
    }

    /// Select the best image (Front prioritized, fallback to first available).
    fn select_best_image(coverart: &Coverart) -> Result<&CoverartImage, &'static str> {
        if coverart.images.is_empty() {
            return Err("No images available");
        }

        // Priority 1: Image marked as "front"
        if let Some(img) = coverart.images.iter().find(|img| img.front) {
            return Ok(img);
        }

        // Priority 2: Image with type "Front"
        if let Some(img) = coverart
            .images
            .iter()
            .find(|img| img.types.iter().any(|t| t.eq_ignore_ascii_case("front")))
        {
            return Ok(img);
        }

        // Fallback: First available image
        coverart
            .images
            .first()
            .ok_or("No images available after fallback")
    }

    /// Get URL for requested size with intelligent fallback.
    /// Supports both new format (res_250, res_500, res_1200) and legacy format (small, large).
    fn get_image_url(image: &CoverartImage, requested_size: &str) -> (String, String) {
        match requested_size {
            "250" => {
                // Try 250 (new format), fallback to small (legacy), then 500, 1200, original
                image
                    .thumbnails
                    .res_250
                    .clone()
                    .or_else(|| image.thumbnails.small.clone())
                    .map(|url| (url, "250".to_string()))
                    .or_else(|| {
                        image
                            .thumbnails
                            .res_500
                            .clone()
                            .or_else(|| image.thumbnails.large.clone())
                            .map(|url| (url, "500".to_string()))
                    })
                    .or_else(|| {
                        image
                            .thumbnails
                            .res_1200
                            .clone()
                            .map(|url| (url, "1200".to_string()))
                    })
                    .unwrap_or_else(|| (image.image.clone(), "original".to_string()))
            }
            "500" => {
                // Try 500 (new format), fallback to large (legacy), then 1200, 250/small, original
                image
                    .thumbnails
                    .res_500
                    .clone()
                    .or_else(|| image.thumbnails.large.clone())
                    .map(|url| (url, "500".to_string()))
                    .or_else(|| {
                        image
                            .thumbnails
                            .res_1200
                            .clone()
                            .map(|url| (url, "1200".to_string()))
                    })
                    .or_else(|| {
                        image
                            .thumbnails
                            .res_250
                            .clone()
                            .or_else(|| image.thumbnails.small.clone())
                            .map(|url| (url, "250".to_string()))
                    })
                    .unwrap_or_else(|| (image.image.clone(), "original".to_string()))
            }
            "1200" => {
                // Try 1200, fallback to large/500 (legacy), then original
                image
                    .thumbnails
                    .res_1200
                    .clone()
                    .map(|url| (url, "1200".to_string()))
                    .or_else(|| {
                        image
                            .thumbnails
                            .res_500
                            .clone()
                            .or_else(|| image.thumbnails.large.clone())
                            .map(|url| (url, "500".to_string()))
                    })
                    .unwrap_or_else(|| (image.image.clone(), "original".to_string()))
            }
            // Wildcard captures both `"original"` and any unexpected value —
            // we always fall back to the full-resolution image.
            _ => (image.image.clone(), "original".to_string()),
        }
    }

    /// Detect file extension from URL.
    fn detect_extension(url: &str) -> String {
        if url.ends_with(".png") {
            "png"
        } else if url.ends_with(".jpg") || url.ends_with(".jpeg") {
            "jpg"
        } else if url.ends_with(".gif") {
            "gif"
        } else if url.ends_with(".webp") {
            "webp"
        } else {
            // Fallback: jpg (most common format)
            "jpg"
        }
        .to_string()
    }
}

impl Default for MbCoverDownloadTool {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_defaults() {
        let json = r#"{"mbid": "65c70b9f-fdef-4bc0-a5b6-ac4e34252d3c", "path": "/tmp"}"#;
        let params: MbCoverDownloadParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.filename, "cover");
        assert_eq!(params.thumbnail_size, "500");
        assert_eq!(params.overwrite, false);
    }

    /// Regression test: a `filename` containing `..` must be rejected before any
    /// network call or filesystem write, even when `path` is a validated directory
    /// inside the configured root.
    #[test]
    fn cover_download_filename_traversal() {
        use crate::core::config::SecurityConfig;
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let mut config = Config::default();
        config.security = SecurityConfig {
            root_path: Some(root.path().to_path_buf()),
            allow_symlinks: true,
        };

        let params = MbCoverDownloadParams {
            mbid: "65c70b9f-fdef-4bc0-a5b6-ac4e34252d3c".to_string(),
            path: root.path().to_string_lossy().to_string(),
            filename: "../escape".to_string(),
            thumbnail_size: "500".to_string(),
            overwrite: false,
        };

        let result = MbCoverDownloadTool::execute(&params, &config);

        assert!(
            result.is_error.unwrap_or(false),
            "Traversal filename should be rejected"
        );

        // The validated root must remain empty — no file written anywhere.
        let entries: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "No file should be written when filename is rejected"
        );

        // And nothing landed at the would-be escape location either.
        let parent_dir = root.path().parent().unwrap();
        let escape_jpg = parent_dir.join("escape.jpg");
        assert!(
            !escape_jpg.exists(),
            "Traversal target {:?} must not exist",
            escape_jpg
        );
    }

    #[test]
    fn test_params_custom() {
        let json = r#"{
            "mbid": "65c70b9f-fdef-4bc0-a5b6-ac4e34252d3c",
            "path": "/tmp",
            "filename": "album_art",
            "thumbnail_size": "1200",
            "overwrite": true
        }"#;
        let params: MbCoverDownloadParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.filename, "album_art");
        assert_eq!(params.thumbnail_size, "1200");
        assert_eq!(params.overwrite, true);
    }

    #[test]
    fn read_with_cap_under_limit() {
        let data = vec![0u8; 1024];
        let mut cursor = std::io::Cursor::new(data.clone());
        let out = read_with_cap(&mut cursor, 4096).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn read_with_cap_exactly_at_limit() {
        let data = vec![1u8; 4096];
        let mut cursor = std::io::Cursor::new(data.clone());
        let out = read_with_cap(&mut cursor, 4096).unwrap();
        assert_eq!(out.len(), 4096);
    }

    #[test]
    fn read_with_cap_over_limit_rejected() {
        let data = vec![2u8; 4097];
        let mut cursor = std::io::Cursor::new(data);
        let err = read_with_cap(&mut cursor, 4096).expect_err("should reject");
        assert!(
            err.contains("cover too large"),
            "unexpected error message: {}",
            err
        );
    }

    #[test]
    fn read_with_cap_constant_is_50mb() {
        assert_eq!(MAX_COVER_BYTES, 50 * 1024 * 1024);
    }

    #[test]
    fn test_extension_detection() {
        assert_eq!(
            MbCoverDownloadTool::detect_extension("https://example.com/image.jpg"),
            "jpg"
        );
        assert_eq!(
            MbCoverDownloadTool::detect_extension("https://example.com/image.jpeg"),
            "jpg"
        );
        assert_eq!(
            MbCoverDownloadTool::detect_extension("https://example.com/image.png"),
            "png"
        );
        assert_eq!(
            MbCoverDownloadTool::detect_extension("https://example.com/image.gif"),
            "gif"
        );
        assert_eq!(
            MbCoverDownloadTool::detect_extension("https://example.com/image.webp"),
            "webp"
        );
        assert_eq!(
            MbCoverDownloadTool::detect_extension("https://example.com/noext"),
            "jpg"
        ); // Fallback
    }

    #[test]
    fn test_url_selection_legacy_format() {
        // Test with legacy format (small/large only)
        let thumbnail = Thumbnail {
            small: Some("http://example.com/image-250.jpg".to_string()),
            large: Some("http://example.com/image-500.jpg".to_string()),
            res_250: None,
            res_500: None,
            res_1200: None,
        };

        let image = CoverartImage {
            approved: true,
            back: false,
            comment: String::new(),
            edit: 0,
            front: true,
            id: "12345".to_string(),
            image: "http://example.com/image-original.jpg".to_string(),
            thumbnails: thumbnail,
            types: vec!["Front".to_string()],
        };

        // Test 250px - should use "small" field
        let (url, size) = MbCoverDownloadTool::get_image_url(&image, "250");
        assert_eq!(url, "http://example.com/image-250.jpg");
        assert_eq!(size, "250");

        // Test 500px - should use "large" field
        let (url, size) = MbCoverDownloadTool::get_image_url(&image, "500");
        assert_eq!(url, "http://example.com/image-500.jpg");
        assert_eq!(size, "500");

        // Test 1200px - should fallback to "large"
        let (url, size) = MbCoverDownloadTool::get_image_url(&image, "1200");
        assert_eq!(url, "http://example.com/image-500.jpg");
        assert_eq!(size, "500");
    }

    #[test]
    fn test_url_selection_new_format() {
        // Test with new format (res_250, res_500, res_1200)
        let thumbnail = Thumbnail {
            small: None,
            large: None,
            res_250: Some("http://example.com/image-250.jpg".to_string()),
            res_500: Some("http://example.com/image-500.jpg".to_string()),
            res_1200: Some("http://example.com/image-1200.jpg".to_string()),
        };

        let image = CoverartImage {
            approved: true,
            back: false,
            comment: String::new(),
            edit: 0,
            front: true,
            id: "12345".to_string(),
            image: "http://example.com/image-original.jpg".to_string(),
            thumbnails: thumbnail,
            types: vec!["Front".to_string()],
        };

        let (url, size) = MbCoverDownloadTool::get_image_url(&image, "250");
        assert_eq!(url, "http://example.com/image-250.jpg");
        assert_eq!(size, "250");

        let (url, size) = MbCoverDownloadTool::get_image_url(&image, "1200");
        assert_eq!(url, "http://example.com/image-1200.jpg");
        assert_eq!(size, "1200");
    }

    // Network tests (require actual internet connection, run with --ignored)
    #[ignore]
    #[test]
    fn test_download_real_cover() {
        use tempfile::TempDir;

        // Respect rate limiting
        std::thread::sleep(std::time::Duration::from_millis(1500));

        let temp_dir = TempDir::new().unwrap();
        let params = MbCoverDownloadParams {
            mbid: "65c70b9f-fdef-4bc0-a5b6-ac4e34252d3c".to_string(),
            path: temp_dir.path().to_string_lossy().to_string(),
            filename: "test_cover".to_string(),
            thumbnail_size: "250".to_string(),
            overwrite: false,
        };

        let config = Config::default();
        let result = MbCoverDownloadTool::execute(&params, &config);

        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success but got error"
        );

        // Verify file was created
        let files: Vec<_> = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();

        assert!(
            !files.is_empty(),
            "Expected at least one file to be created"
        );

        // Verify structured content
        if let Some(structured) = result.structured_content {
            let cover_result: CoverDownloadResult = serde_json::from_value(structured).unwrap();
            assert!(cover_result.success);
            assert!(cover_result.file_size_bytes > 0);
            assert!(cover_result.file_path.contains("test_cover"));
        }
    }

    #[ignore]
    #[test]
    fn test_download_original_size() {
        use tempfile::TempDir;

        std::thread::sleep(std::time::Duration::from_millis(1500));

        let temp_dir = TempDir::new().unwrap();
        let params = MbCoverDownloadParams {
            mbid: "65c70b9f-fdef-4bc0-a5b6-ac4e34252d3c".to_string(),
            path: temp_dir.path().to_string_lossy().to_string(),
            filename: "original_cover".to_string(),
            thumbnail_size: "original".to_string(),
            overwrite: false,
        };

        let config = Config::default();
        let result = MbCoverDownloadTool::execute(&params, &config);

        if result.is_error.unwrap_or(true) {
            eprintln!("Error content: {:?}", result.content);
        }
        assert!(!result.is_error.unwrap_or(true), "Expected success");
    }

    #[ignore]
    #[test]
    fn test_download_legacy_format() {
        use tempfile::TempDir;

        std::thread::sleep(std::time::Duration::from_millis(1500));

        let temp_dir = TempDir::new().unwrap();
        // This MBID returns legacy format (small/large instead of res_250/res_500)
        let params = MbCoverDownloadParams {
            mbid: "b70e194e-29ba-4c2e-9f30-d8d2df6f5f42".to_string(),
            path: temp_dir.path().to_string_lossy().to_string(),
            filename: "legacy_cover".to_string(),
            thumbnail_size: "500".to_string(),
            overwrite: false,
        };

        let config = Config::default();
        let result = MbCoverDownloadTool::execute(&params, &config);

        if result.is_error.unwrap_or(true) {
            eprintln!("Error content: {:?}", result.content);
        }
        assert!(
            !result.is_error.unwrap_or(true),
            "Expected success with legacy format"
        );

        // Verify structured content
        if let Some(structured) = result.structured_content {
            let cover_result: CoverDownloadResult = serde_json::from_value(structured).unwrap();
            assert!(cover_result.success);
            assert!(cover_result.file_size_bytes > 0);
            assert_eq!(cover_result.thumbnail_size, "500");
            assert_eq!(cover_result.image_type, "Front");
        } else {
            panic!("Expected structured content");
        }
    }
}
