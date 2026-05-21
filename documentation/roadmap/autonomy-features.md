# Autonomy Features Roadmap

This roadmap closes the gaps that prevent the MCP server from being a fully autonomous music-library-organisation agent. Every task is motivated by a concrete workflow the agent should be able to execute end-to-end without the user supplying external scripts, manual fallbacks, or out-of-band tooling.

The reference workflow is:

```
scan → identify → fetch metadata → tag → embed cover → organise → verify
```

Today the chain breaks at *embed cover*, *organise*, and *scale*. The phases below fix each break in order of impact.

> **Companion**: builds on [code-quality-and-fixes.md](code-quality-and-fixes.md) (now complete — M1 through M6). That work established the clean foundation (`foreach_tool!`, `MbBlockingTool`, `fs_atomic`, CI gate) on which this roadmap relies; adding a new tool here is one line in `foreach_tool!`.

---

## Progress

| Phase | Status | Date | Notes |
|---|---|---|---|
| **1 — Workflow Completion** | ✅ Done | 2026-05-19 | 1.1 `embed_cover` ✅, 1.2 `fs_mkdir` + `fs_move` (+ `validate_unborn_path` helper) ✅, 1.3 `apply_naming_scheme` ✅ (pure templating with sanitisation, fallback chains, `:0Nd` format, refuses absolute paths and `..`). Milestone **A1 — End-to-end** reached. |
| **2 — Scale & Performance** | ✅ Done | 2026-05-20 | 2.1 `fs_scan_audio` ✅, 2.2 `read_metadata_batch` + `write_metadata_batch` ✅, 2.3 MB cache (sqlite, 24h/7d TTL, lazy purge) + throttle (1100ms slots, sync+async) ✅ wired through `MbBlockingTool::execute_cached`. Milestone **A2 — At scale** reached. |
| **3 — Safety & Quality** | ✅ Done | 2026-05-20 | 3.1 `apply_plan` ✅ (mkdir/move/write_metadata/embed_cover ops; native `dry_run` propagation + validation-only stubs for tag-writing ops; 1000-op cap; documented no-rollback policy with `executed`/`skipped`/`stopped_early` counters). 3.2 `mb_match_from_tags` ✅ (RecordingMatch-compatible payload, 0.5/0.3/0.2 title/artist/duration weights, default 0.6 confidence floor, shares MB cache + throttle via `MbBlockingTool`). 3.3 `fs_hash` + `find_duplicates` ✅ (streaming SHA-256, 64 KiB chunks, 500 MB per-file cap; walker reuses the audio extension filter, 5000-file scan cap, oversize-as-warning policy, deterministic group ordering). Milestone **A3 — Autonomous** reached. |
| **4 — Harmonisation** | ✅ Done | 2026-05-21 | 4.1 `inventory_divergences` ✅ (reverse-direction template matcher with rfind-on-critical-literal for clean `{title}.{ext}` behavior; per-directory tag histograms + per-file divergences; case-insensitive by default; numeric-aware comparison for `:0Nd` fields; cursor pages between directories never within). 4.2 `manifest_write` / `manifest_read` / `manifest_list` ✅ (strict id allowlist, 10 MB cap, atomic via `fs_atomic`, `NotFound` returned as structured success on read miss, 100-entry listing cap; no `manifest_delete` by design). Milestone **A4 — Harmonised** reached. |

### Decisions to make before starting

- **1.1 Cover embedding source**: should `mb_cover_download` learn an `embed_into` parameter (one tool, two modes — file vs. embed) OR should a separate `embed_cover` tool consume a path + an image source (file or URL)? Recommended: separate tool, single responsibility; download stays orthogonal.
- **2.3 Cache backend**: sqlite (queryable, durable, +1 dep) vs. JSON file (no dep, easy diff, no query). Recommended: sqlite via `rusqlite` for the queryable side — but defer the decision to when 2.3 starts.
- **3.1 Batch transaction model**: best-effort sequence with per-op result OR all-or-nothing with rollback. Recommended: per-op result with explicit `stop_on_error` flag; full rollback of fs moves is hard to guarantee safely.
- **4.1 Canonisation policy ownership**: the *fuzzy* decisions ("Beatles" ≈ "The Beatles", "Pop Rock" ≈ "Rock") live on the agent side; the server only reports the **facts** (which spellings exist, how often, where they diverge from the path). Rationale: canonisation rules evolve per library and per conversation — hard-coding them in Rust would freeze policy that should stay editable in `CLAUDE.md` / system prompt.
- **4.2 Manifest storage**: opaque-JSON files in `${XDG_CACHE_HOME:-~/.cache}/music-mcp/manifests/<id>.json`. The agent owns the schema; the server only persists, atomically. Avoids a real DB and keeps the agent's intent legible by `cat`.

---

## Table of Contents

1. [Phase 1 — Workflow Completion](#phase-1--workflow-completion) (~3-4 days)
2. [Phase 2 — Scale & Performance](#phase-2--scale--performance) (~3 days)
3. [Phase 3 — Safety & Quality](#phase-3--safety--quality) (~3-4 days)
4. [Phase 4 — Harmonisation](#phase-4--harmonisation) (~2 days)
5. [Effort summary & milestones](#effort-summary--milestones)
6. [Cross-cutting principles](#cross-cutting-principles)

---

## Phase 1 — Workflow Completion

**Goal**: an agent can finish a single end-to-end run for one file without falling back to external tools. After Phase 1, the chain `identify → tag → embed cover → organise` is fully expressible via MCP tool calls.

### 1.1 Embed cover art into audio files

Today `mb_cover_download` writes a JPG sibling to the audio. Most music software (and the user's expectation) is that the cover ships **inside** the file — `APIC` frame for MP3, `PICTURE` block for FLAC/Vorbis, `covr` atom for MP4/M4A. Without this, an autonomous "tag a library" workflow has to call out to external tooling.

**Design**: new tool `embed_cover` (separate from the download to keep responsibilities clean).

```jsonc
// Params
{
  "path": "/library/Artist/Album/01 Track.mp3",
  // Exactly one source must be provided:
  "image_path": "/library/Artist/Album/cover.jpg",  // OR
  "image_bytes_base64": "...",
  // Optional metadata for the embedded picture:
  "picture_type": "CoverFront",  // lofty::picture::PictureType variants
  "description": "Album front cover",
  "replace_existing": false       // by default, append; if true, drop existing pictures of the same type first
}
```

**Tasks**:
- [x] Add `embed_cover` tool under `domains/tools/definitions/metadata/embed_cover.rs`.
- [x] Wrap the lofty `Picture` API; map our `picture_type` strings to `lofty::picture::PictureType`.
- [x] Reuse the atomic-save chain (`temp_sibling` + `save_to_path(&tmp)` + `rename`) — same contract as `write_metadata`.
- [x] Validate image size against a cap (`MAX_EMBEDDED_COVER_BYTES = 10 MB` — embedded covers should be smaller than standalone ones).
- [x] Sniff MIME (`image/jpeg`, `image/png`) from magic bytes; reject anything else. (Delegated to `lofty::picture::Picture::from_reader`, then post-filter to JPEG/PNG only — TIFF/BMP/GIF refused.)
- [x] Register in `foreach_tool!` with `with_config`.

**Acceptance**: round-trip integration test (`tests/embed_cover_roundtrip.rs`): embed a tiny PNG, read back via `read_metadata` with `include_properties=true` (extending it to report embedded picture count + types), assert the picture survived. ✅ 5 tests covering happy path (file + base64 sources), `replace_existing`, append semantics, and non-image rejection (original audio untouched, no leftover temp).

**Effort**: 1 day. **Status: done (2026-05-18).**

---

### 1.2 Cross-directory move with mkdir

`fs_rename` works for in-place renames. The autonomous organise step needs to move files into a freshly-built `Artist/Album/` tree under the library root. Today the agent has no way to create the directories, and even if it did, `fs_rename` semantics for cross-directory targets are murky.

**Design**: two new tools, plus an explicit decision on `fs_rename`'s scope.

```jsonc
// fs_mkdir
{ "path": "/library/New Artist/New Album", "recursive": true, "dry_run": false }

// fs_move (or extend fs_rename — see below)
{
  "from": "/inbox/track.mp3",
  "to":   "/library/Artist/Album/01 Title.mp3",
  "mkdir_parents": true,
  "overwrite": false,
  "dry_run": false
}
```

**Decision needed**: extend `fs_rename` (add `mkdir_parents`, accept cross-dir targets) OR add a separate `fs_move`. Recommended: separate `fs_move` — keeps `fs_rename` narrow (same directory, often within a watched folder) and gives the agent an explicit signal "I am about to traverse directories". **Resolved: separate `fs_move`.**

**Tasks**:
- [x] `fs_mkdir` — validates the target via the new `validate_unborn_path` helper, then `std::fs::create_dir_all` if `recursive=true` (default), else `create_dir`. Idempotent (returns `already_existed=true` on existing dirs). Refuses when the target is a file.
- [x] `fs_move` — validates `from` with `validate_path` and `to` with `validate_unborn_path`; refuses if `to == from` or `to.starts_with(from)` (cycle); atomic same-fs rename, falls back to recursive `copy_dir_recursive` + `remove_dir_all` on `io::ErrorKind::CrossesDevices`. Refuses non-regular entries during the copy fallback.
- [x] `validate_unborn_path` (new in `core::security`): lexically normalises `.`/`..`, walks up to the deepest existing ancestor, validates that against the root, then stitches the unborn suffix back onto the canonical ancestor. Short-circuits when the input path already exists so we never tack on a trailing `/`.
- [x] Dry-run reports the would-be created parents + the strategy without touching the filesystem.

**Acceptance**: 4 integration tests in `tests/fs_mkdir_move.rs`:
1. `organise_workflow_inbox_to_library`: inbox/track.mp3 → library/AC-DC/1980 Back in Black/01-01 Hells Bells.mp3 with `mkdir_parents=true`; verifies file landed, source gone, 3 parents reported as created.
2. `mkdir_then_move_files_into_album`: provision album dir, second mkdir is idempotent, two tracks then move in.
3. `move_refuses_destination_escaping_root`: absolute path outside the root **and** `..` traversal both refused, source untouched.
4. `dry_run_reports_plan_without_side_effects`: 3 parents reported, zero filesystem changes.

**Effort**: 1 day. **Status: done (2026-05-19).**

---

### 1.3 Path templating

The agent currently has to assemble target paths by string concatenation. That's a vector for: (a) `/` injection in tag values (`title = "AC/DC"` breaks the layout), (b) inconsistent sanitisation across agent calls, (c) wasted tokens repeatedly explaining the desired layout.

**Design**: new tool `apply_naming_scheme` that takes a template + a metadata map and returns a sanitised path string. Pure function — no I/O.

```jsonc
{
  "template": "{album_artist|artist}/{year} {album}/{disc:02d}-{track:02d} {title}.{ext}",
  "metadata": {
    "artist": "AC/DC",
    "album": "Back in Black",
    "year": 1980,
    "track": 1,
    "title": "Hells Bells",
    "ext": "mp3"
  },
  "sanitise": true   // default: replace OS-unsafe chars (/, \, :, ?, *, …) with "-"
}
// Returns: "AC-DC/1980 Back in Black/01-01 Hells Bells.mp3"
```

**Tasks**:
- [x] Minimal template language: `{name}`, `{name:0Nd}` (zero-padded integer), `{name|fallback}` (use `fallback` *field* if `name` is absent/empty), and combined `{name|fallback:0Nd}`.
- [x] Sanitise each substituted component independently (so `/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|`, control bytes → `-`; trailing dots and whitespace trimmed). Literal separators in the template survive.
- [x] Reject results that would resolve to absolute paths or `..` components.
- [x] 18 unit tests covering: roadmap example, fallback (missing + empty), missing required, AC/DC-style injection, all unsafe chars, `sanitise=false` passthrough, format spec (zero-pad, numeric string, non-integer rejection), absolute-path rejection, `..` rejection, unclosed placeholder, invalid format spec, empty placeholder, separator survival, control character replacement, trailing-dot/whitespace trimming, combined fallback + format.

**Acceptance**: pure function, no integration test needed. ✅

**Effort**: 1-2 days. **Status: done (2026-05-19).**

---

## Phase 2 — Scale & Performance

**Goal**: the workflow runs on a 5000-file library in minutes, not hours. After Phase 2, the agent uses one tool call per *batch* instead of per *file*, and respects MusicBrainz rate limits without sleeping in user space.

### 2.1 Recursive audio scan

`fs_list_dir` is single-level. An autonomous "process this library" run requires the agent to recursively walk the tree itself, ~one MCP call per directory. For 500 sub-folders that's 500 round-trips.

**Design**: new tool `fs_scan_audio` (separate from `fs_list_dir`, which keeps its strict single-directory semantics).

```jsonc
{
  "root": "/library",
  "extensions": ["mp3", "flac", "m4a", "ogg", "opus", "wav"],  // default: lofty's supported set
  "max_depth": 16,            // protect against pathological deep nests
  "max_results": 5000,        // hard cap, default; if exceeded, paginate via "cursor"
  "cursor": null,             // opaque, returned by previous call when truncated
  "include_hidden": false
}
// Returns: { "files": [...], "total_seen": 4231, "next_cursor": null }
```

**Tasks**:
- [x] Use `walkdir` (now an explicit dep — it was transitive via tempfile but pinning it ourselves keeps it stable).
- [x] Apply `validate_path` to every emitted entry; strict-symlink policy rejects symlinks as a per-entry warning instead of aborting the walk.
- [x] Cursor = base64-encoded `(last_path, scanned_count)`; resumable across calls. Lex-comparison on the sorted DFS order means a single `<= last_path` check skips exactly the already-emitted prefix.
- [x] Hidden directories pruned via `WalkDir::filter_entry` (cheap — we don't descend) unless `include_hidden=true`.
- [x] Truncation flag is only set when a subsequent eligible entry exists past the cap, so the agent never gets a spurious cursor that resolves to 0 files.

**Acceptance**: integration test on a tempdir with 100 nested audio files + 100 non-audio + 1 symlink, asserts: only audio returned, symlink rejected by warning, two-call pagination with `max_results=50` yields all 100 across two calls. ✅ 11 unit tests + 2 integration tests in `tests/fs_scan_audio.rs`.

**Effort**: 0.5 day. **Status: done (2026-05-20).**

---

### 2.2 Batch metadata read/write

500 files × 1 MCP call × ~80 ms round-trip ≈ 40 s of pure wire latency, regardless of how fast lofty is. Batching cuts that by 10-50×.

**Design**: two new tools (`read_metadata_batch`, `write_metadata_batch`) rather than overloading the singletons (keeps the simple-case schema flat).

```jsonc
// read_metadata_batch
{ "paths": ["/a/x.mp3", "/a/y.mp3"], "include_properties": false }
// Returns: { "results": [{"path": ..., "metadata": ..., "error": null}, ...] }

// write_metadata_batch
{
  "writes": [
    { "path": "/a/x.mp3", "title": "X", "artist": "..." },
    { "path": "/a/y.mp3", "title": "Y", ... }
  ],
  "stop_on_error": false
}
// Returns: { "results": [{"path": ..., "fields_updated": 5, "error": null}, ...] }
```

**Tasks**:
- [x] Each batch tool runs the singletons internally — no logic duplication, just iteration + result aggregation. Batch entries carry `path`, the singleton's structured payload (typed `MetadataReadResult` for reads, `fields_updated` for writes), and an `Option<String>` error.
- [x] Hard cap on batch size (`MAX_BATCH = 500`) lives in `read_batch.rs` and is reused by the write batch.
- [ ] `tokio::task::spawn_blocking` per item with a bounded concurrency (e.g. 8 in parallel) — *deferred*. The dominant win is eliminating N×80ms of MCP wire latency by batching, which sequential execution already captures. Adding intra-batch concurrency is a follow-up profile-driven optimisation.
- [x] Each result carries its own `error: Option<String>`; the call itself is `ok` even if some items failed. `stop_on_error=true` on writes halts the loop and reports `stopped_early`/`skipped` so the caller can pick up where the batch left off.
- [x] `MetadataReadResult`, `AudioMetadata`, `AudioProperties`, `EmbeddedPictureInfo` gained `Deserialize` so the batch can roundtrip the singleton's structured output through `serde_json::from_value` without re-implementing the read logic.

**Acceptance**: integration test that batch-writes to 5 WAVs (4 valid, 1 missing), asserts overall call succeeds, 4 results have `error=null`, 1 has the expected error message. ✅ 4 integration tests in `tests/metadata_batch.rs`: the roadmap scenario, `stop_on_error` halt + skip accounting, empty-list no-op, singleton-vs-batch parity. 7 unit tests covering hard cap, stop_on_error semantics, and empty inputs.

**Effort**: 1 day. **Status: done (2026-05-20).**

---

### 2.3 MusicBrainz response cache + throttle

MusicBrainz rate-limits to 1 req/sec. An autonomous run of `mb_release_search` over a 50-album library hits the limit hard. Worse: re-querying the same MBID (often, in our workflows) is pure waste.

**Design**: server-side cache + token-bucket throttle, transparent to tool callers.

- **Cache**: keyed by `(endpoint, params_hash)`, TTL 24 h for entity lookups, 7 days for static data (label, work). Storage: sqlite via `rusqlite` (queryable, durable, ~50 KB dep impact) at `${XDG_CACHE_HOME:-~/.cache}/music-mcp/mb.sqlite`.
- **Throttle**: single `Semaphore`-style permit released every 1100 ms (slight margin over the 1s limit). Shared across all MB tools.
- **Override**: env `MCP_MB_CACHE=off` and `MCP_MB_THROTTLE=off` for debug/testing.

**Tasks**:
- [x] New module `core::mb_cache` — sqlite via `rusqlite` (bundled — no system libsqlite3 dep), table `mb_cache(key TEXT PRIMARY KEY, value TEXT, expires_at INTEGER)`, WAL + NORMAL synchronous for cache-grade durability, lazy purge on read.
- [x] New module `core::mb_throttle` — sync `wait_sync` + async `wait_async`, both reserve a slot via `Mutex<Instant>` and sleep *outside* the critical section so concurrent acquires queue up naturally.
- [x] New module `core::mb_request` — `cached_or_fetch_blocking(key, ttl, fetch_fn)` glue that consults the cache, acquires a throttle slot only on miss, and caches successful structured responses on the way back.
- [x] Wired into `MbBlockingTool` via a default `execute_cached(params)` that derives the cache key from `(NAME, serde_json::to_string(params))`. The trait's `Self::Params` gained a `Serialize` bound; the trait also gained a `TTL` const (default `TTL_ENTITY = 24h`) which `label` and `work` override to `TTL_STATIC = 7d`.
- [x] Env overrides: `MCP_MB_CACHE=off`, `MCP_MB_THROTTLE=off`, plus `MCP_MB_CACHE_PATH` for non-default locations.
- [x] Cache invalidation: relying on TTL only. The cache file is documented as expendable — `rm ~/.cache/music-mcp/mb.sqlite` clears everything.

**Acceptance**: synthetic tests in `core::mb_cache::tests` (roundtrip, expiry, upsert, persistence across reopens, env-disabled), `core::mb_throttle::tests` (3 acquires ≥ 2 × interval, disabled-mode = no-op), and `core::mb_request::tests` (miss-then-hit invokes fetch once, errors don't cache, text-only responses skip the cache, expired entries re-fetch). 11 new unit tests; no live MB network in the test suite. Per the conversation decision, intra-process MB integration tests against the wired trait stay `#[ignore]`'d alongside the existing network tests.

**Effort**: 1.5 days. **Status: done (2026-05-20). Milestone A2 — At scale reached.**

---

## Phase 3 — Safety & Quality

**Goal**: the agent can plan large operations without committing, fall back gracefully when AcoustID isn't available, and surface duplicates so the user can act on them. After Phase 3, "I let it loose on my library overnight" is a safe sentence.

### 3.1 Multi-operation plan / apply

Per-tool `dry_run` flags are great for one call. They don't let the agent say "here's a 200-step organise plan, show me the diff, then commit". Without that, large operations are scary.

**Design**: a single `apply_plan` tool that accepts a list of operations and executes them with explicit semantics.

```jsonc
{
  "operations": [
    {"op": "mkdir", "path": "/lib/A/B"},
    {"op": "write_metadata", "path": "/inbox/x.mp3", "title": "..."},
    {"op": "embed_cover", "path": "/inbox/x.mp3", "image_path": "/tmp/cover.jpg"},
    {"op": "move", "from": "/inbox/x.mp3", "to": "/lib/A/B/01 X.mp3"}
  ],
  "stop_on_error": true,
  "dry_run": false   // when true, validates every op without executing any
}
// Returns: { "results": [{"op_index": 0, "status": "ok", "detail": "..."}, ...], "executed": 3, "skipped": 1 }
```

**Tasks**:
- [x] Define a small `Operation` enum mirroring the tools' params. Internally-tagged on `"op"` (snake_case), with newtype variants wrapping `FsMkdirParams` / `FsMoveParams` / `WriteMetadataParams` / `EmbedCoverParams` — zero schema duplication.
- [x] `dry_run=true`: mkdir/move propagate their native `dry_run` (ORed with per-op authoring); write_metadata + embed_cover get validation-only stubs (`validate_path` + `is_file` + arity for embed_cover's image source) without touching tags. Per-op `dry_run=true` under a committing plan is also honoured.
- [x] `dry_run=false`, `stop_on_error=true`: first failure stops the loop. Already-committed ops are NOT rolled back (filesystem rollback is unsafe in general); they're reported as `status: "ok"` and the user/agent decides next steps.
- [x] `dry_run=false`, `stop_on_error=false`: best-effort, every op runs, individual failures land in the per-op status.
- [x] Clear doc-comment explaining the explicit non-rollback policy (with rationale: a partial rename + a successful tag-write isn't reversible without remembering the original tags, which is more state than a tool should hold).
- [x] Hard cap `MAX_OPERATIONS = 1000` (heterogeneous plans average 3-4 ops per file → ≈250 files per call).

**Acceptance**: 9 unit tests covering hard cap, empty plan, plan-level dry_run forcing into validation, validation-only stub for write_metadata, embed_cover arity validation, stop_on_error halt + skip accounting, best-effort continuation, per-op dry_run respected under committing plan, and the JSON wire format with the `op` discriminator. 3 integration tests in `tests/apply_plan.rs`: full mkdir→write_metadata→embed_cover→move pipeline in one call (assert tags + embedded PNG land on the destination); full dry-run on the same pipeline (assert zero filesystem mutation); `stop_on_error` with a sabotaged embed_cover (assert mkdir + write_metadata stay committed, move never runs). ✅

**Effort**: 1.5 days. **Status: done (2026-05-20).**

---

### 3.2 Tag-based MusicBrainz identification fallback

`mb_identify_record` needs `fpcalc` installed + an AcoustID API key. When either is missing (typical for first-time users or restricted environments), the agent has no path to identification — even when the existing tags say "title: Hells Bells, artist: AC/DC" and a quick MB query would resolve it deterministically.

**Design**: `mb_match_from_tags` — same shape as `mb_identify_record` output, but driven by `(title, artist, duration_seconds)` triples instead of acoustic fingerprints.

```jsonc
{
  "title": "Hells Bells",
  "artist": "AC/DC",                     // optional but improves matching
  "duration_seconds": 312,               // optional; matches within ±3s
  "album": null,                         // optional disambiguation hint
  "limit": 5
}
// Returns the same RecordingMatch shape as mb_identify_record so the agent can swap tools.
```

**Tasks**:
- [x] Internally: `Recording::search()` with the Lucene query builder (`recording`, optional `artist`, optional `release`). Duration stays out of the query (MB's `dur:` filter is exact-ms with no fuzz; tolerance lives in scoring instead).
- [x] Score candidates: title (exact 1.0 / prefix 0.85 / substring 0.7 / 0); artist same matcher with neutral 0.5 when unspecified; duration ±2s = 1.0, ±5s = 0.85, ±10s = 0.6, ±30s = 0.3, beyond = 0. Album is NOT scored (search results lack release lists; query-side `release:` filter is enough).
- [x] Combined confidence = `0.5·title + 0.3·artist + 0.2·duration`. Title-only query caps at 0.75 (above the default floor); title+artist hits 0.9; full triple hits 1.0.
- [x] Return only candidates above `confidence_floor` (default 0.6), sorted desc, capped at `limit` (default 5, max 25).
- [x] Shares the cache + throttle via the existing `MbBlockingTool` trait — no per-tool wiring needed.

**Acceptance**: 12 unit tests covering normalization (incl. `AC/DC` → `ac dc`, apostrophe elision), each scoring component, combined confidence for the perfect / title-only / prefix-only cases, params default parsing, and limit clamping. 1 `#[ignore]`'d network integration test (`live_query_returns_correct_mbid_with_high_confidence`) exercising AC/DC — Hells Bells with title + artist + 312s duration + album, asserting top confidence > 0.85 and the right recording title. ✅

**Effort**: 1 day. **Status: done (2026-05-20).**

---

### 3.3 Hash + duplicate detection

Common library-cleanup task: identify duplicate audio files (by exact byte hash, or by audio-content fingerprint when filenames differ). Without it, the agent's "what's in this folder" report is incomplete.

**Design**: two tools, layered.

- `fs_hash`: pure SHA-256 of file contents. Cheap, deterministic, catches *exact* duplicates (re-encoded files won't match).
- `find_duplicates`: takes a root, walks it via [2.1](#21-recursive-audio-scan), groups by hash, returns groups with >1 entry. Optional `by_audio_content: bool` later (would use a perceptual hash of decoded audio frames, e.g. Chromaprint; out of scope here).

```jsonc
// fs_hash
{ "path": "/lib/x.mp3" }
// Returns: { "sha256": "abc...def", "bytes": 4123456 }

// find_duplicates
{ "root": "/lib", "extensions": ["mp3","flac","..."] }
// Returns: { "groups": [{"hash": "...", "files": ["/lib/A.mp3", "/lib/B.mp3"]}, ...], "total_groups": 7 }
```

**Tasks**:
- [x] `fs_hash`: streams the file through `sha2::Sha256` in 64 KiB chunks via a `BufReader`. Hard-capped at `MAX_HASH_BYTES = 500 MB` (deferred a `--force` flag — caller errors out cleanly today). Catches a TOCTOU race where the file grew past the cap between `metadata()` and the streaming read.
- [x] `find_duplicates`: scans the tree with `WalkDir` (mirrors `fs_scan_audio`'s deterministic pre-order + per-directory sort), filters by extension (default = lofty audio set), hashes each file via the shared `stream_sha256` helper, groups by SHA-256, returns only groups with ≥2 paths. Output sorted by paths count desc then by hash, with paths inside each group sorted lex.
- [x] Skip-and-warn policy: walk errors, per-entry validation rejects (symlink policy, root containment), `metadata()` failures, oversize files, and per-file hash errors all surface as `warnings` strings without aborting the call.
- [x] Hard caps: `max_depth = 16`, `max_files = 5000`. `truncated=true` signals the cap was hit (no resume cursor — splitting a duplicate group across pages would be confusing; narrow the root instead).
- [x] `skip_oversize_silently` flag lets a caller running over a mixed library suppress the per-file oversize warnings.

**Acceptance**: 7 `fs_hash` unit tests (NIST FIPS 180-4 "abc" digest + known empty-string digest + multi-chunk identical-bytes equality + one-byte-difference + directory/outside-root/nonexistent refusals). 7 `find_duplicates` unit tests, including the roadmap scenario (3 identical + 2 distinct → 1 group of 3), no-duplicates baseline, extension filter, cross-subdirectory discovery, root-outside-config refusal, truncation when `max_files` is short, and ranking by descending group size. ✅

**Effort**: 1 day. **Status: done (2026-05-20). Milestone A3 — Autonomous reached.**

---

## Phase 4 — Harmonisation

**Goal**: support the **directory-as-source-of-truth** workflow — a multi-thousand-file library laid out as `{genre}/{artist}/{album}/{track}` whose *tags* drift from the path conventions over time. The agent should be able to scan the tree, observe the divergences, propose canonical values, and apply them across thousands of files in a resumable way.

### Reference workflow

```
1. inventory_divergences(root, "{genre}/{artist}/{album}/{title}.{ext}")
   → structured report: per directory, which fields diverge from path,
     histogram of every spelling that currently exists for each field.

2. agent reasons:
   "/Rock/The Beatles/ has artist values {Beatles:32, The Beatles:4,
    the beatles:1}. Path says 'The Beatles' — canonical is 'The Beatles'."

3. agent builds an operations list (writes) and calls apply_plan(plan,
   dry_run=true) — preview from Phase 3.1.

4. (optional) user reviews the diff.

5. apply_plan(plan, dry_run=false) — execute. Atomic per-file writes
   via the existing fs_atomic chain.

6. manifest_write("harmonize-2026-05-17", {touched_files, summary})
   → snapshot for resumability. Next session can manifest_read it and
     pick up where it left off.
```

The two new tools below (4.1, 4.2) are the missing primitives. Steps 2, 3, 4 are agent-side policy. Steps 5, 6 reuse mechanism already on the roadmap.

---

### 4.1 Inventory divergences between path and tags

The killer report tool. One call returns enough structured data for the agent to plan a whole-library harmonisation pass without re-querying individual files.

**Design**: new tool `inventory_divergences`, pure-read (no mutation). Streams the same `walkdir` traversal as [2.1](#21-recursive-audio-scan) but enriches each entry with the path-template match and the tag divergence set.

```jsonc
// Params
{
  "root": "/library",
  "path_template": "{genre}/{artist}/{album}/{title}.{ext}",
  "fields_to_compare": ["genre", "artist", "album", "title"],   // optional; default = every named capture in the template
  "max_files": 5000,                                            // hard cap; if exceeded, paginate via cursor
  "cursor": null,                                               // opaque, returned by previous call when truncated
  "case_sensitive": false                                       // when comparing strings; default false so "Beatles" ≈ "BEATLES"
}
// Returns
{
  "directories": [
    {
      "path": "/library/Rock/The Beatles/Abbey Road",
      "path_inferred": {
        "genre": "Rock",
        "artist": "The Beatles",
        "album": "Abbey Road"
      },
      // Histogram of every value currently present in tags for this directory.
      // The agent uses this to pick the canonical form at a glance.
      "field_value_counts": {
        "artist": { "Beatles": 12, "The Beatles": 4, "the beatles": 1 },
        "genre":  { "Rock": 14, "Pop Rock": 3 }
      },
      "files": [
        {
          "name": "01 Come Together.mp3",
          "path_inferred_title": "Come Together",
          "tags": {
            "artist": "Beatles",
            "album": "Abbey Road",
            "genre": "Pop Rock",
            "title": "Come Together"
          },
          // Fields whose tag value disagrees with the path-inferred value
          // (case-insensitively if requested). Empty array = file is consistent.
          "divergences": ["artist", "genre"]
        }
        // … one entry per file under this directory
      ]
    }
    // … one entry per directory matching the template's leading components
  ],
  "files_scanned": 4123,
  "files_with_divergences": 1872,
  "next_cursor": null
}
```

**Tasks**:
- [x] Reuse the template parser from 1.3 (made `Segment` + `parse_template` + `FormatSpec` `pub(crate)` in `naming::apply_scheme`). Split the parsed template by `/` into "slots" — one per path component.
- [x] Per-slot matcher walks segments **left-to-right** with `find` for every literal, except the literal immediately preceding a trailing placeholder, which uses `rfind`. That single asymmetry gives `{title}.{ext}` natural extension behavior on titles like `Mr. Brightside.mp3` while keeping multi-capture slots (`{disc}-{track} {title}.{ext}`) on standard left-to-right matching.
- [x] Refuse adjacent placeholders in the same slot (`{a}{b}`) at template-validation time — undecidable in reverse.
- [x] Per-file pipeline: walker yields entries, validate_path filters per-entry (root containment + symlink policy), match path against template (mismatches surface as warnings), read tags via lofty (read errors surface as warnings, file kept in directory with empty tags).
- [x] Group by leaf directory. Histograms are `BTreeMap<field, BTreeMap<value, count>>` for deterministic output. Divergences are case-insensitive by default; whitespace always trimmed; numeric values compared numerically when both parse as i64 (so `01` doesn't diverge from `1` on `:0Nd` fields).
- [x] Pagination cursor encodes `(last_completed_directory_path, files_scanned, files_with_divergences)`. Directories are never split across pages — when the cap hits mid-directory we bail BEFORE the new directory and the cursor points to the previous one.
- [x] Hard cap `MAX_FILES = 5000` (aligned with the metadata batch cap).
- [x] Register in `foreach_tool!` as `with_config` (uses `validate_path`); under a new `harmonisation/` module that will host 4.2 next.

**Acceptance**: 17 unit tests (slot matcher edge cases: single placeholder, multi-capture, dot-in-title rfind, missing literal, empty capture, adjacent placeholders refused, wrong component count, directory-vs-file-level captures, divergence detection, numeric padding, template field collection, cursor round-trip). 3 integration tests in `tests/inventory_divergences.rs`: the roadmap's Beatles+Radiohead fixture (writing real ID3v2-tagged WAVs via the singleton write tool) asserting 1 divergence on `artist`; off-template files surfacing as warnings without aborting; cursor resume across 3 directories with `max_files=2` halting after the first and resuming cleanly into the rest. ✅

**Effort**: 1.5 days. **Status: done (2026-05-20).**

---

### 4.2 Agent-owned manifests

Long-running harmonisation passes need to survive a session crash, a `cargo build` reload, or just an agent decision to "do the rest tomorrow". The agent maintains the *intent* (what's planned, what's done); the server only persists the JSON file the agent gives it, atomically.

**Design**: three thin tools — `manifest_write`, `manifest_read`, `manifest_list`. Storage in `${XDG_CACHE_HOME:-~/.cache}/music-mcp/manifests/<id>.json` (override via `MCP_MANIFEST_DIR` env var for testability and explicit operator control).

```jsonc
// manifest_write — atomic; overwrites if id exists
{
  "id": "harmonize-2026-05-17",     // [A-Za-z0-9._-]{1,128}, validated server-side
  "content": { /* arbitrary JSON the agent owns */ }
}
// Returns: { "path": "/home/seb/.cache/music-mcp/manifests/harmonize-2026-05-17.json", "bytes": 4123 }

// manifest_read
{ "id": "harmonize-2026-05-17" }
// Returns: { "content": { ... }, "written_at": "2026-05-17T10:42:11Z", "bytes": 4123 }
// or:      { "error": "NotFound" }

// manifest_list
{}
// Returns: { "manifests": [{"id": "...", "written_at": "...", "bytes": 4123}, ...] }
```

**Tasks**:
- [x] `id` validated against a tighter allowlist than `is_safe_filename`: `[A-Za-z0-9._-]{1,128}`, no leading `.`. Inlined as `is_valid_manifest_id` because the existing helper allows whitespace + unicode which are friction for ids.
- [x] `MAX_MANIFEST_BYTES = 10 MB` cap on the serialised body.
- [x] Writes go through `core::fs_atomic::write_atomic`.
- [x] Manifest dir resolved as `$MCP_MANIFEST_DIR` → `$XDG_CACHE_HOME/music-mcp/manifests` → `$HOME/.cache/music-mcp/manifests`. Created on first write via `create_dir_all`.
- [x] `manifest_list` reads the dir once, filters `.json` files whose stem passes `is_valid_manifest_id`, sorts by RFC3339 `written_at` desc, caps at `MAX_LIST = 100` with a `truncated` flag.
- [x] `manifest_read` returns `{id, content: null, written_at: null, bytes: null, error: "NotFound"}` on miss as a structured success — agents distinguish "first run" from "malformed call" without exception handling.
- [x] No `manifest_delete` (documented in the module-level docstring).

**Acceptance**: 7 integration tests in `tests/manifests.rs`, scoped per-test with `MCP_MANIFEST_DIR` under a process-global `Mutex` for safe parallel runs: round-trip of a ~500 KB nested payload, overwrite without leftover `.tmp.*`, NotFound shape on a missing id, list sorted by recency, list on missing dir returns empty (not error), invalid ids consistently refused with no leaked files, 11 MB body rejected. 2 unit tests on `is_valid_manifest_id`. ✅

**Effort**: 0.5 day. **Status: done (2026-05-21). Milestone A4 — Harmonised reached.**

---

## Effort summary & milestones

| Phase | Description | Effort | Cumulative |
|---|---|---|---|
| **1** | Workflow completion (embed cover, move/mkdir, path templating) | 3-4 days | 3-4 d |
| **2** | Scale & performance (scan, batch I/O, MB cache + throttle) | 3 days | 6-7 d |
| **3** | Safety & quality (plan/apply, tag fallback, dedup) | 3-4 days | 9-11 d |
| **4** | Harmonisation (inventory divergences, agent manifests) | 2 days | 11-13 d |

**Total**: ~2-2.5 weeks of focused work.

### Suggested milestones

- **A1 — End-to-end** (end of Phase 1): one tool call sequence can identify, tag, embed cover, and organise a single file from inbox to library. Tag this `v1.1.0`.
- **A2 — At scale** (end of Phase 2): a 1000-file library run completes in minutes. Tag `v1.2.0`.
- **A3 — Autonomous** (end of Phase 3): the agent can be left unattended on a library with `apply_plan` doing the heavy lifting and `find_duplicates` flagging follow-ups. Tag `v2.0.0`.
- **A4 — Harmonised** (end of Phase 4): a multi-thousand-file library laid out as `{genre}/{artist}/{album}/{title}` can be harmonised end-to-end against its directory hierarchy, with resumable runs surviving session boundaries. Tag `v2.1.0`.

---

## Cross-cutting principles

1. **One PR per task** — same rule as the code-quality roadmap.
2. **Register in `foreach_tool!`** — single source of truth (Phase 4.2 of the previous roadmap). One line.
3. **Reuse the trait** — pure MusicBrainz search additions (like [3.2](#32-tag-based-musicbrainz-identification-fallback)) implement `MbBlockingTool`. Tools needing `Arc<Config>` stay outside it.
4. **Atomic writes** — every file-replacement path goes through `core::fs_atomic` (already established).
5. **Bounded resources** — every new tool gets a documented hard cap (size, count, depth, time). No unbounded loops.
6. **Test before merge** — at minimum one integration test per new tool. Network tests `#[ignore]`'d; the CI workflow already excludes them.
7. **Doc the surface** — each new tool gets one line in CLAUDE.md §4 and a doc-comment on its `execute` body explaining the agent-facing semantics.

---

## See also

- [code-quality-and-fixes.md](code-quality-and-fixes.md) — the completed cleanup roadmap; established the patterns this one builds on.
- [musicbrainz-enhancements.md](musicbrainz-enhancements.md) — older roadmap covering MB-specific feature additions (some overlap with Phase 2/3 here; reconcile before starting).
- [../../CLAUDE.md](../../CLAUDE.md) — agent guide; will need a §4 update per new tool.
