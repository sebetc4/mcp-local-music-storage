// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for `inventory_divergences`.
//!
//! Builds the roadmap's reference tree on disk with real tagged WAVs:
//!
//! ```text
//! /Rock/The Beatles/Abbey Road/01 Come Together.mp3   (tag artist = "Beatles")
//! /Rock/The Beatles/Abbey Road/02 Something.mp3       (tag artist = "The Beatles")
//! /Rock/Radiohead/OK Computer/01 Airbag.mp3           (tag artist = "Radiohead")
//! ```
//!
//! and asserts the histogram + divergence list match the roadmap's
//! acceptance criteria.

use music_mcp_server::core::config::{Config, SecurityConfig};
use music_mcp_server::domains::tools::definitions::harmonisation::inventory::{
    InventoryDivergencesParams, InventoryDivergencesTool,
};
use music_mcp_server::domains::tools::definitions::metadata::write::{
    WriteMetadataParams, WriteMetadataTool,
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

/// Minimal 144-byte PCM WAV — same shape used by the metadata tests. lofty
/// stores ID3v2 tags inside WAV, so this is enough to round-trip artist /
/// album / title.
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

fn write_wav_with_tags(
    path: &Path,
    artist: &str,
    album: &str,
    title: &str,
    genre: Option<&str>,
    config: &Config,
) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_minimal_wav(path);
    let params = WriteMetadataParams {
        path: path.to_string_lossy().into_owned(),
        title: Some(title.to_string()),
        artist: Some(artist.to_string()),
        album: Some(album.to_string()),
        album_artist: None,
        year: None,
        track: None,
        track_total: None,
        genre: genre.map(|g| g.to_string()),
        comment: None,
        clear_existing: false,
    };
    let result = WriteMetadataTool::execute(&params, config);
    assert!(
        !result.is_error.unwrap_or(false),
        "tag write failed for {:?}: {:?}",
        path,
        result.content
    );
}

#[test]
fn roadmap_fixture_inventory() {
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    // Build the roadmap's tree. The WAV extension is in lofty's default set
    // and the tool's default fields list includes `artist`, so this is the
    // shape the agent would actually hit on a music library scan.
    let beatles_dir = root.path().join("Rock/The Beatles/Abbey Road");
    let radiohead_dir = root.path().join("Rock/Radiohead/OK Computer");
    // Filenames include a track-number prefix — that's the convention real
    // music libraries follow, and exercises a multi-capture file slot with
    // the `:02d` format. The simpler `{title}.{ext}` from the roadmap
    // sketch would capture "01 Come Together" as the title and report a
    // spurious title divergence against the tag "Come Together".
    let come_together = beatles_dir.join("01 Come Together.wav");
    let something = beatles_dir.join("02 Something.wav");
    let airbag = radiohead_dir.join("01 Airbag.wav");

    write_wav_with_tags(
        &come_together,
        "Beatles",
        "Abbey Road",
        "Come Together",
        None,
        &cfg,
    );
    write_wav_with_tags(
        &something,
        "The Beatles",
        "Abbey Road",
        "Something",
        None,
        &cfg,
    );
    write_wav_with_tags(&airbag, "Radiohead", "OK Computer", "Airbag", None, &cfg);

    // Survey the tree with a realistic template that accounts for the
    // track-number prefix in filenames.
    let params = InventoryDivergencesParams {
        root: root.path().to_string_lossy().into_owned(),
        path_template: "{genre}/{artist}/{album}/{track:02d} {title}.{ext}".to_string(),
        fields_to_compare: None,
        case_sensitive: false,
        max_files: 5000,
        cursor: None,
        include_hidden: false,
    };
    let result = InventoryDivergencesTool::execute(&params, &cfg);
    assert!(
        !result.is_error.unwrap_or(false),
        "inventory failed: {:?}",
        result.content
    );
    let payload = result.structured_content.expect("structured content");

    // Three files matched; one diverges (Come Together's artist tag is
    // "Beatles" but the path says "The Beatles").
    assert_eq!(payload["files_scanned"], 3, "{}", payload);
    assert_eq!(payload["files_with_divergences"], 1);
    assert_eq!(payload["truncated"], false);

    // Two directories (alphabetical by path → OK Computer before The
    // Beatles in this filesystem).
    let dirs = payload["directories"].as_array().expect("directories");
    assert_eq!(dirs.len(), 2);

    // Find the Beatles album entry.
    let beatles = dirs
        .iter()
        .find(|d| d["path"].as_str().unwrap().ends_with("Abbey Road"))
        .expect("Abbey Road directory");
    let counts = &beatles["field_value_counts"]["artist"];
    // Histogram should show both spellings once each.
    assert_eq!(counts["Beatles"], 1, "artist histogram: {}", counts);
    assert_eq!(counts["The Beatles"], 1, "artist histogram: {}", counts);

    let files = beatles["files"].as_array().expect("files");
    assert_eq!(files.len(), 2);
    let come_together_entry = files
        .iter()
        .find(|f| f["name"].as_str().unwrap() == "01 Come Together.wav")
        .expect("Come Together entry");
    assert_eq!(
        come_together_entry["divergences"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["artist"]
    );
    let something_entry = files
        .iter()
        .find(|f| f["name"].as_str().unwrap() == "02 Something.wav")
        .expect("Something entry");
    assert!(
        something_entry["divergences"]
            .as_array()
            .unwrap()
            .is_empty(),
        "Something should not diverge (artist tag matches path): {}",
        something_entry
    );

    // Radiohead directory: no divergences anywhere.
    let radiohead = dirs
        .iter()
        .find(|d| d["path"].as_str().unwrap().ends_with("OK Computer"))
        .expect("OK Computer directory");
    let r_files = radiohead["files"].as_array().unwrap();
    assert_eq!(r_files.len(), 1);
    assert!(r_files[0]["divergences"].as_array().unwrap().is_empty());

    // Directory-level captures echoed back for use by the agent.
    assert_eq!(beatles["path_inferred"]["genre"], "Rock");
    assert_eq!(beatles["path_inferred"]["artist"], "The Beatles");
    assert_eq!(beatles["path_inferred"]["album"], "Abbey Road");
    assert!(beatles["path_inferred"].get("title").is_none());
    assert!(beatles["path_inferred"].get("ext").is_none());
}

#[test]
fn files_outside_template_surface_as_warnings() {
    // A file whose path doesn't fit the template must NOT abort the scan;
    // it lands in `warnings` and the well-shaped files still come through.
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let good = root
        .path()
        .join("Rock/Beatles/Abbey Road/01 Come Together.wav");
    write_wav_with_tags(&good, "Beatles", "Abbey Road", "Come Together", None, &cfg);

    // Stray file directly under root — doesn't match {genre}/{artist}/{album}/{title}.{ext}.
    let stray = root.path().join("readme.wav");
    write_minimal_wav(&stray);

    let params = InventoryDivergencesParams {
        root: root.path().to_string_lossy().into_owned(),
        path_template: "{genre}/{artist}/{album}/{title}.{ext}".to_string(),
        fields_to_compare: None,
        case_sensitive: false,
        max_files: 5000,
        cursor: None,
        include_hidden: false,
    };
    let result = InventoryDivergencesTool::execute(&params, &cfg);
    let payload = result.structured_content.unwrap();
    assert_eq!(payload["files_scanned"], 1);
    let warnings = payload["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("readme.wav")),
        "expected a warning for the stray file, got: {:?}",
        warnings
    );
}

#[test]
fn pagination_resumes_from_cursor_without_double_counting() {
    // Build a tree with three directories. Set max_files = 2 so the first
    // call covers exactly the first directory then bails before the second.
    // The cursor should resume cleanly into the remaining ones.
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let dir_a = root.path().join("Pop/AAA/Album1");
    let dir_b = root.path().join("Pop/BBB/Album1");
    let dir_c = root.path().join("Pop/CCC/Album1");

    write_wav_with_tags(&dir_a.join("01 X.wav"), "AAA", "Album1", "X", None, &cfg);
    write_wav_with_tags(&dir_a.join("02 Y.wav"), "AAA", "Album1", "Y", None, &cfg);
    write_wav_with_tags(&dir_b.join("01 X.wav"), "BBB", "Album1", "X", None, &cfg);
    write_wav_with_tags(&dir_b.join("02 Y.wav"), "BBB", "Album1", "Y", None, &cfg);
    write_wav_with_tags(&dir_c.join("01 X.wav"), "CCC", "Album1", "X", None, &cfg);

    let mut params = InventoryDivergencesParams {
        root: root.path().to_string_lossy().into_owned(),
        path_template: "{genre}/{artist}/{album}/{title}.{ext}".to_string(),
        fields_to_compare: None,
        case_sensitive: false,
        max_files: 2,
        cursor: None,
        include_hidden: false,
    };

    // First call: caps at 2 files, finalises dir_a, points cursor at it.
    let r1 = InventoryDivergencesTool::execute(&params, &cfg);
    let p1 = r1.structured_content.unwrap();
    assert_eq!(p1["truncated"], true);
    let dirs1 = p1["directories"].as_array().unwrap();
    assert_eq!(dirs1.len(), 1);
    assert!(dirs1[0]["path"].as_str().unwrap().ends_with("AAA/Album1"));
    let cursor = p1["next_cursor"].as_str().expect("cursor").to_string();
    assert_eq!(p1["files_scanned"], 2);

    // Second call: pick up from the cursor, expect dir_b and dir_c.
    params.cursor = Some(cursor);
    params.max_files = 5000;
    let r2 = InventoryDivergencesTool::execute(&params, &cfg);
    let p2 = r2.structured_content.unwrap();
    let dirs2 = p2["directories"].as_array().unwrap();
    assert_eq!(dirs2.len(), 2);
    assert!(dirs2[0]["path"].as_str().unwrap().ends_with("BBB/Album1"));
    assert!(dirs2[1]["path"].as_str().unwrap().ends_with("CCC/Album1"));
    assert_eq!(p2["truncated"], false);
    // files_scanned is cumulative across pages (2 from p1 + 3 new).
    assert_eq!(p2["files_scanned"], 5);
}
