// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-trip tests for `embed_cover` / `read_metadata`.
//!
//! Embeds a tiny PNG into a minimal WAV file (lofty writes an ID3v2 chunk —
//! `FileType::Wav::primary_tag_type()` is `Id3v2`, which supports APIC
//! pictures), reads back via `read_metadata` with `include_properties=true`,
//! and asserts the picture survived intact.

use music_mcp_server::core::config::Config;
use music_mcp_server::domains::tools::definitions::metadata::{
    embed_cover::{EmbedCoverParams, EmbedCoverTool},
    read::{ReadMetadataParams, ReadMetadataTool},
};
use rmcp::model::RawContent;
use tempfile::TempDir;

/// Write a minimal 144-byte PCM WAV (RIFF/fmt/data, 100 samples of 8-bit
/// mono silence at 8 kHz) — identical to the helper in `metadata_roundtrip`
/// to keep both test crates self-contained.
fn write_minimal_wav(path: &std::path::Path) {
    let sample_count: u32 = 100;
    let data_size = sample_count;
    let riff_size = 36 + data_size;

    let mut buf: Vec<u8> = Vec::with_capacity(44 + sample_count as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&8000u32.to_le_bytes());
    buf.extend_from_slice(&8000u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend(std::iter::repeat(0u8).take(sample_count as usize));

    std::fs::write(path, &buf).unwrap();
}

/// A valid 67-byte 1×1 RGB PNG. lofty's `Picture::from_reader` only sniffs
/// the first 8 bytes to detect the MIME type and stores the rest opaquely,
/// so a real PNG isn't strictly required — but using one keeps the fixture
/// honest and makes the failure mode obvious if lofty ever upgrades to a
/// stricter parser.
fn tiny_png() -> Vec<u8> {
    vec![
        // PNG signature
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
        // IHDR: length=13
        0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R',
        // width=1, height=1
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        // bit depth=8, colour type=2 (RGB), compression=0, filter=0, interlace=0
        0x08, 0x02, 0x00, 0x00, 0x00,
        // IHDR CRC
        0x90, 0x77, 0x53, 0xDE,
        // IDAT: length=12
        0x00, 0x00, 0x00, 0x0C, b'I', b'D', b'A', b'T',
        // zlib-compressed single pixel
        0x08, 0x99, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01,
        // IDAT CRC
        0x5D, 0x76, 0xEA, 0x9E,
        // IEND: length=0
        0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D',
        // IEND CRC
        0xAE, 0x42, 0x60, 0x82,
    ]
}

fn extract_text(content: &[rmcp::model::Content]) -> String {
    content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Happy path: embed a PNG from disk, read back, picture is present with
/// the right MIME, type, and byte count.
#[test]
fn embed_cover_from_file_roundtrip() {
    let dir = TempDir::new().unwrap();
    let audio = dir.path().join("track.wav");
    write_minimal_wav(&audio);

    let png_path = dir.path().join("cover.png");
    let png_bytes = tiny_png();
    std::fs::write(&png_path, &png_bytes).unwrap();

    let cfg = Config::default();

    let params = EmbedCoverParams {
        path: audio.to_string_lossy().into_owned(),
        image_path: Some(png_path.to_string_lossy().into_owned()),
        image_bytes_base64: None,
        picture_type: "CoverFront".to_string(),
        description: Some("test cover".to_string()),
        replace_existing: false,
    };

    let r = EmbedCoverTool::execute(&params, &cfg);
    assert!(
        !r.is_error.unwrap_or(false),
        "embed_cover failed: {}",
        extract_text(&r.content)
    );

    let structured = r
        .structured_content
        .as_ref()
        .expect("embed_cover should emit structured output");
    assert_eq!(structured["mime_type"].as_str(), Some("image/png"));
    assert_eq!(structured["picture_type"].as_str(), Some("CoverFront"));
    assert_eq!(structured["bytes"].as_u64(), Some(png_bytes.len() as u64));
    assert_eq!(structured["pictures_after"].as_u64(), Some(1));
    assert_eq!(structured["replaced_existing"].as_u64(), Some(0));

    let read = ReadMetadataTool::execute(
        &ReadMetadataParams {
            path: audio.to_string_lossy().into_owned(),
            include_properties: true,
        },
        &cfg,
    );
    assert!(
        !read.is_error.unwrap_or(false),
        "read_metadata failed: {}",
        extract_text(&read.content)
    );
    let structured = read.structured_content.as_ref().unwrap();
    let pictures = structured["pictures"]
        .as_array()
        .expect("pictures field must be present when include_properties=true");
    assert_eq!(pictures.len(), 1);
    let pic = &pictures[0];
    assert_eq!(pic["picture_type"].as_str(), Some("CoverFront"));
    assert_eq!(pic["mime_type"].as_str(), Some("image/png"));
    assert_eq!(pic["description"].as_str(), Some("test cover"));
    assert_eq!(pic["bytes"].as_u64(), Some(png_bytes.len() as u64));
}

/// Embedding via inline base64 reaches the same end state.
#[test]
fn embed_cover_from_base64_roundtrip() {
    use base64::Engine;

    let dir = TempDir::new().unwrap();
    let audio = dir.path().join("track.wav");
    write_minimal_wav(&audio);

    let png_bytes = tiny_png();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

    let cfg = Config::default();
    let params = EmbedCoverParams {
        path: audio.to_string_lossy().into_owned(),
        image_path: None,
        image_bytes_base64: Some(b64),
        picture_type: "CoverFront".to_string(),
        description: None,
        replace_existing: false,
    };

    let r = EmbedCoverTool::execute(&params, &cfg);
    assert!(
        !r.is_error.unwrap_or(false),
        "embed_cover (base64) failed: {}",
        extract_text(&r.content)
    );

    let read = ReadMetadataTool::execute(
        &ReadMetadataParams {
            path: audio.to_string_lossy().into_owned(),
            include_properties: true,
        },
        &cfg,
    );
    assert!(!read.is_error.unwrap_or(false));
    let pictures = read
        .structured_content
        .as_ref()
        .unwrap()
        .get("pictures")
        .and_then(|v| v.as_array())
        .expect("pictures array missing");
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0]["mime_type"].as_str(), Some("image/png"));
}

/// `replace_existing=true` drops every existing picture of the same type
/// before appending the new one. A second embed with the flag set replaces
/// rather than appends, so total picture count stays at 1.
#[test]
fn embed_cover_replace_existing() {
    let dir = TempDir::new().unwrap();
    let audio = dir.path().join("track.wav");
    write_minimal_wav(&audio);

    let png_path = dir.path().join("cover.png");
    std::fs::write(&png_path, tiny_png()).unwrap();

    let cfg = Config::default();
    let base = EmbedCoverParams {
        path: audio.to_string_lossy().into_owned(),
        image_path: Some(png_path.to_string_lossy().into_owned()),
        image_bytes_base64: None,
        picture_type: "CoverFront".to_string(),
        description: None,
        replace_existing: false,
    };

    let r1 = EmbedCoverTool::execute(&base, &cfg);
    assert!(!r1.is_error.unwrap_or(false));

    let r2 = EmbedCoverTool::execute(
        &EmbedCoverParams {
            replace_existing: true,
            ..base.clone()
        },
        &cfg,
    );
    assert!(!r2.is_error.unwrap_or(false));

    let structured = r2.structured_content.as_ref().unwrap();
    assert_eq!(structured["pictures_after"].as_u64(), Some(1));
    assert_eq!(structured["replaced_existing"].as_u64(), Some(1));
}

/// Without `replace_existing`, two embeds of the same picture type land as
/// two distinct pictures — confirms append semantics.
#[test]
fn embed_cover_appends_by_default() {
    let dir = TempDir::new().unwrap();
    let audio = dir.path().join("track.wav");
    write_minimal_wav(&audio);

    let png_path = dir.path().join("cover.png");
    std::fs::write(&png_path, tiny_png()).unwrap();

    let cfg = Config::default();
    let params = EmbedCoverParams {
        path: audio.to_string_lossy().into_owned(),
        image_path: Some(png_path.to_string_lossy().into_owned()),
        image_bytes_base64: None,
        picture_type: "CoverFront".to_string(),
        description: None,
        replace_existing: false,
    };

    let r1 = EmbedCoverTool::execute(&params, &cfg);
    assert!(!r1.is_error.unwrap_or(false));
    let r2 = EmbedCoverTool::execute(&params, &cfg);
    assert!(!r2.is_error.unwrap_or(false));

    let structured = r2.structured_content.as_ref().unwrap();
    assert_eq!(structured["pictures_after"].as_u64(), Some(2));
    assert_eq!(structured["replaced_existing"].as_u64(), Some(0));
}

/// Bytes that aren't a recognised picture are rejected before the atomic
/// save runs — the audio file is untouched and no stray `.tmp.*` is left
/// behind.
#[test]
fn embed_cover_rejects_non_image() {
    use base64::Engine;

    let dir = TempDir::new().unwrap();
    let audio = dir.path().join("track.wav");
    write_minimal_wav(&audio);
    let original_bytes = std::fs::read(&audio).unwrap();

    let cfg = Config::default();
    let params = EmbedCoverParams {
        path: audio.to_string_lossy().into_owned(),
        image_path: None,
        image_bytes_base64: Some(
            base64::engine::general_purpose::STANDARD.encode(b"definitely not a picture"),
        ),
        picture_type: "CoverFront".to_string(),
        description: None,
        replace_existing: false,
    };

    let r = EmbedCoverTool::execute(&params, &cfg);
    assert!(r.is_error.unwrap_or(false));

    // Original audio bytes untouched.
    assert_eq!(std::fs::read(&audio).unwrap(), original_bytes);

    // No leftover temp file next to the target.
    let target_name = audio.file_name().unwrap().to_string_lossy().into_owned();
    let prefix = format!("{}.tmp.", target_name);
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(&prefix),
            "leftover temp file after rejection: {}",
            name
        );
    }
}
