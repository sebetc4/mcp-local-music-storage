//! Write metadata tool definition.
//!
//! A tool that writes/updates audio file metadata (ID3 tags, etc.) using lofty.

use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};

use futures::FutureExt;
use lofty::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::fs_atomic::temp_sibling;
use crate::core::security::validate_path;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the write metadata tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WriteMetadataParams {
    /// Path to the audio file to modify.
    pub path: String,

    /// Title of the track
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Artist name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,

    /// Album name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,

    /// Album artist
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,

    /// Year of release
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,

    /// Track number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,

    /// Total tracks in album
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_total: Option<u32>,

    /// Genre
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,

    /// Comment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    /// If true, clear all existing tags before writing new ones
    #[serde(default)]
    pub clear_existing: bool,
}

// ============================================================================
// Structured Output Types
// ============================================================================

/// Structured output for metadata write results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetadataWriteResult {
    pub file: String,
    pub clear_existing: bool,
    pub fields_updated: usize,
    pub updated_fields: HashMap<String, String>,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Write metadata tool - writes/updates audio file metadata using lofty.
pub struct WriteMetadataTool;

impl WriteMetadataTool {
    /// Tool name as registered in MCP.
    pub const NAME: &'static str = "write_metadata";

    /// Tool description shown to clients.
    pub const DESCRIPTION: &'static str = "Write or update metadata tags in audio files (MP3, FLAC, M4A, etc.). \
         Supports title, artist, album, year, track number, genre, and more. \
         Only provided fields will be updated.";

    /// Execute the tool logic (for STDIO/TCP transport via rmcp).
    #[instrument(skip_all, fields(path = %params.path))]
    pub fn execute(params: &WriteMetadataParams, config: &Config) -> CallToolResult {
        info!("Write metadata tool called for path: {}", params.path);

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
        let mut tagged_file = match lofty::read_from_path(&path) {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to read audio file: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to read audio file: {}",
                    e
                ))]);
            }
        };

        // Ensure a primary tag exists, creating one of the right type if needed.
        // Done as straight-line statements rather than an expression so we can
        // bail out cleanly if `primary_tag_mut` somehow returns None after the
        // insert (defensive — should not happen with lofty's current API).
        if params.clear_existing {
            tagged_file.clear();
        }
        if tagged_file.primary_tag().is_none() {
            let tag_type = tagged_file.primary_tag_type();
            tagged_file.insert_tag(lofty::tag::Tag::new(tag_type));
        }
        let tag = match tagged_file.primary_tag_mut() {
            Some(t) => t,
            None => {
                warn!("Failed to obtain primary tag after insert");
                return CallToolResult::error(vec![Content::text(
                    "Internal error: failed to create primary tag".to_string(),
                )]);
            }
        };

        let mut updated_fields = HashMap::new();

        // Update title
        if let Some(title) = &params.title {
            tag.set_title(title.clone());
            updated_fields.insert("title".to_string(), title.clone());
        }

        // Update artist
        if let Some(artist) = &params.artist {
            tag.set_artist(artist.clone());
            updated_fields.insert("artist".to_string(), artist.clone());
        }

        // Update album
        if let Some(album) = &params.album {
            tag.set_album(album.clone());
            updated_fields.insert("album".to_string(), album.clone());
        }

        // Update album artist
        if let Some(album_artist) = &params.album_artist {
            tag.insert_text(lofty::tag::ItemKey::AlbumArtist, album_artist.clone());
            updated_fields.insert("album_artist".to_string(), album_artist.clone());
        }

        // Update year
        if let Some(year) = params.year {
            tag.set_year(year);
            updated_fields.insert("year".to_string(), year.to_string());
        }

        // Update track number
        if let Some(track) = params.track {
            tag.set_track(track);
            updated_fields.insert("track".to_string(), track.to_string());
        }

        // Update track total
        if let Some(track_total) = params.track_total {
            tag.set_track_total(track_total);
            updated_fields.insert("track_total".to_string(), track_total.to_string());
        }

        // Update genre
        if let Some(genre) = &params.genre {
            tag.set_genre(genre.clone());
            updated_fields.insert("genre".to_string(), genre.clone());
        }

        // Update comment
        if let Some(comment) = &params.comment {
            tag.set_comment(comment.clone());
            updated_fields.insert("comment".to_string(), comment.clone());
        }

        // Save changes atomically: lofty rewrites the file in place, so we copy
        // the original to a sibling temp, mutate the copy, then rename it on
        // top of the original. A crash mid-save leaves the source untouched.
        let write_options = lofty::config::WriteOptions::default();

        let tmp = match temp_sibling(&path) {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to compute temp path: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to compute temp path: {}",
                    e
                ))]);
            }
        };

        if let Err(e) = std::fs::copy(&path, &tmp) {
            warn!("Failed to copy source for atomic save: {}", e);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to copy source for atomic save: {}",
                e
            ))]);
        }

        if let Err(e) = tagged_file.save_to_path(&tmp, write_options) {
            warn!("Failed to save metadata: {}", e);
            let _ = std::fs::remove_file(&tmp);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to save metadata: {}",
                e
            ))]);
        }

        if let Err(e) = std::fs::rename(&tmp, &path) {
            warn!("Failed to rename temp into place: {}", e);
            let _ = std::fs::remove_file(&tmp);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to finalize atomic save: {}",
                e
            ))]);
        }

        // Build structured result
        let fields_count = updated_fields.len();
        let structured_data = MetadataWriteResult {
            file: params.path.clone(),
            clear_existing: params.clear_existing,
            fields_updated: fields_count,
            updated_fields: updated_fields.clone(),
        };

        // Build concise text summary
        let summary = if fields_count == 0 {
            format!("No fields updated for '{}'", params.path)
        } else {
            let field_names: Vec<&str> = updated_fields.keys().map(|k| k.as_str()).collect();
            if params.clear_existing {
                format!(
                    "Cleared and updated {} field(s) in '{}': {}",
                    fields_count,
                    params.path,
                    field_names.join(", ")
                )
            } else {
                format!(
                    "Updated {} field(s) in '{}': {}",
                    fields_count,
                    params.path,
                    field_names.join(", ")
                )
            }
        };

        info!(
            "Successfully wrote metadata to {} ({} fields updated)",
            params.path,
            fields_count
        );

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

        info!("Write metadata tool (HTTP) called for path: {}", path);

        let params: WriteMetadataParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;

        let result = Self::execute(&params, &config);

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    /// Create a Tool model for this tool (metadata).
    pub fn to_tool() -> Tool {
        Tool {
            name: Self::NAME.into(),
            description: Some(Self::DESCRIPTION.into()),
            input_schema: schema_for_type::<WriteMetadataParams>(),
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
                let params: WriteMetadataParams =
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
    use tempfile::TempDir;

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_write_metadata_nonexistent() {
        let params = WriteMetadataParams {
            path: "/nonexistent/audio/file.mp3".to_string(),
            title: Some("Test".to_string()),
            artist: None,
            album: None,
            album_artist: None,
            year: None,
            track: None,
            track_total: None,
            genre: None,
            comment: None,
            clear_existing: false,
        };

        let config = test_config();
        let result = WriteMetadataTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));
    }

    #[test]
    fn test_write_metadata_not_a_file() {
        let temp_dir = TempDir::new().unwrap();

        let params = WriteMetadataParams {
            path: temp_dir.path().to_string_lossy().to_string(),
            title: Some("Test".to_string()),
            artist: None,
            album: None,
            album_artist: None,
            year: None,
            track: None,
            track_total: None,
            genre: None,
            comment: None,
            clear_existing: false,
        };

        let config = test_config();
        let result = WriteMetadataTool::execute(&params, &config);
        assert!(result.is_error.unwrap_or(false));
    }

    #[cfg(feature = "http")]
    #[test]
    fn test_write_metadata_http_handler_missing_path() {
        let args = serde_json::json!({
            "title": "Test Title"
        });

        let config = Arc::new(test_config());
        let result = WriteMetadataTool::http_handler(args, config);
        assert!(result.is_err());
    }
}
