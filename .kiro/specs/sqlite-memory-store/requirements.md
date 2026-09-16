# Requirements Document

## Introduction

This feature adds two capabilities to the `halter-goals` crate's Tier 2 procedural-memory subsystem.

**Part 1 — SQLite-backed `MemoryStore`.** A durable, SQLite-backed implementation of the existing `MemoryStore` trait (`crates/halter-goals/src/tier2/store.rs`) that is a drop-in alternative to the in-memory `InMemoryMemoryStore`. It persists `MemoryRecord`s across process restarts, answers the structured applicability filter over indexed columns, supports embedding/ANN tail recall, and preserves the idempotency (`has_memory_for`) and dedup (`find_duplicate`) lookups. Write-time validation via `validate_memory` is preserved unchanged, so no invalid memory is ever persisted.

**Part 2 — Similar-goal summaries on the goal-tool hot path.** A wireable, opt-in capability so that when the agent triggers the goal tool (the `start_goal` hot-path seam in `crates/halter-goals/src/integration.rs`), halter searches the memory store for similar past goals using the existing structured-first retrieval and returns compact, natural-language-style summaries of what those goals did and what conclusions or outcomes they reached. The summaries are surfaced back to the agent alongside (not in place of) the existing replay decision. This capability is off by default and configurable.

The design is constrained by the existing Tier 2 seams and MUST NOT alter the `MemoryStore` trait contract, the `Memory`/`MemoryRecord` model, the `validate_memory` invariants, or the structured-first retrieval semantics.

## Glossary

- **Memory_Store**: The subsystem implementing the `MemoryStore` trait; persists `Memory` objects and surfaces applicable ones for a new goal.
- **Sqlite_Memory_Store**: The SQLite-backed `MemoryStore` implementation introduced by Part 1.
- **In_Memory_Memory_Store**: The existing `InMemoryMemoryStore`, the reference implementation whose observable behavior the `Sqlite_Memory_Store` mirrors.
- **Memory**: The durable, replayable Tier 2 artifact (`crate::tier2::memory::Memory`): intent signature, applicability guard, plan, evidence contract, outcome shape, optional cached outcome, provenance, reinforcement, version.
- **Memory_Record**: The persisted store record (`crate::tier2::memory::MemoryRecord`) wrapping a `Memory` with indexed structured fields (`intent_type`, `target_type`, `target_ref`, `scope`, `kind`), an `Embedding`, provenance origins (`node_id`, `subtree_hash`), and `created_at`/`updated_at` timestamps.
- **Idempotency_Key**: The `(GoalNodeId, SubtreeHash)` pair under which a memory is stored; the key consulted by `has_memory_for`.
- **Applicability_Filter**: The structured primary filter (`MemoryStore::filter`) returning every stored memory whose `Applicability` guard is satisfied by a querying `IntentSignature`.
- **Ann_Recall**: The embedding tail-recall read path (`MemoryStore::ann_recall`) returning up to `limit` memories nearest a query `Embedding` by cosine distance.
- **Validate_Memory**: The write-time validation gate (`validate_memory`) enforcing the Tier 2 invariants; returns `MemoryStoreError` on violation.
- **Memory_Store_Error**: The existing write-time validation error type (`MemoryStoreError`).
- **Retrieval**: The structured-first retrieval engine (`Retrieval`/`MemoryRetrieval`) producing an ordered `Vec<ScoredMemory>`.
- **Goal_Tool_Trigger**: The hot-path entry point invoked when the agent triggers the goal tool, realized by the `start_goal` seam in `integration.rs`.
- **Goal_Summary**: A compact, natural-language-style description of one past similar goal: what it did (its plan/intent) and what conclusion or outcome it reached (its kind and outcome).
- **Summary_Provider**: The component introduced by Part 2 that turns retrieved `ScoredMemory` values into `Goal_Summary` values.
- **Summary_Enabled**: The opt-in configuration flag that controls whether the `Summary_Provider` runs; defaults to disabled.
- **Migration**: The schema-initialization step that creates the SQLite tables and indexes if they are absent, applied idempotently on open.

## Requirements

### Requirement 1: Durable persistence of memories

**User Story:** As a halter operator, I want memories persisted to SQLite, so that learned procedural memory survives process restarts.

#### Acceptance Criteria

1. WHEN a valid `Memory` is inserted into the `Sqlite_Memory_Store` under an `Idempotency_Key`, THE `Sqlite_Memory_Store` SHALL persist the corresponding `Memory_Record` to the SQLite database and return the `Memory`'s `MemoryId`.
2. WHEN a `Sqlite_Memory_Store` is opened against a database file that already contains persisted `Memory_Record`s, THE `Sqlite_Memory_Store` SHALL make every previously persisted `Memory_Record` retrievable through `Applicability_Filter`, `Ann_Recall`, `has_memory_for`, and `find_duplicate`.
5. IF a `Sqlite_Memory_Store` is opened against a database whose schema is incompatible or whose persisted `Memory_Record`s are corrupted, THEN THE `Sqlite_Memory_Store` SHALL fail to open and return an error identifying the schema or corruption failure rather than serving reads or writes.
3. THE `Sqlite_Memory_Store` SHALL store each `Memory_Record` such that the retrieved `Memory` is equal to the `Memory` that was inserted (round-trip equality of the serialized `Memory` body).
4. THE `Sqlite_Memory_Store` SHALL persist the indexed structured fields (`intent_type`, `target_type`, `target_ref`, `scope`, `kind`), the `Embedding`, the provenance origin (`node_id`, `subtree_hash`), and the `created_at`/`updated_at` timestamps for each `Memory_Record`.

### Requirement 2: Write-time validation preserved

**User Story:** As a Tier 2 maintainer, I want the SQLite store to enforce the same write-time invariants as the in-memory store, so that no invalid memory is ever persisted.

#### Acceptance Criteria

1. WHEN a `Memory` that fails `Validate_Memory` is inserted into the `Sqlite_Memory_Store`, THE `Sqlite_Memory_Store` SHALL return the matching `Memory_Store_Error` and SHALL leave the database unchanged.
2. WHEN a `reinforce` candidate that fails `Validate_Memory` is submitted to the `Sqlite_Memory_Store`, THE `Sqlite_Memory_Store` SHALL return the matching `Memory_Store_Error` and SHALL leave the existing persisted `Memory_Record` unchanged.
3. WHEN a `Memory` is submitted for insertion or reinforcement, THE `Sqlite_Memory_Store` SHALL apply `Validate_Memory` before performing any database mutation.

### Requirement 3: Structured applicability filter over indexed columns

**User Story:** As the retrieval engine, I want the SQLite store to answer the structured applicability filter, so that structured-first retrieval works identically to the in-memory store.

#### Acceptance Criteria

1. WHEN `Applicability_Filter` is invoked with an `IntentSignature`, THE `Sqlite_Memory_Store` SHALL return every persisted `Memory` whose `Applicability` guard is satisfied by that `IntentSignature`.
2. WHEN `Applicability_Filter` is invoked with an `IntentSignature`, THE `Sqlite_Memory_Store` SHALL exclude every persisted `Memory` whose `Applicability` guard is not satisfied by that `IntentSignature`.
3. FOR ALL `IntentSignature` values and all sets of persisted memories, THE set of memories returned by the `Sqlite_Memory_Store` `Applicability_Filter` SHALL equal the set returned by the `In_Memory_Memory_Store` `Applicability_Filter` for the same inputs.
4. IF indexed columns cannot guarantee that the `Applicability` guard is correctly evaluated for a query, THEN THE `Sqlite_Memory_Store` SHALL evaluate the guard by full scan over persisted memories so that correctness is preserved, or SHALL return an error identifying the failure.

### Requirement 4: Embedding tail recall

**User Story:** As the retrieval engine, I want the SQLite store to support embedding tail recall, so that a thin structured head can be widened.

#### Acceptance Criteria

1. WHEN `Ann_Recall` is invoked with a query `Embedding` and a `limit`, THE `Sqlite_Memory_Store` SHALL return at most `limit` persisted memories ordered by ascending cosine distance to the query `Embedding`.
2. IF `Ann_Recall` is invoked with a `limit` of zero, THEN THE `Sqlite_Memory_Store` SHALL return an empty result.
3. WHEN `Ann_Recall` ranks persisted memories, THE `Sqlite_Memory_Store` SHALL treat a zero-magnitude stored or query `Embedding` as maximally distant so that ranking remains defined.
4. WHEN `Ann_Recall` is invoked with a zero-magnitude query `Embedding`, THE `Sqlite_Memory_Store` SHALL return up to `limit` persisted memories under the maximal-distance ranking rather than returning an empty result.

### Requirement 5: Idempotency and dedup lookups

**User Story:** As the induction engine, I want the SQLite store to answer idempotency and dedup lookups, so that recurring goals reinforce rather than duplicate memories.

#### Acceptance Criteria

1. WHEN `has_memory_for` is invoked with an `Idempotency_Key` for which a `Memory_Record` has been persisted, THE `Sqlite_Memory_Store` SHALL return that record's `MemoryId`.
2. IF `has_memory_for` is invoked with an `Idempotency_Key` for which no `Memory_Record` has been persisted, THEN THE `Sqlite_Memory_Store` SHALL return no `MemoryId`.
3. WHEN `find_duplicate` is invoked with an `IntentSignature` and a `Plan` that both match a persisted `Memory_Record`, THE `Sqlite_Memory_Store` SHALL return that record's `MemoryId`.
4. IF `find_duplicate` is invoked with an `IntentSignature` and `Plan` that do not both match any persisted `Memory_Record`, THEN THE `Sqlite_Memory_Store` SHALL return no `MemoryId`.

### Requirement 6: Reinforcement bookkeeping

**User Story:** As the induction engine, I want reinforcement to update persisted bookkeeping, so that confidence and dedup counters accumulate durably.

#### Acceptance Criteria

1. WHEN `reinforce` is invoked with a valid candidate against a `MemoryId` that identifies a persisted `Memory_Record`, THE `Sqlite_Memory_Store` SHALL advance that record's `hits` and `confirms` counters, advance its `last_updated` and `updated_at` timestamps, and return the same `MemoryId`.
2. WHEN `reinforce` is invoked with a valid candidate against a `MemoryId` that identifies no persisted `Memory_Record`, THE `Sqlite_Memory_Store` SHALL persist the validated candidate as a new `Memory_Record` and return the candidate's `MemoryId`.

### Requirement 7: Drop-in trait compatibility

**User Story:** As an integrator, I want the SQLite store to satisfy the same trait as the in-memory store, so that wiring code can select either without other changes.

#### Acceptance Criteria

1. THE `Sqlite_Memory_Store` SHALL implement the `MemoryStore` trait with the same method signatures used by `In_Memory_Memory_Store`.
2. THE `Sqlite_Memory_Store` SHALL expose read and write methods that take a shared reference (`&self`), preserving the interior-mutability contract of the `MemoryStore` trait.
3. WHERE existing wiring accepts any `MemoryStore` implementation (for example the induction and retrieval seams), THE `Sqlite_Memory_Store` SHALL be usable in place of `In_Memory_Memory_Store` without changes to that wiring's public contract.

### Requirement 8: Schema initialization

**User Story:** As a halter operator, I want the SQLite store to initialize its own schema, so that a fresh database file works without manual setup.

#### Acceptance Criteria

1. WHEN a `Sqlite_Memory_Store` is opened against a database that lacks the required tables or indexes, THE `Sqlite_Memory_Store` SHALL create the required tables and indexes before serving any read or write.
2. WHEN a `Sqlite_Memory_Store` is opened against a database whose required tables and indexes already exist, THE `Sqlite_Memory_Store` SHALL leave the existing schema and data intact.
3. WHEN `Migration` is applied more than once to the same database, THE `Sqlite_Memory_Store` SHALL produce the same schema as applying it once (idempotent initialization).

### Requirement 9: Concurrency safety

**User Story:** As a runtime engineer, I want the SQLite store to be safe under concurrent access, so that it can be shared like the in-memory store.

#### Acceptance Criteria

1. WHILE multiple readers access the `Sqlite_Memory_Store` concurrently through shared references, THE `Sqlite_Memory_Store` SHALL return results consistent with the persisted database state without data races.
2. WHEN a write and one or more reads are issued concurrently against the `Sqlite_Memory_Store`, THE `Sqlite_Memory_Store` SHALL serialize the write so that each read observes either the pre-write or post-write state and never a partially written `Memory_Record`.

### Requirement 10: Store open and I/O error handling

**User Story:** As a halter operator, I want clear errors when the database cannot be opened or accessed, so that misconfiguration is diagnosable.

#### Acceptance Criteria

1. IF a `Sqlite_Memory_Store` cannot open or initialize its database file at the configured path, THEN THE `Sqlite_Memory_Store` SHALL return an error identifying the failure rather than serving reads or writes.
2. IF a read against the `Sqlite_Memory_Store` encounters an unpersisted or malformed stored `Memory_Record`, THEN THE `Sqlite_Memory_Store` SHALL surface an error identifying the failure rather than returning a silently corrupted `Memory`.

### Requirement 11: Similar-goal retrieval on the goal-tool trigger

**User Story:** As the agent, I want halter to find similar past goals when I trigger the goal tool, so that I can benefit from prior conclusions.

#### Acceptance Criteria

1. WHILE `Summary_Enabled` is true, WHEN the `Goal_Tool_Trigger` fires with an `IntentSignature`, THE `Summary_Provider` SHALL obtain candidate memories by invoking the existing structured-first `Retrieval` for that `IntentSignature`.
2. WHILE `Summary_Enabled` is true, WHEN the `Goal_Tool_Trigger` fires, THE `Summary_Provider` SHALL order candidate memories by the `Retrieval` score so that stronger structured matches are summarized before weaker ones.
3. WHILE `Summary_Enabled` is true, WHERE a configured maximum summary count is set, THE `Summary_Provider` SHALL return at most that many `Goal_Summary` values.

### Requirement 12: Compact goal summaries

**User Story:** As the agent, I want compact summaries of what similar goals did and concluded, so that I get "similar goals did X, Y, Z and reached these conclusions."

#### Acceptance Criteria

1. WHEN the `Summary_Provider` produces a `Goal_Summary` for a retrieved memory, THE `Goal_Summary` SHALL describe what the goal did, drawn from the memory's `IntentSignature` and `Plan`.
2. WHEN the `Summary_Provider` produces a `Goal_Summary` for a retrieved memory, THE `Goal_Summary` SHALL describe the conclusion or outcome reached, drawn from the memory's `MemoryKind` and outcome information.
3. WHEN the `Summary_Provider` produces a `Goal_Summary` for a `Negative` memory, THE `Goal_Summary` SHALL indicate that the approach was a learned dead-end.
4. WHEN the `Summary_Provider` produces a `Goal_Summary`, THE `Goal_Summary` SHALL exclude concrete evidence values and raw transcripts, carrying only the distilled plan and outcome description.

### Requirement 13: Opt-in and default-off wiring

**User Story:** As a halter operator, I want similar-goal summaries to be off by default and configurable, so that the feature does not change behavior unless enabled.

#### Acceptance Criteria

1. WHERE `Summary_Enabled` is not configured, THE `Summary_Provider` SHALL default to disabled.
2. WHILE `Summary_Enabled` is false, WHEN the `Goal_Tool_Trigger` fires, THE `Summary_Provider` SHALL not invoke `Retrieval` on behalf of summarization and SHALL produce no freshly retrieved `Goal_Summary` values, though a separately configured source (for example a cache or fallback provider) MAY still supply `Goal_Summary` values.
3. WHILE `Summary_Enabled` is false, WHEN the `Goal_Tool_Trigger` fires, THE `Goal_Tool_Trigger` SHALL return the same replay decision it returns when the `Summary_Provider` is absent.

### Requirement 14: Summaries surfaced alongside the replay decision

**User Story:** As an integrator, I want summaries returned in addition to the existing replay decision, so that summarization is additive and does not disturb the hot path.

#### Acceptance Criteria

1. WHILE `Summary_Enabled` is true, WHEN the `Goal_Tool_Trigger` fires, THE `Goal_Tool_Trigger` SHALL return the produced `Goal_Summary` values together with the existing replay decision for the top candidate.
2. WHILE `Summary_Enabled` is true, WHEN `Retrieval` returns no candidate memories, THE `Summary_Provider` SHALL return an empty set of `Goal_Summary` values and THE `Goal_Tool_Trigger` SHALL return the from-scratch replay decision.
3. THE `Summary_Provider` SHALL derive `Goal_Summary` values without mutating any persisted `Memory_Record`.

### Requirement 15: Summary resilience

**User Story:** As a runtime engineer, I want summarization failures to be non-fatal, so that the goal-tool hot path never breaks because of summaries.

#### Acceptance Criteria

1. IF obtaining or producing `Goal_Summary` values fails while `Summary_Enabled` is true, THEN THE `Goal_Tool_Trigger` SHALL return the existing replay decision with no `Goal_Summary` values rather than propagating the failure.
2. WHILE `Summary_Enabled` is true and the embedding backend used by `Retrieval` is unavailable, THE `Summary_Provider` SHALL produce `Goal_Summary` values from the structured-head candidates without error (structured-head fallback always succeeds).
