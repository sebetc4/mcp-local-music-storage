// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end integration tests for `apply_plan`.
//!
//! These tests drive the same mkdir → tag → embed cover → move pipeline an
//! autonomous run would assemble, in a single `apply_plan` call, against a
//! real tempdir. The unit tests in `domains::tools::definitions::plan` cover
//! the four dry-run × stop-on-error quadrants in isolation; this file proves
//! the four singletons compose cleanly under one call.

use base64::Engine;
use music_mcp_server::core::config::{Config, SecurityConfig};
use music_mcp_server::domains::tools::definitions::fs::{mkdir::FsMkdirParams, mv::FsMoveParams};
use music_mcp_server::domains::tools::definitions::metadata::{
    embed_cover::EmbedCoverParams,
    read::{ReadMetadataParams, ReadMetadataTool},
    write::WriteMetadataParams,
};
use music_mcp_server::domains::tools::definitions::plan::apply_plan::{
    ApplyPlanParams, ApplyPlanTool, Operation,
};
use std::path::Path;
use tempfile::TempDir;

fn config_rooted_at(root: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.security = SecurityConfig {
        root_path: Some(root.to_path_buf()),
        allow_symlinks: true,
    };
    cfg
}

/// Minimal 144-byte PCM WAV — same fixture shape used by
/// `embed_cover_roundtrip.rs` and `metadata_roundtrip.rs`.
fn write_minimal_wav(path: &Path) {
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

/// 67-byte 1×1 RGB PNG (same fixture as `embed_cover_roundtrip.rs`).
fn tiny_png_base64() -> String {
    let bytes: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, b'I', b'D', b'A', b'T', 0x08, 0x99, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x5D, 0x76, 0xEA, 0x9E, 0x00, 0x00, 0x00,
        0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
    ];
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

fn write_metadata_params(path: &Path) -> WriteMetadataParams {
    WriteMetadataParams {
        path: path.to_string_lossy().into_owned(),
        title: Some("Hells Bells".to_string()),
        artist: Some("AC/DC".to_string()),
        album: Some("Back in Black".to_string()),
        album_artist: None,
        year: Some(1980),
        track: Some(1),
        track_total: None,
        genre: None,
        comment: None,
        clear_existing: false,
    }
}

fn build_organise_plan(
    inbox_track: &Path,
    library_album_dir: &Path,
    final_dest: &Path,
    dry_run: bool,
    stop_on_error: bool,
) -> ApplyPlanParams {
    ApplyPlanParams {
        operations: vec![
            Operation::Mkdir(FsMkdirParams {
                path: library_album_dir.to_string_lossy().into_owned(),
                recursive: true,
                dry_run: false,
            }),
            Operation::WriteMetadata(write_metadata_params(inbox_track)),
            Operation::EmbedCover(EmbedCoverParams {
                path: inbox_track.to_string_lossy().into_owned(),
                image_path: None,
                image_bytes_base64: Some(tiny_png_base64()),
                picture_type: "CoverFront".to_string(),
                description: None,
                replace_existing: false,
            }),
            Operation::Move(FsMoveParams {
                from: inbox_track.to_string_lossy().into_owned(),
                to: final_dest.to_string_lossy().into_owned(),
                // Defensive: under dry-run the prior mkdir never landed, so
                // the move would otherwise refuse "parent does not exist".
                mkdir_parents: true,
                overwrite: false,
                dry_run: false,
            }),
        ],
        stop_on_error,
        dry_run,
    }
}

#[test]
fn end_to_end_organise_pipeline_in_one_call() {
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let inbox = root.path().join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let inbox_track = inbox.join("track.wav");
    write_minimal_wav(&inbox_track);

    let album_dir = root
        .path()
        .join("library")
        .join("AC-DC")
        .join("Back in Black");
    let dest = album_dir.join("01 Hells Bells.wav");

    let plan = build_organise_plan(&inbox_track, &album_dir, &dest, false, false);
    let result = ApplyPlanTool::execute(&plan, &cfg);
    assert!(
        !result.is_error.unwrap_or(false),
        "plan failed: {:?}",
        result.content
    );

    let s = result.structured_content.unwrap();
    assert_eq!(s["ok_count"], 4, "structured: {}", s);
    assert_eq!(s["error_count"], 0);
    assert_eq!(s["skipped"], 0);
    assert_eq!(s["dry_run"], false);

    // Filesystem reflects the full pipeline.
    assert!(album_dir.is_dir());
    assert!(dest.is_file());
    assert!(!inbox_track.exists());

    // Tags landed; verify via the singleton read path.
    let read = ReadMetadataTool::execute(
        &ReadMetadataParams {
            path: dest.to_string_lossy().into_owned(),
            include_properties: true,
        },
        &cfg,
    );
    assert!(!read.is_error.unwrap_or(false));
    let sr = read.structured_content.unwrap();
    assert_eq!(sr["metadata"]["title"], "Hells Bells");
    assert_eq!(sr["metadata"]["artist"], "AC/DC");
    assert_eq!(sr["metadata"]["album"], "Back in Black");
    // Picture embedded — pictures block lists it.
    let pics = sr["pictures"]
        .as_array()
        .expect("pictures should be reported under include_properties");
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0]["mime_type"], "image/png");
}

#[test]
fn dry_run_validates_full_pipeline_without_touching_anything() {
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let inbox = root.path().join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let inbox_track = inbox.join("track.wav");
    write_minimal_wav(&inbox_track);
    let before = std::fs::read(&inbox_track).unwrap();

    let album_dir = root.path().join("library").join("Artist").join("Album");
    let dest = album_dir.join("01 Track.wav");

    let plan = build_organise_plan(&inbox_track, &album_dir, &dest, true, false);
    let result = ApplyPlanTool::execute(&plan, &cfg);
    assert!(!result.is_error.unwrap_or(false));

    let s = result.structured_content.unwrap();
    assert_eq!(s["ok_count"], 4);
    assert_eq!(s["dry_run"], true);

    // Every per-op result records dry_run=true.
    for op_result in s["results"].as_array().unwrap() {
        assert_eq!(
            op_result["dry_run"], true,
            "op {} not dry: {}",
            op_result["op_index"], op_result
        );
        assert_eq!(op_result["status"], "ok");
    }

    // Filesystem unchanged.
    assert!(!album_dir.exists());
    assert!(inbox_track.is_file());
    assert_eq!(std::fs::read(&inbox_track).unwrap(), before);
}

#[test]
fn stop_on_error_halts_pipeline_without_rollback() {
    // Run the same 4-op pipeline, but break the EmbedCover op by giving it
    // two image sources at once. Under stop_on_error=true the move should
    // never run, and the earlier mkdir + write_metadata MUST stay committed
    // (the non-rollback contract documented in the tool's doc-comment).
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let inbox = root.path().join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let inbox_track = inbox.join("track.wav");
    write_minimal_wav(&inbox_track);

    let album_dir = root.path().join("library").join("X").join("Y");
    let dest = album_dir.join("01 Track.wav");

    let mut plan = build_organise_plan(&inbox_track, &album_dir, &dest, false, true);
    // Sabotage the embed_cover op: both image sources provided.
    if let Operation::EmbedCover(ref mut p) = plan.operations[2] {
        p.image_path = Some("/whatever.jpg".to_string());
        // image_bytes_base64 already set by the builder
    }

    let result = ApplyPlanTool::execute(&plan, &cfg);
    assert!(!result.is_error.unwrap_or(false));
    let s = result.structured_content.unwrap();

    // Three ops attempted (mkdir ok, write ok, embed_cover error). The move
    // (op 3) never ran.
    assert_eq!(s["executed"], 3, "executed counter wrong: {}", s);
    assert_eq!(s["ok_count"], 2);
    assert_eq!(s["error_count"], 1);
    assert_eq!(s["skipped"], 1);
    assert_eq!(s["stopped_early"], true);

    // mkdir and write_metadata stayed committed; move did not run.
    assert!(album_dir.is_dir(), "non-rollback: mkdir should persist");
    assert!(
        inbox_track.is_file(),
        "non-rollback: source still in inbox (move never ran)"
    );
    assert!(
        !dest.exists(),
        "destination should be untouched (move never ran)"
    );

    // The committed write_metadata is observable on the source file.
    let read = ReadMetadataTool::execute(
        &ReadMetadataParams {
            path: inbox_track.to_string_lossy().into_owned(),
            include_properties: false,
        },
        &cfg,
    );
    let sr = read.structured_content.unwrap();
    assert_eq!(sr["metadata"]["title"], "Hells Bells");

    // Final op is the embed_cover failure.
    let results = s["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[2]["status"], "error");
    assert_eq!(results[2]["op_kind"], "embed_cover");
}
