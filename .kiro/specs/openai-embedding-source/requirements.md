# Requirements Document

## Introduction

The Tier 2 procedural-memory subsystem in `halter-goals` retrieves memories
structured-first and only widens a thin structured head with embedding (ANN)
tail recall. Today the embedding seam is a pure abstraction: the
`EmbeddingSource` trait exists with test spies only, and `MemoryRecord.embedding`
is written as an empty vector, so tail recall never contributes real semantic
matches and retrieval always degrades to structured-head-only behavior.

This feature provides a concrete OpenAI embeddings implementation of the
`EmbeddingSource` abstraction for the query path, and the analogous write-time
path that populates a memory's embedding when it is induced/stored. With this in
place, goals/memory tail recall uses real semantic embeddings instead of always
degrading, while preserving the existing graceful-degradation contract: when the
backend is unavailable the source yields no embedding and retrieval falls back to
the structured head without error.

The implementation reuses the repository's existing OpenAI authentication
patterns (API key and OAuth, as already modeled in `halter-config`'s
`[providers.openai]` block and the `halter-cli` OAuth flow) and the existing
provider resilience surface (timeouts and retries).

## Glossary

- **OpenAI_Embedding_Source**: The concrete implementation of the existing
  `EmbeddingSource` trait that produces embeddings by calling the OpenAI
  embeddings API. It is the System under specification for the query path.
- **Memory_Embedding_Writer**: The component that produces the embedding stored
  in `MemoryRecord.embedding` at memory write time (induction/insert), using the
  same embedding backend and input-construction rules as the
  OpenAI_Embedding_Source.
- **Embedding_Backend**: The abstraction over the (possibly remote) OpenAI
  embeddings HTTP endpoint, including transport, authentication, timeouts, and
  retries.
- **EmbeddingSource trait**: The existing trait in
  `crates/halter-goals/src/tier2/retrieval.rs` with method
  `fn embed(&self, sig: &IntentSignature) -> Option<Embedding>`.
- **Embedding**: The existing newtype `Embedding(pub Vec<f32>)` in
  `crates/halter-goals/src/tier2/memory.rs`.
- **IntentSignature**: The four-field structured retrieval key
  (`intent_type`, `target_type`, `target_ref`, `scope`).
- **Embedding_Input_Text**: The deterministic text string derived from an
  `IntentSignature` (query path) or from memory content (write path) that is
  sent to the Embedding_Backend as the value to embed.
- **Embedding_Model**: The configured OpenAI embeddings model identifier (for
  example `text-embedding-3-small`).
- **Embedding_Dimension**: The length of the `Vec<f32>` an embedding request is
  expected to return, determined by the Embedding_Model (and any configured
  dimension override).
- **None_Degradation_Contract**: The existing behavior (Requirement 18.7 of the
  `halter-goals` spec) whereby `EmbeddingSource::embed` returning `None` causes
  retrieval to skip tail recall and return the structured-head-only candidate
  set without error.
- **Embedding_Cache**: An in-process cache keyed by (Embedding_Model,
  Embedding_Input_Text) that stores previously computed embeddings to avoid
  redundant Embedding_Backend calls.
- **Embedding_Config**: The configuration surface controlling the OpenAI
  embedding source (model, dimension, base URL, authentication source,
  timeouts, retries, cache bounds, enable/disable).

## Requirements

### Requirement 1: Concrete OpenAI EmbeddingSource implementation

**User Story:** As a goals/memory maintainer, I want a concrete OpenAI-backed
`EmbeddingSource`, so that tail recall uses real semantic embeddings instead of
always degrading to structured-head-only retrieval.

#### Acceptance Criteria

1. THE OpenAI_Embedding_Source SHALL implement the existing EmbeddingSource
   trait method `embed(&self, sig: &IntentSignature) -> Option<Embedding>`,
   accepting a borrowed IntentSignature and returning a value of type
   `Option<Embedding>`.
2. WHEN `embed` is called and the Embedding_Backend returns a response containing
   a numeric embedding vector with at least 1 element and no NaN or infinite
   values, THE OpenAI_Embedding_Source SHALL return `Some(Embedding(vector))`
   wrapping that same vector unchanged.
3. IF `embed` is called and the Embedding_Backend returns no response, returns a
   response containing no embedding vector, or returns a vector that is empty or
   contains any NaN or infinite value, THEN THE OpenAI_Embedding_Source SHALL
   return `None` without returning an error to the caller and without altering
   the input IntentSignature.
4. THE OpenAI_Embedding_Source SHALL be usable in every position where the
   EmbeddingSource trait is accepted, in place of the existing test embedders,
   without requiring any change to the MemoryRetrieval retrieval algorithm or its
   head/tail bounds.

### Requirement 2: Deterministic embedding input construction

**User Story:** As a goals/memory maintainer, I want the text sent to the
embeddings API to be derived deterministically from the input, so that identical
inputs always produce identical requests and cache keys.

#### Acceptance Criteria

1. WHEN the OpenAI_Embedding_Source builds Embedding_Input_Text from an
   IntentSignature, THE OpenAI_Embedding_Source SHALL derive the text solely from
   the four IntentSignature fields in the fixed order `intent_type`,
   `target_type`, `target_ref`, `scope`, joined by a single fixed field separator
   and encoded as UTF-8, such that no other input influences the result.
2. WHERE two IntentSignature values are field-wise equal across all four fields
   (`intent_type`, `target_type`, `target_ref`, `scope`), THE
   OpenAI_Embedding_Source SHALL produce byte-identical Embedding_Input_Text for
   both.
3. WHEN the Memory_Embedding_Writer builds Embedding_Input_Text for a memory at
   write time, THE Memory_Embedding_Writer SHALL derive the text from the
   memory's `intent` IntentSignature using the same field order, field separator,
   and UTF-8 encoding as the OpenAI_Embedding_Source query path.
4. WHERE a memory's `intent` and a query IntentSignature are field-wise equal
   across all four fields, THE Memory_Embedding_Writer and THE
   OpenAI_Embedding_Source SHALL produce byte-identical Embedding_Input_Text.
5. WHERE two IntentSignature values differ in at least one of the four fields
   (`intent_type`, `target_type`, `target_ref`, `scope`), THE
   OpenAI_Embedding_Source SHALL produce Embedding_Input_Text values that are not
   byte-identical.
6. WHEN any of the four IntentSignature fields is absent or an empty string, THE
   OpenAI_Embedding_Source and THE Memory_Embedding_Writer SHALL represent that
   field as a fixed empty placeholder in its ordered position so that field
   boundaries are preserved and construction remains byte-identical for
   field-wise-equal inputs.

### Requirement 3: Embedding API request

**User Story:** As a goals/memory maintainer, I want the source to issue a
well-formed embeddings request, so that the OpenAI API returns an embedding for
the input.

#### Acceptance Criteria

1. WHEN the OpenAI_Embedding_Source calls the Embedding_Backend, THE
   OpenAI_Embedding_Source SHALL send a request that includes the configured
   Embedding_Model and the Embedding_Input_Text as the input to embed.
2. WHERE Embedding_Config specifies an Embedding_Dimension override, THE
   OpenAI_Embedding_Source SHALL include that dimension in the request; WHERE no
   Embedding_Dimension override is configured, THE OpenAI_Embedding_Source SHALL
   omit the dimension from the request so the Embedding_Model's default dimension
   applies.
3. THE OpenAI_Embedding_Source SHALL send the request to the embeddings endpoint
   path resolved by appending the fixed embeddings path segment to the configured
   base URL, defaulting the base URL to `https://api.openai.com` when no base URL
   override is configured.
4. IF the Embedding_Input_Text for a request is empty, THEN THE
   OpenAI_Embedding_Source SHALL return `None` without calling the
   Embedding_Backend.

### Requirement 4: Embedding API response handling

**User Story:** As a goals/memory maintainer, I want responses parsed into the
existing `Embedding` type, so that recalled vectors feed the ANN index directly.

#### Acceptance Criteria

1. WHEN the Embedding_Backend returns a success response containing one or more
   embedding objects, THE OpenAI_Embedding_Source SHALL parse the first embedding
   object's vector into an `Embedding` whose `Vec<f32>` preserves the element
   order returned by the API.
2. IF the Embedding_Backend returns a success response whose body cannot be
   parsed into at least one embedding object with a numeric vector, THEN THE
   OpenAI_Embedding_Source SHALL return `None`.
3. IF the Embedding_Backend returns a success response that contains zero
   embedding objects, THEN THE OpenAI_Embedding_Source SHALL return `None`.
4. IF the Embedding_Backend returns a success response whose selected embedding
   vector length is zero, THEN THE OpenAI_Embedding_Source SHALL return `None`.
5. FOR ALL success-response payloads that parse into a non-empty `Embedding`,
   parsing the payload, then serializing the resulting request-and-response
   representation, then parsing it again SHALL yield an `Embedding` whose
   `Vec<f32>` is element-wise equal in value and order to the `Embedding`
   produced by the first parse (round-trip property for the request/response
   codec).

### Requirement 5: Authentication reuse

**User Story:** As an operator, I want the embedding source to authenticate the
same way the rest of the OpenAI integration does, so that I do not configure
credentials twice.

#### Acceptance Criteria

1. THE OpenAI_Embedding_Source SHALL resolve the OpenAI credential using the same
   resolution and precedence rules halter already applies to the `openai`
   provider, where a configured API key and a configured OAuth credential are
   mutually exclusive, and where either configured credential takes precedence
   over the `OPENAI_API_KEY` environment variable.
2. WHERE the resolved OpenAI credential is an API key, THE OpenAI_Embedding_Source
   SHALL authenticate embeddings requests using that API key as the bearer
   credential.
3. WHERE the resolved OpenAI credential is an OAuth credential, THE
   OpenAI_Embedding_Source SHALL authenticate embeddings requests using the OAuth
   access token as the bearer credential.
4. WHERE no API key and no OAuth credential are configured, THE
   OpenAI_Embedding_Source SHALL fall back to the `OPENAI_API_KEY` environment
   variable as the API key source.
5. IF no credential can be resolved from any configured source, THEN THE
   OpenAI_Embedding_Source SHALL return `None` from `embed` without issuing a
   network request and without altering any cached state.
6. THE OpenAI_Embedding_Source SHALL exclude the resolved credential value from
   every log message and every error value it produces.

### Requirement 6: Backend-unavailability degradation

**User Story:** As a goals/memory maintainer, I want backend failures to degrade
gracefully, so that retrieval continues to return the structured head without
error.

#### Acceptance Criteria

1. IF the Embedding_Backend request fails with a transport or connection error,
   THEN THE OpenAI_Embedding_Source SHALL return `None` from `embed` without
   propagating the error to the retrieval caller.
2. IF the Embedding_Backend returns a response with an HTTP status outside the
   200–299 success range, THEN THE OpenAI_Embedding_Source SHALL return `None`
   from `embed` without propagating the error to the retrieval caller.
3. WHEN `embed` returns `None`, THE OpenAI_Embedding_Source SHALL preserve the
   None_Degradation_Contract by neither panicking nor returning an error to the
   retrieval caller, such that retrieval skips tail recall and returns the
   structured-head-only candidate set.
4. IF the Memory_Embedding_Writer's embedding request returns `None` at write
   time, THEN THE Memory_Embedding_Writer SHALL store an `Embedding` of length
   zero for the memory, complete the write successfully, and leave the memory
   retrievable via the structured filter.

### Requirement 7: Request timeout

**User Story:** As an operator, I want embedding requests bounded by a timeout,
so that a slow backend cannot stall goal retrieval.

#### Acceptance Criteria

1. THE OpenAI_Embedding_Source SHALL apply a request timeout to every
   Embedding_Backend call, using the configured embedding request timeout when
   set and defaulting to 30 seconds when no embedding-specific timeout is
   configured.
2. IF an Embedding_Backend call exceeds the applied request timeout, THEN THE
   OpenAI_Embedding_Source SHALL abandon that call and treat it as a retryable
   failure whose final outcome, once attempts are exhausted, is a `None` return.
3. IF a configured embedding request timeout is provided that is not an integer
   number of seconds of 1 or greater, THEN THE OpenAI_Embedding_Source SHALL use
   the 30-second default.

### Requirement 8: Retry on transient failure

**User Story:** As an operator, I want transient backend failures retried a
bounded number of times, so that occasional errors do not force degradation.

#### Acceptance Criteria

1. IF an Embedding_Backend call fails with a retryable condition (a connection
   error, a request timeout, HTTP status 429, or HTTP status in the 500–599
   range), THEN THE OpenAI_Embedding_Source SHALL retry the call until either the
   call succeeds or the configured maximum number of total attempts has been
   reached.
2. THE OpenAI_Embedding_Source SHALL apply at most the configured maximum number
   of total attempts per `embed` call, where the configured value is an integer
   in the range 1 to 10 inclusive, defaulting to 3 total attempts when no
   embedding-specific retry count is configured.
3. IF a configured maximum number of total attempts is provided that is outside
   the range 1 to 10 inclusive, THEN THE OpenAI_Embedding_Source SHALL use the
   default of 3 total attempts.
4. IF an Embedding_Backend call fails with a non-retryable condition (HTTP status
   in the 400–499 range other than 429), THEN THE OpenAI_Embedding_Source SHALL
   return `None` without any further attempts.
5. WHEN all permitted attempts for an `embed` call have failed, THE
   OpenAI_Embedding_Source SHALL return `None`.

### Requirement 9: Write-time and query-time dimension consistency

**User Story:** As a goals/memory maintainer, I want stored and query embeddings
to share the same model and dimension, so that ANN cosine distance is
meaningful.

#### Acceptance Criteria

1. THE Memory_Embedding_Writer and THE OpenAI_Embedding_Source SHALL request
   embeddings using a single configured Embedding_Model value and a single
   configured Embedding_Dimension value, and both SHALL read these two values
   from the same configuration source such that the Embedding_Model strings are
   byte-for-byte equal and the Embedding_Dimension integers are equal.
2. WHEN the Memory_Embedding_Writer stores a non-empty embedding for a memory,
   THE Memory_Embedding_Writer SHALL store an `Embedding` whose length equals the
   configured Embedding_Dimension.
3. WHEN the OpenAI_Embedding_Source returns a non-empty embedding, THE
   OpenAI_Embedding_Source SHALL return an `Embedding` whose length equals the
   configured Embedding_Dimension.
4. IF a returned embedding vector length does not equal the configured
   Embedding_Dimension, THEN THE OpenAI_Embedding_Source SHALL return `None`
   rather than an embedding of a mismatched length.
5. IF an embedding produced for a memory has a length that does not equal the
   configured Embedding_Dimension, THEN THE Memory_Embedding_Writer SHALL NOT
   store that `Embedding` and SHALL surface a failure indication to the caller,
   leaving any previously stored embedding for that memory unchanged.
6. IF the Embedding_Dimension configured for the Memory_Embedding_Writer is not
   equal to the Embedding_Dimension configured for the OpenAI_Embedding_Source,
   THEN THE Memory_Embedding_Writer SHALL reject the store operation and SHALL
   surface a failure indication to the caller rather than storing an `Embedding`.

### Requirement 10: Embedding caching

**User Story:** As an operator, I want repeated embeddings served from a cache,
so that identical inputs do not trigger redundant API calls.

#### Acceptance Criteria

1. THE Embedding_Cache SHALL be keyed by the pair (Embedding_Model,
   Embedding_Input_Text), such that two lookups whose Embedding_Model and
   Embedding_Input_Text are each byte-identical resolve to the same cache entry.
2. WHEN `embed` is called for an input whose (Embedding_Model,
   Embedding_Input_Text) key is present in the Embedding_Cache, THE
   OpenAI_Embedding_Source SHALL return the cached `Embedding` without calling
   the Embedding_Backend.
3. WHEN the Embedding_Backend returns a usable embedding for an input, THE
   OpenAI_Embedding_Source SHALL store that `Embedding` in the Embedding_Cache
   under its (Embedding_Model, Embedding_Input_Text) key.
4. IF storing an embedding in the Embedding_Cache does not succeed, THEN THE
   OpenAI_Embedding_Source SHALL still return the usable embedding to the caller,
   and the Embedding_Cache SHALL hold no entry for that key.
5. WHEN `embed` returns `None`, THE OpenAI_Embedding_Source SHALL leave the
   Embedding_Cache unchanged for that input's key.
6. THE Embedding_Cache SHALL hold at most the configured maximum entry count,
   defaulting to 1024 entries when no maximum is configured.
7. WHILE the Embedding_Cache is at its maximum entry count and a new distinct key
   must be inserted, THE Embedding_Cache SHALL evict the least-recently-accessed
   entry before inserting the new one.
8. WHEN the same input is embedded twice with no intervening eviction of its key,
   THE OpenAI_Embedding_Source SHALL issue at most one Embedding_Backend call
   across both embeds.

### Requirement 11: Configuration surface

**User Story:** As an operator, I want to configure the embedding source in
halter configuration, so that I can select the model, endpoint, and bounds
without code changes.

#### Acceptance Criteria

1. WHERE no Embedding_Model identifier is configured, THE Embedding_Config SHALL
   expose the Embedding_Model identifier defaulting to the OpenAI embeddings
   model `text-embedding-3-small`.
2. THE Embedding_Config SHALL expose an optional Embedding_Dimension override
   that, when set, is an integer of 1 or greater.
3. THE Embedding_Config SHALL expose an optional base URL override, an optional
   embedding request timeout defaulting to 30 seconds when unset, an optional
   maximum retry attempt count defaulting to 3 total attempts when unset, and an
   optional maximum Embedding_Cache entry count defaulting to 1024 entries when
   unset.
4. WHERE no embedding enable flag is configured, THE Embedding_Config SHALL
   expose the enable flag defaulting to enabled.
5. WHERE the operator sets the embedding enable flag to disabled, THE
   OpenAI_Embedding_Source SHALL return `None` from every `embed` call without
   issuing a network request.
6. IF Embedding_Config specifies an Embedding_Dimension that is not an integer of
   1 or greater, THEN configuration validation SHALL reject the configuration
   with an error message identifying the Embedding_Dimension field and the
   required value range.
7. IF Embedding_Config specifies an embedding request timeout, maximum retry
   attempt count, or maximum Embedding_Cache entry count that is not an integer
   of 1 or greater, THEN configuration validation SHALL reject the configuration
   with an error message identifying the rejected field and the required value
   range.
