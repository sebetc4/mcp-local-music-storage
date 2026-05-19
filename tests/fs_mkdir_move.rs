// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end tests for the `fs_mkdir` + `fs_move` pair — the autonomous
//! "organise" step. The reference scenario is: an inbox file lands at
//! `root/Artist/Album/01 Track.mp3` after the agent calls `fs_move` with
//! `mkdir_parents=true`, the intermediate directories appear, and the
//! source disappears.

use music_mcp_server::core::config::{Config, SecurityConfig};
use music_mcp_server::domains::tools::definitions::{
    FsMkdirTool, FsMoveTool,
    fs::{mkdir::FsMkdirParams, mv::FsMoveParams},
};
use rmcp::model::RawContent;
use std::fs;
use tempfile::TempDir;

fn config_rooted_at(root: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.security = SecurityConfig {
        root_path: Some(root.to_path_buf()),
        allow_symlinks: true,
    };
    cfg
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

/// Acceptance scenario from the roadmap: an inbox file lands under
/// `root/Artist/Album/`, the intermediate directories are created on the
/// way, and the source path no longer exists.
#[test]
fn organise_workflow_inbox_to_library() {
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    // Inbox holds a freshly-tagged file the agent picked up.
    let inbox = root.path().join("inbox");
    fs::create_dir(&inbox).unwrap();
    let source = inbox.join("track.mp3");
    fs::write(&source, b"audio bytes").unwrap();

    // Library tree doesn't exist yet — the agent calls fs_move with
    // mkdir_parents=true so the destination chain materialises atomically.
    let target = root
        .path()
        .join("library")
        .join("AC-DC")
        .join("1980 Back in Black")
        .join("01-01 Hells Bells.mp3");

    let r = FsMoveTool::execute(
        &FsMoveParams {
            from: source.to_string_lossy().into_owned(),
            to: target.to_string_lossy().into_owned(),
            mkdir_parents: true,
            overwrite: false,
            dry_run: false,
        },
        &cfg,
    );
    assert!(
        !r.is_error.unwrap_or(false),
        "fs_move failed: {}",
        extract_text(&r.content)
    );

    // Source gone, target landed with original bytes.
    assert!(!source.exists());
    assert!(target.is_file());
    assert_eq!(fs::read(&target).unwrap(), b"audio bytes");

    // Every intermediate directory exists.
    assert!(root.path().join("library").is_dir());
    assert!(root.path().join("library/AC-DC").is_dir());
    assert!(
        root.path()
            .join("library/AC-DC/1980 Back in Black")
            .is_dir()
    );

    // Structured payload reports the parents that were created (3 here:
    // library, library/AC-DC, library/AC-DC/1980 Back in Black).
    let s = r.structured_content.unwrap();
    assert_eq!(s["item_type"], "file");
    assert_eq!(s["strategy"], "rename");
    assert_eq!(s["created_parents"].as_array().unwrap().len(), 3);
}

/// A second canonical workflow step: agent uses `fs_mkdir` to provision an
/// album directory tree up-front, then moves files into it one by one.
/// Tests that `fs_mkdir` is idempotent (the second call succeeds with
/// `already_existed=true`).
#[test]
fn mkdir_then_move_files_into_album() {
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let inbox = root.path().join("inbox");
    fs::create_dir(&inbox).unwrap();
    fs::write(inbox.join("01.flac"), b"track 1").unwrap();
    fs::write(inbox.join("02.flac"), b"track 2").unwrap();

    let album = root.path().join("library/Artist/Album");
    let r = FsMkdirTool::execute(
        &FsMkdirParams {
            path: album.to_string_lossy().into_owned(),
            recursive: true,
            dry_run: false,
        },
        &cfg,
    );
    assert!(
        !r.is_error.unwrap_or(false),
        "fs_mkdir failed: {}",
        extract_text(&r.content)
    );
    assert!(album.is_dir());

    // Idempotent: re-running on the same album path is a no-op.
    let r2 = FsMkdirTool::execute(
        &FsMkdirParams {
            path: album.to_string_lossy().into_owned(),
            recursive: true,
            dry_run: false,
        },
        &cfg,
    );
    let s = r2.structured_content.unwrap();
    assert_eq!(s["already_existed"], true);

    // Then move each track into the album.
    for (name, expected) in [("01.flac", b"track 1"), ("02.flac", b"track 2")] {
        let from = inbox.join(name);
        let to = album.join(name);
        let r = FsMoveTool::execute(
            &FsMoveParams {
                from: from.to_string_lossy().into_owned(),
                to: to.to_string_lossy().into_owned(),
                mkdir_parents: false,
                overwrite: false,
                dry_run: false,
            },
            &cfg,
        );
        assert!(!r.is_error.unwrap_or(false));
        assert_eq!(&fs::read(&to).unwrap()[..], &expected[..]);
        assert!(!from.exists());
    }
}

/// Security regression: a destination resolving outside the configured root
/// must be refused — even when the agent constructs a clever traversal via
/// `..` components or names another existing directory outside.
#[test]
fn move_refuses_destination_escaping_root() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());

    let source = root.path().join("inbox.mp3");
    fs::write(&source, b"x").unwrap();

    // Case A: absolute path outside the root.
    let dst_outside = outside.path().join("stolen.mp3");
    let r = FsMoveTool::execute(
        &FsMoveParams {
            from: source.to_string_lossy().into_owned(),
            to: dst_outside.to_string_lossy().into_owned(),
            mkdir_parents: true,
            overwrite: false,
            dry_run: false,
        },
        &cfg,
    );
    assert!(r.is_error.unwrap_or(false));
    assert!(source.exists());
    assert!(!dst_outside.exists());

    // Case B: traversal via `..` that lexically resolves outside the root.
    let traversal_target = root
        .path()
        .join("inner")
        .join("..")
        .join("..")
        .join("escape.mp3");
    let r = FsMoveTool::execute(
        &FsMoveParams {
            from: source.to_string_lossy().into_owned(),
            to: traversal_target.to_string_lossy().into_owned(),
            mkdir_parents: true,
            overwrite: false,
            dry_run: false,
        },
        &cfg,
    );
    assert!(r.is_error.unwrap_or(false));
    assert!(source.exists());
}

/// `dry_run` reports the plan without writing anything. Useful when the
/// agent wants to preview a big organise pass before committing.
#[test]
fn dry_run_reports_plan_without_side_effects() {
    let root = TempDir::new().unwrap();
    let cfg = config_rooted_at(root.path());
    let src = root.path().join("inbox.mp3");
    fs::write(&src, b"x").unwrap();
    let dst = root.path().join("library/A/B/inbox.mp3");

    let r = FsMoveTool::execute(
        &FsMoveParams {
            from: src.to_string_lossy().into_owned(),
            to: dst.to_string_lossy().into_owned(),
            mkdir_parents: true,
            overwrite: false,
            dry_run: true,
        },
        &cfg,
    );
    assert!(!r.is_error.unwrap_or(false));

    let s = r.structured_content.unwrap();
    assert_eq!(s["dry_run"], true);
    assert_eq!(s["created_parents"].as_array().unwrap().len(), 3);

    // Nothing touched.
    assert!(src.exists());
    assert!(!root.path().join("library").exists());
}
