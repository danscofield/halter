# Implementation Plan: OpenAI Embedding Source

## Overview

This plan turns the Tier 2 embedding seam in `halter-goals` from a test-only
abstraction into a working OpenAI-backed implementation on both the query path
and the write path, following the revised async + `halter-providers` design.

The sequence is deliberately incremental and test-driven:

1. Add the `EmbeddingConfig` schema and resolution helper in `halter-config`.
2. Build the async `EmbeddingClient` transport, wire DTOs, and error surface in
   `halter-providers`.
3. Wire the `halter-goals -> halter-providers` dependency and migrate the
   `EmbeddingSource` / `Retrieval` seam to async, fixing the full blast radius.
4. Implement deterministic input construction.
5. Implement the LRU embedding cache.
6. Implement `OpenAiEmbeddingSource` (retry, degradation, dimension, caching).
7. Implement `MemoryEmbeddingWriter` and integrate it into the store write path.
8. Implement the 13 property-based tests.
9. Add the real-HTTP (mocked) integration tests.

All code is Rust. Property tests use `proptest` (already a dev-dependency of
`halter-goals`), a minimum of 100 iterations each, one property test per design
property, each tagged with a `// Feature: openai-embedding-source, Property {n}: ...`
comment. Async property/behavior tests use `#[tokio::test]` and `.await`;
pure-function tests stay synchronous.

## Tasks

- [x] 1. Add embedding configuration surface in `halter-config`
  - [x] 1.1 Add `EmbeddingConfig` schema and defaults to `halter-config/src/schema.rs`
    - Define `DEFAULT_EMBEDDING_MODEL` (`text-embedding-3-small`),
      `DEFAULT_EMBEDDING_TIMEOUT_SECS` (30), `DEFAULT_EMBEDDING_MAX_ATTEMPTS` (3),
      `DEFAULT_EMBEDDING_CACHE_MAX_ENTRIES` (1024) constants
    - Add `default_embedding_model()` and `default_embedding_enabled()` helpers
    - Define `EmbeddingConfig` with `enabled`, `model`, `dimension`, `base_url`,
      `timeout_secs`, `max_attempts`, `cache_max_entries` fields, deriving
      `Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq` with
      `#[serde(deny_unknown_fields)]` and per-field `#[serde(default)]`
    - Implement `Default for EmbeddingConfig` (enabled=true, model=default, rest None)
    - Add `#[serde(default)] pub embedding: EmbeddingConfig` to `HarnessConfig`
    - _Requirements: 11.1, 11.2, 11.3, 11.4_

  - [x] 1.2 Implement `EmbeddingConfig::validate(path)` and wire into `HarnessConfig::validate`
    - Validate `dimension`, `timeout_secs`, `max_attempts`, `cache_max_entries`
      via the existing `validate_optional_positive_u32` /
      `validate_optional_positive_u64` helpers, producing the existing
      `invalid configuration: {path} ...` message shape with field names
    - Call `self.embedding.validate("embedding")?` inside `HarnessConfig::validate`
    - _Requirements: 11.6, 11.7_

  - [ ]* 1.3 Write unit tests for `EmbeddingConfig` defaults and validation
    - Assert defaults for model, enabled, timeout, attempts, cache size
    - Assert validation rejects zero-valued `dimension`, `timeout_secs`,
      `max_attempts`, `cache_max_entries` with field-naming messages
    - Assert unknown fields are rejected by `deny_unknown_fields`
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.6, 11.7_

  - [x] 1.4 Implement `ResolvedEmbeddingSettings` resolution helper
    - Turn `EmbeddingConfig` + resolved OpenAI provider auth
      (`resolve_provider_runtime_config` -> `ResolvedProviderAuth`) into a
      runtime-ready `ResolvedEmbeddingSettings` carrying model, dimension,
      max_attempts, timeout, enabled, base URL, and the `SecretString` bearer
    - Apply the `max_attempts` runtime clamp to `1..=10` (default 3) and the
      timeout `>= 1 else 30s` fallback; single source of truth for model +
      dimension shared by source and writer
    - Resolve the base URL, defaulting to `https://api.openai.com`
    - _Requirements: 3.3, 5.1, 5.2, 5.3, 5.4, 7.1, 7.3, 8.2, 8.3, 9.1_

  - [ ]* 1.5 Write unit tests for `ResolvedEmbeddingSettings`
    - Timeout resolution: configured vs 30s default, invalid -> 30s
    - `max_attempts` runtime clamp of out-of-range values to 3
    - Base URL default `https://api.openai.com` vs override
    - Credential precedence: api_key wins, oauth wins, env fallback,
      api_key+oauth rejected; bearer selection wrapped in `SecretString`
    - _Requirements: 3.3, 5.1, 5.2, 5.3, 5.4, 7.1, 7.3, 8.3_

- [x] 2. Build the embeddings transport in `halter-providers`
  - [x] 2.1 Define wire DTOs and domain request/response types
    - Create `crates/halter-providers/src/openai_embeddings.rs` and register the
      module in the crate root
    - Define internal serde DTOs `OpenAiEmbeddingRequestBody`
      (`model`, `input`, `#[serde(skip_serializing_if = "Option::is_none")] dimensions`),
      `OpenAiEmbeddingResponseBody` (`data`, `model`), `OpenAiEmbeddingObject`
      (`embedding`, `index`)
    - Define public `EmbeddingRequest { model, input, dimensions }` and
      `EmbeddingResponse { vector }`
    - _Requirements: 3.1, 3.2, 4.1_

  - [x] 2.2 Implement the response codec (`parse_embedding` / `to_wire`)
    - `parse_embedding(body, dimension) -> Option<Embedding-equivalent vector>`
      applying the None rules: empty `data`, unparseable, zero-length vector,
      any NaN/infinite element -> `None`; otherwise first object's vector with
      order preserved
    - `to_wire(vector, model)` producing a single-data-object response body
    - _Requirements: 4.1, 4.2, 4.3, 4.4_

  - [x] 2.3 Define `EmbeddingClientError` and retry classification
    - `thiserror` enum with `Transport`, `Timeout`,
      `RateLimited { retry_after }`, `Server(u16)`, `Client(u16)`, `Unusable`
      variants carrying no credential/URL/body text
    - `is_retryable()` true iff `Transport | Timeout | RateLimited | Server`
    - _Requirements: 6.1, 6.2, 7.2, 8.1, 8.4, 4.2, 5.6_

  - [x] 2.4 Implement the async `EmbeddingClient` trait and `OpenAiEmbeddingClient`
    - `#[async_trait] pub trait EmbeddingClient: Send + Sync` with
      `async fn embed_once(&self, request, cancel) -> Result<EmbeddingResponse, EmbeddingClientError>`
      (exactly one attempt; retry is layered by the caller)
    - `OpenAiEmbeddingClient` driving `async-openai` / `JsonHttpClient::post_json`
      with `SecretString` bearer, resolved `{base_url}/v1/embeddings` endpoint,
      and per-request timeout from `ResiliencePolicy`/`ProviderTimeouts`
    - Map the transport error surface (`provider_error_from_anyhow` /
      async-openai errors) onto `EmbeddingClientError`, carrying parsed
      `Retry-After` into `RateLimited`
    - _Requirements: 3.1, 3.2, 3.3, 4.1, 6.1, 6.2, 7.1, 7.2, 8.1, 8.4, 5.2, 5.3, 5.6_

  - [ ]* 2.5 Write property test for the request/response codec round-trip
    - **Property 6: Request/response codec round-trips**
    - **Validates: Requirements 4.5**
    - Pure/sync proptest, >= 100 cases

  - [ ]* 2.6 Write unit tests for response mapping and error classification
    - `parse_embedding` edge cases: empty data, zero-object, zero-length,
      NaN/inf, unparseable body
    - `EmbeddingClientError::is_retryable()` for each variant
    - _Requirements: 4.2, 4.3, 4.4, 6.1, 6.2, 8.1, 8.4_

- [x] 3. Wire `halter-goals -> halter-providers` and migrate the seam to async
  - [x] 3.1 Add crate dependencies to `halter-goals`
    - Add `halter-providers = { path = "../halter-providers", version = "0.5.0" }`
      and the workspace `lru` dependency to `crates/halter-goals/Cargo.toml`
      (no `reqwest`; `async-trait` already present)
    - Verify no dependency cycle is introduced
    - _Requirements: 1.4, 10.6_

  - [x] 3.2 Make `EmbeddingSource` and `Retrieval` async in `tier2/retrieval.rs`
    - Add `#[async_trait]` to `EmbeddingSource` (`async fn embed`, `Send + Sync`)
      and to `Retrieval` (`async fn retrieve_memories`)
    - Update `MemoryRetrieval::retrieve_memories` to `.await` the single
      `self.embedder.embed(sig)` call site; keep the algorithm and head/tail
      bounds unchanged; `filter`/`ann_recall` stay synchronous
    - Update the in-file `SpyEmbedder` and `DownEmbedder` fakes to
      `#[async_trait] async fn embed`; make affected tests `#[tokio::test]` and
      `.await` `retrieve_memories`
    - _Requirements: 1.1, 1.4, 6.3_

  - [x] 3.3 Fix the async blast radius in `summary.rs` and `integration.rs`
    - `tier2/summary.rs`: make the method calling `retrieve_memories` async and
      await it; update `SummaryProvider` and the `SpyRetrieval` fake
      (`#[async_trait] async fn retrieve_memories`); affected tests -> `#[tokio::test]`
    - `integration.rs`: make `start_goal(...)` async and await
      `retrieve_memories`; update the `FixedEmbedder` fake
      (`#[async_trait] async fn embed`); affected tests -> `#[tokio::test]`
    - _Requirements: 1.4, 6.3_

  - [x] 3.4 Fix the out-of-crate blast radius in the integration test file
    - `tests/sqlite_memory_store_integration.rs`: update `FixedEmbedder` to
      `#[async_trait] async fn embed`; convert affected tests to `#[tokio::test]`
      and `.await` the retrieval calls
    - Confirm `MemoryStore::filter`/`ann_recall` remain synchronous
    - _Requirements: 1.4, 6.3_

- [x] 4. Implement deterministic embedding input construction
  - [x] 4.1 Implement `embedding_input_text` in new `tier2/embedding/` module
    - Create `crates/halter-goals/src/tier2/embedding/mod.rs` and register it in
      `tier2/mod.rs`
    - Define `FIELD_SEPARATOR` (U+001F) and `EMPTY_FIELD_PLACEHOLDER` ("")
    - Implement `embedding_input_text(sig)` deriving text solely from the four
      fields in order `intent_type`, `target_type`, `target_ref`, `scope`,
      joined by the separator, UTF-8 encoded, using the placeholder for
      absent/empty fields so boundaries are preserved
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.6_

  - [ ]* 4.2 Write property test for deterministic, path-agnostic construction
    - **Property 1: Input construction is deterministic and path-agnostic**
    - **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.6**
    - Pure/sync proptest, >= 100 cases

  - [ ]* 4.3 Write property test for injectivity over field tuples
    - **Property 2: Input construction is injective over field tuples**
    - **Validates: Requirements 2.5**
    - Pure/sync proptest, >= 100 cases

- [x] 5. Implement the LRU embedding cache
  - [x] 5.1 Implement `EmbeddingCache` and `EmbeddingCacheKey`
    - In `tier2/embedding/`, define `EmbeddingCacheKey { model, input }`
      (`Debug, Clone, PartialEq, Eq, Hash`) and `EmbeddingCache` backed by
      `lru::LruCache`
    - `new(capacity)`, `get(&mut self, key)` updating recency on hit,
      `put(&mut self, key, value) -> bool` evicting the least-recently-accessed
      entry at capacity and reporting whether the entry is now present
    - _Requirements: 10.1, 10.2, 10.6, 10.7, 10.4_

  - [ ]* 5.2 Write property test for cache bounds and LRU eviction
    - **Property 10: Cache is bounded and evicts least-recently-accessed**
    - **Validates: Requirements 10.6, 10.7**
    - Pure/sync proptest, >= 100 cases

  - [ ]* 5.3 Write unit tests for cache best-effort behavior
    - Zero-capacity cache: `put` retains no entry and reports absence
    - Recency-on-access ordering for a small hand-built sequence
    - _Requirements: 10.4, 10.2_

- [x] 6. Implement `OpenAiEmbeddingSource`
  - [x] 6.1 Implement `OpenAiEmbeddingSource<C: EmbeddingClient>` construction
    - In `tier2/embedding/`, define the struct (client, `Mutex<EmbeddingCache>`,
      model, dimension, max_attempts, timeout, enabled, resolved credential
      state) and `new(client, settings: ResolvedEmbeddingSettings)`
    - Capture a "no credential" state when resolution found no credential
    - _Requirements: 9.1, 11.1, 11.4_

  - [x] 6.2 Implement the async `embed` method with retry, degradation, caching
    - `#[async_trait] impl EmbeddingSource`: disabled -> `None` (no network);
      build input, empty -> `None` (no network); cache lookup (lock not held
      across await); no credential -> `None` (no network, cache untouched);
      bounded attempt loop with retry classification and per-attempt timeout,
      calling `client.embed_once(..).await`; validate finiteness + dimension;
      best-effort cache put; map every failure to `None` leaving cache unchanged
    - Build the `EmbeddingRequest` with model, input, and conditional dimension
    - _Requirements: 1.1, 1.2, 1.3, 3.1, 3.2, 3.4, 4.1, 5.5, 6.1, 6.2, 6.3, 7.2, 8.1, 8.2, 8.4, 8.5, 9.3, 9.4, 10.2, 10.3, 10.4, 10.5, 11.5_

  - [x] 6.3 Add fake/spy async `EmbeddingClient` test doubles
    - A scripted fake whose `embed_once` returns a caller-supplied sequence of
      `Result<EmbeddingResponse, EmbeddingClientError>`, plus an attempt-counting
      spy, for deterministic retry/caching/degradation tests
    - _Requirements: 8.1, 8.2, 10.8, 6.3_

  - [ ]* 6.4 Write property test for the client request contents
    - **Property 3: The client request carries model, input, and conditional dimension**
    - **Validates: Requirements 3.1, 3.2**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 6.5 Write property test for no-network preconditions
    - **Property 4: No-network preconditions yield None without side effects**
    - **Validates: Requirements 3.4, 5.5, 11.5**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 6.6 Write property test for response mapping (finiteness + dimension)
    - **Property 5: Response mapping enforces finiteness and dimension**
    - **Validates: Requirements 1.2, 1.3, 4.1, 9.3, 9.4**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 6.7 Write property test for degradation invariant
    - **Property 7: Degradation always yields an Option, never an error or panic**
    - **Validates: Requirements 6.1, 6.2, 6.3, 7.2, 8.5, 10.5**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 6.8 Write property test for bounded, classified retry
    - **Property 8: Retry attempts are bounded and classified**
    - **Validates: Requirements 8.1, 8.2, 8.4, 8.5, 7.2**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 6.9 Write property test for caching memoization
    - **Property 9: Caching memoizes identical inputs**
    - **Validates: Requirements 10.1, 10.2, 10.3, 10.8**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 6.10 Write property test for credential redaction
    - **Property 13: Credentials never appear in logs or errors**
    - **Validates: Requirements 5.6**
    - Pure/sync proptest, >= 100 cases

  - [ ]* 6.11 Write unit tests for trait substitutability and credential/timeout resolution
    - Concrete async source used where `EmbeddingSource` is accepted, under
      `#[tokio::test]`, with unchanged head/tail bounds
    - Bearer selection from `ApiKey` vs OAuth `access_token`; timeout resolution;
      `max_attempts` clamp behavior at the source
    - _Requirements: 1.1, 1.4, 5.2, 5.3, 7.1, 7.3, 8.3_

- [x] 7. Checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

- [x] 8. Implement `MemoryEmbeddingWriter` and integrate the write path
  - [x] 8.1 Define `EmbeddingWriteError`, `WriteEmbedding`, and `MemoryEmbeddingWriter`
    - In `tier2/embedding/`, define `EmbeddingWriteError`
      (`DimensionMismatch { actual, configured }`,
      `DimensionMisconfigured { writer, source }`) and
      `WriteEmbedding { Embedded(Embedding), Unavailable }`
    - Define `MemoryEmbeddingWriter<C: EmbeddingClient>` (client, model,
      dimension, max_attempts, timeout, enabled) reading the same
      `ResolvedEmbeddingSettings` model/dimension as the source
    - _Requirements: 9.1, 9.5, 9.6_

  - [x] 8.2 Implement async `embed_for_write`
    - Build input via `embedding_input_text(intent)` (same rule as query path);
      run the same attempt/retry/timeout logic as the source; usable vector of
      configured dimension -> `Embedded`; unavailable -> `Unavailable`; length
      mismatch -> `DimensionMismatch`; writer≠source dimension ->
      `DimensionMisconfigured`
    - _Requirements: 2.3, 2.4, 6.4, 9.1, 9.2, 9.5, 9.6_

  - [x] 8.3 Add `MemoryStoreError` dimension variants and integrate into write path
    - Add `EmbeddingDimensionMismatch` / `EmbeddingDimensionMisconfigured`
      variants to `MemoryStoreError`
    - Replace hardcoded `Embedding::default()` in
      `InMemoryMemoryStore::record_from` (and the sqlite store equivalent) so the
      caller produces the embedding async up-front, outside the sync store lock,
      then passes the resulting `Embedding` into the synchronous `insert`;
      `Unavailable` -> empty embedding + successful insert; dimension failure ->
      rejected store leaving any prior embedding unchanged
    - Keep `MemoryStore::insert` synchronous
    - _Requirements: 6.4, 9.2, 9.5, 9.6_

  - [ ]* 8.4 Write property test for write-time unavailability degradation
    - **Property 11: Write-time client unavailability degrades to an empty embedding**
    - **Validates: Requirements 6.4**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 8.5 Write property test for write-time dimension mismatch rejection
    - **Property 12: Write-time dimension mismatch rejects the store**
    - **Validates: Requirements 9.2, 9.5**
    - `#[tokio::test]` proptest, >= 100 cases

  - [ ]* 8.6 Write unit tests for single-source-of-truth and misconfiguration
    - Source + writer share one `ResolvedEmbeddingSettings` model/dimension
    - Writer≠source dimension rejection surfaces `DimensionMisconfigured`
    - _Requirements: 9.1, 9.6_

- [ ] 9. Add real-HTTP integration tests (mocked server)
  - [ ]* 9.1 Write integration tests for `OpenAiEmbeddingClient` over mocked HTTP
    - 1–2 tests against a mocked HTTP server (or recorded fixture) asserting a
      well-formed `POST /v1/embeddings` with the bearer header (marked sensitive)
      and correct parsing of a representative response
    - _Requirements: 3.1, 3.3, 4.1, 5.2_

- [x] 10. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional test sub-tasks and can be skipped for a
  faster MVP; core implementation tasks are never optional.
- Each task references specific requirement sub-clauses and/or design properties
  for traceability.
- All 13 design correctness properties are each implemented by exactly one
  property test (`proptest`, >= 100 iterations), tagged with a
  `// Feature: openai-embedding-source, Property {n}: {property text}` comment.
  Async properties run under `#[tokio::test]`; pure-function properties stay sync.
- The async-ification of `EmbeddingSource`/`Retrieval` (Tasks 3.2–3.4) is a
  breaking change with a known blast radius; those tasks must be completed
  together before the source/writer can compile against the async seam.
- `MemoryStore::filter`/`ann_recall` and `MemoryStore::insert` stay synchronous;
  embeddings are produced async up-front, outside the store lock.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "2.1", "3.1", "4.1", "5.1"] },
    { "id": 1, "tasks": ["1.2", "2.2", "2.3", "3.2", "4.2", "4.3", "5.2", "5.3"] },
    { "id": 2, "tasks": ["1.3", "1.4", "2.4", "3.3", "6.1"] },
    { "id": 3, "tasks": ["1.5", "2.5", "2.6", "3.4", "6.2", "6.3"] },
    { "id": 4, "tasks": ["6.4", "6.5", "6.6", "6.7", "6.8", "6.9", "6.10", "6.11", "8.1"] },
    { "id": 5, "tasks": ["8.2", "8.3"] },
    { "id": 6, "tasks": ["8.4", "8.5", "8.6", "9.1"] }
  ]
}
```
