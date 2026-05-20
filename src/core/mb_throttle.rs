//! Token-bucket throttle for MusicBrainz requests.
//!
//! MusicBrainz publishes a "1 request per second" rate limit; abusing it
//! results in 503 responses and eventually a tarpit. Every call that hits
//! MB-adjacent endpoints (the search APIs via `musicbrainz_rs`, the Cover
//! Art Archive JSON metadata, AcoustID lookups) shares one throttle so
//! concurrent tools can't accidentally double-up.
//!
//! Implementation: a `Mutex<Instant>` that reserves the next slot lock-
//! and-release-fast. Each acquire computes its target slot (`last_slot +
//! interval`) inside the critical section, updates the slot stamp, then
//! sleeps (sync or async) until the slot starts. The lock therefore never
//! covers the sleep, so concurrent acquires queue up without serialising
//! their wall-clock cost.
//!
//! Disabled via `MCP_MB_THROTTLE=off` for tests that don't talk to the
//! real network and shouldn't pay 1.1s/op for the privilege.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Gap between successive MB requests. 1100ms = 1s rate limit + 100ms
/// safety margin to absorb clock skew / queue jitter.
pub const THROTTLE_INTERVAL: Duration = Duration::from_millis(1100);

/// Process-global "earliest time the next request may start". Initialised
/// to "now" so the first acquire incurs zero delay.
static NEXT_SLOT: OnceLock<Mutex<Instant>> = OnceLock::new();

fn slot_mutex() -> &'static Mutex<Instant> {
    NEXT_SLOT.get_or_init(|| Mutex::new(Instant::now()))
}

/// Reserve the next available slot and return when it should start.
/// Internal helper shared by sync/async variants.
fn reserve_slot() -> Instant {
    let mutex = slot_mutex();
    // Brief critical section: read `next`, schedule `next + interval` (or
    // `now` if we've drifted past), update, return. Sleeping happens
    // *outside* the lock so concurrent reservations don't serialise.
    let mut next = match mutex.lock() {
        Ok(g) => g,
        // A poisoned mutex shouldn't be fatal — the throttle is best-effort,
        // and a panicked previous holder doesn't invalidate our slot logic.
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    let target = if *next > now { *next } else { now };
    *next = target + THROTTLE_INTERVAL;
    target
}

/// Sync acquire. Blocks the current thread until the reserved slot starts.
/// Used by the HTTP-dispatch path and any sync test helpers.
pub fn wait_sync() {
    if !is_enabled() {
        return;
    }
    let target = reserve_slot();
    let now = Instant::now();
    if target > now {
        std::thread::sleep(target - now);
    }
}

/// Async acquire. Like [`wait_sync`] but yields to the runtime instead of
/// parking the OS thread. Used by the STDIO/TCP async route handlers.
pub async fn wait_async() {
    if !is_enabled() {
        return;
    }
    let target = reserve_slot();
    let now = Instant::now();
    if target > now {
        tokio::time::sleep(target - now).await;
    }
}

/// `true` unless `MCP_MB_THROTTLE` is set to `off` / `0` / `false` / `no`.
fn is_enabled() -> bool {
    match std::env::var("MCP_MB_THROTTLE") {
        Ok(v) => {
            let trimmed = v.trim().to_ascii_lowercase();
            !matches!(trimmed.as_str(), "off" | "0" | "false" | "no")
        }
        Err(_) => true,
    }
}

/// Reset the throttle to "now". Tests only — production code never resets
/// because the slot pointer is a process-global rate-limit budget.
#[cfg(test)]
pub fn reset_for_tests() {
    let mutex = slot_mutex();
    if let Ok(mut next) = mutex.lock() {
        *next = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three sync acquires back-to-back must elapse at least 2 × interval —
    /// the first runs immediately, the second waits one interval, the third
    /// waits another. (We can't be stricter than the lower bound because the
    /// process-global slot might already have been advanced by a sibling
    /// test running in parallel — that only ever *increases* elapsed time.)
    #[test]
    fn three_sync_acquires_take_at_least_two_intervals() {
        // Disable to isolate from other tests, then re-enable just for this
        // test. We can't share global state with neighbours that may also
        // exercise the throttle, so we rely on the time-between-acquires
        // being independent of the absolute slot pointer.
        //
        // SAFETY: tests are single-threaded by default in cargo's harness
        // unless `--test-threads` is passed. We restore the value.
        let saved = std::env::var("MCP_MB_THROTTLE").ok();
        unsafe { std::env::remove_var("MCP_MB_THROTTLE") };
        reset_for_tests();

        let start = Instant::now();
        wait_sync();
        wait_sync();
        wait_sync();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= 2 * THROTTLE_INTERVAL,
            "expected >= {:?}, got {:?}",
            2 * THROTTLE_INTERVAL,
            elapsed
        );

        match saved {
            Some(v) => unsafe { std::env::set_var("MCP_MB_THROTTLE", v) },
            None => unsafe { std::env::remove_var("MCP_MB_THROTTLE") },
        }
    }

    /// With `MCP_MB_THROTTLE=off`, acquires return instantly so noisy tests
    /// don't pay 1.1s/op.
    #[test]
    fn disabled_acquires_return_immediately() {
        let saved = std::env::var("MCP_MB_THROTTLE").ok();
        unsafe { std::env::set_var("MCP_MB_THROTTLE", "off") };

        let start = Instant::now();
        for _ in 0..10 {
            wait_sync();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "10 disabled acquires must be ~free, got {:?}",
            elapsed
        );

        match saved {
            Some(v) => unsafe { std::env::set_var("MCP_MB_THROTTLE", v) },
            None => unsafe { std::env::remove_var("MCP_MB_THROTTLE") },
        }
    }
}
