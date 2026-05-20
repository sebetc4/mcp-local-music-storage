// Integration tests legitimately use `.unwrap()` on test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end protocol tests for the HTTP transport.
//!
//! Drives the JSON-RPC layer through the real axum router via
//! `tower::ServiceExt::oneshot`, no socket involved. Covers the three
//! protocol methods clients use in normal operation: `initialize`,
//! `tools/list`, and `tools/call`.
//!
//! STDIO is exercised by rmcp's own harness (used in unit tests through the
//! router). TCP would need a live socket; deferred until we have a need.

#![cfg(feature = "http")]

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use music_mcp_server::core::transport::http::HttpTransport;
use music_mcp_server::core::{Config, McpServer};
use serde_json::{Value, json};
use tower::ServiceExt;

const RPC_PATH: &str = "/mcp";

async fn jsonrpc_call(server: McpServer, body: Value) -> (StatusCode, Value) {
    let app = HttpTransport::build_router(server, RPC_PATH);
    let request = Request::builder()
        .method(Method::POST)
        .uri(RPC_PATH)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

fn server() -> McpServer {
    McpServer::new(Config::default())
}

#[tokio::test]
async fn initialize_returns_server_info_and_capabilities() {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });

    let (status, resp) = jsonrpc_call(server(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    let result = &resp["result"];
    assert!(!result.is_null(), "expected result, got: {}", resp);
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["serverInfo"]["name"].is_string());
    assert!(result["serverInfo"]["version"].is_string());
}

#[tokio::test]
async fn tools_list_returns_all_twenty_three_tools() {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });

    let (status, resp) = jsonrpc_call(server(), req).await;
    assert_eq!(status, StatusCode::OK);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        23,
        "expected 23 tools (the `foreach_tool!` inventory), got {}",
        tools.len()
    );

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "apply_naming_scheme",
        "apply_plan",
        "embed_cover",
        "find_duplicates",
        "fs_hash",
        "fs_delete",
        "fs_list_dir",
        "fs_mkdir",
        "fs_move",
        "fs_rename",
        "fs_scan_audio",
        "mb_artist_search",
        "mb_cover_download",
        "mb_identify_record",
        "mb_label_search",
        "mb_match_from_tags",
        "mb_recording_search",
        "mb_release_search",
        "mb_work_search",
        "read_metadata",
        "read_metadata_batch",
        "write_metadata",
        "write_metadata_batch",
    ] {
        assert!(names.contains(&expected), "missing tool: {}", expected);
    }
}

#[tokio::test]
async fn tools_call_dispatches_to_fs_list_dir() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "fs_list_dir",
            "arguments": {
                "path": dir.path().to_string_lossy()
            }
        }
    });

    let (status, resp) = jsonrpc_call(server(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp["error"].is_null(),
        "tools/call returned error: {}",
        resp
    );
    // The tool must have run and reported the file we just created.
    let body = serde_json::to_string(&resp["result"]).unwrap();
    assert!(
        body.contains("hello.txt"),
        "expected hello.txt in result, got: {}",
        body
    );
}

#[tokio::test]
async fn tools_call_unknown_method_returns_method_not_found() {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "this/does/not/exist"
    });

    let (status, resp) = jsonrpc_call(server(), req).await;
    // HTTP layer always returns 200; JSON-RPC carries the error.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["error"]["code"], -32601);
}

#[tokio::test]
async fn tools_call_missing_name_returns_invalid_params() {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "arguments": {}
        }
    });

    let (status, resp) = jsonrpc_call(server(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["error"]["code"], -32602);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("name"),
        "expected 'name'-related error message, got: {}",
        resp["error"]["message"]
    );
}

#[tokio::test]
async fn invalid_jsonrpc_version_returns_invalid_request() {
    let req = json!({
        "jsonrpc": "1.0",
        "id": 6,
        "method": "initialize"
    });

    let (status, resp) = jsonrpc_call(server(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["error"]["code"], -32600);
}
