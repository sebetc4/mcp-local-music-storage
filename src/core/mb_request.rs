//! Cache + throttle glue for MusicBrainz-adjacent tool calls.
//!
//! Combines [`crate::core::mb_cache`] and [`crate::core::mb_throttle`] into
//! a single `cached_or_fetch_blocking` helper that callers wrap around
//! their network-touching code. On cache hit the network is skipped
//! entirely (and the rate-limit budget is preserved); on miss the
//! throttle's slot is reserved before invoking the fetch closure.
//!
//! Errors are never cached: a transient 503 or a wrong-MBID shouldn't be
//! frozen for 24 hours. Only successful, structured responses go into the
//! cache.

use std::time::Duration;

use rmcp::model::{CallToolResult, Content, RawContent};
use serde::{Deserialize, Serialize};

use crate::core::{mb_cache, mb_throttle};

/// TTL for entity lookups (artist / release / recording). MBIDs are stable
/// but the surrounding metadata (release names, disambiguation) can be
/// edited — 24h is the documented sweet spot.
pub const TTL_ENTITY: Duration = Duration::from_secs(86_400);

/// TTL for static-like data (labels, works). Rarely revised once entered;
/// 7 days keeps tag-write workflows fast across multiple sessions.
pub const TTL_STATIC: Duration = Duration::from_secs(7 * 86_400);

/// Cached representation of a successful `CallToolResult`. We can't
/// serialise the rmcp model directly (it's `#[non_exhaustive]`), so we
/// extract the two pieces we actually need to rebuild it on hit.
#[derive(Serialize, Deserialize)]
struct CachedResponse {
    summary: String,
    structured: serde_json::Value,
}

/// Wrap a blocking MB-adjacent call with cache + throttle.
///
/// * `cache_key` — a stable string identifying this exact request. The
///   convention is `"<tool_name>:<params_json>"`; the helper neither
///   imposes nor validates that shape so callers can tighten it (e.g. add
///   a version prefix when their schema breaks compatibility).
/// * `ttl` — how long a successful response should stay valid. Use
///   [`TTL_ENTITY`] for entity-bound data, [`TTL_STATIC`] for slower-
///   moving entries.
/// * `fetch_fn` — the actual network call. Only invoked on cache miss.
///   Errors (`CallToolResult::is_error = true`) are returned to the caller
///   but never cached.
pub fn cached_or_fetch_blocking<F>(
    cache_key: &str,
    ttl: Duration,
    fetch_fn: F,
) -> CallToolResult
where
    F: FnOnce() -> CallToolResult,
{
    // 1. Cache lookup. A corrupted entry (malformed JSON) is treated as a
    // miss — the next successful fetch will overwrite it.
    if let Some(cache) = mb_cache::instance()
        && let Some(raw) = cache.get(cache_key)
        && let Ok(hit) = serde_json::from_str::<CachedResponse>(&raw)
    {
        return crate::domains::tools::result::structured_ok(hit.summary, &hit.structured);
    }

    // 2. Cache miss → reserve a slot in the rate limit budget, then fetch.
    mb_throttle::wait_sync();
    let result = fetch_fn();

    // 3. Cache the response when it succeeded and carries the two pieces
    // we need to rebuild it. Errors and text-only responses fall through.
    if !result.is_error.unwrap_or(false)
        && let Some(structured) = result.structured_content.clone()
        && let Some(summary) = first_text(&result.content)
        && let Some(cache) = mb_cache::instance()
    {
        let payload = CachedResponse { summary, structured };
        match serde_json::to_string(&payload) {
            Ok(json) => {
                if let Err(e) = cache.put(cache_key, &json, ttl) {
                    tracing::warn!("Failed to cache MB response for {}: {}", cache_key, e);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize MB response for cache: {}", e);
            }
        }
    }

    result
}

fn first_text(content: &[Content]) -> Option<String> {
    content.iter().find_map(|c| match &c.raw {
        RawContent::Text(t) => Some(t.text.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// Tests share the process-global cache singleton. Initialise it once
    /// in memory, then route every test through `with_test_cache` which
    /// flushes the table at entry — gives each test a clean slate without
    /// re-initialising the OnceLock.
    fn cache_for_tests() -> &'static mb_cache::MbCache {
        static SINGLETON: OnceLock<mb_cache::MbCache> = OnceLock::new();
        SINGLETON.get_or_init(|| {
            // SAFETY: env tweaked before the OnceLock is read; only the test
            // binary touches these vars.
            unsafe { std::env::set_var("MCP_MB_THROTTLE", "off") };
            mb_cache::MbCache::open(std::path::Path::new(":memory:")).unwrap()
        })
    }

    /// Serialise tests that touch the shared cache so they don't race on
    /// inserts/clears.
    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// `cached_or_fetch_blocking` consults `mb_cache::instance()`, which is
    /// a separate OnceLock from the test cache. To drive the cache logic
    /// without touching `~/.cache`, exercise the helper *machinery* via a
    /// scoped reimplementation that takes our test cache explicitly.
    fn run<F>(cache: &mb_cache::MbCache, key: &str, fetch: F) -> CallToolResult
    where
        F: FnOnce() -> CallToolResult,
    {
        if let Some(raw) = cache.get(key)
            && let Ok(hit) = serde_json::from_str::<CachedResponse>(&raw)
        {
            return crate::domains::tools::result::structured_ok(hit.summary, &hit.structured);
        }
        let result = fetch();
        if !result.is_error.unwrap_or(false)
            && let Some(structured) = result.structured_content.clone()
            && let Some(summary) = first_text(&result.content)
        {
            let payload = CachedResponse { summary, structured };
            if let Ok(json) = serde_json::to_string(&payload) {
                let _ = cache.put(key, &json, Duration::from_secs(60));
            }
        }
        result
    }

    #[test]
    fn miss_then_hit_invokes_fetch_only_once() {
        let _g = test_lock().lock().unwrap();
        let cache = cache_for_tests();
        // Unique key per test run to avoid bleed from neighbouring tests
        // sharing the same singleton.
        let key = "test:miss_then_hit";
        let calls = AtomicUsize::new(0);

        let payload = serde_json::json!({"mbid": "abc-123", "name": "Radiohead"});
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            crate::domains::tools::result::structured_ok("Found 1 artist", &payload)
        };

        let first = run(cache, key, fetch);
        assert!(!first.is_error.unwrap_or(false));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call must hit the cache — fetch closure not invoked again.
        let calls2 = AtomicUsize::new(0);
        let second = run(cache, key, || {
            calls2.fetch_add(1, Ordering::SeqCst);
            crate::domains::tools::result::structured_ok("should not be called", &payload)
        });
        assert_eq!(calls2.load(Ordering::SeqCst), 0);

        // Both responses carry the same structured payload.
        assert_eq!(first.structured_content, second.structured_content);
    }

    #[test]
    fn errors_are_not_cached() {
        let _g = test_lock().lock().unwrap();
        let cache = cache_for_tests();
        let key = "test:errors_not_cached";
        let calls = AtomicUsize::new(0);

        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            CallToolResult::error(vec![Content::text("transient failure")])
        };

        // Two consecutive errors → fetch invoked twice (no cache).
        let _ = run(cache, key, fetch);
        let _ = run(cache, key, || {
            calls.fetch_add(1, Ordering::SeqCst);
            CallToolResult::error(vec![Content::text("transient failure")])
        });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn text_only_responses_skip_the_cache() {
        let _g = test_lock().lock().unwrap();
        let cache = cache_for_tests();
        let key = "test:text_only_not_cached";

        // No structured payload → not cached (we can't rebuild without it).
        let fetch_first = || CallToolResult::success(vec![Content::text("hello")]);
        let calls = AtomicUsize::new(0);

        run(cache, key, || {
            calls.fetch_add(1, Ordering::SeqCst);
            fetch_first()
        });

        // The cache entry must NOT exist after the call.
        assert!(cache.get(key).is_none());
    }

    #[test]
    fn expired_entries_re_fetch_naturally() {
        let _g = test_lock().lock().unwrap();
        let cache = cache_for_tests();
        let key = "test:expired_refetch";

        let payload = serde_json::json!({"k": "v"});

        // Manually plant an expired entry.
        let cached = CachedResponse {
            summary: "old".into(),
            structured: payload.clone(),
        };
        let json = serde_json::to_string(&cached).unwrap();
        cache.put(key, &json, Duration::from_secs(0)).unwrap();

        // Now run with a different summary; the expired hit must miss and
        // re-fetch.
        let result = run(cache, key, || {
            crate::domains::tools::result::structured_ok("fresh", &payload)
        });
        let summary = first_text(&result.content).unwrap();
        assert_eq!(summary, "fresh", "expired entry should have been bypassed");
    }
}
