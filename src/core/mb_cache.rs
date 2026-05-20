//! SQLite-backed response cache for MusicBrainz tools.
//!
//! MBIDs are stable forever — a `mb_release_search` query for "Back in
//! Black" by AC/DC returns the same release MBID today as five years from
//! now. Combined with MusicBrainz's 1-req-per-sec rate limit, this makes
//! response caching extremely high-leverage: every cache hit saves a 1+
//! second round-trip AND a network request that would have eaten the rate
//! limit budget for other tools.
//!
//! Cache layout: a single SQLite table keyed by an opaque cache key
//! (typically `"<tool_name>:<params_json>"`) with TTL stored as a UNIX
//! timestamp. Lookup is O(log N) via the primary key index.
//!
//! Disabled via `MCP_MB_CACHE=off` — useful for debugging "fresh" responses
//! or for tests that don't want a cached fixture.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

/// SQLite-backed key-value cache with per-entry TTL. Concurrent access is
/// serialised by an internal mutex — fine at MCP scale (one writer, occa-
/// sionally a second async read) and removes the need to plumb a thread-
/// safe connection pool for what is, in practice, a sparse store.
pub struct MbCache {
    conn: Mutex<Connection>,
}

impl MbCache {
    /// Open (or create) the cache at the given path. Parent directories are
    /// created on demand. `:memory:` is accepted for tests.
    pub fn open(path: &Path) -> Result<Self, String> {
        if path != Path::new(":memory:")
            && let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("Failed to create cache directory {:?}: {}", parent, e)
            })?;
        }

        let conn = if path == Path::new(":memory:") {
            Connection::open_in_memory()
        } else {
            Connection::open(path)
        }
        .map_err(|e| format!("Failed to open SQLite cache at {:?}: {}", path, e))?;

        // Reasonable defaults for a cache: WAL gives concurrent readers while
        // a writer is committing, and NORMAL synchronous trades a touch of
        // durability for considerable throughput — acceptable since we treat
        // the cache as expendable (`rm mb.sqlite` is documented).
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("Failed to set WAL mode: {}", e))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| format!("Failed to set synchronous mode: {}", e))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS mb_cache (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL,
                 expires_at INTEGER NOT NULL
             )",
            [],
        )
        .map_err(|e| format!("Failed to create mb_cache table: {}", e))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Fetch a non-expired entry. Expired rows are deleted lazily on read so
    /// the cache self-prunes without a background sweeper.
    pub fn get(&self, key: &str) -> Option<String> {
        let now = unix_now();
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("mb_cache mutex poisoned: {}", e);
                return None;
            }
        };

        match conn
            .query_row(
                "SELECT value, expires_at FROM mb_cache WHERE key = ?1",
                params![key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
        {
            Ok(Some((value, expires_at))) => {
                if expires_at > now {
                    debug!(key, "mb_cache hit");
                    Some(value)
                } else {
                    debug!(key, "mb_cache expired");
                    // Best-effort lazy purge. Failure is non-fatal.
                    let _ = conn.execute("DELETE FROM mb_cache WHERE key = ?1", params![key]);
                    None
                }
            }
            Ok(None) => {
                debug!(key, "mb_cache miss");
                None
            }
            Err(e) => {
                warn!("mb_cache read failed for {}: {}", key, e);
                None
            }
        }
    }

    /// Store `value` under `key` for the given TTL. Upserts on collision.
    pub fn put(&self, key: &str, value: &str, ttl: Duration) -> Result<(), String> {
        let expires_at = unix_now().saturating_add(ttl.as_secs() as i64);

        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("mb_cache mutex poisoned: {}", e))?;

        conn.execute(
            "INSERT INTO mb_cache (key, value, expires_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
            params![key, value, expires_at],
        )
        .map_err(|e| format!("mb_cache write failed: {}", e))?;

        Ok(())
    }

    /// Count current rows. Diagnostic only; expired rows are still counted
    /// here (they get lazily deleted on read).
    pub fn len(&self) -> usize {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        conn.query_row("SELECT COUNT(*) FROM mb_cache", [], |row| row.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// `true` when the cache table holds no rows. Convenience companion to
    /// [`Self::len`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the on-disk cache directory:
/// `${XDG_CACHE_HOME:-$HOME/.cache}/music-mcp/`. Returns `None` when neither
/// env var is set — caller falls back to in-memory mode in that case.
pub fn default_cache_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("music-mcp"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(PathBuf::from(home).join(".cache").join("music-mcp"));
    }
    None
}

/// Process-global cache singleton. `None` when `MCP_MB_CACHE=off` or when
/// the database failed to open (logged once).
static CACHE: OnceLock<Option<MbCache>> = OnceLock::new();

/// Returns the cache instance, or `None` when disabled. Initialises on
/// first call; subsequent calls are lock-free.
pub fn instance() -> Option<&'static MbCache> {
    CACHE
        .get_or_init(|| {
            if !is_enabled() {
                info!("mb_cache disabled via MCP_MB_CACHE=off");
                return None;
            }

            // Override path via env (handy for tests + ops who want to
            // colocate the cache with the library root).
            let path_override = std::env::var("MCP_MB_CACHE_PATH").ok();
            let path = if let Some(p) = path_override.as_ref() {
                PathBuf::from(p)
            } else {
                match default_cache_dir() {
                    Some(dir) => dir.join("mb.sqlite"),
                    None => {
                        warn!(
                            "Could not resolve cache dir (XDG_CACHE_HOME / HOME unset); \
                             mb_cache disabled. Set MCP_MB_CACHE_PATH explicitly to override."
                        );
                        return None;
                    }
                }
            };

            match MbCache::open(&path) {
                Ok(cache) => {
                    info!("mb_cache opened at {:?}", path);
                    Some(cache)
                }
                Err(e) => {
                    warn!("Failed to open mb_cache at {:?}: {}; running without cache", path, e);
                    None
                }
            }
        })
        .as_ref()
}

/// `true` unless `MCP_MB_CACHE` is set to `off` / `0` / `false` / `no`.
fn is_enabled() -> bool {
    match std::env::var("MCP_MB_CACHE") {
        Ok(v) => {
            let trimmed = v.trim().to_ascii_lowercase();
            !matches!(trimmed.as_str(), "off" | "0" | "false" | "no")
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_cache_roundtrip() {
        let cache = MbCache::open(Path::new(":memory:")).unwrap();
        assert!(cache.get("nope").is_none());

        cache
            .put("artist:radiohead", r#"{"mbid":"abc"}"#, Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            cache.get("artist:radiohead").as_deref(),
            Some(r#"{"mbid":"abc"}"#)
        );
    }

    #[test]
    fn expired_entries_lazily_purge() {
        let cache = MbCache::open(Path::new(":memory:")).unwrap();
        // TTL of 0 → expires immediately on the next call (now + 0 == now,
        // and the predicate is strict `>`).
        cache.put("k", "v", Duration::from_secs(0)).unwrap();
        assert!(cache.get("k").is_none(), "expired entry must not be returned");
        // After the read, the row is gone.
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn upsert_overwrites_existing_entry() {
        let cache = MbCache::open(Path::new(":memory:")).unwrap();
        cache.put("k", "first", Duration::from_secs(60)).unwrap();
        cache.put("k", "second", Duration::from_secs(60)).unwrap();
        assert_eq!(cache.get("k").as_deref(), Some("second"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn disabled_singleton_when_env_off() {
        // Direct unit test of the env predicate — we can't easily test the
        // OnceLock without subprocess isolation.
        // SAFETY: tests are single-threaded by default in the harness; we
        // restore the original env at the end.
        let saved = std::env::var("MCP_MB_CACHE").ok();
        // SAFETY: serial-isolated by `unsafe { ... }`. See test note above.
        unsafe { std::env::set_var("MCP_MB_CACHE", "off") };
        assert!(!is_enabled());
        unsafe { std::env::set_var("MCP_MB_CACHE", "true") };
        assert!(is_enabled());
        match saved {
            Some(v) => unsafe { std::env::set_var("MCP_MB_CACHE", v) },
            None => unsafe { std::env::remove_var("MCP_MB_CACHE") },
        }
    }

    #[test]
    fn cache_persists_across_reopens() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        // Drop the empty placeholder file so MbCache::open creates the schema.
        drop(tmp);

        let cache = MbCache::open(&path).unwrap();
        cache
            .put("durable", "yes", Duration::from_secs(3600))
            .unwrap();
        drop(cache);

        let reopened = MbCache::open(&path).unwrap();
        assert_eq!(reopened.get("durable").as_deref(), Some("yes"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
}
