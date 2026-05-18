//! Read metadata tool definition.
//!
//! A tool that reads audio file metadata (ID3 tags, etc.) using lofty.

use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};

use futures::FutureExt;
use lofty::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::security::validate_path;
use crate::domains::tools::definitions::metadata::embed_cover::picture_type_str;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the read metadata tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadMetadataParams {
    /// Path to the audio file to read.
    pub path: String,

    /// Include technical audio properties (bitrate, sample rate, duration)
    #[serde(default)]
    pub include_properties: bool,
}

// ============================================================================
// Structured Output Types
// ============================================================================

/// Structured output for metadata read results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetadataReadResult {
    pub file: String,
    pub format: String,
    pub metadata: Option<AudioMetadata>,
    pub properties: Option<AudioProperties>,
    /// Embedded pictures attached to the primary tag (APIC frames, PICTURE
    /// blocks, covr atoms). Only populated when `include_properties = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pictures: Option<Vec<EmbeddedPictureInfo>>,
}

/// Summary of one embedded picture. Image bytes themselves are deliberately
/// *not* returned — the agent should call a dedicated tool to extract them
/// when needed, to keep `read_metadata` responses small.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EmbeddedPictureInfo {
    pub picture_type: String,
    pub mime_type: Option<String>,
    pub description: Option<String>,
    pub bytes: usize,
}

/// Audio metadata tags.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AudioMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<u32>,
    pub track: Option<u32>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    pub total_tags: u32,
}

/// Audio technical properties.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AudioProperties {
    pub duration_seconds: Option<u64>,
    pub duration_formatted: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
    pub channel_description: Option<String>,
    pub bit_depth: Option<u8>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Read metadata tool - reads audio file metadata using lofty.
pub struct ReadMetadataTool;

impl ReadMetadataTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "read_metadata";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Read metadata from audio files (MP3, FLAC, M4A, etc.). Returns tags like artist, album, title, year, and optionally technical properties.";

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    #[instrument(skip_all, fields(path = %params.path))]
    pub fn execute(params: &ReadMetadataParams, config: &Config) -> CallToolResult {
        info!("Read metadata tool called for path: {}", params.path);

        // Validate path security first
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

        // Validate it's a file
        if !path.is_file() {
            warn!("Path is not a file: {}", params.path);
            return CallToolResult::error(vec![Content::text(format!(
                "Path is not a file: {}",
                params.path
            ))]);
        }

        // Read the audio file
        let tagged_file = match lofty::read_from_path(path) {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to read audio file: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to read audio file: {}",
                    e
                ))]);
            }
        };

        let format_str = format!("{:?}", tagged_file.file_type());

        // Build metadata structure
        let metadata = tagged_file.primary_tag().map(|tag| AudioMetadata {
            title: tag.title().map(|s| s.to_string()),
            artist: tag.artist().map(|s| s.to_string()),
            album: tag.album().map(|s| s.to_string()),
            album_artist: tag
                .get_string(&lofty::tag::ItemKey::AlbumArtist)
                .map(|s| s.to_string()),
            year: tag.year(),
            track: tag.track(),
            genre: tag.genre().map(|s| s.to_string()),
            comment: tag.comment().map(|s| s.to_string()),
            total_tags: tag.item_count(),
        });

        // Build embedded picture summary if properties were requested. Lives
        // on `Tag`, so we collect them before potentially dropping the borrow.
        let pictures = if params.include_properties {
            tagged_file.primary_tag().map(|tag| {
                tag.pictures()
                    .iter()
                    .map(|p| EmbeddedPictureInfo {
                        picture_type: picture_type_str(&p.pic_type()),
                        mime_type: p.mime_type().map(|m| m.as_str().to_string()),
                        description: p.description().map(|s| s.to_string()),
                        bytes: p.data().len(),
                    })
                    .collect::<Vec<_>>()
            })
        } else {
            None
        };

        // Build properties structure if requested
        let properties = if params.include_properties {
            let props = tagged_file.properties();
            let duration_secs = props.duration().as_secs();
            let duration_formatted = if duration_secs > 0 {
                let minutes = duration_secs / 60;
                let seconds = duration_secs % 60;
                Some(format!("{}:{:02}", minutes, seconds))
            } else {
                None
            };

            let channel_desc = props.channels().map(|ch| match ch {
                1 => "Mono".to_string(),
                2 => "Stereo".to_string(),
                n => format!("{}-channel", n),
            });

            Some(AudioProperties {
                duration_seconds: Some(duration_secs),
                duration_formatted,
                bitrate_kbps: props.audio_bitrate(),
                sample_rate_hz: props.sample_rate(),
                channels: props.channels(),
                channel_description: channel_desc,
                bit_depth: props.bit_depth(),
            })
        } else {
            None
        };

        // Build structured result
        let structured_data = MetadataReadResult {
            file: params.path.clone(),
            format: format_str,
            metadata: metadata.clone(),
            properties: properties.clone(),
            pictures: pictures.clone(),
        };

        // Build text summary
        let summary = if let Some(ref meta) = metadata {
            let title = meta.title.as_deref().unwrap_or("Unknown");
            let artist = meta.artist.as_deref().unwrap_or("Unknown Artist");
            if let Some(ref props) = properties {
                if let Some(ref duration) = props.duration_formatted {
                    format!(
                        "'{}' by {} ({}, {} tags)",
                        title, artist, duration, meta.total_tags
                    )
                } else {
                    format!("'{}' by {} ({} tags)", title, artist, meta.total_tags)
                }
            } else {
                format!("'{}' by {} ({} tags)", title, artist, meta.total_tags)
            }
        } else {
            format!("No metadata found in '{}'", params.path)
        };

        info!("Successfully read metadata from {}", params.path);

        // Return structured result
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

    /// HTTP handler for this tool (for HTTP transport).
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'path' parameter".to_string())?
            .to_string();

        let include_properties = arguments
            .get("include_properties")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        info!("Read metadata tool (HTTP) called for path: {}", path);

        let params = ReadMetadataParams {
            path,
            include_properties,
        };

        let result = Self::execute(&params, &config);

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<ReadMetadataParams>(),
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
                let params: ReadMetadataParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
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

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_read_metadata_nonexistent() {
        let params = ReadMetadataParams {
            path: "/nonexistent/audio/file.mp3".to_string(),
            include_properties: false,
        };

        let config = test_config();
        let result = ReadMetadataTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_read_metadata_http_handler_missing_param() {
        let args = serde_json::json!({
            "include_properties": true
        });

        let config = Arc::new(test_config());
        let result = ReadMetadataTool::http_handler(args, config);
        assert!(result.is_err());
    }
}
