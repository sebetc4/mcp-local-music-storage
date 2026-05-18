# Code Quality & Architecture Cleanup Roadmap

This roadmap addresses all findings from the full project review (architecture, security, code quality, tests). Tasks are ordered by impact and dependency: do Phase 0 first (broken state), then 1 (security), then incrementally tackle the rest.

> **Companion document**: see [musicbrainz-enhancements.md](musicbrainz-enhancements.md) for new-feature roadmap. The two are independent and can progress in parallel.

---

## Progress

| Phase | Status | Date | Notes |
|---|---|---|---|
| **0 — Stop the Bleeding** | ✅ Done | 2026-05-09 | All 12 broken tests fixed (now 80 passing / 0 failing); cover_download path-traversal hole closed. Phase 4.1 (Option A — remove resources/prompts) and Phase 1.4 (Option A — strict symlinks) executed as prerequisites. Milestone **M1** reached. |
| **1 — Security Hardening** | ✅ Done | 2026-05-15 | 1.1 ✅ (50 MB cover download cap), 1.2 ✅ (atomic writes via `core::fs_atomic`), 1.3 ✅ (no embedded AcoustID key — `MissingApiKey` short-circuits before I/O), 1.4 ✅ (done as Phase 0 prerequisite), 1.5 ✅ (CORS allow-list refuses startup on non-loopback bind without explicit origins). Milestone **M2** reached. |
| **2 — Code Quality Cleanup** | ✅ Done | 2026-05-17 | 2.1 ✅, 2.2 ✅, 2.3 ✅, 2.4 ✅, 2.5 ✅ (`parse_bool_env` helper accepts `true/false/1/0/yes/no`, warns + falls back on anything else — `MCP_ALLOW_SYMLINKS=flase` no longer silently keeps the default). Milestone **M3** reached. |
| **3 — MusicBrainz Tools Refactor** | ✅ Done | 2026-05-17 | 3.1 ✅ (new `MbBlockingTool` trait owns all transport scaffolding for the 5 search tools — net -361 LOC), 3.2 ✅, 3.3 ✅ (`#[instrument]` on every MB `execute` via the migration). Milestone **M4** reached. |
| **4 — Architecture & Coherence** | ✅ Done | 2026-05-17 | 4.1 ✅ (Phase 0), 4.2 ✅ (`foreach_tool!` X-macro is the single source of truth — registry/router derive from it), 4.3 ✅ (dropped unused `McpServer::config()` getter; `PromptsConfig` already gone), 4.4 ✅ (CLAUDE.md & lib.rs docs updated), 4.5 ✅ (`Error::Internal(#[from] anyhow::Error)` preserves cause chains). Milestone **M5** reached. |
| **5 — Tests & Observability** | ✅ Done | 2026-05-17 | 5.1 ✅ (`tests/metadata_roundtrip.rs` — WAV round trip, clear+partial, atomic-write preservation), 5.2 ✅ (`tests/mcp_e2e.rs` — initialize / tools/list / tools/call via `ServiceExt::oneshot`), 5.3 ✅ (Phase 0), 5.4 ✅ (`[lints.clippy]` in Cargo.toml + cfg_attr exemption for tests), 5.5 ✅ (`.github/workflows/ci.yml` runs build + test + clippy `-D warnings` + fmt `--check`). Milestone **M6** reached — **roadmap complete**. |

### Decisions taken

- **4.1 Resources/prompts capabilities**: Option A — removed entirely. The `domains/resources/` and `domains/prompts/` modules, their HTTP handlers, and the `enable_resources()`/`enable_prompts()` capability declarations were all dropped. Re-introducing them requires an explicit decision.
- **1.4 Symlink policy**: Option A — strict. `allow_symlinks = false` rejects any symlink encountered as the input path, regardless of where it points. New error variant `PathSecurityError::SymlinkNotAllowed` distinguishes it from `SymlinkOutsideRoot` (target escape with symlinks allowed) and `OutsideRootDirectory` (plain `..` traversal).

---

## Table of Contents

1. [Phase 0 — Stop the Bleeding](#phase-0--stop-the-bleeding) (urgent, ~1 day)
2. [Phase 1 — Security Hardening](#phase-1--security-hardening) (~2 days)
3. [Phase 2 — Code Quality Cleanup](#phase-2--code-quality-cleanup) (~3 days)
4. [Phase 3 — MusicBrainz Tools Refactor](#phase-3--musicbrainz-tools-refactor) (~3-4 days)
5. [Phase 4 — Architecture & Coherence](#phase-4--architecture--coherence) (~2 days)
6. [Phase 5 — Tests & Observability](#phase-5--tests--observability) (~2-3 days)
7. [Effort summary & milestones](#effort-summary--milestones)

---

## Phase 0 — Stop the Bleeding

✅ **Completed 2026-05-09** — `cargo test --features all --lib`: 80 passing / 0 failing / 17 ignored. Milestone **M1** reached.

**Goal**: get `cargo test --features all --lib` green and close the one real security hole. Nothing else should ship until this is done.

### 0.1 Fix the 12 broken tests ✅

`cargo test --features all --lib` previously failed with **12 failures / 79 passing**. The failures were leftover after `acc7f9a refacto: remove unused ressouces and prompts` (registries emptied, tests not updated) and one `.env`-pollution issue.

**Tasks**:
- [x] Decide whether the resources/prompts capabilities should be removed entirely or repopulated. See [4.1](#41-resourcesprompts-capabilities) — both options need to be aligned with the same answer. → **Option A (remove)** chosen.
- [x] If removed: drop the orphan tests + the modules themselves. Done: `domains/resources/` and `domains/prompts/` deleted, HTTP handlers removed, `enable_resources()`/`enable_prompts()` dropped, `ResourcesConfig`/`PromptsConfig` removed, `MCP_RESOURCES_BASE_PATH` no longer read.
- [ ] ~~If repopulated: re-add at least one resource and one prompt definition, register them, and let the tests pass naturally.~~ N/A
- [x] Fix `test_credentials_*` env contamination — refactored `Config::from_env` to delegate to a new `Config::from_env_with(impl Fn(&str) -> Option<String>)`. Tests inject a controlled closure and no longer touch `std::env`. `TransportConfig::from_env` got the same treatment.
- [x] Fix the two failing symlink tests — addressed by [1.4](#14-clarify-symlink-policy).

**Acceptance**: `cargo test --features all --lib` returns 0 failures (ignored network tests excluded). ✅ 80 / 0 / 17.

**Effort**: 0.5 day.

---

### 0.2 Plug the cover_download path-traversal hole ✅

🔴 **Concrete vulnerability**: in [cover_download.rs](../../src/domains/tools/definitions/mb/cover_download.rs), `params.directory` was validated through `validate_path` (line ~200), but `params.filename` was concatenated via `dir_path.join(&params.filename)` and **never validated**. A caller could pass `filename = "../../../etc/exploit.jpg"` to escape the configured root.

**Tasks**:
- [x] Reject unsafe filenames upfront via the new `core::security::is_safe_filename` helper (rejects empty, leading `.`, `/`, `\`, NUL bytes — `..` is rejected via the leading-dot rule).
- [x] Defensive re-check after the join: `file_path.starts_with(&dir_path)` before any I/O, with an error if it somehow escaped.
- [x] Regression test `cover_download_filename_traversal` exercises `filename = "../escape"` with a configured `root_path`: it asserts the call returns an error, the validated root stays empty, and no `escape.jpg` lands in the parent directory.

**Acceptance**: ✅ `cover_download::tests::cover_download_filename_traversal` passes; the validator rejects the malicious filename before the HTTP fetch fires.

**Effort**: 0.5 day.

---

## Phase 1 — Security Hardening

### 1.1 Bound cover-art download size ✅ (done 2026-05-09)

In `cover_download.rs`, `response.bytes()` previously read the entire body into memory with no cap. A misbehaving or malicious server could have exhausted memory.

**Tasks**:
- [x] Added `const MAX_COVER_BYTES: u64 = 50 * 1024 * 1024;` at module scope.
- [x] Reject upfront when `response.content_length()` declares a body larger than the cap, before any read.
- [x] Stream via `Read::take(cap + 1)` and abort if more than `cap` bytes arrive (the `+1` makes the overflow byte detectable). Wrapped in a reusable `read_with_cap` helper.
- [x] Clear error message: `"cover too large: {n} bytes, max {cap}"` (Content-Length path) and `"cover too large: > {cap} bytes, max {cap}"` (streaming path).

**Acceptance**: ✅ four unit tests on `read_with_cap` (under, at, and over the limit, plus a constant pinning test) — no HTTP mocking needed since the helper is decoupled from `reqwest`.

**Effort**: 0.5 day.

---

### 1.2 Atomic writes for cover_download and write_metadata ✅ (done 2026-05-09)

Both tools previously wrote directly to the destination. Failure mid-write left a corrupt file. CLAUDE.md §2.2 ("reversible when possible") was not honored.

**Tasks**:
- [x] Introduced [`core::fs_atomic`](../../src/core/fs_atomic.rs) with `write_atomic(path, contents) -> io::Result<()>` and `temp_sibling(path) -> io::Result<PathBuf>`. Temp lives next to the target as `<file>.tmp.<pid>.<nanos>`, so `rename(2)` stays on a single filesystem and is atomic on Unix. Doc-comments call out the same-filesystem caveat and the lack of `fsync` (we guard against partial writes, not power loss).
- [x] `write_metadata` now copies the source to a sibling temp, runs `tagged_file.save_to_path(&tmp, ...)`, then `rename(tmp, original)`. On any failure step the temp is cleaned up and the original is left untouched.
- [x] `cover_download` final write goes through `write_atomic`.

**Acceptance**: ✅ unit test `write_atomic_no_partial_write_on_failure` proves a failed rename leaves the original target intact and removes the temp; `write_atomic_creates_new_file` and `write_atomic_replaces_existing_file` cover the happy paths; `temp_sibling_lives_in_same_dir` and `temp_sibling_unique_per_call` lock down the path-naming contract.

**Effort**: 1 day.

---

### 1.3 Drop the embedded default AcoustID key ✅ (done 2026-05-09)

`config.rs` previously shipped a hard-coded fallback key `Kok2GHQlrAg`. Public repo + embedded credential = bad practice even if the key is throwaway.

**Tasks**:
- [x] `CredentialsConfig::default().acoustid_api_key` is now `None`.
- [x] In `mb_identify_record`, a new `IdentificationError::MissingApiKey` variant is returned at the top of `execute()` — before any path validation, fpcalc invocation, or HTTP call. An empty-string key is treated like `None`. Message points to https://acoustid.org/api-key.
- [x] Startup warning in `Config::from_env_with` updated: instead of "using default key", it now says the tool will refuse to run.
- [x] `.env.example` rewritten to mark the key as REQUIRED for `mb_identify_record`; `documentation/architecture/config-workflow.md` example no longer suggests embedding a default.

**Acceptance**: ✅ `test_credentials_default_is_none` and `test_config_default_has_no_acoustid_key` pin the default to `None`. `test_mb_identify_missing_api_key_short_circuits` asserts the error message contains both `MCP_ACOUSTID_API_KEY` and the signup URL, with a bogus file path that proves no I/O ran. `test_mb_identify_empty_api_key_short_circuits` covers the empty-string case.

**Effort**: 0.5 day.

---

### 1.4 Clarify symlink policy ✅ (done as Phase 0 prerequisite, 2026-05-09)

**Decision**: Option A — strict. `allow_symlinks=false` rejects any symlink encountered as the input path, regardless of where it points.

**Tasks**:
- [x] Implement chosen semantics, update doc-comments at the top of `path_validator.rs`.
- [x] Rewrite the two failing tests to match (`test_symlink_disallowed_by_config` now expects `SymlinkNotAllowed`; `test_symlink_outside_root_blocked` already worked once symlinks were detected explicitly).
- [ ] ~~Add a doc page [reference/path-security.md](../reference/path-security.md) update describing the new policy.~~ Doc page does not yet exist; deferred to a docs-only task.

**Implementation notes**: new error variant `PathSecurityError::SymlinkNotAllowed` distinguishes the strict-policy rejection from `SymlinkOutsideRoot` (target escape with symlinks allowed) and `OutsideRootDirectory` (plain `..` traversal). Caveat: `Path::is_symlink()` only inspects the leaf, so symlinks in intermediate path components are not detected at validation time — track as a follow-up if full-path detection is needed.

**Acceptance**: 8/8 path_validator tests pass. ✅

---

### 1.5 Tighten HTTP transport CORS in production mode ✅ (done 2026-05-15)

[transport/http.rs](../../src/core/transport/http.rs) previously set `Any/Any/Any` unconditionally. Fine for local dev, dangerous as soon as the binary was exposed.

**Tasks**:
- [x] Added `cors_allow_origins: Vec<String>` to `HttpConfig`, parsed from `MCP_HTTP_CORS_ORIGINS` (comma-separated).
- [x] Extracted a pure `decide_cors_policy(&HttpConfig) -> CorsDecision` helper. Empty allow-list ⇒ `Any` only when `is_loopback_host(host)` is true (any 127/8 IP, `::1`, or the literal `localhost`); on a non-loopback bind the policy becomes `Reject` and `HttpTransport::run` returns `TransportError::init` instead of binding.
- [x] Loud `warn!` line at startup when `Any` is in effect on loopback; `info!` listing the explicit origins when an allow-list is in use.
- [x] `.env.example` documents `MCP_HTTP_CORS_ORIGINS` and explicitly states it's required on non-loopback binds.

**Acceptance**: ✅ 7 unit tests in `core::transport::http::tests` lock down the loopback recognition and every branch of the decision helper (Disabled / Allowlist / AllowAnyLoopback / Reject, plus allowlist-wins-over-loopback). Reject message contains both the offending host and the `MCP_HTTP_CORS_ORIGINS` hint.

**Effort**: 0.5 day.

---

## Phase 2 — Code Quality Cleanup

### 2.1 Eliminate `unwrap()` / `expect()` from production paths ✅ (done 2026-05-15)

CLAUDE.md §2.1 forbids them. Cleared all 14 production hits across three patterns:

| Pattern | Location | Fix |
|---|---|---|
| `.expect("Just inserted tag")` ×2 | [metadata/write.rs](../../src/domains/tools/definitions/metadata/write.rs) | Linearised the `if/match` into three statements: optional `clear`, conditional `insert_tag`, then `match primary_tag_mut` with a graceful `CallToolResult::error` fallback for the (defensive) None case. |
| `response.as_object_mut().unwrap()` ×9 (all MB tools, both metadata tools) | http handlers | Introduced [`domains::tools::http_response::tool_result_to_json`](../../src/domains/tools/http_response.rs) which builds the envelope as a `serde_json::Map` from the start. Every `http_handler` is now a one-liner that delegates to it. |
| `serde_json::to_value(&result).unwrap()` ×3 | [fs/delete.rs](../../src/domains/tools/definitions/fs/delete.rs), [fs/list_dir.rs](../../src/domains/tools/definitions/fs/list_dir.rs), [fs/rename.rs](../../src/domains/tools/definitions/fs/rename.rs) | Inline `match`: on `Err`, log via `warn!` and degrade to text-only success. |

**Acceptance**: ✅ `cargo clippy --features all --lib --no-deps -- -W clippy::unwrap_used -W clippy::expect_used` reports 0 hits for either lint. Three new tests in `http_response::tests` lock down the envelope shape (error, success+structured, missing-is_error).

**Effort**: 1 day. (Adding the lints to `Cargo.toml` permanently is deferred to [5.4](#54-add-clippypedantic-opt-in-lints) so the rule lives next to the others.)

---

### 2.2 Remove dead code ✅ (done 2026-05-15)

CLAUDE.md §6.7 forbids speculative code. Cleared the following dead items:

| Path | What was removed | Effect |
|---|---|---|
| `src/domains/tools/handlers.rs` (whole file) | `ToolInput`, `ToolOutput`, `ToolHandler` trait, `FileOperationsHandler` | Template scaffolding with no wired callers; also dropped the `pub use handlers::*;` re-export in `tools/mod.rs`. |
| [transport/service.rs](../../src/core/transport/service.rs) | `TransportServiceBuilder` + its `impl Default` (previously `#[allow(dead_code)]`) | `TransportService::new` / `from_env` are the only constructors anything actually uses. |
| [mb/common.rs](../../src/domains/tools/definitions/mb/common.rs) | `format_date` (identity pass-through) | No live callers. |
| 5× MB tools (`artist`, `label`, `recording`, `release`, `work`) | 10× `handle_http()` + `handle_stdio()` legacy methods marked `#[deprecated]`; companion `use futures::future::BoxFuture;` imports | All MB tools now route exclusively through `create_route` / `http_handler` introduced in Phase 2.1. |
| [mb/identify_record.rs](../../src/domains/tools/definitions/mb/identify_record.rs) | `AcoustIDDate` struct entirely, plus dead fields from `AcoustIDRecording` (`releases`), `AcoustIDArtist` (`id`), `AcoustIDReleaseGroup` (none — `id` is read), `AcoustIDRelease` (`id`, `title`, `country`, `date`, `track_count`, `medium_count`), `AcoustIDMedium` (`position`, `format`, `track_count`), `AcoustIDTrack` (`id`, `position`) | Only the fields actually consumed at call sites stay typed; serde silently drops the rest. All the `#[allow(dead_code)]` markers in this region disappear. |
| `src/domains/resources/definitions/refactor/` | (already gone in Phase 0) | — |

**Acceptance**: ✅ `cargo build --features all` clean. `cargo clippy --features all --lib --no-deps` warnings dropped from 17 → 6 with **zero** `dead_code` / `never_used` / `unused` hits. Cumulative diff since start of cleanup work: 39 files, +661/-2335 lines.

**Effort**: 0.5 day.

---

### 2.3 Replace `format!("{:?}", enum)` with stable Display ✅ (done 2026-05-17)

MusicBrainz enum variants were rendered via `Debug` in 4 call sites (`label.rs`, `release.rs` ×2, `work.rs`) plus an in-house `MetadataLevel` in `identify_record.rs`. A library-side rename would silently change the wire format clients see.

**Tasks**:
- [x] Added centralized stable mappings in [`mb/common.rs`](../../src/domains/tools/definitions/mb/common.rs): `release_group_primary_type_str`, `label_type_str`, `work_type_str`. Each mirrors the upstream `#[serde(rename = "…")]` or `From<String>` form, with a `_ => "Unknown"` arm for `#[non_exhaustive]` enums. `WorkType::UnrecognizedWorkType(raw)` surfaces the raw type name.
- [x] Added `MetadataLevel::as_str()` returning `&'static str` ("minimal"/"basic"/"full") and replaced `format!("{:?}", metadata_level).to_lowercase()` with `metadata_level.as_str().to_string()`.
- [x] Migrated every call site to the new helpers.

**Acceptance**: ✅ `git grep '"{:?}"' src/domains/tools/definitions/mb/` returns only the explanatory comment in `common.rs`; zero remaining call sites. Three new tests (`release_group_primary_type_str_mapping`, `label_type_str_mapping`, `work_type_str_mapping`) pin every variant — they will fail loudly if upstream renames or removes one.

**Effort**: 0.5 day.

---

### 2.4 Fix concrete logic bugs ✅ (done 2026-05-17)

| Bug | Location | Fix applied |
|---|---|---|
| `search_release_recordings` applied `limit` per disc and `total_tracks` only counted tracks with a `recording`, so subsequent `position`s drifted | [release.rs](../../src/domains/tools/definitions/mb/release.rs) | Single `remaining` budget shared across discs; uses MusicBrainz's `track.position` directly (no synthetic counter); empty discs are not emitted. |
| `search_releases_by_artist` always did a second `Artist::fetch` to retrieve the display name even when the search step already returned it | [artist.rs](../../src/domains/tools/definitions/mb/artist.rs) | Resolved `(id, name)` in a single round-trip; the `fetch` only runs when the user supplied an MBID. |
| `channel_description` hardcoded a "Multi-channel" string for >2 channels, losing the count | [metadata/read.rs](../../src/domains/tools/definitions/metadata/read.rs) | `n => format!("{}-channel", n)` for any count above 2. |
| `extract_year` accepted any 4-byte prefix, including `"unknown"` or `"XXXX-01"` | [mb/common.rs](../../src/domains/tools/definitions/mb/common.rs) | Validates ASCII-digit prefix via byte iteration (safe across UTF-8 boundaries). |

**Acceptance**: ✅ `cargo test --features all --lib` clean; new `extract_year_rejects_non_digit_prefix` regression test covers junk prefixes (`"unknown"`, `"XXXX-01-01"`, mixed `"19-7-06"`, multi-byte `"é1997"`). The two MB search bugs are exercised by the existing `#[ignore]` network tests when run with `--ignored --test-threads=1`.

**Effort**: 1 day.

---

### 2.5 Strict env-bool parsing ✅ (done 2026-05-17)

`MCP_ALLOW_SYMLINKS` previously used `raw.parse().unwrap_or(true)` and `MCP_HTTP_CORS` used a `to_lowercase() != "false"` truthy check. In both cases a typo (`flase`, `noo`) silently became "true".

**Tasks**:
- [x] Added [`pub fn parse_bool_env(name, raw, default) -> bool`](../../src/core/config.rs) at module scope. Accepts `true`/`false`/`1`/`0`/`yes`/`no` case-insensitively with trim; anything else emits `warn!("Invalid boolean value … for …; using default …")` and returns `default`.
- [x] Migrated both call sites (`MCP_ALLOW_SYMLINKS` in `core::config::from_env_with`, `MCP_HTTP_CORS` in `transport::config::from_env_with`).

**Acceptance**: ✅ Three new tests — `parse_bool_env_accepts_canonical_values`, `parse_bool_env_falls_back_on_typo`, `allow_symlinks_typo_uses_default` (E2E regression for `MCP_ALLOW_SYMLINKS=flase` → still `true`, then `=false` → `false`).

**Effort**: 0.25 day.

---

## Phase 3 — MusicBrainz Tools Refactor

### 3.1 Factor out the per-tool boilerplate ✅ (done 2026-05-17)

The 5 search tools (`artist`, `release`, `recording`, `work`, `label`) each repeated ~150 lines of identical `to_tool` / `create_route` / `http_handler` / thread-spawn shape.

**Final design** ([`mb/blocking_tool.rs`](../../src/domains/tools/definitions/mb/blocking_tool.rs)):

```rust
pub trait MbBlockingTool: Send + Sync + 'static {
    type Params: DeserializeOwned + JsonSchema + Send + 'static;

    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    fn execute(params: &Self::Params) -> CallToolResult;

    // Default impls provided:
    fn to_tool() -> Tool { ... }
    fn create_route<S: Send + Sync + 'static>() -> ToolRoute<S> { ... }
    #[cfg(feature = "http")] fn http_handler(args: serde_json::Value) -> Result<serde_json::Value, String> { ... }
}
```

The simpler `CallToolResult` return type (matching the existing flow) avoids needing an `Output` associated type and a macro — every tool just provides three lines (`type Params`, two const strings, one `fn execute`). Tools needing extra runtime context (`cover_download`, `identify_record` both take `Arc<Config>`) intentionally stay outside the trait so the abstraction stays clean.

**Tasks**:
- [x] Added the trait with full default impls — no macro needed.
- [x] Migrated `work`, `label`, `artist`, `recording`, `release` in that order, with `cargo build` + `cargo test --features all --lib` between each.
- [x] `registry.rs` and `router.rs` now `use crate::domains::tools::definitions::mb::MbBlockingTool;` so the existing `MbWorkTool::create_route()` / `MbWorkTool::NAME` call sites resolve through the trait — no dispatch changes needed.
- [x] Dropped 5× dead `pub fn new()`, 5× dead `impl Default for ...`, and the now-unused `Debug, Clone` derives that were never instantiated.

**Acceptance**: ✅ 108/0 tests after every migration step. LOC deltas (production, non-blank): `work.rs` 226→139, `label.rs` 228→141, `artist.rs` 447→353, `recording.rs` 541→447, `release.rs` 685→591. Sum: **-456 LOC at call sites**, **+95 LOC trait** = **-361 LOC net**.

**Effort**: 2 days.

---

### 3.2 Unify on `tokio::spawn_blocking` ✅ (done 2026-05-17)

The MB-search tools previously spawned raw OS threads via `std::thread::spawn(...).join()` "to avoid a nested runtime panic". The hypothesis was wrong: `identify_record::create_route` was already using `tokio::task::spawn_blocking` with the same blocking `reqwest::Client` and worked fine.

**Tasks**:
- [x] Confirmed empirically — `identify_record` has been shipping the spawn_blocking pattern. Both `reqwest::blocking` and `musicbrainz_rs::blocking` create their own internal mini-runtime per call; `spawn_blocking` simply moves the call onto tokio's dedicated blocking pool, which is *not* "inside" the async runtime — no nesting occurs.
- [x] Migrated all 6 MB `create_route` paths (`work`, `artist`, `label`, `recording`, `release`, `cover_download`) from `std::thread::spawn(...).join()` to `tokio::task::spawn_blocking(...).await`. The misleading "nested runtime" comment is replaced with a one-line explanation of the actual model.
- [x] HTTP-handler unification deferred to [3.1](#31-factor-out-the-per-tool-boilerplate) — the trait is the right place to standardize both transports together.

**Acceptance**: ✅ `cargo build --features all` clean; `cargo test --features all --lib` → 108 / 0; `main.rs` already uses default `#[tokio::main]` (multi-thread flavor), so the blocking thread pool is available.

**Effort**: 0.5 day.

---

### 3.3 Add `#[instrument]` to all MB tools ✅ (done 2026-05-17)

Done as a side effect of [3.1](#31-factor-out-the-per-tool-boilerplate): each of the 5 trait impls carries `#[instrument(skip_all, fields(query = %params.query, limit = params.limit, ...))]` on `execute`. (Used `%` over `?` for cleaner string output.) Multi-action tools (`artist`, `recording`, `release`) also record `search_type` as a span field for filterability.

Not baked into the trait default impl because Rust traits can't combine `#[instrument]` with `Self::Params`-typed field expressions in a useful way — each tool's params have different relevant fields. The per-impl annotation is one line and gains nothing from indirection.

**Effort**: 0.25 day (folded into 3.1).

---

## Phase 4 — Architecture & Coherence

### 4.1 Resources/prompts capabilities ✅ (done as Phase 0 prerequisite, 2026-05-09)

**Decision**: Option A — Remove. No concrete use-case for resources/prompts in the music-library automation domain.

**Tasks**:
- [x] Drop `enable_resources()` and `enable_prompts()` from `McpServer::get_info()`.
- [x] Remove `ResourceService` and `PromptService` initialization (and the fields) from `McpServer`.
- [x] Delete the `domains/resources/` and `domains/prompts/` modules entirely.
- [x] Clean up HTTP transport handlers for resources/prompts (`resources/list`, `resources/templates/list`, `resources/read`, `prompts/list`, `prompts/get`).
- [x] Drop `ResourcesConfig` and `PromptsConfig` and the `MCP_RESOURCES_BASE_PATH` env var.
- [x] Drop `Error::Resource` / `Error::Prompt` variants.

Re-introduction now requires an explicit decision; do not silently bring the modules back.

---

### 4.2 Single source of truth for the tool list ✅ (done 2026-05-17)

The four lists (`tool_names`, `get_all_tools`, `call_tool` dispatch, `build_tool_router`) all duplicated the same enumeration.

**Solution** — X-macro `foreach_tool!` exported from [`definitions/mod.rs`](../../src/domains/tools/definitions/mod.rs):

```rust
#[macro_export]
macro_rules! foreach_tool {
    ($visit:ident) => {
        $visit!($crate::…::FsDeleteTool, with_config);
        // … 11 more entries …
        $visit!($crate::…::MbWorkTool, no_config);
    };
}
```

Each consumer defines a local `$visit!` callback (one per shape — name push, Tool push, HTTP dispatch arm, route addition) and invokes `crate::foreach_tool!(visitor)`. The `with_config` / `no_config` token differentiates tools that need `Arc<Config>` at construction.

**Tasks**:
- [x] Added the macro; both arms (with_config, no_config) covered by the visitor pattern.
- [x] Rewrote `registry.rs` (`tool_names`, `get_all_tools`, `call_tool`) and `router.rs` (`build_tool_router`) to derive from the macro.
- [x] Kept the `test_registry_matches_router` consistency check as a belt-and-suspenders net for any future divergence (e.g. someone adds a tool by bypassing the macro).

**Acceptance**: ✅ Adding a new tool now requires:
1. Define the tool in `domains/tools/definitions/.../my_tool.rs`.
2. Re-export it from the parent `mod.rs`.
3. Add **one line** to `foreach_tool!`.

No edits to `registry.rs` or `router.rs` needed. Both transports stay in sync automatically. 108/0 tests after migration.

**Effort**: 1 day.

---

### 4.3 Drop unused config fields ✅ (done 2026-05-17)

The original audit predated several intermediate cleanups:
- `PromptsConfig` was deleted entirely in Phase 0 with the rest of the prompts subsystem.
- `McpServer.config` is now genuinely used — by `name()`, `version()` (which read `self.config.server.*`), and the HTTP `call_tool()` (which clones `self.config` into a fresh `ToolRegistry`). The field stays.

**Tasks**:
- [x] Confirmed `McpServer.config` is read — keep it.
- [x] Removed the unused `McpServer::config()` getter (3 LOC of dead public surface — no callers in `src/` or `main.rs`).
- [x] `PromptsConfig` — N/A, already removed in Phase 0.

**Effort**: 0.25 day.

---

### 4.4 Update CLAUDE.md and project docs ✅ (done 2026-05-17)

CLAUDE.md §4 said "9 tools" and listed a fictional `mb_advanced_search`.

**Tasks**:
- [x] Rewrote [CLAUDE.md §4](../../CLAUDE.md) to list the actual **12 tools** (3 fs + 2 metadata + 7 mb), with `mb_cover_download` and `mb_work_search` / `mb_label_search` now properly listed and `mb_advanced_search` removed.
- [x] Rewrote CLAUDE.md §3 "Key Patterns": now mentions the `foreach_tool!` single source of truth, the unified `spawn_blocking` model, the `MbBlockingTool` trait, `is_safe_filename()` for filename hygiene, and `core::fs_atomic::write_atomic` for the atomic-write contract.
- [x] Rewrote CLAUDE.md §6 critical-rule #3 to say "Add ONE entry to `foreach_tool!`" instead of "Update both registry.rs and router.rs".
- [x] Cleaned up [lib.rs](../../src/lib.rs) crate-doc — dropped the bullets mentioning `resources` / `prompts` (gone in Phase 0), now correctly describes the `tools` domain with its 12 tools.
- [x] Cross-checked `documentation/README.md` — already correct.
- [x] No remaining `handle_http`/`handle_stdio` references in active architecture docs (only in this roadmap's historical notes).

**Effort**: 0.25 day.

---

### 4.5 Better internal error type ✅ (done 2026-05-17)

`Internal(String)` lost the original cause chain. Replaced with `Internal(#[from] anyhow::Error)` so:
- `?`-propagation from any `anyhow::Error`-producing context auto-wraps via `From`.
- The original source chain is preserved (`err.source()` walks it, `err.downcast_ref::<T>()` recovers typed sources).
- The free-form `Error::internal(msg)` constructor still works — it now wraps the message in `anyhow::anyhow!(...)`.

**Tasks**:
- [x] [error.rs](../../src/core/error.rs) variant updated to `Internal(#[from] anyhow::Error)` with doc-comment explaining the cause-chain preservation and the typed-source recovery pattern.
- [x] `Error::internal` updated to build via `anyhow::anyhow!`.

**Note**: no internal call sites construct `Error::Internal` today, so this is purely API-readiness for future callers — the variant is now useful rather than lossy when someone needs it.

**Effort**: 0.5 day.

---

## Phase 5 — Tests & Observability

### 5.1 Add round-trip tests for metadata tools ✅ (done 2026-05-17)

Added [`tests/metadata_roundtrip.rs`](../../tests/metadata_roundtrip.rs) with three integration tests:

- `wav_roundtrip_all_fields` — write a full set of 9 tags (title, artist, album, album_artist, year, track, track_total, genre, comment) → read back → assert each field round-trips intact. Generates a minimal 144-byte PCM WAV in tempdir at runtime; no bundled binary fixture needed.
- `wav_roundtrip_clear_then_partial_write` — write tags, then `clear_existing=true` + partial write, prove only the new fields survive. Catches a subtle regression where the atomic-save restructure could ignore `clear_existing`.
- `wav_roundtrip_original_preserved_on_write_failure` — when write_metadata refuses (bad path), the original target's existing tags are intact AND no `.tmp.*` lingers next to it.

**Tasks**:
- [x] `tests/metadata_roundtrip.rs` integration test (3 tests, all passing).
- [ ] MP3 / FLAC / M4A coverage — deferred. Would need bundled binary fixtures (or a programmatic generator for each format). WAV exercises the full `read_metadata` / `write_metadata` / atomic-save chain with the same lofty backend.

**Effort**: 0.5 day.

---

### 5.2 Add MCP-protocol integration tests ✅ (done 2026-05-17)

Added [`tests/mcp_e2e.rs`](../../tests/mcp_e2e.rs). Drives the JSON-RPC layer through the real axum router via `tower::ServiceExt::oneshot` — no socket involved. Six tests covering the methods clients exercise in practice plus error paths:

- `initialize_returns_server_info_and_capabilities` — `protocolVersion`, `capabilities.tools`, `serverInfo.{name,version}`.
- `tools_list_returns_all_twelve_tools` — confirms every tool in `foreach_tool!` is exposed.
- `tools_call_dispatches_to_fs_list_dir` — full call with arguments, asserts the result mentions a file we just created in tempdir.
- `tools_call_unknown_method_returns_method_not_found` — JSON-RPC error code `-32601`.
- `tools_call_missing_name_returns_invalid_params` — JSON-RPC error code `-32602`.
- `invalid_jsonrpc_version_returns_invalid_request` — JSON-RPC error code `-32600`.

**Refactor**: extracted [`HttpTransport::build_router`](../../src/core/transport/http.rs) as a thin pub helper used by both `run()` and the tests. CORS/listener handling stays inside `run()`.

**Tasks**:
- [x] HTTP e2e via `ServiceExt::oneshot` — 6 tests, all passing.
- [ ] STDIO is already exercised by rmcp's own harness through the unit tests on the router.
- [ ] TCP — deferred. Would need a live socket harness; HTTP coverage already validates the JSON-RPC dispatch logic, which is what TCP would re-test.

**Effort**: 1 day.

---

### 5.3 Filename-injection regression test

Already specified in [0.2](#02-plug-the-cover_download-path-traversal-hole) — make sure it lives in a permanent test file, not just a one-off check.

**Effort**: included in Phase 0.

---

### 5.4 Add `clippy::pedantic` opt-in lints ✅ (done 2026-05-17)

Locked the Phase 2.1 rules into Cargo's [`[lints.clippy]`](../../Cargo.toml) table:

```toml
[lints.clippy]
unwrap_used = "warn"
expect_used = "warn"
todo = "warn"
unimplemented = "warn"
```

Test code is exempted via `#![cfg_attr(test, allow(clippy::unwrap_used, ...))]` at the crate roots ([lib.rs](../../src/lib.rs), [main.rs](../../src/main.rs)). Integration test files (`tests/`) opt out with `#![allow(...)]` at file scope since `cfg_attr(test, …)` doesn't apply to them — `tests/` is a separate compilation unit.

Cleaned up six unrelated clippy nits the new lint config surfaced (derivable Default impls, clamp pattern, collapsible if, redundant return, redundant wildcard) so `cargo clippy -- -D warnings` (the CI step in [5.5](#55-ci-smoke-check)) passes from day one.

**Effort**: 0.25 day.

---

### 5.5 CI smoke check ✅ (done 2026-05-17)

[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) runs four checks on every push and PR to `main`:

1. `cargo build --features all --locked`
2. `cargo test --features all --locked` (includes lib + the new integration tests; `#[ignore]` network tests are deliberately not run)
3. `cargo clippy --features all --locked -- -D warnings` (promotes the lints.clippy warns into errors)
4. `cargo fmt --all -- --check`

Uses `Swatinem/rust-cache@v2` for cargo registry + target caching. The original 12 broken tests from Phase 0 would have been caught on the very first push under this workflow.

**Effort**: 0.5 day.

---

## Effort summary & milestones

| Phase | Description | Effort | Cumulative |
|---|---|---|---|
| **0** | Stop the bleeding (broken tests + filename hole) | **1 day** | 1d |
| **1** | Security hardening (download cap, atomic writes, key, symlink, CORS) | 3 days | 4d |
| **2** | Code quality (unwrap/expect, dead code, format-debug, bugs, env-bool) | 3 days | 7d |
| **3** | MB tools refactor (trait, spawn_blocking, instrumentation) | 3 days | 10d |
| **4** | Architecture (capabilities, single-source, doc, error type) | 2 days | 12d |
| **5** | Tests & observability (round-trip, e2e, clippy, CI) | 2.5 days | 14.5d |

**Total**: ~3 weeks of focused work.

### Suggested milestones

- **M1 — green baseline** (end of Phase 0): all tests pass; the cover_download exploit is closed. Tag `v0.1.1`.
- **M2 — secure** (end of Phase 1): no embedded keys, atomic writes, bounded downloads, clear symlink policy. Tag `v0.2.0`.
- **M3 — clean** (end of Phase 2): no `unwrap` in production paths, no dead code, all known logic bugs fixed. Tag `v0.3.0`.
- **M4 — DRY** (end of Phase 3): MB tools consolidated, ~500 LOC removed, instrumentation parity.
- **M5 — coherent** (end of Phase 4): single source of truth for tools; capabilities honest; docs aligned.
- **M6 — guarded** (end of Phase 5): integration tests + CI prevent regression of all of the above. Tag `v1.0.0` candidate.

---

## Cross-cutting principles for execution

1. **One PR per task** — small, reviewable.
2. **Tests before the fix** when feasible — write the failing test, then fix.
3. **No mixed concerns** — security fixes don't piggy-back on refactors.
4. **CLAUDE.md is the contract** — every change should align with §2 (Core Principles) and §6 (Critical Rules).
5. **Don't gold-plate** — Phase 0 is the only urgent block; the rest can be incremental.

---

## See also

- [../../CLAUDE.md](../../CLAUDE.md) — guiding rules used to derive these tasks
- [musicbrainz-enhancements.md](musicbrainz-enhancements.md) — feature-roadmap (orthogonal to this cleanup)
- [../reference/path-security.md](../reference/path-security.md) — current security model (will be updated by Phase 1.4)
