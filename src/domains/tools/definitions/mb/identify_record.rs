//! MusicBrainz audio identification tool using AcoustID/Chromaprint.
//!
//! This tool identifies audio files by their acoustic fingerprint, even when
//! metadata is missing or incorrect (e.g., files downloaded from YouTube).

use futures::FutureExt;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::validate_path;

// ============================================================================
// Configuration & Constants
// ============================================================================

const ACOUSTID_API_URL: &str = "https://api.acoustid.org/v2/lookup";
const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 1000;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const MAX_RESULT_LIMIT: usize = 10;

// ============================================================================
// Structured Output Types
// ============================================================================

/// Structured output for audio identification results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IdentificationResult {
    pub file: String,
    pub metadata_level: String,
    pub matches: Vec<FingerprintMatch>,
    pub status: String,
}

/// A single fingerprint match from AcoustID.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FingerprintMatch {
    pub rank: usize,
    pub confidence: f64,
    pub acoustid: String,
    pub recordings: Vec<RecordingMatch>,
}

/// Recording information from a match.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingMatch {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artists: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_groups: Option<Vec<ReleaseGroupMatch>>,
}

/// Release group information.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReleaseGroupMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
}

// ============================================================================
// Tool Parameters
// ============================================================================

/// Metadata detail level for AcoustID API responses.
///
/// Controls how much information is retrieved from the AcoustID database.
/// Higher levels provide more data but may take slightly longer to process.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(rename_all = "lowercase")]
pub enum MetadataLevel {
    /// Only MusicBrainz recording IDs (fastest, use when you only need IDs for further queries)
    Minimal,
    /// Recording IDs with title, artists, and duration (recommended for most cases)
    Basic,
    /// Complete metadata including release groups, albums, formats, and dates
    Full,
}

impl MetadataLevel {
    /// Convert to AcoustID API meta parameter value.
    ///
    /// Based on real API testing:
    /// - "recordingids" returns only IDs
    /// - "recordings" returns IDs + title + artists + duration
    /// - "recordings releasegroups compress" returns everything + album info
    fn as_api_param(self) -> &'static str {
        match self {
            Self::Minimal => "recordingids",
            Self::Basic => "recordings",
            Self::Full => "recordings releasegroups compress",
        }
    }

    /// Stable wire string for this level. Mirrors the JSON serde form so the
    /// output never silently shifts if a variant gets renamed in the Rust enum.
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Basic => "basic",
            Self::Full => "full",
        }
    }
}

impl Default for MetadataLevel {
    fn default() -> Self {
        Self::Basic
    }
}

/// Parameters for the audio identification tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MbIdentifyRecordParams {
    /// Path to the audio file to identify
    pub file_path: String,

    /// Maximum number of results to return (default: 3, max: 10)
    #[serde(default = "default_result_limit")]
    pub limit: usize,

    /// Metadata detail level (default: basic)
    #[serde(default)]
    pub metadata_level: MetadataLevel,
}

fn default_result_limit() -> usize {
    3
}

// ============================================================================
// AcoustID API Response Structures
// ============================================================================

#[derive(Debug, Deserialize)]
struct AcoustIDResponse {
    status: String,
    #[serde(default)]
    results: Vec<AcoustIDResult>,
    error: Option<AcoustIDError>,
}

#[derive(Debug, Deserialize)]
struct AcoustIDResult {
    id: String,
    score: f64,
    #[serde(default)]
    recordings: Vec<AcoustIDRecording>,
}

// Only the fields actually consumed at the call sites are typed. Serde silently
// drops the rest of the AcoustID response — no `#[serde(deny_unknown_fields)]`.
#[derive(Debug, Deserialize)]
struct AcoustIDRecording {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    artists: Vec<AcoustIDArtist>,
    #[serde(default)]
    releasegroups: Vec<AcoustIDReleaseGroup>,
}

#[derive(Debug, Clone, Deserialize)]
struct AcoustIDArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIDReleaseGroup {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    artists: Vec<AcoustIDArtist>,
    #[serde(default)]
    releases: Vec<AcoustIDRelease>,
}

#[derive(Debug, Deserialize)]
struct AcoustIDRelease {
    #[serde(default)]
    mediums: Vec<AcoustIDMedium>,
}

#[derive(Debug, Deserialize)]
struct AcoustIDMedium {
    #[serde(default)]
    tracks: Vec<AcoustIDTrack>,
}

#[derive(Debug, Deserialize)]
struct AcoustIDTrack {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AcoustIDError {
    message: String,
}

// ============================================================================
// Helper Structures
// ============================================================================

#[derive(Debug)]
struct FingerprintData {
    duration: u32,
    fingerprint: String,
}

#[derive(Debug, Deserialize)]
struct FpcalcOutput {
    duration: f64,
    fingerprint: String,
}

// ============================================================================
// Custom Error Types
// ============================================================================

#[derive(Debug, thiserror::Error)]
enum IdentificationError {
    #[error("Audio file not found or inaccessible: {0}")]
    FileNotFound(String),

    #[error("Chromaprint (fpcalc) is not installed.\n{0}")]
    FpcalcNotFound(String),

    #[error("Failed to generate audio fingerprint: {0}")]
    FingerprintFailed(String),

    #[error("AcoustID API request failed: {0}")]
    ApiError(String),

    #[error(
        "No matches found in AcoustID database.\n\
             This audio file may not be indexed yet, or the quality might be too low for accurate fingerprinting.\n\
             Try:\n  \
             - Ensuring the file is not corrupted\n  \
             - Using a higher quality source\n  \
             - Checking if this is a very obscure or unreleased recording"
    )]
    NoMatches,

    #[error("Invalid API response: {0}")]
    InvalidResponse(String),

    #[error(
        "AcoustID API key is invalid or expired.\n\
            Please set your own API key via environment variable: MCP_ACOUSTID_API_KEY\n\
            You can request a free API key at: https://acoustid.org/api-key"
    )]
    InvalidApiKey,

    #[error(
        "AcoustID API key is not configured.\n\
            Set MCP_ACOUSTID_API_KEY before running mb_identify_record.\n\
            Get a free key at: https://acoustid.org/api-key"
    )]
    MissingApiKey,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// MusicBrainz audio identification tool.
pub struct MbIdentifyRecordTool;

impl MbIdentifyRecordTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "mb_identify_record";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Identify audio files using acoustic fingerprinting via AcoustID/Chromaprint.\n\
         \n\
         Ideal for identifying songs when:\n\
         - Metadata is missing, incorrect, or incomplete\n\
         - Files were downloaded from YouTube, SoundCloud, or other streaming platforms\n\
         - Files were converted from video to audio formats\n\
         - Files have been renamed or lack proper ID3 tags\n\
         \n\
         This tool analyzes the actual audio waveform and matches it against the AcoustID database\n\
         (linked to MusicBrainz). Returns structured data with concise summary including:\n\
         - Confidence scores for each match\n\
         - MusicBrainz Recording IDs\n\
         - Artist names and titles\n\
         - Release groups and album information (with full metadata level)\n\
         \n\
         Supports all common audio formats: MP3, FLAC, WAV, OGG, M4A, AAC, WMA, OPUS, and more.";

    /// Execute the tool logic.
    #[instrument(skip_all, fields(file_path = %params.file_path, limit = params.limit))]
    pub fn execute(params: &MbIdentifyRecordParams, config: &Config) -> CallToolResult {
        info!("Starting audio identification");

        // Bail out before any I/O if no key is configured: the AcoustID API
        // requires one, and there is no embedded fallback.
        let api_key = match config.credentials.acoustid_api_key.as_deref() {
            Some(key) if !key.is_empty() => key,
            _ => {
                let err = IdentificationError::MissingApiKey;
                error!("{}", err);
                return CallToolResult::error(vec![Content::text(err.to_string())]);
            }
        };

        match Self::identify_audio_internal(params, api_key, config) {
            Ok((summary, structured_data)) => {
                info!("Audio identification completed successfully");
                match serde_json::to_value(&structured_data) {
                    Ok(structured) => CallToolResult {
                        content: vec![Content::text(summary)],
                        structured_content: Some(structured),
                        is_error: Some(false),
                        meta: None,
                    },
                    Err(e) => {
                        warn!("Failed to serialize structured content: {}", e);
                        // Fallback to text-only
                        CallToolResult::success(vec![Content::text(summary)])
                    }
                }
            }
            Err(e) => {
                error!("Audio identification failed: {}", e);
                CallToolResult::error(vec![Content::text(e.to_string())])
            }
        }
    }

    /// Internal identification logic with proper error handling.
    fn identify_audio_internal(
        params: &MbIdentifyRecordParams,
        api_key: &str,
        config: &Config,
    ) -> Result<(String, IdentificationResult), IdentificationError> {
        // Validate path security first
        validate_path(&params.file_path, config).map_err(|e| {
            IdentificationError::FileNotFound(format!("Path security validation failed: {}", e))
        })?;

        // Validate file exists and is accessible
        Self::validate_file(&params.file_path)?;

        // Validate and clamp limit
        let limit = params.limit.clamp(1, MAX_RESULT_LIMIT);

        // Generate fingerprint
        let fingerprint_data = Self::generate_fingerprint(&params.file_path)?;

        // Query API
        let response = Self::query_acoustid(api_key, &fingerprint_data, params.metadata_level)?;

        // Build structured result and summary
        Self::build_results(&response, &params.file_path, limit, &params.metadata_level)
    }

    /// Validate that the file exists and is accessible.
    fn validate_file(file_path: &str) -> Result<(), IdentificationError> {
        let path = Path::new(file_path);

        if !path.exists() {
            return Err(IdentificationError::FileNotFound(file_path.to_string()));
        }

        if !path.is_file() {
            return Err(IdentificationError::FileNotFound(format!(
                "{file_path} is not a regular file (it may be a directory or symlink)"
            )));
        }

        // Check if file is readable
        if let Err(e) = std::fs::metadata(path) {
            return Err(IdentificationError::FileNotFound(format!(
                "Cannot access {file_path}: {e}"
            )));
        }

        Ok(())
    }

    /// Generate audio fingerprint using fpcalc command-line tool.
    #[instrument(skip_all, fields(file = %file_path))]
    fn generate_fingerprint(file_path: &str) -> Result<FingerprintData, IdentificationError> {
        // Check if fpcalc is installed
        if !Self::is_fpcalc_installed() {
            return Err(IdentificationError::FpcalcNotFound(
                "Installation instructions:\n\
                 • Linux (Debian/Ubuntu): sudo apt-get install libchromaprint-tools\n\
                 • Linux (Fedora/RHEL):   sudo dnf install chromaprint-tools\n\
                 • macOS:                 brew install chromaprint\n\
                 • Windows:               Download from https://acoustid.org/chromaprint\n\
                 \nAfter installation, verify with: fpcalc -version"
                    .to_string(),
            ));
        }

        debug!("Running fpcalc on {}", file_path);

        // Run fpcalc to generate fingerprint
        let output = Command::new("fpcalc")
            .arg("-json")
            .arg(file_path)
            .output()
            .map_err(|e| {
                IdentificationError::FingerprintFailed(format!("Failed to run fpcalc: {}", e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(IdentificationError::FingerprintFailed(stderr));
        }

        // Parse output directly from bytes (more efficient than converting to string first)
        let fpcalc_output: FpcalcOutput = serde_json::from_slice(&output.stdout).map_err(|e| {
            IdentificationError::FingerprintFailed(format!("Invalid JSON output: {e}"))
        })?;

        let duration = fpcalc_output.duration as u32;
        let fingerprint = fpcalc_output.fingerprint;

        debug!(
            "Generated fingerprint: duration={duration}s, len={}",
            fingerprint.len()
        );

        Ok(FingerprintData {
            duration,
            fingerprint,
        })
    }

    /// Check if fpcalc is installed on the system.
    fn is_fpcalc_installed() -> bool {
        Command::new("fpcalc").arg("-version").output().is_ok()
    }

    /// Query the AcoustID API with the fingerprint.
    #[instrument(skip(fingerprint_data), fields(duration = fingerprint_data.duration, metadata_level = ?metadata_level))]
    fn query_acoustid(
        api_key: &str,
        fingerprint_data: &FingerprintData,
        metadata_level: MetadataLevel,
    ) -> Result<AcoustIDResponse, IdentificationError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| {
                IdentificationError::ApiError(format!("Failed to create HTTP client: {}", e))
            })?;

        let mut last_error = String::new();

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                let delay = BASE_DELAY_MS * 2u64.pow(attempt - 1);
                debug!(
                    "Retrying (attempt {}/{}) after {}ms",
                    attempt + 1,
                    MAX_RETRIES,
                    delay
                );
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }

            match Self::try_api_request(&client, api_key, fingerprint_data, metadata_level) {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Don't retry on API key errors - fail fast
                    if matches!(e, IdentificationError::InvalidApiKey) {
                        return Err(e);
                    }
                    last_error = e.to_string();
                    warn!("API request attempt {} failed: {}", attempt + 1, last_error);
                }
            }
        }

        Err(IdentificationError::ApiError(format!(
            "All {} retry attempts failed. Last error: {}\n\n\
             Troubleshooting:\n\
             • Check AcoustID service status: https://acoustid.org/\n\
             • Verify network connectivity (can you reach acoustid.org?)\n\
             • Rate limit exceeded? Wait 2-5 minutes before retrying\n\
             • Firewall or proxy blocking api.acoustid.org?\n\
             • For persistent issues, try using a custom API key",
            MAX_RETRIES, last_error
        )))
    }

    /// Attempt a single API request.
    fn try_api_request(
        client: &reqwest::blocking::Client,
        api_key: &str,
        fingerprint_data: &FingerprintData,
        metadata_level: MetadataLevel,
    ) -> Result<AcoustIDResponse, IdentificationError> {
        // Pre-format duration string to avoid allocation in form builder
        let duration_str = fingerprint_data.duration.to_string();

        let response = client
            .post(ACOUSTID_API_URL)
            .form(&[
                ("client", api_key),
                ("duration", duration_str.as_str()),
                ("fingerprint", fingerprint_data.fingerprint.as_str()),
                ("meta", metadata_level.as_api_param()),
            ])
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    IdentificationError::ApiError("Request timed out".to_string())
                } else if e.is_connect() {
                    IdentificationError::ApiError("Connection failed".to_string())
                } else {
                    IdentificationError::ApiError(e.to_string())
                }
            })?;

        let status = response.status();

        // Handle rate limiting
        if status.as_u16() == 429 {
            return Err(IdentificationError::ApiError(
                "Rate limit exceeded".to_string(),
            ));
        }

        // Handle server errors (retryable)
        if status.is_server_error() {
            return Err(IdentificationError::ApiError(format!(
                "Server error ({})",
                status
            )));
        }

        // Handle client errors (not retryable)
        if status.is_client_error() {
            // Specific handling for authentication errors (invalid API key)
            if status.as_u16() == 400 || status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(IdentificationError::InvalidApiKey);
            }
            return Err(IdentificationError::ApiError(format!(
                "Client error ({status}): Check your request parameters"
            )));
        }

        // Parse response directly from bytes for better performance
        let response_bytes = response
            .bytes()
            .map_err(|e| IdentificationError::InvalidResponse(e.to_string()))?;

        debug!("API response received: {} bytes", response_bytes.len());

        let acoustid_response: AcoustIDResponse = serde_json::from_slice(&response_bytes)
            .map_err(|e| IdentificationError::InvalidResponse(format!("JSON parse error: {e}")))?;

        // Check for API-level errors
        if acoustid_response.status != "ok" {
            if let Some(error) = acoustid_response.error {
                return Err(IdentificationError::ApiError(error.message));
            }
            return Err(IdentificationError::ApiError(
                "Unknown API error".to_string(),
            ));
        }

        Ok(acoustid_response)
    }


    /// Build both structured results and text summary.
    fn build_results(
        response: &AcoustIDResponse,
        file_path: &str,
        limit: usize,
        metadata_level: &MetadataLevel,
    ) -> Result<(String, IdentificationResult), IdentificationError> {
        if response.results.is_empty() {
            return Err(IdentificationError::NoMatches);
        }

        // Build structured data
        let mut matches = Vec::new();

        for (i, result) in response.results.iter().take(limit).enumerate() {
            let mut recordings = Vec::new();

            for recording in &result.recordings {
                // Extract title and artists based on metadata level
                let (title_opt, artists_opt) = match metadata_level {
                    MetadataLevel::Minimal => (None, None),
                    MetadataLevel::Basic | MetadataLevel::Full => {
                        let (title, artists_vec) = if !recording.releasegroups.is_empty() {
                            let rg = &recording.releasegroups[0];
                            let track_title = rg
                                .releases
                                .first()
                                .and_then(|rel| rel.mediums.first())
                                .and_then(|med| med.tracks.first())
                                .and_then(|track| track.title.as_ref())
                                .map(|s| s.to_string());

                            let artists = if !rg.artists.is_empty() {
                                rg.artists.clone()
                            } else {
                                recording.artists.clone()
                            };

                            (track_title.or_else(|| recording.title.clone()), artists)
                        } else {
                            (recording.title.clone(), recording.artists.clone())
                        };

                        let artist_names = if !artists_vec.is_empty() {
                            Some(artists_vec.iter().map(|a| a.name.clone()).collect())
                        } else {
                            None
                        };

                        (title, artist_names)
                    }
                };

                // Extract release groups for Full metadata level
                let release_groups = if matches!(metadata_level, MetadataLevel::Full) {
                    let groups: Vec<ReleaseGroupMatch> = recording
                        .releasegroups
                        .iter()
                        .map(|rg| ReleaseGroupMatch {
                            id: rg.id.clone(),
                            name: rg.title.clone().unwrap_or_else(|| "Untitled".to_string()),
                            r#type: rg.r#type.clone(),
                        })
                        .collect();

                    if groups.is_empty() {
                        None
                    } else {
                        Some(groups)
                    }
                } else {
                    None
                };

                recordings.push(RecordingMatch {
                    id: recording.id.clone(),
                    title: title_opt,
                    duration: recording.duration.map(|d| d as u32),
                    artists: artists_opt,
                    release_groups,
                });
            }

            matches.push(FingerprintMatch {
                rank: i + 1,
                confidence: result.score,
                acoustid: result.id.clone(),
                recordings,
            });
        }

        let structured_data = IdentificationResult {
            file: file_path.to_string(),
            metadata_level: metadata_level.as_str().to_string(),
            matches,
            status: "success".to_string(),
        };

        // Build text summary
        let summary = Self::build_text_summary(&structured_data, metadata_level);

        Ok((summary, structured_data))
    }

    /// Build a concise text summary from structured data.
    fn build_text_summary(
        data: &IdentificationResult,
        metadata_level: &MetadataLevel,
    ) -> String {
        if data.matches.is_empty() {
            return "No matches found".to_string();
        }

        // Build a concise summary
        let total_matches = data.matches.len();
        let best_match = &data.matches[0];
        let confidence_pct = (best_match.confidence * 100.0) as u32;

        // Try to get title and artist from best match
        let best_recording = best_match.recordings.first();

        let (title_str, artist_str) = if let Some(rec) = best_recording {
            let title = rec.title.as_deref().unwrap_or("Unknown");
            let artists = rec.artists.as_ref()
                .map(|a| a.join(", "))
                .unwrap_or_else(|| "Unknown Artist".to_string());
            (title, artists)
        } else {
            ("Unknown", "Unknown Artist".to_string())
        };

        match metadata_level {
            MetadataLevel::Minimal => {
                format!(
                    "Identified audio: {} match(es) found (best: {}% confidence, Recording ID: {})",
                    total_matches,
                    confidence_pct,
                    best_recording.map(|r| r.id.as_str()).unwrap_or("N/A")
                )
            }
            MetadataLevel::Basic => {
                format!(
                    "Identified: '{}' by {} ({}% confidence, {} match(es))",
                    title_str, artist_str, confidence_pct, total_matches
                )
            }
            MetadataLevel::Full => {
                let release_count = best_recording
                    .and_then(|r| r.release_groups.as_ref())
                    .map(|rg| rg.len())
                    .unwrap_or(0);

                if release_count > 0 {
                    format!(
                        "Identified: '{}' by {} ({}% confidence, {} release group(s), {} total match(es))",
                        title_str, artist_str, confidence_pct, release_count, total_matches
                    )
                } else {
                    format!(
                        "Identified: '{}' by {} ({}% confidence, {} match(es))",
                        title_str, artist_str, confidence_pct, total_matches
                    )
                }
            }
        }
    }

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: MbIdentifyRecordParams =
            serde_json::from_value(arguments).map_err(|e| format!("Invalid parameters: {}", e))?;

        info!(
            "Audio identification tool (HTTP) called for: {}",
            params.file_path
        );

        let handle = std::thread::spawn(move || Self::execute(&params, &config));

        let result = handle
            .join()
            .map_err(|_| "Identification thread panicked".to_string())?;

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<MbIdentifyRecordParams>(),
            annotations: None,
            output_schema: None,
            icons: None,
            meta: None,
            title: None,
        }
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
                let params: MbIdentifyRecordParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

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
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_file_not_found() {
        let result = MbIdentifyRecordTool::validate_file("/nonexistent/file.mp3");
        assert!(matches!(result, Err(IdentificationError::FileNotFound(_))));
    }

    #[test]
    fn test_default_limit() {
        assert_eq!(default_result_limit(), 3);
    }

    #[test]
    fn test_params_deserialization() {
        let json = r#"{"file_path": "test.mp3"}"#;
        let params: MbIdentifyRecordParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.file_path, "test.mp3");
        assert_eq!(params.limit, 3);
    }

    #[test]
    fn test_mb_identify_missing_file() {
        let mut config = Config::default();
        // Provide a placeholder key so the missing-file error path is exercised
        // instead of being short-circuited by MissingApiKey.
        config.credentials.acoustid_api_key = Some("placeholder".to_string());

        let params = MbIdentifyRecordParams {
            file_path: "/nonexistent/file.mp3".to_string(),
            limit: 3,
            metadata_level: MetadataLevel::Basic,
        };

        let result = MbIdentifyRecordTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));
    }

    #[test]
    fn test_mb_identify_missing_api_key_short_circuits() {
        // Default config has no AcoustID key. The tool must refuse before any
        // I/O — even an obviously bogus file path should not produce a "file
        // not found" error.
        let config = Config::default();
        assert!(config.credentials.acoustid_api_key.is_none());

        let params = MbIdentifyRecordParams {
            file_path: "/nonexistent/file.mp3".to_string(),
            limit: 3,
            metadata_level: MetadataLevel::Basic,
        };

        let result = MbIdentifyRecordTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));

        let body = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("MCP_ACOUSTID_API_KEY")
                && body.contains("https://acoustid.org/api-key"),
            "expected MissingApiKey guidance, got: {}",
            body
        );
    }

    #[test]
    fn test_mb_identify_empty_api_key_short_circuits() {
        // An empty string is treated the same as None.
        let mut config = Config::default();
        config.credentials.acoustid_api_key = Some(String::new());

        let params = MbIdentifyRecordParams {
            file_path: "/nonexistent/file.mp3".to_string(),
            limit: 3,
            metadata_level: MetadataLevel::Basic,
        };

        let result = MbIdentifyRecordTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));
    }

    #[test]
    #[ignore = "requires test audio file"]
    fn test_mb_identify_integration() {
        let config = Config::default();
        let test_file = "test_audio.mp3";

        if !std::path::Path::new(test_file).exists() {
            println!("Skipping integration test: test file not found");
            return;
        }

        let params = MbIdentifyRecordParams {
            file_path: test_file.to_string(),
            limit: 3,
            metadata_level: MetadataLevel::Basic,
        };

        let result = MbIdentifyRecordTool::execute(&params, &config);
        assert!(!result.content.is_empty());
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_mb_identify_http_handler_invalid_params() {
        let config = Arc::new(Config::default());
        let args = serde_json::json!({
            "limit": 3
            // missing file_path
        });

        let result = MbIdentifyRecordTool::http_handler(args, config);
        assert!(result.is_err());
    }
}
