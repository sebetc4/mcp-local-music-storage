# MusicBrainz Track B Roadmap

This roadmap follows the autonomy roadmap ([autonomy-features.md](autonomy-features.md), now fully complete) and adds the small set of MusicBrainz features that materially extend the agent's capability *without* duplicating what `apply_plan`, `mb_match_from_tags`, or composition of existing tools already cover.

> **Companion**: [musicbrainz-enhancements.md](musicbrainz-enhancements.md) is the older, exhaustive proposal — this file is the **disciplined slice** of it. Items rejected here are explicitly listed in the "Not in scope" section with reasons, so they don't keep coming back.

The repo today has **27 tools**, of which 8 are MB-facing. Each new tool adds cognitive load for the agent (more descriptions to load, more dispatch choices to make), so the bar for "new tool" is high. The bar for "enrich existing payloads" is much lower.

---

## Progress

| Phase | Status | Date | Notes |
|---|---|---|---|
| **B1 — Enrichment Primitives** | 🚧 In progress | 2026-05-21 | B1.1 `mb_get_relations` ✅ (6 source entity types — artist/release/release_group/recording/work/label; all 13 relation categories fetched in one round-trip via macro-driven `with_*_relations()` chain; post-fetch filter on `kinds` + `include_reverse`; `RelationContent` flattened to stable `(target_type, target_mbid, target_name)` triple; 200-relation hard cap with `raw_count` / `matched_count` / `truncated` counters). B1.2 not started. |
| **B2 — In-place Enhancements** | ✅ Done | 2026-05-21 | B2.1 aliases ✅, B2.4 `raw_lucene_query` ✅, B2.2 `include_tags` ✅ (shared `TagInfo` + `map_tags` helper; sorted by upvote count desc then alphabetical for deterministic output; wired on artist/release/release_group), B2.3 `country` filter ✅ (`validate_country_code` helper normalises to uppercase; wired on release mode only; refused on release_group / 2-step lookups / alongside raw_lucene_query with explicit error each). Phase B2 complete. |
| **B3 — Deferred (concrete-need-gated)** | ⏸ Deferred | — | `mb_batch_identify`, `mb_get_timeline`. Wait for a real workflow that needs them before committing. |

### Tool-count budget

| Action | Tools delta |
|---|---|
| Phase B1 (commit) | **+2** (27 → 29) |
| Phase B2 (commit, in-place) | **+0** |
| Phase B3 (deferred) | reserved |
| **Net if B1 + B2 land** | **27 → 29** (+7%) |

For comparison, the maximalist list in [musicbrainz-enhancements.md](musicbrainz-enhancements.md) would push the count to **39+** (+44%), of which ~60% would duplicate capabilities already available via composition.

### Decisions to make before starting

- **B1.1 Relations shape**: should `mb_get_relations` be a standalone tool, OR an `include_relations: bool` parameter on every entity-fetch path? Recommended: **standalone tool**. The relation kinds are a domain in their own right (producer / composer / cover / remix / …), and surfacing a `kinds: Vec<String>` filter is cleaner than a boolean on N entry points.
- **B1.2 External-id lookup scope**: should `mb_lookup_external` cover only Spotify/Discogs/Bandcamp (the three with stable URL formats), OR every catalog MB tracks? Recommended: **start with the three**, document the parser as a closed list — easy to extend later, but a "best-effort URL recognizer" is a future maintenance bog.
- **B2 enrichment flag granularity**: one boolean per concern (`include_aliases`, `include_tags`) OR a single `include: Vec<String>` array? Recommended: **separate booleans**. Discoverable in the JSON schema, no need for the agent to remember magic strings, and matches the existing `include_properties` pattern on `read_metadata`.

---

## Table of Contents

1. [Phase B1 — Enrichment Primitives](#phase-b1--enrichment-primitives) (~5 days)
2. [Phase B2 — In-place Enhancements](#phase-b2--in-place-enhancements) (~3 days)
3. [Phase B3 — Deferred](#phase-b3--deferred)
4. [Not in scope (with reasons)](#not-in-scope-with-reasons)
5. [Cross-cutting principles](#cross-cutting-principles)

---

## Phase B1 — Enrichment Primitives

**Goal**: unlock two MB capabilities the agent simply cannot reach today by composing existing tools. After Phase B1, "who produced this album?" and "I have a Spotify URL, what's the MBID?" become single calls.

### B1.1 `mb_get_relations`

The existing search/lookup tools return entities in isolation. MusicBrainz's actual value sits in the **relation graph** — producers, engineers, performers, cover versions, remixes, songwriting credits, label-imprint chains. None of that is exposed today.

**Design**: new tool implementing `MbBlockingTool` (no config needed; pure read; shares the cache + 1100 ms throttle).

```jsonc
// Params
{
  "entity_type": "release",            // artist | release | recording | work | label
  "mbid": "18079f7b-78c3-3980-b16e-c5db63cc10a5",
  "kinds": ["producer", "engineer"],    // optional; default = all relation types
  "include_reverse": true,              // default true; relations pointing AT this entity
  "limit": 100                          // hard cap MB_MAX_RELATIONS = 200
}
// Returns
{
  "entity_type": "release",
  "mbid": "...",
  "entity_name": "OK Computer",
  "relations": [
    {
      "kind": "producer",
      "direction": "backward",
      "target_type": "artist",
      "target_mbid": "...",
      "target_name": "Nigel Godrich",
      "attributes": ["additional"],
      "begin_date": "1997",
      "end_date": null
    },
    ...
  ],
  "total_count": 23,
  "truncated": false
}
```

**Tasks**:
- [x] New module `src/domains/tools/definitions/mb/relations.rs`, struct `MbRelationsTool`, impl `MbBlockingTool` (cache + throttle inherited).
- [x] One MB call per request: an `enable_all_relations!` macro pushes all 13 `with_*_relations()` toggles onto the fetch builder regardless of entity type. The bandwidth-scoping optimisation (mapping `kinds` → a subset of `with_*_relations()`) is deferred — it would require a kind → target-entity-type table and adds maintenance for unclear gain.
- [x] **`kinds` passed through verbatim from MB** rather than mapped through a `relation_kind_str` helper. Rationale: MB's relation-type vocabulary is in the hundreds (producer, vocals, composer, arranger, sampled-by, cover, mix, mastering, …). Pinning each variant in Rust would create a stale list. Anything new MB adds in the future "just works"; the agent already gets the string verbatim and can match on whatever taxonomy is current. The Rust enum we DO pin is `RelationEntityType` (the 6 source-entity types we dispatch on) — that's a closed set.
- [x] `include_reverse: bool = true` default. When `false`, filters out `direction="backward"` post-fetch.
- [x] `limit: usize = 100` default, hard-capped at `MAX_RELATIONS = 200`. Response carries `raw_count` (total MB returned), `matched_count` (after filtering, before truncation), `truncated` (cap fired).
- [x] MBID validated via `is_mbid` — refuses before the network call.
- [x] Six source entity types supported: artist / release / release_group / recording / work / label. JSON wire form uses `snake_case` via serde (so `"release_group"` → `RelationEntityType::ReleaseGroup`).
- [x] `RelationContent` flattened to stable `(target_type, target_mbid, target_name)` triple in `target_of`. URL targets surface the resource string as `target_name` so the agent doesn't need a separate field.
- [x] Registered in `foreach_tool!` as `no_config` (no `Arc<Config>` needed — pure MB read).
- [x] 6 unit tests: entity-type → MB string mapping, params parsing (defaults + snake_case for release_group), invalid-MBID early refusal, `target_of` extraction for artist + url variants.

**Acceptance**: 257 unit tests pass (lib gained 6 new relations tests). Network integration test deferred — the `#[ignore]`'d network suite is currently throttled by MB at 1100ms/call and adding a dependent test on OK Computer's exact relation graph (which evolves community-side) is brittle.

**Effort**: 3 days. **Status: done (2026-05-21).**

---

### B1.2 `mb_lookup_external`

The agent often receives external URLs ("here's the Spotify link to this album"). Today there's no way to resolve those into MBIDs without the agent leaving the MCP server — at which point the rest of the workflow can't pick up.

MB's `/url/?resource=…` endpoint returns the URL entity with its `url-rels` pointing back to the MB entity. Wrapping it gives the agent a clean bridge.

**Design**: new tool implementing `MbBlockingTool`.

```jsonc
// Params
{
  "url": "https://open.spotify.com/album/2sCWmF8j3yQjeQXLn1bC9V",
  "target_types": ["release", "release-group"]   // optional; default = all known
}
// Returns
{
  "url": "...",
  "resolved_provider": "spotify",      // spotify | discogs | bandcamp | other
  "matches": [
    {
      "target_type": "release",
      "mbid": "...",
      "title": "OK Computer",
      "artist": "Radiohead",
      "relation_kind": "free streaming"
    }
  ],
  "match_count": 1
}
```

**Tasks**:
- [ ] New module `src/domains/tools/definitions/mb/lookup_external.rs`, impl `MbBlockingTool`.
- [ ] Endpoint: `/ws/2/url/?resource=<url>&inc=*-rels&fmt=json`.
- [ ] Provider detection: pure URL inspection (host + path shape). Start with **Spotify** (`open.spotify.com/{album|track|artist}/…`), **Discogs** (`discogs.com/release|master|artist/…`), **Bandcamp** (`<artist>.bandcamp.com/album/…`). Anything else maps to `"other"` — MB still tries the resource lookup and may return matches.
- [ ] `target_types` filter is applied **after** the MB response so a single call can ask "this URL but only releases please".
- [ ] When MB returns 404 (no URL entity), respond with success + empty `matches` (not a tool error — "no match" is a normal answer).
- [ ] Validate the URL with `url::Url::parse` (already a transitive dep via reqwest; add explicit if not).

**Acceptance**: unit tests on the provider-detection function (Spotify / Discogs / Bandcamp / Other from sample URLs). One `#[ignore]`'d network test resolving a known Spotify album URL to its MBID. Empty-matches test ensuring 404 → success-with-empty rather than error.

**Effort**: 2 days.

---

## Phase B2 — In-place Enhancements

**Goal**: enrich existing tool payloads with fields the agent regularly wants but currently has to look up separately. **Zero new tools.** Each change is a parameter on an existing tool plus a small additive output field.

### B2.1 Alias support on artist / label / work

MusicBrainz tracks alternate spellings ("The Beatles" / "Beatles") via per-entity aliases. The agent loses canonisation power without them.

**Design**: add `include_aliases: bool` (default `false`) to `MbArtistParams`, `MbLabelParams`, `MbWorkParams`. When `true`, the result includes an `aliases: Vec<AliasInfo>` field.

```rust
pub struct AliasInfo {
    pub name: String,
    pub sort_name: Option<String>,
    pub locale: Option<String>,
    pub primary: bool,
    pub alias_type: Option<String>, // "Legal name", "Search hint", etc.
}
```

**Tasks**:
- [x] `?inc=aliases` toggled via `.with_aliases()` on the upstream builder when the flag is set. Builder methods need `&mut self`, so the binding was rewritten as `let mut builder; if flag { builder.with_aliases(); }` instead of an if-expression returning two different `execute()` results.
- [x] `AliasInfo` + `map_aliases` helper live in `mb::common` — single definition, three reuses. Drops upstream fields the agent doesn't need (`ended`, `type_id`) and flattens dates to plain strings. `locale` is *not* surfaced because `musicbrainz_rs` 0.13 doesn't expose it on the `Alias` struct.
- [x] `MbArtistParams` / `MbLabelParams` / `MbWorkParams` gained `include_aliases: bool` (default `false`). Same flag name + semantics across all three for predictability.
- [x] Cache invalidation is free: `MbBlockingTool::execute_cached` derives the cache key from `(NAME, serde_json::to_string(params))`, so aliases-on and aliases-off responses live in distinct cache entries.
- [x] 2 unit tests on `map_aliases` (None/empty input + field normalisation including `primary=None → false`).

**Acceptance**: existing 243 unit tests still pass with the default `include_aliases=false`. Network integration tests (`#[ignore]`'d) for `mb_artist_search` would surface the field but are deferred — no asserts added there since they'd duplicate the unit-tested mapping logic against a live MB endpoint.

**Effort**: 1 day. **Status: done (2026-05-21).**

---

### B2.2 Genre/tag aggregation on artist / release

Same shape as B2.1 but for MB's folksonomy tags (`?inc=tags`). Agents asking "what genre is this artist?" today have to consult external sources; MB itself has a community-tagged answer.

**Design**: add `include_tags: bool` (default `false`) to `MbArtistParams` and `MbReleaseParams`. New `tags: Vec<TagInfo>` field on the output.

```rust
pub struct TagInfo {
    pub name: String,
    pub count: u32,   // upvote-style count from MB users
}
```

**Tasks**:
- [x] `?inc=tags` toggled via `.with_tags()` on the upstream builder when `include_tags=true`. Same `let mut builder; if flag { builder.with_tags(); }` shape as the aliases work.
- [x] `TagInfo` + `map_tags` helper live in `mb::common`. Drops upstream `score` (only present on search responses, agent-irrelevant) and keeps `name` + `count`. `count` is `Option<i32>` because MB elides it on some entries; absent = sorted as zero for ordering.
- [x] Sort by `count` desc, then `name` asc, for stable output across calls regardless of MB's internal ordering.
- [x] Applied to `MbArtistTool` (artist mode only — `artist_releases` returns releases not artists) and `MbReleaseTool` (both `release` and `release_group` modes — RG tags tend to be more "genre"-like).
- [x] No cap (community-bounded; MB tag lists in the wild rarely exceed ~30 entries).

**Acceptance**: 2 unit tests on `map_tags` — sort order (count tie → alphabetical, `None` count sorts last) and None/empty input handling. ✅

**Effort**: 0.5 day. **Status: done (2026-05-21).**

---

### B2.3 `country` filter on `mb_release_search`

MB has a native `country=US` filter. Today the agent post-filters our results, which wastes the API quota.

**Design**: add `country: Option<String>` (ISO 3166-1 alpha-2, validated) to `MbReleaseParams`. Pushed into the Lucene query as `country:US`.

**Tasks**:
- [x] `validate_country_code(raw)` in `mb::common` — trims, refuses anything that isn't exactly 2 ASCII letters, uppercases on output. Lowercase input is accepted as a convenience.
- [x] Pushed into `ReleaseSearchQuery::query_builder().country(...)` on the typed path. Refused alongside `raw_lucene_query` (with a clear "embed `country:<code>` in your raw query instead" error) so the two paths don't quietly redundantly filter.
- [x] Refused for `search_type="release_group"` (release groups don't carry a country in MB — country is per-release) and for the 2-step lookups (`release_recordings`, `release_group_releases`).
- [x] Param description documents the ISO 3166-1 alpha-2 format with examples.

**Acceptance**: 2 unit tests on `validate_country_code` (well-formed accepted with normalisation; malformed rejected for too short / too long / digits / non-ASCII). Refusal paths exercised at the dispatcher level via the existing test harness. ✅

**Effort**: 0.5 day. **Status: done (2026-05-21).**

---

### B2.4 `raw_lucene_query` escape hatch

MB's `/ws/2/{entity}/?query=…` accepts full Lucene syntax (boolean operators, field filters, ranges, fuzzy matches). Our current search tools expose only the friendliest subset. Power users want the rest.

**Design**: add `raw_lucene_query: Option<String>` to `MbArtistParams`, `MbReleaseParams`, `MbRecordingParams`, `MbWorkParams`, `MbLabelParams`. When provided, it bypasses the builder and goes straight to the MB endpoint as the `query` parameter; the other typed fields (`query`, `country`, `artist`, …) become mutually exclusive — using both raises a parse error.

**Tasks**:
- [x] Shared `resolve_search_query(typed, raw)` helper in `mb::common`. Both empty → "Missing query" error; both set → "exactly one" error; whitespace-only treated as empty so callers can pass `""` rather than `null`.
- [x] All 5 search tools: `query` became `#[serde(default)]` so callers can omit it; `raw_lucene_query: Option<String>` added with `skip_serializing_if = "Option::is_none"`. Cache key (via `MbBlockingTool::execute_cached`) naturally differentiates by the raw flag through the serialised params.
- [x] Multi-mode tools (`mb_artist_search`, `mb_release_search`, `mb_recording_search`): the 2-step lookup modes refuse `raw_lucene_query` with an explicit error pointing the caller at the direct-search mode + a manual MBID resolution. The 4 simple-search modes accept it.
- [x] MBID fast-path is skipped when `is_raw=true` — a raw query expresses its own field constraints (e.g. `arid:<uuid>` or `rid:<uuid>`), so short-circuiting on a UUID-shaped raw string would shadow the caller's intent.
- [x] Path safety: the raw string goes through `musicbrainz_rs`'s URL encoder via `Entity::search(String)`, identical to typed-builder output. No URL injection surface added.
- [x] 4 unit tests on `resolve_search_query` (typed-only, raw-only with whitespace edge cases, both-set rejection, both-empty rejection).

**Acceptance**: 247 unit tests pass (lib gained 4 new resolver tests + the existing test calls were updated for the new signatures). No new integration tests added — the resolver logic is fully unit-tested and the network path is identical to the typed-builder one (we just hand MB a different string).

**Effort**: 1 day. **Status: done (2026-05-21).**

---

## Phase B3 — Deferred

These items have plausible value, but we lack a concrete workflow that needs them. Each one is in a holding pattern: revisit once a real user request collides with the absence.

### B3.1 `mb_batch_identify`

Batch AcoustID fingerprinting across many files. Already partially covered by `mb_match_from_tags` (Phase 3.2) for libraries with usable tags. The batch fingerprint path matters only for **tag-less** libraries.

**Trigger to start**: a user (or self-test) actually runs `mb_identify_record` in a loop over >100 files and complains about latency. Today there's no such workflow.

**Effort estimate**: 4-5 days when it lands. Needs token-bucket rate limiting against AcoustID (separate from the MB throttle).

### B3.2 `mb_get_timeline`

Chronological "discography by year" view of an artist or label. Today the agent does `mb_artist_search` (releases) + sort, which gives ~80% of the value. The remaining 20% is grouping by era / formatting — both of which are agent-side concerns.

**Trigger to start**: agents start producing time-bucketed reports for users and the manual sort burns visible tokens.

**Effort estimate**: 3-4 days.

---

## Not in scope (with reasons)

These items appear in the broader [musicbrainz-enhancements.md](musicbrainz-enhancements.md) but are deliberately rejected here. Listed so future contributors don't re-propose them without addressing the rationale.

### ❌ `mb_validate_metadata` ("quality score, required fields")

**Why not**: "Quality" is policy, not data. What counts as "complete" tags differs between a classical library, a bootleg collection, and a podcast archive. The agent already has every primitive (`read_metadata`, field presence checks, MBID validation) needed to compute its own quality score under user-specific rules; baking one in Rust would freeze policy that should stay editable in CLAUDE.md or the system prompt.

### ❌ `mb_aggregate_artist` ("full artist profile in one call")

**Why not**: It bundles 4-5 MB calls behind one tool, hiding the cost (which is real — each underlying call pays the 1100 ms throttle). The agent can already compose `mb_artist_search` + `mb_get_relations` (B1.1) + tags (B2.2) when it wants the full picture, and choose to skip the expensive parts when it doesn't.

### ❌ Composite workflows (`enrich_release`, `identify_and_tag`)

**Why not**: This is exactly what `apply_plan` (autonomy roadmap Phase 3.1) was built to make unnecessary. Every composite tool we'd add here can be expressed as a plan the agent assembles in the system prompt — and the plan stays editable, while a built-in composite freezes the workflow in code. Net effect of adding them: +N tools for −1 design choice.

### ❌ Export formats (Picard JSON / Beets YAML / CSV)

**Why not**: Pure format translation belongs on the agent side. The structured outputs from existing tools are already JSON; agents that need Picard / Beets / CSV produce them in a handful of lines without server help.

### ❌ `mb_find_similar` (recommendation engine)

**Why not**: Requires a similarity model — not a MusicBrainz primitive. Wrong abstraction layer for an MB wrapper.

### ❌ `mb_search_by_location` as a separate tool

**Why not**: MB's `country:` filter is exactly the right hook. Handled by [B2.3](#b23-country-filter-on-mb_release_search) as a parameter on the existing search tool — no new tool needed.

### ❌ `mb_advanced_search` as a separate tool

**Why not**: Same. Handled by [B2.4](#b24-raw_lucene_query-escape-hatch) as a parameter.

### ❌ Caching SQLite "in `src/core/cache/`"

**Why not**: **Already shipped**. `core::mb_cache` (autonomy Phase 2.3) is sqlite-backed, per-entity-type TTL, shared across every MB tool via the `MbBlockingTool::execute_cached` default. The cache is also queryable, durable, env-overridable (`MCP_MB_CACHE=off`, `MCP_MB_CACHE_PATH=...`).

---

## Cross-cutting principles

The same rules that drove the autonomy roadmap apply here verbatim. Restated so a future contributor doesn't have to chase them across files:

1. **One PR per task** — keep changes reviewable.
2. **Register in `foreach_tool!`** — single source of truth (cleanup roadmap Phase 4.2). One line.
3. **Reuse `MbBlockingTool`** — every pure-MB tool here implements the trait and gets cache + throttle + HTTP/STDIO/TCP wiring for free.
4. **Atomic writes** — N/A for this roadmap (read-only tools), but the rule still holds for anything that touches the filesystem.
5. **Bounded resources** — every new tool documents its hard cap (entity count, response size, time).
6. **Test before merge** — unit test for any pure logic; integration tests `#[ignore]`'d when they hit the network (CI skips ignored tests by default; opt-in via `cargo test --ignored --test-threads=1`).
7. **Doc the surface** — each new tool gets one line in CLAUDE.md §4 and a doc-comment on its `execute` body explaining the agent-facing semantics.
8. **No tool for what `apply_plan` can compose** — composite workflows belong in the agent's system prompt, not in Rust. If you feel the urge for a composite tool, write the equivalent `apply_plan` body in the prompt instead and observe whether the friction is real.

---

## Effort summary

| Phase | Description | Effort | Cumulative |
|---|---|---|---|
| **B1** | Enrichment primitives (relations, external lookup) | 5 days | 5 d |
| **B2** | In-place enhancements (aliases, tags, country, raw Lucene) | 3 days | 8 d |
| **B3** | Deferred (batch identify, timeline) | — | — |

**Total active scope**: ~8 days, +2 tools.

### Suggested sequencing

The order minimises risk and yields value early:

1. **B2.1 — Aliases** (1 day). Smallest change, no new tool, lights up canonisation everywhere.
2. **B2.4 — `raw_lucene_query`** (1 day) alongside it. Power users get the escape hatch immediately.
3. **B2.2 + B2.3 — Tags + country filter** (1 day combined). Same pattern as B2.1, builds confidence in the in-place enhancement workflow.
4. **B1.1 — `mb_get_relations`** (3 days). Highest unique-value tool; relations unlock workflows the toolkit literally cannot do today.
5. **B1.2 — `mb_lookup_external`** (2 days). External-ecosystem bridge.
6. **STOP and observe** for 2-3 weeks. Track which of B3.1 / B3.2 (or anything from "Not in scope") shows up as friction in actual conversations. Only then commit further.

---

## See also

- [autonomy-features.md](autonomy-features.md) — completed predecessor; established `apply_plan`, `MbBlockingTool`, `core::mb_cache`, `core::mb_throttle`.
- [musicbrainz-enhancements.md](musicbrainz-enhancements.md) — older, maximalist proposal. This roadmap is its disciplined slice.
- [code-quality-and-fixes.md](code-quality-and-fixes.md) — the foundation (`foreach_tool!`, atomic writes, CI gate).
- [../../CLAUDE.md](../../CLAUDE.md) — agent guide; will need one line per new tool + per enriched payload.
