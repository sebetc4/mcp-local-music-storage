// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end tests for `read_metadata_batch` / `write_metadata_batch`.
//!
//! Roadmap acceptance scenario (Phase 2.2): batch-write to 5 WAVs (4 valid,
//! 1 missing). Assert the call itself succeeds, 4 entries report `error=null`
//! and the missing one carries an explanatory error. Then read them all back
//! in a single `read_metadata_batch` to confirm the tags survived.

use music_mcp_server::core::config::Config;
use music_mcp_server::domains::tools::definitions::metadata::{
    read::ReadMetadataParams,
    read_batch::{ReadMetadataBatchParams, ReadMetadataBatchTool},
    write::WriteMetadataParams,
    write_batch::{WriteMetadataBatchParams, WriteMetadataBatchTool},
};
use tempfile::TempDir;

/// Minimal 144-byte PCM WAV (copied from `metadata_roundtrip.rs`). Smallest
/// container lofty will accept for the WAV read-and-tag path.
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
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&8000u32.to_le_bytes());
    buf.extend_from_slice(&8000u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    buf.extend(std::iter::repeat(0u8).take(sample_count as usize));

    std::fs::write(path, &buf).unwrap();
}

fn title_write(path: &std::path::Path, title: &str, track: u32) -> WriteMetadataParams {
    WriteMetadataParams {
        path: path.to_string_lossy().into_owned(),
        title: Some(title.to_string()),
        artist: Some("Batch Artist".into()),
        album: Some("Batch Album".into()),
        album_artist: None,
        year: Some(2026),
        track: Some(track),
        track_total: None,
        genre: Some("Test".into()),
        comment: None,
        clear_existing: false,
    }
}

/// Roadmap acceptance: 4 valid WAVs + 1 missing target. Batch succeeds
/// overall, missing target lands as a per-item error, the 4 valid writes
/// landed on disk (verified via a batch read-back).
#[test]
fn batch_write_then_read_with_one_missing() {
    let dir = TempDir::new().unwrap();
    let cfg = Config::default();

    // 4 real WAVs.
    let mut writes: Vec<WriteMetadataParams> = Vec::new();
    let mut real_paths: Vec<std::path::PathBuf> = Vec::new();
    for i in 0..4 {
        let p = dir.path().join(format!("track_{:02}.wav", i + 1));
        write_minimal_wav(&p);
        writes.push(title_write(&p, &format!("Title {}", i + 1), (i + 1) as u32));
        real_paths.push(p);
    }
    // Insert the missing target in the middle of the batch so we exercise
    // both "before" and "after" siblings.
    let missing = dir.path().join("does_not_exist.wav");
    writes.insert(2, title_write(&missing, "Phantom", 99));
    let missing_index = 2;

    let batch = WriteMetadataBatchParams {
        writes,
        stop_on_error: false,
    };
    let r = WriteMetadataBatchTool::execute(&batch, &cfg);
    assert!(
        !r.is_error.unwrap_or(false),
        "batch call must succeed even with per-item failures"
    );

    let s = r.structured_content.expect("structured output");
    let results = s["results"].as_array().unwrap();
    assert_eq!(results.len(), 5);
    assert_eq!(s["ok_count"], 4);
    assert_eq!(s["error_count"], 1);
    assert_eq!(s["stopped_early"], false);
    assert_eq!(s["skipped"], 0);

    // The missing entry carries an error; the four neighbours don't.
    for (i, entry) in results.iter().enumerate() {
        if i == missing_index {
            assert!(
                entry["error"].as_str().is_some(),
                "expected error on missing entry, got: {}",
                entry
            );
            assert_eq!(entry["fields_updated"], 0);
        } else {
            assert!(
                entry["error"].is_null(),
                "expected no error on valid entry, got: {}",
                entry
            );
            // 5 fields written: title, artist, album, year, track, genre = 6.
            // (Empty optionals don't count.) Validate it's positive — exact
            // count is locked in by the singleton's contract.
            assert!(entry["fields_updated"].as_u64().unwrap() >= 5);
        }
    }

    // Read everything back in one batch call and confirm the tags landed.
    let read_paths: Vec<String> = real_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let read = ReadMetadataBatchParams {
        paths: read_paths,
        include_properties: false,
    };
    let r = ReadMetadataBatchTool::execute(&read, &cfg);
    assert!(!r.is_error.unwrap_or(false));
    let s = r.structured_content.unwrap();
    assert_eq!(s["ok_count"], 4);
    assert_eq!(s["error_count"], 0);
    let results = s["results"].as_array().unwrap();
    for (i, entry) in results.iter().enumerate() {
        assert!(entry["error"].is_null());
        let meta = &entry["metadata"]["metadata"];
        assert_eq!(
            meta["title"].as_str(),
            Some(format!("Title {}", i + 1)).as_deref()
        );
        assert_eq!(meta["artist"].as_str(), Some("Batch Artist"));
        assert_eq!(meta["album"].as_str(), Some("Batch Album"));
        assert_eq!(meta["year"].as_u64(), Some(2026));
        assert_eq!(meta["track"].as_u64(), Some((i + 1) as u64));
        assert_eq!(meta["genre"].as_str(), Some("Test"));
    }
}

/// `stop_on_error=true`: a failing write halts the loop. Already-written
/// files stay written; remaining items are reported as skipped (not in
/// `results`, but counted in `skipped`).
#[test]
fn batch_write_stop_on_error_halts_after_first_failure() {
    let dir = TempDir::new().unwrap();
    let cfg = Config::default();

    let valid = dir.path().join("ok.wav");
    write_minimal_wav(&valid);

    let writes = vec![
        title_write(&valid, "Will Land", 1),
        title_write(&dir.path().join("missing.wav"), "Phantom", 2),
        title_write(&valid, "Should Be Skipped", 3),
    ];

    let r = WriteMetadataBatchTool::execute(
        &WriteMetadataBatchParams {
            writes,
            stop_on_error: true,
        },
        &cfg,
    );
    assert!(!r.is_error.unwrap_or(false));
    let s = r.structured_content.unwrap();
    assert_eq!(s["results"].as_array().unwrap().len(), 2);
    assert_eq!(s["ok_count"], 1);
    assert_eq!(s["error_count"], 1);
    assert_eq!(s["stopped_early"], true);
    assert_eq!(s["skipped"], 1);

    // Read back: the first write landed, the third never ran.
    let r = ReadMetadataBatchTool::execute(
        &ReadMetadataBatchParams {
            paths: vec![valid.to_string_lossy().into_owned()],
            include_properties: false,
        },
        &cfg,
    );
    let s = r.structured_content.unwrap();
    let entry = &s["results"][0];
    assert!(entry["error"].is_null());
    let meta = &entry["metadata"]["metadata"];
    // Title is the first write's value — the third never had a chance.
    assert_eq!(meta["title"].as_str(), Some("Will Land"));
}

/// A read batch handed an empty list returns an empty (but well-formed)
/// payload. Used by the agent as a no-op probe — must not error.
#[test]
fn batch_read_empty_list_is_a_clean_noop() {
    let cfg = Config::default();
    let r = ReadMetadataBatchTool::execute(
        &ReadMetadataBatchParams {
            paths: vec![],
            include_properties: false,
        },
        &cfg,
    );
    assert!(!r.is_error.unwrap_or(false));
    let s = r.structured_content.unwrap();
    assert_eq!(s["results"].as_array().unwrap().len(), 0);
    assert_eq!(s["ok_count"], 0);
    assert_eq!(s["error_count"], 0);
}

/// Singleton parity: a `read_metadata_batch` over one path should produce a
/// `metadata` payload structurally compatible with a singleton
/// `read_metadata` call — proves batches don't drift from the per-file
/// contract.
#[test]
fn batch_read_singleton_parity() {
    let dir = TempDir::new().unwrap();
    let cfg = Config::default();
    let path = dir.path().join("one.wav");
    write_minimal_wav(&path);

    // Stage tags via the singleton write tool.
    use music_mcp_server::domains::tools::definitions::metadata::write::WriteMetadataTool;
    let w = title_write(&path, "Parity", 1);
    let r = WriteMetadataTool::execute(&w, &cfg);
    assert!(!r.is_error.unwrap_or(false));

    // Singleton read.
    use music_mcp_server::domains::tools::definitions::metadata::read::ReadMetadataTool;
    let singleton = ReadMetadataTool::execute(
        &ReadMetadataParams {
            path: path.to_string_lossy().into_owned(),
            include_properties: false,
        },
        &cfg,
    );
    let singleton_meta = singleton.structured_content.unwrap();

    // Batch read with the same path.
    let batch = ReadMetadataBatchTool::execute(
        &ReadMetadataBatchParams {
            paths: vec![path.to_string_lossy().into_owned()],
            include_properties: false,
        },
        &cfg,
    );
    let batch_meta = &batch.structured_content.unwrap()["results"][0]["metadata"];

    // The batch's per-item metadata is the singleton output verbatim.
    assert_eq!(*batch_meta, singleton_meta);
}
