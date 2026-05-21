// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for `manifest_write` / `manifest_read` / `manifest_list`.
//!
//! Uses `MCP_MANIFEST_DIR` to scope every test to its own tempdir so they
//! can run concurrently with the rest of the suite without scribbling on
//! the user's `~/.cache`.

use music_mcp_server::core::config::Config;
use music_mcp_server::domains::tools::definitions::harmonisation::manifest::{
    ManifestListParams, ManifestListTool, ManifestReadParams, ManifestReadTool,
    ManifestWriteParams, ManifestWriteTool,
};
use serde_json::json;
use std::sync::Mutex;
use tempfile::TempDir;

/// `std::env::set_var` is process-global, so serialise tests that mutate
/// `MCP_MANIFEST_DIR`. cargo runs integration tests in parallel by default;
/// without this guard they'd race and observe each other's manifest dirs.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct ScopedEnv<'a> {
    key: &'a str,
    previous: Option<String>,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl<'a> ScopedEnv<'a> {
    fn set(key: &'a str, value: &str) -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(key).ok();
        // SAFETY: tests are serialised by ENV_LOCK; this is the only thread
        // touching MCP_MANIFEST_DIR while the guard is held.
        unsafe { std::env::set_var(key, value) };
        Self {
            key,
            previous,
            _guard: guard,
        }
    }
}

impl<'a> Drop for ScopedEnv<'a> {
    fn drop(&mut self) {
        // SAFETY: still under ENV_LOCK.
        unsafe {
            match self.previous.as_deref() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn config() -> Config {
    Config::default()
}

#[test]
fn write_then_read_round_trips_non_trivial_payload() {
    let tmp = TempDir::new().unwrap();
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &tmp.path().to_string_lossy());

    // ~500 KB nested object — large enough to exercise the serialisation
    // path but well under the 10 MB cap.
    let big_payload = json!({
        "plan": (0..500).map(|i| json!({
            "op_index": i,
            "op": "write_metadata",
            "path": format!("/library/Artist {}/Album/01 Track.mp3", i),
            "fields": {
                "artist": format!("Artist {}", i),
                "album": "Album",
                "title": format!("Track {}", i),
            }
        })).collect::<Vec<_>>(),
        "session": "harmonize-2026-05-20",
        "notes": "regenerate after the upstream rename"
    });

    let write_result = ManifestWriteTool::execute(
        &ManifestWriteParams {
            id: "harmonize-big".to_string(),
            content: big_payload.clone(),
        },
        &config(),
    );
    assert!(
        !write_result.is_error.unwrap_or(false),
        "write failed: {:?}",
        write_result.content
    );
    let write_payload = write_result.structured_content.unwrap();
    assert!(write_payload["bytes"].as_u64().unwrap() > 10_000);
    assert!(
        write_payload["path"]
            .as_str()
            .unwrap()
            .ends_with("harmonize-big.json")
    );

    let read_result = ManifestReadTool::execute(
        &ManifestReadParams {
            id: "harmonize-big".to_string(),
        },
        &config(),
    );
    assert!(!read_result.is_error.unwrap_or(false));
    let read_payload = read_result.structured_content.unwrap();
    assert!(read_payload["error"].is_null() || read_payload.get("error").is_none());
    assert_eq!(read_payload["content"], big_payload);
    assert!(read_payload["bytes"].as_u64().unwrap() > 10_000);
    assert!(read_payload["written_at"].as_str().is_some());
}

#[test]
fn second_write_with_same_id_overwrites_without_corruption() {
    let tmp = TempDir::new().unwrap();
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &tmp.path().to_string_lossy());

    let id = "harmonize-overwrite";
    let v1 = json!({ "version": 1, "step": "scan" });
    let v2 = json!({ "version": 2, "step": "apply", "files_touched": 42 });

    let r1 = ManifestWriteTool::execute(
        &ManifestWriteParams {
            id: id.to_string(),
            content: v1.clone(),
        },
        &config(),
    );
    assert!(!r1.is_error.unwrap_or(false));

    let r2 = ManifestWriteTool::execute(
        &ManifestWriteParams {
            id: id.to_string(),
            content: v2.clone(),
        },
        &config(),
    );
    assert!(!r2.is_error.unwrap_or(false));

    // Read back: must be v2, no leftover bytes from v1, no .tmp.* lingering.
    let read = ManifestReadTool::execute(&ManifestReadParams { id: id.to_string() }, &config());
    assert_eq!(read.structured_content.unwrap()["content"], v2);

    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        leftover.is_empty(),
        "atomic write left a temp file behind: {:?}",
        leftover
    );
}

#[test]
fn read_of_missing_manifest_returns_notfound_not_error() {
    let tmp = TempDir::new().unwrap();
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &tmp.path().to_string_lossy());

    let result = ManifestReadTool::execute(
        &ManifestReadParams {
            id: "never-written".to_string(),
        },
        &config(),
    );
    // The call SUCCEEDS at the tool level — agents need to distinguish
    // "first run" from "malformed call".
    assert!(!result.is_error.unwrap_or(false));
    let payload = result.structured_content.unwrap();
    assert_eq!(payload["error"], "NotFound");
    assert!(payload["content"].is_null());
    assert!(payload["written_at"].is_null());
    assert!(payload["bytes"].is_null());
}

#[test]
fn list_includes_every_manifest_sorted_by_recency() {
    let tmp = TempDir::new().unwrap();
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &tmp.path().to_string_lossy());

    // Write three manifests with explicit ordering. We use a small sleep
    // between writes so the RFC3339-second-precision mtimes stay distinct
    // on platforms whose mtime resolution is coarser than nanoseconds.
    for (i, id) in ["run-alpha", "run-bravo", "run-charlie"].iter().enumerate() {
        let r = ManifestWriteTool::execute(
            &ManifestWriteParams {
                id: id.to_string(),
                content: json!({ "index": i }),
            },
            &config(),
        );
        assert!(!r.is_error.unwrap_or(false));
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }

    let result = ManifestListTool::execute(&ManifestListParams {}, &config());
    assert!(!result.is_error.unwrap_or(false));
    let payload = result.structured_content.unwrap();
    assert_eq!(payload["total"], 3);
    assert_eq!(payload["truncated"], false);
    let manifests = payload["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 3);
    // First entry is the most recent → "run-charlie".
    assert_eq!(manifests[0]["id"], "run-charlie");
    assert_eq!(manifests[1]["id"], "run-bravo");
    assert_eq!(manifests[2]["id"], "run-alpha");
    // Every entry has the structured fields.
    for m in manifests {
        assert!(m["bytes"].as_u64().unwrap() > 0);
        assert!(m["written_at"].as_str().unwrap().contains('T'));
    }
}

#[test]
fn list_on_missing_dir_returns_empty_not_error() {
    let tmp = TempDir::new().unwrap();
    let absent = tmp.path().join("never-created");
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &absent.to_string_lossy());

    let result = ManifestListTool::execute(&ManifestListParams {}, &config());
    assert!(!result.is_error.unwrap_or(false));
    let payload = result.structured_content.unwrap();
    assert_eq!(payload["total"], 0);
    assert!(payload["manifests"].as_array().unwrap().is_empty());
}

#[test]
fn invalid_id_refused_consistently_across_tools() {
    let tmp = TempDir::new().unwrap();
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &tmp.path().to_string_lossy());

    // All three forms are refused.
    for bad_id in ["../escape", ".hidden", "a/b", "with spaces", ""] {
        let r = ManifestWriteTool::execute(
            &ManifestWriteParams {
                id: bad_id.to_string(),
                content: json!({}),
            },
            &config(),
        );
        assert!(
            r.is_error.unwrap_or(false),
            "write should reject id={:?}",
            bad_id
        );
        // No file landed.
        let dir_listing: Vec<_> = std::fs::read_dir(tmp.path())
            .map(|i| i.flatten().collect())
            .unwrap_or_default();
        assert!(
            dir_listing.is_empty(),
            "refused id={:?} leaked a file: {:?}",
            bad_id,
            dir_listing
        );
        // read also refuses.
        let read = ManifestReadTool::execute(
            &ManifestReadParams {
                id: bad_id.to_string(),
            },
            &config(),
        );
        assert!(
            read.is_error.unwrap_or(false),
            "read should reject id={:?}",
            bad_id
        );
    }
}

#[test]
fn manifest_too_large_refused() {
    let tmp = TempDir::new().unwrap();
    let _scope = ScopedEnv::set("MCP_MANIFEST_DIR", &tmp.path().to_string_lossy());

    // Build a manifest just over the 10 MB cap by stuffing a large string
    // into the JSON. The exact byte count after serialisation is slightly
    // larger than the raw string (quotes + key + braces), so 11 MB of 'a'
    // is comfortably over the cap.
    let huge: String = "a".repeat(11 * 1024 * 1024);
    let r = ManifestWriteTool::execute(
        &ManifestWriteParams {
            id: "too-big".to_string(),
            content: json!({ "blob": huge }),
        },
        &config(),
    );
    assert!(r.is_error.unwrap_or(false), "expected refusal, got OK");
    // No file landed.
    assert!(
        !tmp.path().join("too-big.json").exists(),
        "refused write left a file behind"
    );
}
