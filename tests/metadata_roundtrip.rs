// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Round-trip tests for `read_metadata` / `write_metadata`.
//!
//! Generates a minimal valid WAV file in a tempdir, drives a write through
//! [`WriteMetadataTool`], reads it back via [`ReadMetadataTool`], and asserts
//! the tags survived intact. WAV is enough to exercise the full atomic-save
//! path (copy → save_to_path → rename) since lofty writes RIFF INFO tags into
//! WAV files. MP3 / FLAC / M4A coverage would require bundled binary fixtures
//! — tracked as a separate follow-up.

use music_mcp_server::core::config::Config;
use music_mcp_server::domains::tools::definitions::metadata::{
    read::{ReadMetadataParams, ReadMetadataTool},
    write::{WriteMetadataParams, WriteMetadataTool},
};
use rmcp::model::RawContent;
use tempfile::TempDir;

/// Write a minimal 144-byte PCM WAV file: RIFF/fmt/data chunks, 100 samples
/// of 8-bit mono silence at 8 kHz. Smallest container lofty will accept for
/// the WAV read-and-tag path.
fn write_minimal_wav(path: &std::path::Path) {
    let sample_count: u32 = 100;
    let data_size = sample_count; // 8-bit mono → 1 byte per sample
    let riff_size = 36 + data_size; // header sans "RIFF" + size = 4+4+(8+16)+(8+data)

    let mut buf: Vec<u8> = Vec::with_capacity(44 + sample_count as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&riff_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
    buf.extend_from_slice(&8000u32.to_le_bytes()); // byte rate (sr * channels * bytes)
    buf.extend_from_slice(&1u16.to_le_bytes()); // block align
    buf.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    // 100 silent samples (8-bit unsigned silence is 0x80, but lofty just needs
    // the header to be valid — 0x00 is fine for our tag-only round-trip).
    buf.extend(std::iter::repeat(0u8).take(sample_count as usize));

    std::fs::write(path, &buf).unwrap();
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

/// Happy-path round trip: write a full set of fields, read them back, every
/// field matches. Exercises the atomic save (copy → save_to_path on temp →
/// rename) and the structured read response.
#[test]
fn wav_roundtrip_all_fields() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("track.wav");
    write_minimal_wav(&target);

    let cfg = Config::default();

    // 1. Write a full set of tags.
    let write = WriteMetadataParams {
        path: target.to_string_lossy().into_owned(),
        title: Some("Roundtrip Title".into()),
        artist: Some("Roundtrip Artist".into()),
        album: Some("Roundtrip Album".into()),
        album_artist: Some("Roundtrip Album Artist".into()),
        year: Some(2026),
        track: Some(7),
        track_total: Some(12),
        genre: Some("Electronic".into()),
        comment: Some("written by integration test".into()),
        clear_existing: false,
    };

    let write_result = WriteMetadataTool::execute(&write, &cfg);
    assert!(
        !write_result.is_error.unwrap_or(false),
        "write_metadata failed: {}",
        extract_text(&write_result.content)
    );

    // The structured payload reports the fields that landed.
    let structured = write_result
        .structured_content
        .as_ref()
        .expect("write_metadata should emit structured output");
    assert_eq!(structured["fields_updated"].as_u64(), Some(9));

    // 2. Read it back and assert the tags survived.
    let read = ReadMetadataParams {
        path: target.to_string_lossy().into_owned(),
        include_properties: false,
    };
    let read_result = ReadMetadataTool::execute(&read, &cfg);
    assert!(
        !read_result.is_error.unwrap_or(false),
        "read_metadata failed: {}",
        extract_text(&read_result.content)
    );

    let result = read_result
        .structured_content
        .as_ref()
        .expect("read_metadata should emit structured output");
    let tags = &result["metadata"];
    assert!(
        !tags.is_null(),
        "read_metadata.metadata must be present, full result: {}",
        result
    );
    assert_eq!(tags["title"].as_str(), Some("Roundtrip Title"));
    assert_eq!(tags["artist"].as_str(), Some("Roundtrip Artist"));
    assert_eq!(tags["album"].as_str(), Some("Roundtrip Album"));
    assert_eq!(
        tags["album_artist"].as_str(),
        Some("Roundtrip Album Artist")
    );
    assert_eq!(tags["year"].as_u64(), Some(2026));
    assert_eq!(tags["track"].as_u64(), Some(7));
    assert_eq!(tags["genre"].as_str(), Some("Electronic"));
    assert_eq!(
        tags["comment"].as_str(),
        Some("written by integration test")
    );
}

/// `clear_existing: true` followed by a partial write should yield a tag
/// containing only the explicitly written fields — no leftovers from earlier
/// writes. Catches a subtle regression where `clear_existing` could be
/// ignored by the atomic-save restructure.
#[test]
fn wav_roundtrip_clear_then_partial_write() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("track.wav");
    write_minimal_wav(&target);
    let cfg = Config::default();

    // First pass: populate.
    let pass1 = WriteMetadataParams {
        path: target.to_string_lossy().into_owned(),
        title: Some("First Title".into()),
        artist: Some("First Artist".into()),
        album: None,
        album_artist: None,
        year: None,
        track: None,
        track_total: None,
        genre: None,
        comment: None,
        clear_existing: false,
    };
    let r = WriteMetadataTool::execute(&pass1, &cfg);
    assert!(!r.is_error.unwrap_or(false));

    // Second pass: clear + write only title.
    let pass2 = WriteMetadataParams {
        path: target.to_string_lossy().into_owned(),
        title: Some("Second Title".into()),
        artist: None,
        album: None,
        album_artist: None,
        year: None,
        track: None,
        track_total: None,
        genre: None,
        comment: None,
        clear_existing: true,
    };
    let r = WriteMetadataTool::execute(&pass2, &cfg);
    assert!(!r.is_error.unwrap_or(false));

    // Read back: title is the new one, artist is gone.
    let read = ReadMetadataParams {
        path: target.to_string_lossy().into_owned(),
        include_properties: false,
    };
    let r = ReadMetadataTool::execute(&read, &cfg);
    assert!(!r.is_error.unwrap_or(false));
    let result = r.structured_content.as_ref().unwrap();
    let tags = &result["metadata"];
    assert_eq!(tags["title"].as_str(), Some("Second Title"));
    assert!(
        tags["artist"].as_str().is_none() || tags["artist"].as_str() == Some(""),
        "artist should have been cleared, got {:?}",
        tags["artist"]
    );
}

/// Original file content is preserved when an atomic save fails halfway —
/// here we trigger failure by pointing the write at a directory that doesn't
/// exist (path validation surface), proving the original target stays intact.
#[test]
fn wav_roundtrip_original_preserved_on_write_failure() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("track.wav");
    write_minimal_wav(&target);
    let cfg = Config::default();

    // First, lay down a baseline tag so we have something to "lose" if a
    // partial write corrupted the file.
    let baseline = WriteMetadataParams {
        path: target.to_string_lossy().into_owned(),
        title: Some("Baseline Title".into()),
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
    let r = WriteMetadataTool::execute(&baseline, &cfg);
    assert!(!r.is_error.unwrap_or(false));

    // Now ask write_metadata to write a non-existent file — it must refuse
    // BEFORE any temp file is materialised next to the real target.
    let bad = WriteMetadataParams {
        path: dir.path().join("nope.wav").to_string_lossy().into_owned(),
        title: Some("Should Never Land".into()),
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
    let r = WriteMetadataTool::execute(&bad, &cfg);
    assert!(r.is_error.unwrap_or(false), "expected write to refuse");

    // Original target's tag is intact.
    let read = ReadMetadataParams {
        path: target.to_string_lossy().into_owned(),
        include_properties: false,
    };
    let r = ReadMetadataTool::execute(&read, &cfg);
    assert!(!r.is_error.unwrap_or(false));
    let result = r.structured_content.as_ref().unwrap();
    let tags = &result["metadata"];
    assert_eq!(tags["title"].as_str(), Some("Baseline Title"));

    // And no stray .tmp.* lingers next to the target.
    let target_name = target.file_name().unwrap().to_string_lossy().into_owned();
    let tmp_prefix = format!("{}.tmp.", target_name);
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(&tmp_prefix),
            "leftover temp file: {}",
            name
        );
    }
}
