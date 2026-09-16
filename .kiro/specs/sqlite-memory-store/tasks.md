# Implementation Plan: sqlite-memory-store

## Overview

This plan implements two additive capabilities in the `halter-goals` Tier 2 subsystem in Rust, grounded in the approved design.

- **Part 1 — `SqliteMemoryStore`**: a durable, SQLite-backed `MemoryStore` (new `crates/halter-goals/src/tier2/sqlite_store.rs`) that is a drop-in for `InMemoryMemoryStore`. It reuses `validate_memory` and the shared `cosine_distance`, serializes the `Memory` body to JSON for lossless round-trip, indexes structured fields, and serializes access through a `Mutex<Connection>`.
- **Part 2 — `SummaryProvider`**: an opt-in, default-off summary capability (new `crates/halter-goals/src/tier2/summary.rs`) plus an additive `start_goal_with_summaries` in `integration.rs` that wraps the untouched `start_goal`.

The build order is bottom-up: shared math first, then the SQLite store (schema → errors → open → trait methods), then the summary provider, then additive wiring, with property tests placed next to the code they validate. `InMemoryMemoryStore` is the executable reference model for all parity properties.

Property-based tests use `proptest` (already a dev-dependency), run a minimum of 100 iterations per property, and each carries a tag comment `Feature: sqlite-memory-store, Property {n}: {property text}`.

## Tasks

- [x] 1. Shared ranking math and module scaffolding
  - [x] 1.1 Promote shared math and scaffold the module
    - Promote `cosine_distance` in `crates/halter-goals/src/tier2/store.rs` from private `fn` to `pub(crate) fn` so both stores share identical ranking math (non-breaking, internal change).
    - Create empty module `crates/halter-goals/src/tier2/sqlite_store.rs` and declare `pub mod sqlite_store;` in `tier2/mod.rs`.
    - Verify the crate still compiles with the promoted visibility and the new empty module.
    - _Requirements: 4.3, 7.1_

- [x] 2. SQLite schema, migration, and error type
  - [x] 2.1 Define `SqliteStoreError`
    - Add `#[derive(Debug, thiserror::Error)] pub enum SqliteStoreError` with `Open { path, #[source] source }`, `Schema(String)`, `Corruption(String)`, `Io(#[from] rusqlite::Error)`, and `Validation(#[from] MemoryStoreError)` as designed.
    - Re-export `SqliteStoreError` from `tier2/mod.rs`.
    - _Requirements: 7.1, 10.1, 10.2, 1.5_

  - [x] 2.2 Implement idempotent schema migration
    - Add a private `const` current schema version and a `fn migrate(conn: &rusqlite::Connection) -> Result<(), SqliteStoreError>`.
    - Run the `CREATE TABLE IF NOT EXISTS memories (...)` and the three `CREATE INDEX IF NOT EXISTS` statements from the design's schema.
    - Check `PRAGMA user_version`: set current when `0`, leave intact when it matches, reject a higher/unknown version with `SqliteStoreError::Schema`.
    - _Requirements: 8.1, 8.2, 8.3, 1.5_

  - [x]* 2.3 Write property test for migration idempotence
    - **Property 8: Migration idempotence** — applying `migrate` one or more times produces the same tables and indexes and leaves any pre-existing data intact.
    - **Validates: Requirements 8.1, 8.2, 8.3**

  - [x]* 2.4 Write unit tests for migration and schema-version handling
    - Concrete examples: fresh file migrates, already-migrated file is untouched, double-applied migration is stable; a DB stamped with an incompatible `user_version` returns `SqliteStoreError::Schema`.
    - _Requirements: 8.1, 8.2, 8.3, 1.5_

- [x] 3. Store construction and open-time integrity
  - [x] 3.1 Implement `open`, `open_in_memory`, and the `SqliteMemoryStore` struct
    - Define `pub struct SqliteMemoryStore { conn: std::sync::Mutex<rusqlite::Connection> }`.
    - `open(path)` maps connection-open failure to `SqliteStoreError::Open { path, source }`, runs `migrate`, and only constructs the store after migration succeeds; `open_in_memory()` opens an in-memory connection and migrates.
    - Run an open-time integrity pass that `serde_json`-deserializes every persisted `body`; return `SqliteStoreError::Corruption` if any row is malformed, so corruption is caught at open rather than served.
    - Re-export `SqliteMemoryStore` from `tier2/mod.rs`.
    - _Requirements: 1.5, 8.1, 10.1, 10.2_

  - [x]* 3.2 Write unit tests for open and corruption errors
    - A path that cannot be created/opened returns `SqliteStoreError::Open`; a row whose `body` is deliberately garbled causes `open`'s integrity pass to return `SqliteStoreError::Corruption`.
    - _Requirements: 10.1, 10.2, 1.5_

- [x] 4. Write path: insert and reinforce
  - [x] 4.1 Implement `insert`
    - Call `validate_memory(&mem)?` before any SQL so a rejected write leaves the DB unchanged; on success serialize the body to JSON, serialize `Embedding` to a JSON blob, extract indexed columns from `mem.intent`/`mem.kind`, and `INSERT OR REPLACE` keyed by `MemoryId` with `created_at == updated_at == Timestamp::default()`; return `mem.id`.
    - Acquire the `Mutex<Connection>` for the SQL work, recovering a poisoned lock via `into_inner`.
    - _Requirements: 1.1, 1.3, 1.4, 2.1, 2.3, 9.2_

  - [x] 4.2 Implement `reinforce` transactionally
    - Call `validate_memory(&candidate)?` first; inside a single rusqlite transaction, bump `hits`/`confirms` in the stored body and advance `updated_at`/`last_updated` (mirroring in-memory `updated_at + 1`) when the id exists, otherwise insert the validated candidate as a new record (`node_id` from first provenance origin, `subtree_hash` from `candidate.version.0`); return the id.
    - _Requirements: 2.2, 2.3, 6.1, 6.2, 9.2_

  - [x]* 4.3 Write property test for validation preserved on write
    - **Property 5: Validation preserved on write, database unchanged on rejection** — for any `Memory` failing `validate_memory`, `insert`/`reinforce` return the same `MemoryStoreError` variant as the reference store and leave persisted records unchanged.
    - **Validates: Requirements 2.1, 2.2, 2.3**

  - [x]* 4.4 Write property test for reinforcement bookkeeping
    - **Property 7: Reinforcement advances bookkeeping and is idempotent in shape** — reinforcing an existing id strictly increases `hits`/`confirms` and advances `updated_at`/`last_updated` matching the reference store; reinforcing an unknown id persists the validated candidate.
    - **Validates: Requirements 6.1, 6.2**

- [x] 5. Read path: filter, ann_recall, and lookups
  - [x] 5.1 Implement `filter`
    - `SELECT` candidate rows, deserialize each `body`, keep those where `body.applicability.applies_to(sig)`; use a full scan + `applies_to` as the authoritative check so the returned set equals the in-memory store even for guards over non-indexed fields.
    - Fail-safe on a malformed row: log and skip it for this collection-returning read (open-time integrity already guards diagnosable corruption).
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 1.2_

  - [x] 5.2 Implement `ann_recall`
    - Return empty when `limit == 0`; otherwise load all `(embedding, body)` rows, compute `cosine_distance(stored, query)` with the shared `pub(crate)` function, sort ascending via `total_cmp`, take `limit`; zero-magnitude vectors sort last at distance `2.0`.
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 1.2_

  - [x] 5.3 Implement `has_memory_for` and `find_duplicate`
    - `has_memory_for`: `SELECT id WHERE node_id = ?1 AND subtree_hash = ?2`.
    - `find_duplicate`: load candidate rows and return the first whose deserialized `body.intent == intent && body.plan == plan`.
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 1.2_

  - [x]* 5.4 Write property test for filter set-equality
    - **Property 1: Filter set-equality with the reference store** — for any inserts and any `IntentSignature`, the set of ids from `SqliteMemoryStore::filter` equals the set from `InMemoryMemoryStore::filter`.
    - **Validates: Requirements 3.1, 3.2, 3.3, 3.4**

  - [x]* 5.5 Write property test for round-trip equality
    - **Property 2: Persisted memory round-trip equality** — for any valid inserted `Memory`, retrieving it (via `filter` with a matching signature) yields a `Memory` equal to the one inserted.
    - **Validates: Requirements 1.1, 1.3, 1.4**

  - [x]* 5.6 Write property test for ann_recall ordering and limit
    - **Property 4: `ann_recall` ordering and limit match the reference store** — for any memories, query `Embedding`, and `limit`, returns at most `limit` memories ascending by cosine distance, equal to the reference store (including `limit == 0` and zero-magnitude handling).
    - **Validates: Requirements 4.1, 4.2, 4.3, 4.4**

  - [x]* 5.7 Write property test for idempotency and dedup lookups
    - **Property 6: Idempotency and dedup lookups match the reference store** — for any inserts, `Idempotency_Key`, and `(IntentSignature, Plan)`, `has_memory_for` and `find_duplicate` return the same `Option<MemoryId>` as the reference store.
    - **Validates: Requirements 5.1, 5.2, 5.3, 5.4**

- [x] 6. Durability across reopen
  - [x]* 6.1 Write property test for durability across reopen
    - **Property 3: Durability across reopen** — for any inserts into a file-backed store, closing and reopening at the same path yields, for every `filter`/`ann_recall`/`has_memory_for`/`find_duplicate` query, results equal to the pre-close store.
    - **Validates: Requirements 1.2, 8.2**

- [x] 7. Checkpoint - Ensure Part 1 tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 8. Summary provider (Part 2)
  - [x] 8.1 Define summary types and config
    - Create `crates/halter-goals/src/tier2/summary.rs`; declare `pub mod summary;` in `tier2/mod.rs` and re-export the public types.
    - Add `SummaryConfig { enabled: bool, max_summaries: Option<usize> }` with `Default` = disabled/unbounded, `GoalSummary { did, concluded, dead_end }`, and `pub struct SummaryProvider { config }` with `new`.
    - _Requirements: 13.1, 11.3_

  - [x] 8.2 Implement `summarize` pure function
    - Build `did` from `mem.intent` and `mem.plan` step descriptions (joined compactly), `concluded` from `mem.kind` + `mem.outcome_shape.schema` and the *shape* of `cached_outcome` (never the raw answer); set `dead_end = true` and dead-end phrasing for `MemoryKind::Negative`; exclude all `evidence_contract` values and raw transcript.
    - _Requirements: 12.1, 12.2, 12.3, 12.4, 14.3_

  - [x] 8.3 Implement `summaries_for`
    - When `config.enabled` is false, return an empty `Vec` without invoking `retrieval`; when enabled, call `retrieval.retrieve_memories(sig)`, map up to `max_summaries` candidates to `GoalSummary` preserving retrieval order, and never mutate any persisted record.
    - _Requirements: 11.1, 11.2, 11.3, 13.2, 14.3_

  - [x]* 8.4 Write property test for summary order and bound
    - **Property 9: Summaries preserve retrieval order and count bound** — when enabled, `GoalSummary` values appear in the same order as retrieval candidates and number at most `max_summaries` when set.
    - **Validates: Requirements 11.2, 11.3**

  - [x]* 8.5 Write property test for disabled provider
    - **Property 10: Disabled provider retrieves nothing and yields no summaries** — when disabled, `summaries_for` returns empty without invoking `Retrieval` (use a retrieval spy that records invocation).
    - **Validates: Requirements 13.1, 13.2**

  - [x]* 8.6 Write property test for negative dead-ends and evidence exclusion
    - **Property 12: Negative memories are summarized as dead-ends, excluding evidence values** — a `Negative` memory yields `dead_end == true` with dead-end phrasing, and no summary text contains raw evidence values or transcript content.
    - **Validates: Requirements 12.1, 12.2, 12.3, 12.4**

  - [x]* 8.7 Write unit tests for summary content examples
    - A `Negative` memory produces a dead-end summary; an enabled provider with `max_summaries = 2` over three candidates returns two in retrieval order; a disabled provider returns none.
    - _Requirements: 12.3, 11.3, 13.1_

- [x] 9. Additive hot-path wiring
  - [x] 9.1 Add `GoalStartWithSummaries` and `start_goal_with_summaries`
    - In `integration.rs`, add `#[derive(Debug, Clone, PartialEq, Eq)] pub struct GoalStartWithSummaries { start: GoalStart, summaries: Vec<GoalSummary> }`.
    - Add `start_goal_with_summaries(...)` that computes `start` via the untouched `start_goal` first, then attaches `summary_provider.summaries_for(sig, retrieval)`; leave `start_goal` and `GoalStart` unchanged.
    - _Requirements: 13.3, 14.1, 14.2, 15.1, 15.2_

  - [x]* 9.2 Write property test for additive replay decision
    - **Property 11: Summaries are additive and never alter the replay decision** — the `GoalStart` from `start_goal_with_summaries` equals the `GoalStart` from `start_goal` for the same inputs, regardless of whether summaries are enabled or produced.
    - **Validates: Requirements 13.3, 14.1, 14.2**

  - [x]* 9.3 Write unit test for empty-candidate resilience
    - When retrieval returns no candidates, `summaries` is empty and `start` is the from-scratch decision; a disabled provider yields the same `start` as `start_goal`.
    - _Requirements: 14.2, 15.1_

- [x] 10. Drop-in compatibility and end-to-end integration
  - [x] 10.1 Write drop-in wiring integration test
    - Wire `SqliteMemoryStore` (in-memory) through `MemoryRetrieval` and run `start_goal` unchanged, proving it substitutes for `InMemoryMemoryStore` without wiring changes.
    - _Requirements: 7.1, 7.2, 7.3_

  - [x]* 10.2 Write end-to-end reopen + summaries integration test
    - Insert memories via a file-backed `SqliteMemoryStore`, reopen, retrieve through `MemoryRetrieval`, and run `start_goal_with_summaries` with summaries enabled to confirm the replay decision is unchanged and summaries are attached.
    - _Requirements: 14.1, 14.2, 1.2_

- [x] 11. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for a faster MVP; core implementation tasks are never optional.
- Each task references specific requirements (granular sub-clauses) for traceability.
- Property tests use `proptest` at a minimum of 100 iterations each and are placed next to the code they validate to catch errors early.
- `InMemoryMemoryStore` is the executable reference model for all parity properties (1, 4, 5, 6, 7).
- Checkpoints ensure incremental validation at the Part 1/Part 2 boundary and at the end.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["2.1", "2.2"] },
    { "id": 2, "tasks": ["2.3", "2.4", "3.1"] },
    { "id": 3, "tasks": ["3.2", "4.1", "4.2"] },
    { "id": 4, "tasks": ["4.3", "4.4", "5.1", "5.2", "5.3"] },
    { "id": 5, "tasks": ["5.4", "5.5", "5.6", "5.7", "6.1", "8.1"] },
    { "id": 6, "tasks": ["8.2", "8.3"] },
    { "id": 7, "tasks": ["8.4", "8.5", "8.6", "8.7", "9.1"] },
    { "id": 8, "tasks": ["9.2", "9.3", "10.1"] },
    { "id": 9, "tasks": ["10.2"] }
  ]
}
```
