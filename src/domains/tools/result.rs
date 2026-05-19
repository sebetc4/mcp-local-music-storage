//! Shared helpers for building `CallToolResult` payloads.
//!
//! rmcp 1.7 made [`CallToolResult`] and [`Tool`] `#[non_exhaustive]`: callers
//! can no longer use struct literals from external crates. These helpers
//! centralise the "success + text summary + structured JSON" pattern that
//! every tool execute() converges on, and avoid duplicating the
//! "serialise-or-degrade" fallback across 13+ tools.

use rmcp::model::{CallToolResult, Content};
use serde::Serialize;
use tracing::warn;

/// Build a successful [`CallToolResult`] carrying a human-readable text
/// summary plus a structured JSON payload derived from `data`. If serialising
/// `data` fails, falls back to a text-only success so the caller still sees
/// the operation succeeded.
pub fn structured_ok<T: Serialize>(summary: impl Into<String>, data: &T) -> CallToolResult {
    let summary = summary.into();
    let mut result = CallToolResult::success(vec![Content::text(summary.clone())]);
    match serde_json::to_value(data) {
        Ok(value) => {
            result.structured_content = Some(value);
        }
        Err(e) => {
            warn!("Failed to serialize structured content: {}", e);
        }
    }
    result
}
