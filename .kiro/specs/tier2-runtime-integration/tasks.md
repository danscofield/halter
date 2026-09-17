# Implementation Plan: Tier 2 Runtime Integration

## Overview

This plan delivers two independent runtime capabilities and removes the orphaned
verification-tier plumbing, in incremental, test-driven Rust steps. It builds
bottom-up so each step compiles and is exercised before it is wired in:

- **Code removal first** (unblocks a clean `halter-goals` and removes dead
  plumbing before new code lands).
- **Capability B foundations** (`ToolResult` envelope, cacheability policy,
  `ToolResultStore` trait + in-memory backend, config) before interposing in
  `ToolRuntime::execute`.
- **Capability A foundations** (advisory entry point + `AdvisorySummary`,
  `GoalRetrieval` seam, `InductionEngine::with_writer` + `NoEmbeddingClient`)
  before wiring into `HalterBuilder::build`.
- **Builder wiring** for both capabilities, ending with `RuntimeServices` and
  end-to-end integration tests.

Property-based tests (Properties 1–13 from the design) are placed as optional
sub-tasks next to the code they validate so regressions surface early. The crate's
established PBT library is `proptest`.

## Tasks

- [x] 1. Remove the orphaned verification-tier plumbing from `halter-goals`
  - [x] 1.1 Remove the validator/resolver types and their re-exports
    - Delete `Tier1EvidenceValidator<'a, C, P, R>`, the `SourceResolver` trait, and the `SourceResolution` enum from `crates/halter-goals/src/integration.rs`
    - Update the `integration::{..}` re-export in `crates/halter-goals/src/lib.rs` to `pub use integration::{start_goal, EngineInductionQueue, GoalStart};` (drop `SourceResolution`, `SourceResolver`, `Tier1EvidenceValidator`)
    - KEEP `decide_replay`, `ReplayDecision`, the `tier2::replay` module, `EvidenceValidator`, `TokensHold`, the entire `tier1` module, `CachedOutcome`/`AnswerMode`, `start_goal`/`start_goal_with_summaries`/`GoalStart`
    - _Requirements: (code removal — design "Code Removal" section)_

  - [x] 1.2 Clean up tests that reference the removed types
    - Remove the `MapResolver` fixture (`impl SourceResolver`), its helpers, and the `FakeProvider`-based validator fixtures that only fed the validator
    - Remove the validator-focused unit tests: `validator_reports_fresh_when_item_token_holds`, `validator_reports_stale_when_item_token_does_not_hold`, `validator_reports_stale_when_source_unresolvable`, `validator_reports_stale_when_backend_unavailable`, `validator_tokens_all_hold_when_all_fresh`, `validator_tokens_some_stale_when_one_fails`, `validator_tokens_unavailable_when_backend_down`
    - In the kept `start_goal`/`start_goal_with_summaries` tests, replace the removed concrete validator with a tiny local test-only `EvidenceValidator` fixture (returning `Freshness::Stale` / `TokensHold::SomeStale`) so those kept functions stay covered
    - Ensure `halter-goals` compiles and all kept tests pass
    - _Requirements: (code removal — design "Test cleanup" section)_

- [x] 2. Checkpoint - `halter-goals` compiles cleanly after removal
  - Ensure all tests pass, ask the user if questions arise.

- [x] 3. Extend the `ToolResult` envelope with an optional cache-hit indicator
  - [x] 3.1 Introduce `ToolResultKind` and wrap `ToolResult` as a struct
    - In `crates/halter-protocol/src/lib.rs`, define `ToolResultKind` (the former `Empty`/`Text`/`Json` variants, `#[serde(tag = "kind", rename_all = "snake_case")]`)
    - Redefine `ToolResult` as a struct: `{ #[serde(flatten)] kind: ToolResultKind, #[serde(default, skip_serializing_if = "Option::is_none")] cache_hit: Option<bool> }`
    - Add associated constructors `ToolResult::empty()`, `ToolResult::text(..)`, `ToolResult::json(..)`, and `with_cache_hit(self, bool) -> Self`
    - _Requirements: 16.1, 16.3, 16.5, Q5_

  - [x] 3.2 Migrate construction and match/destructure sites across the workspace
    - Rewrite construction sites (`ToolResult::Json { value }`, etc.) to the new constructors (`ToolResult::json(value)` / `text` / `empty`)
    - Rewrite match/destructure sites to match on `.kind` against `ToolResultKind::{Empty,Text,Json}` (e.g. `halter-tools/builtin` tests, `halter-session/history.rs`, examples)
    - Update `halter-session/history.rs` to match on `result.content.kind`
    - Ensure the whole workspace compiles
    - _Requirements: 16.1, 16.5_

  - [ ]* 3.3 Write property test for `ToolResult` round-trip and legacy compatibility
    - **Property 12: `ToolResult` round-trips through storage and legacy bytes read as not-from-cache**
    - Generate all kinds × `cache_hit ∈ {None, Some(false), Some(true)}`, plus a legacy-shaped JSON (no `cache_hit`) generator; assert serialize→deserialize preserves payload + indicator, and absent `cache_hit` ⇒ `None`
    - **Validates: Requirements 16.4, 16.6**

- [x] 4. Implement the cacheability policy on `ToolSpec`/`ToolCapabilities`
  - [x] 4.1 Add the `cacheable` opt-in and the `is_cacheable` predicate
    - Add `#[serde(default)] pub cacheable: bool` to `ToolCapabilities` in `crates/halter-protocol/src/lib.rs` (defaults false; existing sites use `..Default::default()`/field lists)
    - Add `fn is_cacheable(spec: &ToolSpec) -> bool` in `crates/halter-tools/src/runtime.rs`: `cacheable == true && mutating == false && concurrency ∈ {ReadOnly, ParallelSafe}`
    - _Requirements: 15.1, 15.2, 15.3, 15.4, Q1_

  - [ ]* 4.2 Write property test for cacheability determination
    - **Property 10: Only side-effect-free tools are cacheable**
    - Generate `ToolSpec` with arbitrary `mutating`/`concurrency`/`cacheable` combinations; assert `is_cacheable` is true only under the conjunction and that mutating/exclusive tools are never cacheable
    - **Validates: Requirements 15.1, 15.2, 15.3, 15.4**

- [x] 5. Implement the `ToolResultStore` trait and in-memory backend
  - [x] 5.1 Define the async `ToolResultStore` trait
    - Create `crates/halter-tools/src/cache.rs` with `#[async_trait] pub trait ToolResultStore: Send + Sync` exposing `get(&self, key: &str) -> Option<Vec<u8>>` and `put(&self, key: String, value: Vec<u8>, ttl: Duration)`
    - Value boundary is bytes (`Vec<u8>`), store owns expiry
    - _Requirements: 14.3, 14.4, 14.5, Q2_

  - [x] 5.2 Implement `InMemoryToolResultStore`
    - Back it with `RwLock<HashMap<String, (Vec<u8>, Instant)>>` storing a deadline `Instant = stored_at + ttl`
    - `get` returns bytes iff `Instant::now() <= deadline` (else `None`); `put` performs a lazy expiry sweep, enforces a global entry cap with nearest-deadline eviction, then inserts
    - Recover poisoned locks with `unwrap_or_else(PoisonError::into_inner)`
    - _Requirements: 14.3, 14.4, 14.5, Q4_

  - [x] 5.3 Implement the `CacheKey` composite and stable string rendering
    - In `crates/halter-tools/src/runtime.rs` (or `cache.rs`), define `CacheKey { session, tool, normalized_args }`
    - `compute(session, tool, input)` calls `normalize_args` and returns `None` on `Err` (non-canonicalizable)
    - `to_key_string()` renders a length-prefixed, injective flat string embedding `SessionId`, tool name, and canonical args
    - _Requirements: 14.1, 14.2, Q3_

- [x] 6. Interpose the cache in `ToolRuntime::execute`
  - [x] 6.1 Add optional store + TTL + enabled flag to `ToolRuntime`
    - Add fields `cache_enabled: bool` (default false), `cache_ttl: Duration`, `store: Option<Arc<dyn ToolResultStore>>` (default `None`) to `ToolRuntime`
    - Add `with_cache(&mut self, store: Arc<dyn ToolResultStore>, ttl: Duration)`; ensure `new`/`Default`/`clone_filtered` default to no caching (fresh store on clone)
    - _Requirements: 13.2, 13.4_

  - [x] 6.2 Implement the interposed `execute` path
    - Short-circuit to `tool.execute(context, input).await` when cache disabled/absent OR `!is_cacheable(spec)` (byte-identical dispatch)
    - Bypass on non-canonicalizable key; on hit within TTL deserialize bytes, return `with_cache_hit(true)` without invoking; on miss/expiry invoke, then on `Ok` serialize the untagged result and `put` with the resolved TTL, returning it untagged; on `Err` propagate and store nothing; treat corrupt/legacy bytes as a miss
    - _Requirements: 13.4, 14.1, 14.2, 14.3, 14.4, 14.5, 14.6, 14.7, 15.3, 16.2, 16.3, 17.1, 17.2, 17.3_

  - [ ]* 6.3 Write property test for hit/miss/expiry and tagging
    - **Property 7: Tool-result cache hit / miss / expiry and tagging are correct**
    - Use a fake `Tool` with a call counter and a fake/short-TTL `ToolResultStore`; assert first call invokes once and stores untagged, repeat within TTL returns tagged without invoking, aged entry is a miss that re-invokes
    - **Validates: Requirements 14.3, 14.4, 14.5, 14.6, 16.2, 16.3, 17.1**

  - [ ]* 6.4 Write property test for canonicalization-equal keying
    - **Property 8: Canonicalization-equal calls share a stored entry**
    - Generate key-reordered-equivalent args; assert the second is served from the first's bytes (same string key), and non-equal canonicalizations render distinct keys (miss)
    - **Validates: Requirements 14.1**

  - [ ]* 6.5 Write property test for non-canonicalizable bypass
    - **Property 9: Non-canonicalizable arguments bypass the store**
    - Generate non-canonicalizable args (non-finite numbers / duplicate keys); assert the tool is invoked directly with no `get`/`put`
    - **Validates: Requirements 14.2**

  - [ ]* 6.6 Write property test for errored-invocation caching
    - **Property 11: An errored cacheable invocation caches nothing**
    - Fake tool returns `Err`; assert the error propagates and no entry is `put`, so a subsequent identical call re-invokes
    - **Validates: Requirements 14.7, 17.2**

  - [ ]* 6.7 Write property test for disabled/non-cacheable pass-through
    - **Property 13: Disabled cache and non-cacheable tools pass through unchanged**
    - Assert invoke-exactly-once, result equal to no-cache dispatch, `cache_hit` absent, no `get`/`put`, dispatch/ordering/error preserved
    - **Validates: Requirements 13.4, 15.3, 17.3**

- [x] 7. Add `[tools]` cache configuration
  - [x] 7.1 Extend `ToolsConfig` and add `ToolCacheBackend`
    - In `crates/halter-config/src/schema.rs` add `#[serde(default)]` fields `cache_enabled: bool`, `tool_cache_ttl_secs: Option<u64>`, `tool_cache_backend: ToolCacheBackend`
    - Define `ToolCacheBackend { #[default] Memory, Redis, Sqlite }` (`rename_all = "snake_case"`); add `const DEFAULT_TOOL_CACHE_TTL_SECS: u64 = 60`
    - Add `ToolsConfig::validate` applying `validate_optional_positive_u64` to `tool_cache_ttl_secs`; keep `deny_unknown_fields`
    - _Requirements: 13.1, 13.2, 13.3, 13.5_

  - [ ]* 7.2 Write unit tests for tools config defaults and validation
    - Assert omitted flag ⇒ disabled, omitted TTL ⇒ runtime default, omitted backend ⇒ `Memory`, and `tool_cache_ttl_secs = 0` is rejected
    - _Requirements: 13.1, 13.2, 13.3, 13.5_

- [x] 8. Wire the tool cache into `HalterBuilder::build`
  - [x] 8.1 Construct and install the backend when enabled
    - In `crates/halter/src/builder.rs`, when `cache_enabled`, map `ToolCacheBackend`: `Memory` ⇒ construct `InMemoryToolResultStore` (with global entry cap) and call `ToolRuntime::with_cache(store, ttl)`; resolve TTL from `tool_cache_ttl_secs` or `DEFAULT_TOOL_CACHE_TTL_SECS`
    - `Redis`/`Sqlite` ⇒ fail `build` fast with a clear "tool cache backend '..' is not yet supported" error
    - When disabled, install no store (today's behavior)
    - _Requirements: 13.1, 13.2, 13.3, 13.5_

- [x] 9. Checkpoint - Capability B compiles and cache tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 10. Add the advisory entry point and `GoalRetrieval` seam (Capability A foundations)
  - [x] 10.1 Implement `AdvisorySummary` and `retrieve_advisory`
    - In `crates/halter-goals/src/integration.rs` add `AdvisorySummary { memories: Vec<ScoredMemory>, summaries: Vec<GoalSummary> }` with `empty()`/`is_empty()`
    - Add `async fn retrieve_advisory(sig, retrieval, summary_provider) -> AdvisorySummary` that runs the unchanged `Retrieval::retrieve_memories` + `SummaryProvider::summaries_for` and never calls `decide_replay`/`EvidenceValidator`/`ReplayDecision`
    - Re-export `AdvisorySummary` from `crates/halter-goals/src/lib.rs`
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5, 5.6_

  - [ ]* 10.2 Write property test for advisory content
    - **Property 2: Advisory is memories + summaries only, never a verification result**
    - For arbitrary store contents/intent, assert the advisory holds exactly the retrieval-ordered applicable memories + summaries, no replay/verification/answer, and is empty (no error) when nothing applies
    - **Validates: Requirements 5.1, 5.3, 5.4, 5.5**

  - [ ]* 10.3 Write property test for structured-head-only degradation
    - **Property 3: Retrieval degrades to structured-head-only when the backend is down**
    - With an `embed`-returns-`None` source, assert `retrieve_advisory` yields the structured-head-only set and propagates no error
    - **Validates: Requirements 10.3, 10.5, 5.2, 5.6**

  - [x] 10.4 Add the `GoalRetrieval` seam to `GoalTool`
    - In `crates/halter-tools/src/builtin/goal.rs` add `#[async_trait] pub trait GoalRetrieval` with `retrieve_advisory(&self, intent: &IntentSignature) -> AdvisorySummary`
    - Add `retrieval: Option<Arc<dyn GoalRetrieval>>` to `GoalTool`; keep `new(store)` (`None` ⇒ byte-identical) and add `with_retrieval(store, retrieval)`
    - In `create_action`, fold the advisory into the existing `ToolResult::Json` only when `retrieval.is_some()`; add `render_advisory` producing `{ applicable_memories, similar_goals }` (empty arrays when empty)
    - _Requirements: 1.3, 5.1, 5.3, 5.5_

  - [ ]* 10.5 Write property test for off-mode byte-identical result
    - **Property 1: Off mode emits a byte-identical goal-create result**
    - For arbitrary hypotheses, a `GoalTool::new` result is exactly `{ id, active }` with no `advisory` key, `cache_hit` absent, and issues no embedding request
    - **Validates: Requirements 1.1, 1.2, 1.3, 1.4**

- [x] 11. Route induction inserts through the writer
  - [x] 11.1 Add `InductionEngine::with_writer` and route the insert
    - In `crates/halter-goals/src/tier2/induction.rs` add the `C: EmbeddingClient` generic plus `writer: MemoryEmbeddingWriter<C>` and `source_dimension: Option<u32>` fields
    - Add `with_writer(store, tracker, judge, author, threshold, writer, source_dimension)`
    - Change the single non-dedup insert call site to `insert_memory_with_writer(&self.writer, self.source_dimension, self.store.as_ref(), key, candidate).await`, mapping `Ok`⇒`Inserted`, `Err`⇒`AuthorFailed`; leave the dedup/reinforce path unchanged
    - _Requirements: 6.1, 6.2, 6.3, 8.2, 9.1, 9.2_

  - [x] 11.2 Add `NoEmbeddingClient` and keep `InductionEngine::new` as the disabled path
    - Add zero-sized `NoEmbeddingClient` implementing `EmbeddingClient` (never reached)
    - Reimplement `InductionEngine::new` (unchanged params) to build a disabled `MemoryEmbeddingWriter<NoEmbeddingClient>` with empty-credential settings and `source_dimension = None`, so existing callers/tests degrade to empty embedding as today
    - _Requirements: 6.4, 10.4_

  - [ ]* 11.3 Write property test for induction embedding storage and retrievability
    - **Property 4: Induction stores the writer's embedding and always remains retrievable**
    - Assert real vector stored when available, empty `Embedding` when unavailable/disabled (no request when disabled), insertion always succeeds, memory always retrievable via structured `filter`
    - **Validates: Requirements 6.1, 6.2, 6.3, 6.4, 10.4**

  - [ ]* 11.4 Write property test for dimension-mismatch rejection
    - **Property 5: Dimension mismatch rejects the insertion and leaves the store unchanged**
    - When `source_dimension` differs from the writer's configured dimension, assert rejection with the dimension-misconfiguration error and any prior embedding unchanged
    - **Validates: Requirements 8.2, 8.3**

- [x] 12. Checkpoint - Capability A foundations compile and tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 13. Construct the embedding source, client, and writer in the builder
  - [x] 13.1 Add non-fatal `resolve_embedding_settings`
    - In `crates/halter/src/builder.rs` add `resolve_embedding_settings(config)` reusing `resolve_provider_runtime_config(.., OpenAi)`, catching the no-credential `Err` and synthesizing an empty bearer, then `ResolvedEmbeddingSettings::resolve(&config.embedding, &auth)`
    - _Requirements: 3.3, 3.4, 10.2, 11.1_

  - [x] 13.2 Construct source + writer from one settings value
    - Under `auto`, build `source_dimension = settings.dimension`, one `OpenAiEmbeddingClient` + `OpenAiEmbeddingSource` (query), and a second client + `MemoryEmbeddingWriter` (write) from the same `settings.clone()` so dimensions agree
    - Log only `enabled` + store backend, never the credential
    - _Requirements: 3.1, 3.2, 8.1, 11.2, 11.3_

  - [ ]* 13.3 Write property test for credential non-exposure
    - **Property 6: The embedding credential is never exposed**
    - For arbitrary bearer strings, assert debug/display of settings, source, writer, and every construction-path error value never contains the credential
    - **Validates: Requirements 11.1, 11.2**

- [x] 14. Construct the memory store from the session backend
  - [x] 14.1 Implement `build_memory_store` and `memory_store_path`
    - In `crates/halter/src/builder.rs` add feature-gated `build_memory_store(&SessionsConfig)`: `Memory` ⇒ `InMemoryMemoryStore`; `Sqlite` ⇒ `SqliteMemoryStore::open(memory_store_path)` mapping open failure to a build error identifying the memory-store init failure
    - Add `memory_store_path` (sqlite): a `memory.sqlite3` sibling in the session DB's directory, defaulting to the halter dir when unset
    - Return `Arc<dyn MemoryStore>` shared by retrieval and induction
    - _Requirements: 4.1, 4.2, 4.3, 4.4, Q6_

- [x] 15. Wire retrieval, induction, and services into `build` (auto mode)
  - [x] 15.1 Add the `RuntimeGoalRetrieval` adapter
    - In `crates/halter/src/builder.rs` add `RuntimeGoalRetrieval { store, source, summary }` implementing `GoalRetrieval` by building `MemoryRetrieval::new(store, source)` (default head/tail bounds) and calling `retrieve_advisory`
    - _Requirements: 5.1, 5.2, 5.6, 10.3_

  - [x] 15.2 Build the induction knot and register the goal tool
    - Construct `InductionEngine::with_writer(mem_store.clone(), .., writer, source_dimension)`
    - Break the construction knot: build a resolution-clone `EventLogGoalStore::new(SessionStoreGoalEventLog::new(sessions.clone()))`, then `EngineInductionQueue::new(resolution_store, engine)`, then the mutation `EventLogGoalStore::with_induction_queue(.., queue)`
    - Register `GoalTool::with_retrieval(goal_store.clone(), advisory)`; under `off`, construct none of these and keep the default `NoopInductionQueue`
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 6.1, 7.1, 7.2, 7.3, 9.3, 9.4, 12.1, 12.2, 12.3_

  - [x] 15.3 Add the `Tier2Services` bundle to `RuntimeServices`
    - Add `tier2: Option<Tier2Services>` to `RuntimeServices` in `crates/halter-runtime/src/session.rs`, with `Tier2Services { memory_store, embedding_source, embedding_enabled, memory_backend }` (non-secret only)
    - Populate `Some(..)` under `auto`, `None` under `off`; emit the Req 12.3 diagnostic when `auto` + embedding disabled
    - _Requirements: 1.1, 1.3, 11.3, 12.3_

- [x] 16. Checkpoint - full workspace builds and all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 17. Runtime integration tests (few, representative — not PBT)
  - [ ]* 17.1 Write off-mode and auto-mode builder integration tests
    - Off mode: `build` constructs no Tier 2 components, `tier2: None`, no embedding request; auto + embedding disabled: builds with structured-head-only retrieval and the diagnostic log; auto + sqlite: memory store opens the `memory.sqlite3` sibling
    - _Requirements: 1.1, 1.4, 2.1, 2.4, 4.2, 10.1, 12.3_

  - [ ]* 17.2 Write tool-cache builder integration tests
    - `cache_enabled = false` ⇒ no store installed (pass-through); `cache_enabled = true` + `tool_cache_backend = memory` ⇒ builds successfully; `redis`/`sqlite` ⇒ build fails fast
    - _Requirements: 13.1, 13.2, 13.4_

## Notes

- Tasks marked with `*` are optional and can be skipped for a faster MVP; core implementation tasks are never optional.
- Each task references specific requirement clauses (not just user stories) for traceability.
- Property sub-tasks each reference a single design property and are placed next to the code they validate to catch regressions early.
- Checkpoints ensure incremental validation; run the workspace build and relevant tests at each.
- The crate's established PBT library is `proptest`; the async store trait makes TTL/expiry cases deterministic via a fake `ToolResultStore` (no real sleeps).
- Type-level/architectural invariants (synchronous `MemoryStore` signatures Req 9.3; no-await-under-lock Req 9.1/9.2/9.4; `ToolResult` matchsite validity Req 16.1/16.5; `Tier2Services` non-secret shape) are enforced by the compiler and structure review as smoke checks, not property tests.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "3.1", "7.1"] },
    { "id": 1, "tasks": ["1.2", "3.2", "5.1", "7.2"] },
    { "id": 2, "tasks": ["3.3", "4.1", "5.2", "10.1", "11.1"] },
    { "id": 3, "tasks": ["4.2", "5.3", "10.2", "10.3", "10.4", "11.2", "11.3", "11.4"] },
    { "id": 4, "tasks": ["6.1", "10.5"] },
    { "id": 5, "tasks": ["6.2"] },
    { "id": 6, "tasks": ["6.3", "6.4", "6.5", "6.6", "6.7", "8.1", "13.1", "14.1"] },
    { "id": 7, "tasks": ["13.2", "15.1"] },
    { "id": 8, "tasks": ["13.3", "15.2"] },
    { "id": 9, "tasks": ["15.3"] },
    { "id": 10, "tasks": ["17.1", "17.2"] }
  ]
}
```
