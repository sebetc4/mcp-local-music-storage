// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Acceptance test for the `fs_scan_audio` tool — the recursive audio walk
//! that lets the agent inspect a library in a single round-trip instead of
//! one call per directory.
//!
//! Roadmap scenario: synthesize a tempdir with N nested audio files + M
//! non-audio + 1 symlink, scan in two paginated calls, verify the symlink
//! is rejected by policy and reported as a warning rather than aborting.

use music_mcp_server::core::config::{Config, SecurityConfig};
use music_mcp_server::domains::tools::definitions::{
    FsScanAudioTool, fs::scan_audio::FsScanAudioParams,
};
use std::fs;
use tempfile::TempDir;

fn config_strict_symlinks(root: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.security = SecurityConfig {
        root_path: Some(root.to_path_buf()),
        allow_symlinks: false,
    };
    cfg
}

fn default_extensions() -> Vec<String> {
    vec![
        "mp3".into(),
        "flac".into(),
        "m4a".into(),
        "ogg".into(),
        "opus".into(),
        "wav".into(),
    ]
}

/// Roadmap acceptance: 100 nested audio files + 100 non-audio + 1 symlink.
/// First call with `max_results=50` returns 50 + cursor; second call resumes
/// with the cursor and returns the remaining 50. Symlink is rejected by the
/// strict policy and surfaced as a warning instead of aborting the scan.
#[cfg(unix)]
#[test]
fn scan_audio_paginates_and_rejects_symlinks() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let cfg = config_strict_symlinks(root.path());

    // 100 audio files spread across 5 album directories (20 each).
    for album in 0..5 {
        let dir = root.path().join(format!("album_{:02}", album));
        fs::create_dir(&dir).unwrap();
        for i in 0..20 {
            fs::write(dir.join(format!("track_{:02}.mp3", i)), b"audio").unwrap();
        }
    }

    // 100 non-audio files in a peer directory.
    let notes = root.path().join("notes");
    fs::create_dir(&notes).unwrap();
    for i in 0..100 {
        fs::write(notes.join(format!("note_{:03}.txt", i)), b"text").unwrap();
    }

    // One symlink to a real audio file, placed inside the first album so
    // walkdir visits it during page 1 (per-directory sort puts `_alias`
    // ahead of `track_…`). Under strict policy this is rejected and
    // surfaced as a warning during the walk.
    let real = root.path().join("album_00/track_00.mp3");
    symlink(&real, root.path().join("album_00/_alias.mp3")).unwrap();

    // First page.
    let mut params = FsScanAudioParams {
        root: root.path().to_string_lossy().into_owned(),
        extensions: default_extensions(),
        max_depth: 16,
        max_results: 50,
        cursor: None,
        include_hidden: false,
    };

    let r1 = FsScanAudioTool::execute(&params, &cfg);
    assert!(!r1.is_error.unwrap_or(false));
    let s1 = r1.structured_content.unwrap();

    let files1 = s1["files"].as_array().unwrap();
    assert_eq!(files1.len(), 50);
    assert_eq!(s1["truncated"], true);
    assert_eq!(s1["total_seen"], 50);
    let cursor = s1["next_cursor"].as_str().unwrap().to_string();
    assert!(!cursor.is_empty());

    // Symlink rejection lands in warnings on page 1: per-directory sort
    // visits `_alias.mp3` ahead of the `track_NN.mp3` siblings, well
    // before the page hits its max_results cap.
    let warnings1 = s1["warnings"].as_array().unwrap();
    let warned_about_symlink = warnings1
        .iter()
        .any(|w| w.as_str().unwrap_or("").contains("_alias.mp3"));
    assert!(
        warned_about_symlink,
        "expected a symlink warning on page 1, got warnings: {:?}",
        warnings1
    );

    // Second page resumes from the cursor.
    params.cursor = Some(cursor);
    let r2 = FsScanAudioTool::execute(&params, &cfg);
    assert!(!r2.is_error.unwrap_or(false));
    let s2 = r2.structured_content.unwrap();

    let files2 = s2["files"].as_array().unwrap();
    assert_eq!(files2.len(), 50);
    assert_eq!(s2["truncated"], false);
    assert!(s2["next_cursor"].is_null());
    assert_eq!(s2["total_seen"], 100);

    // Combined output: 100 unique audio paths, none of them the symlink.
    let mut combined: Vec<String> = files1
        .iter()
        .chain(files2.iter())
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    combined.sort();
    combined.dedup();
    assert_eq!(combined.len(), 100);
    assert!(
        !combined.iter().any(|p| p.ends_with("_alias.mp3")),
        "symlink should not appear in scan results"
    );

    // Every emitted entry is one of the real .mp3 tracks we created.
    for path in &combined {
        assert!(path.ends_with(".mp3"));
        assert!(path.contains("album_"));
    }
}

/// Sanity check: with the default extension list (which excludes `txt`), a
/// directory containing only non-audio files yields an empty `files` array
/// and a non-truncated response — not an error.
#[test]
fn scan_audio_empty_when_no_matches() {
    let root = TempDir::new().unwrap();
    let cfg = config_strict_symlinks(root.path());

    fs::write(root.path().join("readme.txt"), b"hi").unwrap();
    fs::write(root.path().join("cover.jpg"), b"img").unwrap();

    let params = FsScanAudioParams {
        root: root.path().to_string_lossy().into_owned(),
        extensions: default_extensions(),
        max_depth: 16,
        max_results: 5000,
        cursor: None,
        include_hidden: false,
    };

    let r = FsScanAudioTool::execute(&params, &cfg);
    assert!(!r.is_error.unwrap_or(false));
    let s = r.structured_content.unwrap();
    assert_eq!(s["files"].as_array().unwrap().len(), 0);
    assert_eq!(s["truncated"], false);
    assert!(s["next_cursor"].is_null());
    assert_eq!(s["total_seen"], 0);
}
