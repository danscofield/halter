# Requirements Document

## Introduction

This feature originally set out to perform a single end-to-end wiring job: connect
the built-but-unwired Tier 2 procedural-memory components into the `halter`
runtime, including a Tier 1 "is this cached evidence still fresh?" replay-and-
verify apparatus. That verification tier has been **removed from scope**. The
user rejected predicting whether a tool call reproduces its result: there is no
evidence validator, no source resolution, no content-hashing, no replay
verification, and no "verified vs believed-unverified" answer modes in this
feature.

What remains is **two distinct capabilities**, specified separately below.

**Capability A — Tier 2 retrieval with advisory context injection.** When a goal
starts, the runtime runs structured-first memory retrieval and injects a short
**advisory** summary of applicable past memories into the agent's context. The
injection is advisory only: the runtime never acts on it, verifies it, or serves
a cached answer in place of running the goal. It exists solely to inform the
agent. This reuses the already-built Tier 2 embedding, memory-store, retrieval,
and induction components (`OpenAiEmbeddingClient`, `OpenAiEmbeddingSource`,
`MemoryEmbeddingWriter`, `ResolvedEmbeddingSettings`, `InMemoryMemoryStore`,
`SqliteMemoryStore`, `MemoryRetrieval`, `InductionEngine`,
`EngineInductionQueue`, `insert_memory_with_writer`), none of which is
constructed by `HalterBuilder::build` today, and wires them into the goal hot
path — **minus** any Tier 1 validation. Retrieval surfaces applicable memories
and their summaries; it does not serve or verify cached answers.

**Capability B — TTL tool-result cache.** A time-bounded cache of tool-call
**results** (not conclusions). When a tool call repeats within a configured TTL
window, the runtime returns the cached result and skips re-invoking the tool.
The rationale is cost: many tool results consume downstream tokens and latency,
so short-circuiting an identical call within a short window avoids re-running
expensive tools. This is a plain time-window cache — it does not re-read or
re-verify sources, and it stores results, not derived conclusions. A served
result is tagged with a cache-hit indicator so the model can see it came from
cache. Only side-effect-free tool calls are cached.

The two capabilities share a runtime (both live under `[context].goal_tracking`
and the tool-execution path respectively) but are otherwise independent: either
may be enabled without the other, and neither reintroduces the removed
verification tier.

Capability A is constrained by three established invariants that MUST be
preserved: the synchronous `MemoryStore` / asynchronous embedding split (produce
embeddings async up-front, outside any store lock, then hand the finished
`Embedding` to the synchronous insert); backward compatibility (with defaults,
runtime behavior is byte-identical to today and issues no new network calls); and
end-to-end graceful degradation (disabled embedding, missing credential, or an
unavailable backend never breaks harness build or goal execution).

This spec covers only the wiring of existing components into the runtime
(Capability A) and the new TTL cache (Capability B). It does not change the
retrieval algorithm, the `MemoryStore` trait contract, the `Memory`/
`MemoryRecord` model, `validate_memory`, the embedding request/response codec, or
the credential-resolution precedence — those are owned by the prior specs and are
treated as ground truth here. For Capability B, the argument-normalization
canonicalization (`normalize_args`) is likewise reused as ground truth.

## Glossary

- **Halter_Builder**: The runtime assembler `HalterBuilder` in
  `crates/halter/src/builder.rs` whose `build` method validates configuration and
  constructs the `Halter` runtime, its providers, stores, tools, and
  `RuntimeServices`. It is the System under specification for all wiring.
- **Tier2_Retrieval_Path**: The end-to-end runtime capability enabled by
  Capability A: a constructed Memory_Store, Memory_Retrieval, embedding
  source/writer, Induction_Engine, and Engine_Induction_Queue, reachable from the
  goal hot path, producing advisory context injection.
- **Advisory_Injection**: The act of folding retrieved applicable memories and
  their summaries into the `goal(action=create)` tool result the agent reads on
  its next turn. It is informational only; the runtime takes no action on it,
  verifies nothing, and serves no cached answer in its place.
- **OpenAI_Embedding_Client**: The async OpenAI embeddings transport
  `OpenAiEmbeddingClient` in `halter-providers`, constructed with a `SecretString`
  bearer and a resolved base URL.
- **OpenAI_Embedding_Source**: The query-path `EmbeddingSource` implementation
  `OpenAiEmbeddingSource<C>` in `halter-goals::tier2::embedding`, built from an
  OpenAI_Embedding_Client and Resolved_Embedding_Settings.
- **Memory_Embedding_Writer**: The write-path embedding producer
  `MemoryEmbeddingWriter<C>` in `halter-goals::tier2::embedding`, built from the
  same Resolved_Embedding_Settings as the OpenAI_Embedding_Source.
- **Resolved_Embedding_Settings**: The runtime-ready settings
  `ResolvedEmbeddingSettings` that bridge `EmbeddingConfig` and resolved OpenAI
  provider auth (including the `SecretString` bearer, model, dimension, and
  retry/timeout bounds) into a single source of truth shared by the source and the
  writer.
- **Embedding_Config**: The `[embedding]` block on `HarnessConfig`
  (`EmbeddingConfig`), including its `enabled` flag, already parsed and validated
  by the runtime today.
- **Embedding_Enabled**: The state where `Embedding_Config.enabled` is true and a
  usable OpenAI credential resolves.
- **Memory_Store**: A constructed implementation of the synchronous `MemoryStore`
  trait — either `InMemoryMemoryStore` or `SqliteMemoryStore` — with interior
  mutability and `&self` methods (`insert`, `insert_with_embedding`, `filter`,
  `ann_recall`).
- **Memory_Retrieval**: The async structured-first retrieval engine
  `MemoryRetrieval` (`Retrieval::retrieve_memories`) built over a Memory_Store and
  an `EmbeddingSource`.
- **Retrieval_Outcome**: The applicable-memory set and any similar-goal summaries
  that Memory_Retrieval produces for a newly-started goal; the payload of
  Advisory_Injection. It contains no verification result and no served answer.
- **Induction_Engine**: The async memory author `InductionEngine` whose
  `induce_memory` runs the gate -> Judge -> Author -> dedup/insert pipeline and
  currently inserts empty embeddings.
- **Engine_Induction_Queue**: The `EngineInductionQueue` seam in
  `halter-goals::integration` that, as an `InductionQueue`, resolves a closed goal
  node from the Goal_Store and runs Induction_Engine on a spawned task off the hot
  path.
- **Insert_With_Writer**: The write-path integration function
  `insert_memory_with_writer(writer, source_dimension, store, key, mem)` that
  checks writer/source dimension consistency, produces the embedding async
  up-front, and then calls the synchronous `insert_with_embedding`.
- **Goal_Store**: The runtime goal store the Halter_Builder already constructs
  under `goal_tracking = auto` (`EventLogGoalStore` over
  `SessionStoreGoalEventLog`).
- **Goal_Tool**: The built-in `GoalTool` that handles `goal(action=create|
  resolve|...)`; its `create_action` handler is the Goal_Hot_Path seam into which
  retrieval and Advisory_Injection are wired.
- **Goal_Tracking_Mode**: The `[context].goal_tracking` config
  (`GoalTrackingMode`), either `auto` or `off` (default `off`).
- **Session_Backend**: The `[sessions].backend` config (`SessionBackend`), either
  `memory` or `sqlite` (default `memory`), that already selects the session store
  implementation.
- **Goal_Hot_Path**: The runtime path taken when a new goal starts and the
  Goal_Tool fires; the seam into which retrieval is wired.
- **None_Degradation_Contract**: The established behavior whereby
  `EmbeddingSource::embed` returning `None` causes retrieval to skip tail recall
  and return the structured-head-only candidate set without error, and a
  write-time unavailable embedding stores an empty `Embedding` while the write
  still succeeds.
- **Store_Lock**: The internal synchronization a Memory_Store holds during a
  synchronous read or write; embeddings MUST be produced outside it.
- **Tool_Runtime**: The tool registry and dispatcher `ToolRuntime` in
  `crates/halter-tools/src/runtime.rs` whose `execute(name, context, input)`
  method dispatches a tool call; the seam into which the Tool_Result_Cache is
  interposed.
- **Tool_Result**: The `ToolResult` enum in `halter-protocol`
  (`enum ToolResult { Empty, Text { text }, Json { value } }`, tagged
  `#[serde(tag = "kind")]`) returned by a tool execution and stored in session
  history.
- **Tool_Result_Cache**: The Capability B cache that maps a Cache_Key to a stored
  Tool_Result plus the timestamp it was stored, and serves a stored result within
  the configured TTL window.
- **Cache_Key**: The lookup key for the Tool_Result_Cache: the tool name combined
  with the Normalized_Args of the call.
- **Normalized_Args**: The canonical, byte-stable argument form produced by the
  existing `normalize_args` canonicalization, so that logically-equal calls key to
  the same Tool_Result_Cache entry.
- **TTL**: The time-to-live window (in seconds) during which a stored Tool_Result
  may be served; a single global default read from configuration.
- **Cacheable_Tool**: A tool whose calls are eligible for the Tool_Result_Cache
  because they are side-effect-free (read-only / pure), as determined by the
  cacheability policy in Requirement 15.
- **Cache_Hit_Indicator**: The optional metadata added to Tool_Result (Option B —
  an extension of the Tool_Result envelope, not a field folded into the JSON
  payload) that marks whether a returned result was served from the
  Tool_Result_Cache.
- **Tool_Cache_Config**: The configuration governing the Tool_Result_Cache: an
  enable/disable flag and a single global default TTL (`tool_cache_ttl_secs`),
  living on the `[tools]` configuration block.

## Requirements

<!-- ================================================================= -->
<!-- Capability A: Tier 2 retrieval + advisory injection + induction   -->
<!-- ================================================================= -->

### Requirement 1: Backward-compatible default behavior

**User Story:** As a halter operator running with default configuration, I want
the runtime to behave exactly as it does today, so that enabling neither goal
tracking nor embedding introduces no new behavior or network traffic.

#### Acceptance Criteria

1. WHERE Goal_Tracking_Mode is `off`, THE Halter_Builder SHALL construct no
   Memory_Store, no Memory_Retrieval, no OpenAI_Embedding_Source, no
   Memory_Embedding_Writer, no Induction_Engine, and no Engine_Induction_Queue.
2. WHILE Goal_Tracking_Mode is `off`, WHEN a goal starts, THE Halter_Builder's
   runtime SHALL perform no memory retrieval, no Advisory_Injection, and no
   embedding backend request.
3. WHERE Goal_Tracking_Mode is `off`, THE Halter_Builder SHALL produce a runtime
   whose observable goal-tracking, tool-registration, and session behavior is
   identical to the behavior produced before the Tier2_Retrieval_Path was
   introduced.
4. WHERE the Tier2_Retrieval_Path is not enabled, THE Halter_Builder SHALL
   complete `build` fully constructing the rest of the runtime without issuing any
   embedding backend network request, so that disabling the Tier2_Retrieval_Path
   never leaves the runtime partially built.

### Requirement 2: Tier 2 retrieval path gating

**User Story:** As a halter operator, I want the Tier 2 retrieval path to activate
only under an explicit configuration, so that the feature is opt-in and its
enablement is predictable.

#### Acceptance Criteria

1. WHERE Goal_Tracking_Mode is `auto`, THE Halter_Builder SHALL always construct a
   Memory_Store, a Memory_Retrieval over that Memory_Store, and wire retrieval into
   the Goal_Hot_Path, regardless of Embedding_Config.
2. WHERE Goal_Tracking_Mode is `off`, THE Halter_Builder SHALL wire no retrieval
   into the Goal_Hot_Path and SHALL pre-wire no Tier2_Retrieval_Path component that
   would only be used if Goal_Tracking_Mode later changes, running no background,
   cached, or deferred retrieval, regardless of Embedding_Config.
3. WHERE Goal_Tracking_Mode is `auto` AND Embedding_Enabled is true, THE
   Halter_Builder SHALL build Memory_Retrieval with an OpenAI_Embedding_Source as
   its embedding source.
4. WHERE Goal_Tracking_Mode is `auto` AND Embedding_Enabled is false, THE
   Halter_Builder SHALL build Memory_Retrieval with an embedding source that
   returns `None` from every embed request, so retrieval degrades to
   structured-head-only.
5. THE Halter_Builder SHALL determine Tier2_Retrieval_Path enablement solely from
   Goal_Tracking_Mode, Embedding_Config, and Session_Backend, deriving no
   enablement from any other configuration field.
6. WHERE the Tier2_Retrieval_Path is enabled, THE Halter_Builder SHALL require
   Goal_Tracking_Mode to be `auto`, so that Goal_Tracking_Mode `off` overrides all
   other configuration and prevents construction of every Tier2_Retrieval_Path
   component.

### Requirement 3: Constructing the embedding source and client

**User Story:** As a halter operator, I want the runtime to build the OpenAI
embedding source and client from my existing configuration, so that tail recall
uses real embeddings without additional setup.

#### Acceptance Criteria

1. WHERE Goal_Tracking_Mode is `auto` AND Embedding_Enabled is true, THE
   Halter_Builder SHALL construct an OpenAI_Embedding_Client using the
   `SecretString` bearer and base URL carried by Resolved_Embedding_Settings.
2. WHERE Goal_Tracking_Mode is `auto` AND Embedding_Enabled is true, THE
   Halter_Builder SHALL construct an OpenAI_Embedding_Source from that
   OpenAI_Embedding_Client and the Resolved_Embedding_Settings.
3. WHEN the Halter_Builder resolves the embedding credential, THE Halter_Builder
   SHALL derive Resolved_Embedding_Settings from Embedding_Config and the resolved
   OpenAI provider auth using the same credential resolution and precedence the
   runtime already applies to the `openai` provider.
4. IF Embedding_Config.enabled is true but no OpenAI credential resolves from any
   configured source, THEN THE Halter_Builder SHALL construct the embedding source
   in a state that returns `None` from every embed request without issuing a
   network request, and SHALL complete `build` successfully.

### Requirement 4: Constructing the memory store from the session backend

**User Story:** As a halter operator, I want the memory store to follow the same
backend selection as sessions, so that a durable deployment persists memories and
an in-memory deployment does not.

#### Acceptance Criteria

1. WHERE Goal_Tracking_Mode is `auto` AND Session_Backend is `memory`, THE
   Halter_Builder SHALL construct an in-memory Memory_Store.
2. WHERE Goal_Tracking_Mode is `auto` AND Session_Backend is `sqlite`, THE
   Halter_Builder SHALL construct a SQLite-backed Memory_Store.
3. THE Halter_Builder SHALL construct the Memory_Store as a value satisfying the
   synchronous `MemoryStore` trait with `&self` methods, so that it is shared by
   Memory_Retrieval and Induction_Engine without changing the trait contract.
4. IF the Halter_Builder cannot open or initialize a SQLite-backed Memory_Store,
   THEN THE Halter_Builder SHALL fail `build` with an error identifying the
   Memory_Store initialization failure.

### Requirement 5: Wiring retrieval into the goal hot path as advisory injection

**User Story:** As the agent, I want a short advisory summary of applicable past
memories injected into my context when I start a new goal, so that past memories
inform me without the runtime acting on them.

#### Acceptance Criteria

1. WHILE Goal_Tracking_Mode is `auto`, WHEN a new goal starts on the Goal_Hot_Path,
   THE runtime SHALL invoke Memory_Retrieval with the new goal's `IntentSignature`
   over the constructed Memory_Store.
2. WHEN retrieval runs, THE runtime SHALL perform the structured applicability
   filter over the Memory_Store first and SHALL request a query embedding only when
   the structured head is thin, preserving the existing retrieval algorithm and
   head/tail bounds.
3. WHEN retrieval produces a Retrieval_Outcome, THE runtime SHALL fold the
   applicable memories and their summaries into the `goal(action=create)`
   Tool_Result the agent reads on its next turn as an Advisory_Injection.
4. THE runtime SHALL treat the Advisory_Injection as informational only, taking no
   runtime action on it, verifying no memory, and serving no cached answer in place
   of running the goal.
5. WHEN retrieval produces no applicable memory, THE runtime SHALL proceed with the
   goal, injecting an empty advisory context and running the goal as if no memory
   existed, without error.
6. THE Halter_Builder SHALL wire retrieval such that the retrieval algorithm,
   ordering, deduplication, and head/tail bounds are unchanged from the values the
   prior specs established.

### Requirement 6: Wiring the writer into the induction insert path

**User Story:** As a Tier 2 maintainer, I want induced memories to store real
embeddings, so that tail-recall ranking becomes semantically meaningful.

#### Acceptance Criteria

1. WHERE the Tier2_Retrieval_Path is enabled AND Embedding_Enabled is true, WHEN
   Induction_Engine authors a new memory for insertion, THE runtime SHALL insert
   that memory through Insert_With_Writer using the constructed
   Memory_Embedding_Writer.
2. WHEN Insert_With_Writer produces a usable embedding of the configured dimension,
   THE runtime SHALL store that embedding on the memory's `MemoryRecord` via the
   synchronous `insert_with_embedding`.
3. IF Insert_With_Writer reports the embedding backend unavailable, THEN THE runtime
   SHALL store an empty `Embedding` for the memory, complete the insertion
   successfully, and leave the memory retrievable through the structured filter,
   preserving the None_Degradation_Contract.
4. WHERE Embedding_Enabled is false, WHEN Induction_Engine authors a new memory, THE
   runtime SHALL complete the insertion storing an empty `Embedding` without issuing
   any embedding backend request.

### Requirement 7: Wiring closure-driven induction off the hot path

**User Story:** As the agent, I want closed goals to feed memory induction without
blocking goal completion, so that learning happens without slowing the hot path.

#### Acceptance Criteria

1. WHERE Goal_Tracking_Mode is `auto` AND the Tier2_Retrieval_Path is enabled, THE
   Halter_Builder SHALL construct an Engine_Induction_Queue wiring the Goal_Store to
   an Induction_Engine over the constructed Memory_Store.
2. WHEN a goal closes and the Engine_Induction_Queue receives the closure signal,
   THE runtime SHALL run Induction_Engine on a spawned task so that goal closure
   returns to its caller before induction runs.
3. WHERE Goal_Tracking_Mode is `off`, THE Halter_Builder SHALL construct no
   Engine_Induction_Queue and no other Tier2_Retrieval_Path component, with
   Goal_Tracking_Mode taking precedence over any Tier2_Retrieval_Path notion, so
   that a `build` performing a single synchronous construction with no pre-queued
   or deferred component leaves no Tier2_Retrieval_Path component constructed.

### Requirement 8: Dimension consistency at wiring time

**User Story:** As a Tier 2 maintainer, I want the write-path writer and the
query-path source to agree on dimension when the runtime wires them, so that ANN
cosine distance stays meaningful.

#### Acceptance Criteria

1. WHEN the Halter_Builder constructs the Memory_Embedding_Writer and the
   OpenAI_Embedding_Source, THE Halter_Builder SHALL derive both from a single
   Resolved_Embedding_Settings value so their configured model and dimension are
   equal.
2. WHEN the runtime inserts a memory through Insert_With_Writer, THE runtime SHALL
   pass the source's configured dimension as the `source_dimension` argument so
   Insert_With_Writer verifies writer/source dimension consistency before writing.
3. IF the Memory_Embedding_Writer dimension is not equal to the
   OpenAI_Embedding_Source dimension at an insertion, THEN THE runtime SHALL reject
   that insertion with the matching dimension-misconfiguration error and SHALL leave
   any previously stored embedding for that memory unchanged.

### Requirement 9: Preserving the synchronous-store / asynchronous-embedding split

**User Story:** As a runtime engineer, I want embeddings produced outside the store
lock, so that the synchronous store contract is preserved and no sync-over-async
bridge is introduced.

#### Acceptance Criteria

1. WHEN the runtime writes a memory with a real embedding, THE runtime SHALL produce
   the embedding by awaiting the Memory_Embedding_Writer before acquiring the
   Store_Lock, and SHALL pass the finished `Embedding` into the synchronous
   `insert_with_embedding`.
2. THE runtime SHALL produce every embedding backend request outside any held
   Store_Lock.
3. THE Halter_Builder SHALL wire Memory_Store reads and writes such that `filter`,
   `ann_recall`, `insert`, and `insert_with_embedding` remain synchronous and only
   the embedding production and Memory_Retrieval entry points are asynchronous.
4. THE Halter_Builder SHALL connect embedding production to the synchronous
   Memory_Store using only the async-up-front / sync-insert split, introducing no
   synchronous-over-asynchronous bridge.

### Requirement 10: End-to-end graceful degradation

**User Story:** As a halter operator, I want goal execution to continue when the
embedding path is unavailable, so that missing credentials or a down backend never
break the runtime.

#### Acceptance Criteria

1. IF Embedding_Config.enabled is false, THEN THE Halter_Builder SHALL complete
   `build` successfully with respect to embedding in all cases and the runtime SHALL
   run goals using structured-head-only retrieval without issuing embedding backend
   requests.
2. IF no OpenAI credential resolves while Embedding_Config.enabled is true, THEN THE
   Halter_Builder SHALL complete `build` successfully with respect to the embedding
   credential and the runtime SHALL run goals using structured-head-only retrieval,
   while the Halter_Builder MAY still fail `build` for non-embedding reasons already
   specified, including Memory_Store initialization failure per Requirement 4.4 and
   Embedding_Config validation failure per Requirement 12.
3. IF the embedding backend is unavailable during retrieval, THEN THE runtime SHALL
   inject the structured-head-only Retrieval_Outcome for that goal without
   propagating an error to the Goal_Hot_Path.
4. IF the embedding backend is unavailable during induction, THEN THE runtime SHALL
   store an empty `Embedding` for the induced memory and complete the insertion
   successfully.
5. WHILE the Tier2_Retrieval_Path is enabled and the embedding path is unavailable,
   WHEN goals start and close, THE runtime SHALL continue to serve goal execution and
   induction without failing the goal.

### Requirement 11: Secret handling for the embedding credential

**User Story:** As a security-conscious operator, I want the embedding credential
handled like every other provider secret, so that it is never exposed in logs or
errors.

#### Acceptance Criteria

1. THE Halter_Builder SHALL carry the resolved embedding credential as a
   `SecretString` bearer within Resolved_Embedding_Settings and SHALL confine the
   credential value to representations that redact it.
2. THE Halter_Builder SHALL exclude the embedding credential value from every log
   message and every error value it produces while constructing the
   Tier2_Retrieval_Path.
3. WHEN the Halter_Builder logs the construction of the Tier2_Retrieval_Path, THE
   Halter_Builder SHALL log only non-secret attributes such as whether embedding is
   enabled and the selected Memory_Store backend.

### Requirement 12: Configuration validation and reporting

**User Story:** As a halter operator, I want misconfiguration surfaced at build
time, so that an invalid Tier 2 setup fails fast with a clear message.

#### Acceptance Criteria

1. WHEN the Halter_Builder validates configuration, THE Halter_Builder SHALL apply
   the existing Embedding_Config validation before constructing any
   Tier2_Retrieval_Path component.
2. IF Embedding_Config validation fails, THEN THE Halter_Builder SHALL fail `build`
   immediately with the existing configuration error identifying the rejected field,
   before constructing any Tier2_Retrieval_Path component, so that no partial
   Tier2_Retrieval_Path construction exists to unwind or clean up.
3. WHERE Goal_Tracking_Mode is `auto` AND Embedding_Config.enabled is false, THE
   Halter_Builder SHALL emit a diagnostic log entry indicating that the
   Tier2_Retrieval_Path runs with structured-head-only retrieval.

<!-- ================================================================= -->
<!-- Capability B: TTL tool-result cache                               -->
<!-- ================================================================= -->

### Requirement 13: Tool-result cache configuration

**User Story:** As a halter operator, I want to enable and tune a tool-result
cache from configuration, so that repeated tool calls can be short-circuited on a
predictable time window.

#### Acceptance Criteria

1. THE Halter_Builder SHALL read a Tool_Cache_Config from the `[tools]`
   configuration block containing an enable/disable flag and a single global
   default TTL field `tool_cache_ttl_secs`.
2. WHERE Tool_Cache_Config omits the enable flag, THE Halter_Builder SHALL default
   the Tool_Result_Cache to disabled, so that existing configurations retain
   today's behavior.
3. WHERE Tool_Cache_Config omits `tool_cache_ttl_secs`, THE Halter_Builder SHALL
   apply a fixed default TTL defined by the runtime.
4. WHERE the Tool_Result_Cache is disabled, THE Tool_Runtime SHALL execute every
   tool call by invoking the tool, reading and writing no Tool_Result_Cache entry
   and adding no Cache_Hit_Indicator, so that behavior is identical to today.
5. THE Halter_Builder SHALL apply one global TTL from `tool_cache_ttl_secs` to
   every Cacheable_Tool, applying no per-tool TTL.

### Requirement 14: Tool-result cache keying, hit, miss, and expiry

**User Story:** As the agent, I want an identical repeated tool call within the TTL
window to return the previous result without re-running the tool, so that expensive
tool calls are not needlessly repeated.

#### Acceptance Criteria

1. WHEN the Tool_Runtime is about to execute a Cacheable_Tool call, THE Tool_Runtime
   SHALL compute a Cache_Key from the tool name and the Normalized_Args of the call,
   reusing the existing `normalize_args` canonicalization so logically-equal calls
   key to the same entry.
2. IF `normalize_args` reports the arguments non-canonicalizable, THEN THE
   Tool_Runtime SHALL invoke the tool directly, reading and writing no
   Tool_Result_Cache entry for the call.
3. WHEN a Cache_Key has a Tool_Result_Cache entry whose stored timestamp is within
   the TTL window relative to the current time, THE Tool_Runtime SHALL return the
   stored Tool_Result without invoking the tool and SHALL mark the returned result
   with a Cache_Hit_Indicator of hit.
4. WHEN a Cache_Key has no Tool_Result_Cache entry, THE Tool_Runtime SHALL invoke
   the tool, store the resulting Tool_Result under the Cache_Key with the current
   timestamp, and return the result without a Cache_Hit_Indicator of hit.
5. IF a Cache_Key has a Tool_Result_Cache entry whose stored timestamp is outside
   the TTL window relative to the current time, OR whose entry is otherwise
   non-servable (a stale timestamp or any other unusable state), THEN THE
   Tool_Runtime SHALL treat the entry as a miss, SHALL NOT return the stored result,
   SHALL invoke the tool, and SHALL store the fresh Tool_Result under the Cache_Key
   with the current timestamp.
6. WHEN the Tool_Runtime invokes a tool because of a miss or an expired entry, THE
   Tool_Runtime SHALL return the freshly-executed result marked as not served from
   cache.
7. IF the Tool_Runtime invokes a Cacheable_Tool because of a miss or an expired
   entry AND the invocation returns an error rather than a Tool_Result, THEN THE
   Tool_Runtime SHALL handle the error gracefully using the existing tool-error
   semantics per Requirement 17.2 and SHALL store no Tool_Result_Cache entry for
   that call.

### Requirement 15: Cacheable-tool determination

**User Story:** As a halter operator, I want only side-effect-free tool calls to be
cached, so that caching never suppresses a tool that mutates state or has other side
effects.

#### Acceptance Criteria

1. THE Tool_Runtime SHALL require a tool to be side-effect-free as a necessary but
   not sufficient condition for being a Cacheable_Tool, SHALL treat every tool that
   is not side-effect-free as not cacheable, and MAY classify a side-effect-free
   tool as not cacheable.
2. WHEN the Tool_Runtime evaluates a tool for cacheability, THE Tool_Runtime SHALL
   base the determination on the tool's declared specification (its non-mutating,
   read-only `ToolConcurrency`/`ToolCapabilities` classification) rather than on the
   tool's runtime behavior.
3. WHERE a tool is not a Cacheable_Tool, THE Tool_Runtime SHALL invoke the tool on
   every call, reading and writing no Tool_Result_Cache entry and adding no
   Cache_Hit_Indicator of hit, so that write, edit, shell, process, pty, and subagent
   tools are never served from cache.
4. WHERE a tool is a Cacheable_Tool, THE Tool_Runtime SHALL apply the hit, miss, and
   expiry behavior of Requirement 14 to that tool's calls.

### Requirement 16: Cache-hit metadata on the tool result envelope

**User Story:** As the agent, I want a served-from-cache result to be visibly
tagged, so that the model can tell a cached result from a freshly-executed one.

#### Acceptance Criteria

1. THE Tool_Result type SHALL carry an optional Cache_Hit_Indicator extending the
   Tool_Result envelope (Option B), separate from the JSON payload of a
   `Json`-variant result.
2. WHEN the Tool_Runtime serves a result from the Tool_Result_Cache within the TTL
   window, THE Tool_Runtime SHALL set the Cache_Hit_Indicator of the returned
   Tool_Result to hit.
3. WHEN the Tool_Runtime returns a freshly-executed result, THE Tool_Runtime SHALL
   set the Cache_Hit_Indicator to not-hit or leave it absent.
4. WHERE the Cache_Hit_Indicator is absent from a serialized Tool_Result, THE
   runtime SHALL deserialize that Tool_Result as not served from cache, so that
   Tool_Result values serialized before this feature remain valid.
5. THE Tool_Result envelope extension SHALL be optional and defaulted so that
   existing `ToolResult` construction sites and match arms
   (`Empty`/`Text`/`Json`) remain valid without carrying the Cache_Hit_Indicator.
6. WHEN a Tool_Result carrying a Cache_Hit_Indicator is written to and read back
   from session history, THE runtime SHALL round-trip the Tool_Result including its
   Cache_Hit_Indicator without loss.

### Requirement 17: Tool-result cache interaction with the execution path

**User Story:** As a runtime engineer, I want the cache interposed cleanly on the
tool execution path, so that caching adds no behavior beyond short-circuiting
eligible calls.

#### Acceptance Criteria

1. THE Tool_Result_Cache SHALL interpose on the Tool_Runtime execution path so that
   a cache hit returns before the tool's `execute` method is invoked.
2. IF a Cacheable_Tool invocation returns an error rather than a Tool_Result, THEN
   THE Tool_Runtime SHALL propagate the error and SHALL store no Tool_Result_Cache
   entry for that call.
3. WHERE the Tool_Result_Cache is enabled, THE Tool_Runtime SHALL preserve the
   existing dispatch, ordering, and error semantics for every non-cacheable tool and
   for every cache miss.

## Open Questions

All six open questions are **RESOLVED** by the design document; the decisions are
recorded here and specified in full (with rationale) in `design.md`.

1. **Cacheable-tool policy. — RESOLVED.** Derive cacheability from the existing
   `ToolSpec` fields **and** an explicit opt-in. A tool is a Cacheable_Tool iff
   `capabilities.cacheable == true` (new `#[serde(default)] bool`, default
   `false`) **and** `capabilities.mutating == false` **and** `concurrency ∈
   {ReadOnly, ParallelSafe}`. The mutating/concurrency conjunction is a hard
   safety floor (write/edit/shell/process/pty/subagent can never be cached even if
   mis-flagged); the `cacheable` opt-in makes caching a deliberate per-tool
   assertion, because `ToolConcurrency` is a scheduler hint, not a statement about
   temporal result stability. See design §6.
2. **Where the cache lives. — RESOLVED.** Owned by `ToolRuntime` and interposed
   inside `ToolRuntime::execute`, so every caller of `execute` benefits and a hit
   returns before `tool.execute`. `ToolContext` already carries `session_id`. See
   design §7.
3. **Cache scope. — RESOLVED: per-session.** `Cache_Key` includes `SessionId` to
   prevent cross-session result bleed for context-dependent reads. Global reuse is
   given up as low-value and unsafe. See design §7.
4. **Eviction and size bound. — RESOLVED: TTL-only for v1** plus a per-session
   entry cap enforced by a lazy expiry sweep with oldest-eviction, to bound memory
   without a full eviction policy. See design §7 / Data Models.
5. **Wire-compatibility of the Tool_Result change. — RESOLVED: no migration or
   version bump.** `ToolResult` becomes `{ #[serde(flatten)] kind: ToolResultKind,
   #[serde(default, skip_serializing_if = "Option::is_none")] cache_hit:
   Option<bool> }`. The `kind`/`text`/`value` on-wire shape is unchanged and
   `cache_hit` is additive/defaulted/skip-when-absent, so pre-feature bytes
   deserialize as not-from-cache. See design §7 / §8.
6. **Memory store persistence location. — RESOLVED: confirmed.** When
   Session_Backend is `sqlite`, the Memory_Store opens a distinct sibling file
   `memory.sqlite3` in the session database's directory, never the session DB. See
   design §4.
