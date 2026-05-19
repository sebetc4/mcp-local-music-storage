//! Embed cover tool definition.
//!
//! Embeds a JPEG or PNG image into the audio file's primary tag (APIC for
//! MP3, PICTURE block for FLAC/Vorbis, `covr` atom for MP4/M4A). The image
//! source is either a sibling file path or inline base64 bytes — exactly
//! one of the two must be provided.
//!
//! Image magic bytes are sniffed via `lofty::picture::Picture::from_reader`;
//! only JPEG and PNG are accepted (most music players support them
//! universally). The atomic-save chain (copy → save_to_path on temp →
//! rename) matches `write_metadata` so a crash mid-save leaves the source
//! audio untouched.

use base64::Engine;
use futures::FutureExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use rmcp::{
    ErrorData as McpError,
    handler::server::tool::{ToolCallContext, ToolRoute, schema_for_type},
    model::{CallToolResult, Content, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use crate::core::config::Config;
use crate::core::fs_atomic::temp_sibling;
use crate::core::security::validate_path;

/// Hard cap on embedded-cover image size. Smaller than the standalone-download
/// cap because a 50 MB picture embedded in every track would balloon a library
/// past any reasonable size budget.
pub const MAX_EMBEDDED_COVER_BYTES: usize = 10 * 1024 * 1024;

// ============================================================================
// Tool Parameters
// ============================================================================

/// Parameters for the embed cover tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EmbedCoverParams {
    /// Path to the audio file to embed the cover into.
    pub path: String,

    /// Filesystem path to the image to embed. Mutually exclusive with
    /// `image_bytes_base64` — exactly one of the two must be provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,

    /// Base64-encoded image bytes. Mutually exclusive with `image_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_bytes_base64: Option<String>,

    /// ID3v2 APIC picture type to attach. Default: `CoverFront`.
    #[serde(default = "default_picture_type")]
    pub picture_type: String,

    /// Optional description stored alongside the picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// When true, drop every existing picture of the same `picture_type`
    /// before appending the new one. When false (default), the new picture
    /// is simply appended.
    #[serde(default)]
    pub replace_existing: bool,
}

fn default_picture_type() -> String {
    "CoverFront".to_string()
}

// ============================================================================
// Structured Output
// ============================================================================

/// Structured output for embed cover results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EmbedCoverResult {
    pub file: String,
    pub picture_type: String,
    pub mime_type: String,
    pub bytes: usize,
    pub replaced_existing: usize,
    pub pictures_after: usize,
}

// ============================================================================
// Tool Definition
// ============================================================================

/// Embed cover tool — writes a JPEG/PNG picture into the primary audio tag.
pub struct EmbedCoverTool;

impl EmbedCoverTool {
    pub const NAME: &'static str = "embed_cover";

    pub const DESCRIPTION: &'static str = "Embed a JPEG or PNG image inside an audio file's primary tag \
         (APIC for MP3, PICTURE block for FLAC/Vorbis, covr atom for MP4/M4A). \
         Image source is either a sibling file path or inline base64 bytes — exactly one of the two must be provided. \
         Atomic save: a crash mid-write leaves the original audio untouched.";

    #[instrument(skip_all, fields(path = %params.path))]
    pub fn execute(params: &EmbedCoverParams, config: &Config) -> CallToolResult {
        info!("Embed cover tool called for path: {}", params.path);

        // 1. Resolve image bytes from exactly one of the two sources.
        let image_bytes = match Self::resolve_image_bytes(params, config) {
            Ok(bytes) => bytes,
            Err(msg) => {
                warn!("Image source rejected: {}", msg);
                return CallToolResult::error(vec![Content::text(msg)]);
            }
        };

        if image_bytes.len() > MAX_EMBEDDED_COVER_BYTES {
            warn!(
                "Image exceeds embedded cover cap: {} > {}",
                image_bytes.len(),
                MAX_EMBEDDED_COVER_BYTES
            );
            return CallToolResult::error(vec![Content::text(format!(
                "Image too large: {} bytes (max {} bytes)",
                image_bytes.len(),
                MAX_EMBEDDED_COVER_BYTES
            ))]);
        }

        // 2. Parse and validate the requested picture type up-front.
        let picture_type = match parse_picture_type(&params.picture_type) {
            Ok(pt) => pt,
            Err(msg) => {
                warn!("{}", msg);
                return CallToolResult::error(vec![Content::text(msg)]);
            }
        };

        // 3. Validate audio path against the configured root.
        let audio_path = match validate_path(&params.path, config) {
            Ok(p) => p,
            Err(e) => {
                warn!("Path security validation failed: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Path security validation failed: {}",
                    e
                ))]);
            }
        };

        if !audio_path.is_file() {
            warn!("Path is not a file: {}", params.path);
            return CallToolResult::error(vec![Content::text(format!(
                "Path is not a file: {}",
                params.path
            ))]);
        }

        // 4. Build a Picture from the bytes. `from_reader` sniffs the magic
        //    bytes and bails with `NotAPicture` on unrecognised formats —
        //    this is our MIME-validation surface.
        let mut picture = {
            let mut cursor = Cursor::new(&image_bytes);
            match Picture::from_reader(&mut cursor) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Image bytes are not a recognised picture: {}", e);
                    return CallToolResult::error(vec![Content::text(format!(
                        "Image is not a recognised picture format: {}",
                        e
                    ))]);
                }
            }
        };

        // Restrict to JPEG/PNG. lofty's sniffer also accepts TIFF/BMP/GIF,
        // but those are poorly supported by music players and bloat files.
        let mime_str = match picture.mime_type() {
            Some(MimeType::Jpeg) => "image/jpeg",
            Some(MimeType::Png) => "image/png",
            Some(other) => {
                let msg = format!(
                    "Unsupported image MIME type '{}' — only image/jpeg and image/png are accepted",
                    other
                );
                warn!("{}", msg);
                return CallToolResult::error(vec![Content::text(msg)]);
            }
            None => {
                let msg = "Unable to detect image MIME type from magic bytes".to_string();
                warn!("{}", msg);
                return CallToolResult::error(vec![Content::text(msg)]);
            }
        };

        picture.set_pic_type(picture_type);
        if let Some(desc) = &params.description {
            picture.set_description(Some(desc.clone()));
        }

        // 5. Read the audio file.
        let mut tagged_file = match lofty::read_from_path(&audio_path) {
            Ok(file) => file,
            Err(e) => {
                warn!("Failed to read audio file: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to read audio file: {}",
                    e
                ))]);
            }
        };

        // 6. Ensure a primary tag exists so we have somewhere to attach the
        //    picture. Same pattern as write_metadata.
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

        // 7. Optionally drop existing pictures of the same type.
        let pictures_before = tag.pictures().len();
        if params.replace_existing {
            tag.remove_picture_type(picture_type);
        }
        let pictures_after_remove = tag.pictures().len();
        let replaced_existing = pictures_before.saturating_sub(pictures_after_remove);

        tag.push_picture(picture);
        let pictures_after = tag.pictures().len();

        // 8. Atomic save: copy → save_to_path(tmp) → rename. Identical
        //    contract to write_metadata so partial writes never corrupt
        //    the original audio.
        let write_options = lofty::config::WriteOptions::default();

        let tmp = match temp_sibling(&audio_path) {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to compute temp path: {}", e);
                return CallToolResult::error(vec![Content::text(format!(
                    "Failed to compute temp path: {}",
                    e
                ))]);
            }
        };

        if let Err(e) = std::fs::copy(&audio_path, &tmp) {
            warn!("Failed to copy source for atomic save: {}", e);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to copy source for atomic save: {}",
                e
            ))]);
        }

        if let Err(e) = tagged_file.save_to_path(&tmp, write_options) {
            warn!("Failed to save picture: {}", e);
            let _ = std::fs::remove_file(&tmp);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to save picture: {}",
                e
            ))]);
        }

        if let Err(e) = std::fs::rename(&tmp, &audio_path) {
            warn!("Failed to rename temp into place: {}", e);
            let _ = std::fs::remove_file(&tmp);
            return CallToolResult::error(vec![Content::text(format!(
                "Failed to finalize atomic save: {}",
                e
            ))]);
        }

        let result = EmbedCoverResult {
            file: params.path.clone(),
            picture_type: params.picture_type.clone(),
            mime_type: mime_str.to_string(),
            bytes: image_bytes.len(),
            replaced_existing,
            pictures_after,
        };

        let summary = format!(
            "Embedded {} cover ({}, {} bytes) into '{}' — {} picture(s) now attached{}",
            params.picture_type,
            mime_str,
            image_bytes.len(),
            params.path,
            pictures_after,
            if replaced_existing > 0 {
                format!(" (replaced {})", replaced_existing)
            } else {
                String::new()
            }
        );

        info!("{}", summary);

        crate::domains::tools::result::structured_ok(summary, &result)
    }

    /// Resolve image bytes from whichever source the caller supplied.
    /// Enforces the "exactly one" constraint and applies the size cap to
    /// the *decoded* bytes (the base64 string may legitimately be larger).
    fn resolve_image_bytes(
        params: &EmbedCoverParams,
        config: &Config,
    ) -> Result<Vec<u8>, String> {
        match (&params.image_path, &params.image_bytes_base64) {
            (Some(_), Some(_)) => Err(
                "Provide exactly one of 'image_path' or 'image_bytes_base64', not both"
                    .to_string(),
            ),
            (None, None) => Err(
                "Missing image source: provide 'image_path' or 'image_bytes_base64'".to_string(),
            ),
            (Some(image_path), None) => {
                let resolved = validate_path(image_path, config)
                    .map_err(|e| format!("Image path security validation failed: {}", e))?;
                if !resolved.is_file() {
                    return Err(format!("Image path is not a file: {}", image_path));
                }
                // Cap on filesystem size before reading anything into memory.
                let metadata = std::fs::metadata(&resolved)
                    .map_err(|e| format!("Failed to stat image file: {}", e))?;
                if metadata.len() as usize > MAX_EMBEDDED_COVER_BYTES {
                    return Err(format!(
                        "Image file too large: {} bytes (max {} bytes)",
                        metadata.len(),
                        MAX_EMBEDDED_COVER_BYTES
                    ));
                }
                std::fs::read(&resolved)
                    .map_err(|e| format!("Failed to read image file: {}", e))
            }
            (None, Some(b64)) => base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| format!("Failed to decode base64 image bytes: {}", e)),
        }
    }

    /// HTTP handler for this tool.
    #[cfg(feature = "http")]
    pub fn http_handler(
        arguments: serde_json::Value,
        config: Arc<Config>,
    ) -> Result<serde_json::Value, String> {
        let params: EmbedCoverParams = serde_json::from_value(arguments)
            .map_err(|e| format!("Failed to parse parameters: {}", e))?;

        let result = Self::execute(&params, &config);

        crate::domains::tools::http_response::tool_result_to_json(result)
    }

    pub fn to_tool() -> Tool {
        Tool::new(
            Self::NAME,
            Self::DESCRIPTION,
            schema_for_type::<EmbedCoverParams>(),
        )
    }

    pub fn create_route<S>(config: Arc<Config>) -> ToolRoute<S>
    where
        S: Send + Sync + 'static,
    {
        ToolRoute::new_dyn(Self::to_tool(), move |ctx: ToolCallContext<'_, S>| {
            let args = ctx.arguments.clone().unwrap_or_default();
            let config = config.clone();
            async move {
                let params: EmbedCoverParams =
                    serde_json::from_value(serde_json::Value::Object(args))
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(Self::execute(&params, &config))
            }
            .boxed()
        })
    }
}

// ============================================================================
// Picture-type string mapping
// ============================================================================

/// Parse a `picture_type` string into a `lofty::picture::PictureType` variant.
///
/// The accepted spelling matches lofty's enum variant names verbatim (e.g.
/// `"CoverFront"`, not `"front"` or `"cover_front"`) so the wire format stays
/// stable independent of casing conventions on the agent side.
pub fn parse_picture_type(s: &str) -> Result<PictureType, String> {
    let pt = match s {
        "Other" => PictureType::Other,
        "Icon" => PictureType::Icon,
        "OtherIcon" => PictureType::OtherIcon,
        "CoverFront" => PictureType::CoverFront,
        "CoverBack" => PictureType::CoverBack,
        "Leaflet" => PictureType::Leaflet,
        "Media" => PictureType::Media,
        "LeadArtist" => PictureType::LeadArtist,
        "Artist" => PictureType::Artist,
        "Conductor" => PictureType::Conductor,
        "Band" => PictureType::Band,
        "Composer" => PictureType::Composer,
        "Lyricist" => PictureType::Lyricist,
        "RecordingLocation" => PictureType::RecordingLocation,
        "DuringRecording" => PictureType::DuringRecording,
        "DuringPerformance" => PictureType::DuringPerformance,
        "ScreenCapture" => PictureType::ScreenCapture,
        "BrightFish" => PictureType::BrightFish,
        "Illustration" => PictureType::Illustration,
        "BandLogo" => PictureType::BandLogo,
        "PublisherLogo" => PictureType::PublisherLogo,
        _ => {
            return Err(format!(
                "Unknown picture_type '{}'. Accepted: Other, Icon, OtherIcon, CoverFront, CoverBack, Leaflet, Media, LeadArtist, Artist, Conductor, Band, Composer, Lyricist, RecordingLocation, DuringRecording, DuringPerformance, ScreenCapture, BrightFish, Illustration, BandLogo, PublisherLogo",
                s
            ));
        }
    };
    Ok(pt)
}

/// Reverse of `parse_picture_type` — used by `read_metadata` to report the
/// embedded pictures' types in a stable, agent-facing form.
pub fn picture_type_str(t: &PictureType) -> String {
    match t {
        PictureType::Other => "Other".to_string(),
        PictureType::Icon => "Icon".to_string(),
        PictureType::OtherIcon => "OtherIcon".to_string(),
        PictureType::CoverFront => "CoverFront".to_string(),
        PictureType::CoverBack => "CoverBack".to_string(),
        PictureType::Leaflet => "Leaflet".to_string(),
        PictureType::Media => "Media".to_string(),
        PictureType::LeadArtist => "LeadArtist".to_string(),
        PictureType::Artist => "Artist".to_string(),
        PictureType::Conductor => "Conductor".to_string(),
        PictureType::Band => "Band".to_string(),
        PictureType::Composer => "Composer".to_string(),
        PictureType::Lyricist => "Lyricist".to_string(),
        PictureType::RecordingLocation => "RecordingLocation".to_string(),
        PictureType::DuringRecording => "DuringRecording".to_string(),
        PictureType::DuringPerformance => "DuringPerformance".to_string(),
        PictureType::ScreenCapture => "ScreenCapture".to_string(),
        PictureType::BrightFish => "BrightFish".to_string(),
        PictureType::Illustration => "Illustration".to_string(),
        PictureType::BandLogo => "BandLogo".to_string(),
        PictureType::PublisherLogo => "PublisherLogo".to_string(),
        PictureType::Undefined(n) => format!("Undefined({})", n),
        // `#[non_exhaustive]` — keep a fallback for forward compatibility.
        _ => "Unknown".to_string(),
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
    fn parse_picture_type_known() {
        assert!(matches!(
            parse_picture_type("CoverFront"),
            Ok(PictureType::CoverFront)
        ));
        assert!(matches!(
            parse_picture_type("CoverBack"),
            Ok(PictureType::CoverBack)
        ));
        assert!(matches!(parse_picture_type("Band"), Ok(PictureType::Band)));
    }

    #[test]
    fn parse_picture_type_unknown() {
        let err = parse_picture_type("front").unwrap_err();
        assert!(err.contains("Unknown picture_type"));
        assert!(err.contains("CoverFront"));
    }

    #[test]
    fn picture_type_str_roundtrips() {
        for s in [
            "Other",
            "Icon",
            "OtherIcon",
            "CoverFront",
            "CoverBack",
            "Leaflet",
            "Media",
            "LeadArtist",
            "Artist",
            "Conductor",
            "Band",
            "Composer",
            "Lyricist",
            "RecordingLocation",
            "DuringRecording",
            "DuringPerformance",
            "ScreenCapture",
            "BrightFish",
            "Illustration",
            "BandLogo",
            "PublisherLogo",
        ] {
            let pt = parse_picture_type(s).unwrap();
            assert_eq!(picture_type_str(&pt), s, "roundtrip failed for {}", s);
        }
    }

    #[test]
    fn picture_type_str_undefined() {
        assert_eq!(picture_type_str(&PictureType::Undefined(42)), "Undefined(42)");
    }

    #[test]
    fn rejects_missing_source() {
        let params = EmbedCoverParams {
            path: "/tmp/audio.mp3".to_string(),
            image_path: None,
            image_bytes_base64: None,
            picture_type: "CoverFront".to_string(),
            description: None,
            replace_existing: false,
        };
        let err = EmbedCoverTool::resolve_image_bytes(&params, &test_config()).unwrap_err();
        assert!(err.contains("Missing image source"));
    }

    #[test]
    fn rejects_both_sources() {
        let params = EmbedCoverParams {
            path: "/tmp/audio.mp3".to_string(),
            image_path: Some("/tmp/x.jpg".to_string()),
            image_bytes_base64: Some("AAAA".to_string()),
            picture_type: "CoverFront".to_string(),
            description: None,
            replace_existing: false,
        };
        let err = EmbedCoverTool::resolve_image_bytes(&params, &test_config()).unwrap_err();
        assert!(err.contains("exactly one"));
    }

    #[test]
    fn rejects_invalid_base64() {
        let params = EmbedCoverParams {
            path: "/tmp/audio.mp3".to_string(),
            image_path: None,
            image_bytes_base64: Some("!!!not-base64!!!".to_string()),
            picture_type: "CoverFront".to_string(),
            description: None,
            replace_existing: false,
        };
        let err = EmbedCoverTool::resolve_image_bytes(&params, &test_config()).unwrap_err();
        assert!(err.contains("Failed to decode base64"));
    }

    /// A short base64 string that happens to *not* be a valid image. The
    /// resolver should hand the bytes to `execute`, which then rejects them
    /// via `Picture::from_reader`.
    #[test]
    fn execute_rejects_non_image_bytes() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("audio.wav");
        std::fs::write(&audio, b"not actually audio either").unwrap();

        let params = EmbedCoverParams {
            path: audio.to_string_lossy().into_owned(),
            image_path: None,
            // base64("hello world") — not a picture
            image_bytes_base64: Some("aGVsbG8gd29ybGQ=".to_string()),
            picture_type: "CoverFront".to_string(),
            description: None,
            replace_existing: false,
        };

        let result = EmbedCoverTool::execute(&params, &test_config());
        assert!(result.is_error.unwrap_or(false));
    }

    #[test]
    fn execute_rejects_unknown_picture_type() {
        let params = EmbedCoverParams {
            path: "/nonexistent".to_string(),
            image_path: None,
            image_bytes_base64: Some(base64::engine::general_purpose::STANDARD.encode(b"junk")),
            picture_type: "front".to_string(),
            description: None,
            replace_existing: false,
        };
        let result = EmbedCoverTool::execute(&params, &test_config());
        assert!(result.is_error.unwrap_or(false));
    }

    #[test]
    fn max_size_constant_is_ten_mb() {
        assert_eq!(MAX_EMBEDDED_COVER_BYTES, 10 * 1024 * 1024);
    }
}
