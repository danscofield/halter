# Implementation Plan: Goal Pace — hit your goals faster

## Overview

This plan builds the `halter-goals` crate implementing three owned subsystems: the Goal
Model (goal tree, resolution lifecycle, `subtree_hash`, `IntentSignature`, persistence on the
existing event-sourced session store, and the goal-closure hook), Tier 1 (argument
normalization, the Validity Token Service, and the exact-match evidence cache), and Tier 2
(induction, memory store/retrieval, replay, and two-mode answer caching).

The implementation is incremental and bottom-up: shared types first, then Tier 1 (which
issues the tokens both other subsystems depend on), then the Goal Model (which persists on
the session store and fires closure), then Tier 2 (which consumes Goal Model intent
signatures and Tier 1 tokens), and finally the wiring that connects goal closure to
induction. All code is Rust, matching the `halter` workspace, and reuses `halter-protocol`
`fold`, `halter-session` (`SqliteSessionStore` / `InMemorySessionStore`), and `halter-hooks`.

Property tests use `proptest` (already a workspace dependency) and encode the 20 Correctness
Properties from the design.

## Tasks

- [ ] 1. Scaffold the `halter-goals` crate and shared type foundations
  - [ ] 1.1 Create the crate and wire it into the workspace
    - Add `crates/halter-goals` with `Cargo.toml` depending on `halter-protocol`,
      `halter-session`, `halter-hooks`, `serde`, `sha2`, `tokio`, `async-trait`, and
      `proptest` (dev-dependency)
    - Register the crate in the workspace root `Cargo.toml` members list
    - Create `src/lib.rs` exposing empty `goal_model`, `tier1`, and `tier2` modules
    - _Requirements: 1.6_

  - [ ] 1.2 Define shared identifier and value types
    - Define `GoalNodeId`, `SubtreeHash`, `MemoryId`, `IntentType`, `TargetType`,
      `TargetRef`, `Scope`, `ToolName`, `CanonicalJson`, `EvidenceValue`, `Timestamp`,
      `Duration`, `EventKey`, `EventSeq`, and `Sha256` newtypes with `serde` derives
    - Define the `IntentSignature` struct with `intent_type`, `target_type`, `target_ref`,
      and `scope`
    - _Requirements: 7.1, 8.1_

  - [ ] 1.3 Define the `ValidityToken`, `SourceDescriptor`, and `ToolCall` types
    - Define `ValidityToken` enum (`ContentHash`, `Ttl`, `EventDriven`) and
      `SourceDescriptor` enum (`Pinnable`, `Volatile`, `Signalled`)
    - Define `ToolCall` with `tool`, `normalized_args`, `validity_token`, and `outcome`
    - _Requirements: 9.4, 8.1_

- [ ] 2. Implement Tier 1 argument normalization
  - [ ] 2.1 Implement `normalize_args`
    - Write the pure, total canonicalization function producing byte-stable `CanonicalJson`
      (sorted object keys, whitespace stripped, scalar encodings folded, defaults explicit)
    - Return a canonicalization error for arguments that cannot be canonicalized for the tool
    - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.6_

  - [ ]* 2.2 Write property test for normalization determinism
    - **Property 5: Normalization determinism / exact-match determinism**
    - **Validates: Requirements 8.1, 8.2, 8.3, 8.4**

  - [ ]* 2.3 Write unit tests for normalization error path
    - Test non-canonicalizable arguments return an error and touch no cache entry
    - _Requirements: 8.6_

- [ ] 3. Implement the Tier 1 Validity Token Service
  - [ ] 3.1 Implement `issue_token`
    - Mint `ContentHash` (SHA-256 of readable content), `Ttl` (`issued_at`/`ttl`), and
      `EventDriven` (current `EventSeq` on subscription) per source volatility class
    - Reject `Pinnable` sources whose content is unreadable at issuance
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6_

  - [ ] 3.2 Implement `holds` re-validation
    - Implement per-variant checks: `ContentHash` iff current content hashes to `h`; `Ttl`
      iff `now() < issued_at + ttl`; `EventDriven` iff no event newer than `last_seen`
    - Treat an unreachable source as not held (fail-safe); never mutate token/source/evidence
    - _Requirements: 10.1, 10.2, 10.3, 10.4, 10.5_

  - [ ]* 3.3 Write property test for ContentHash validity
    - **Property 6: ContentHash holds iff content unchanged**
    - **Validates: Requirements 10.1**

  - [ ]* 3.4 Write property test for Ttl validity
    - **Property 7: Ttl holds iff within window**
    - **Validates: Requirements 10.2**

  - [ ]* 3.5 Write property test for EventDriven validity
    - **Property 8: EventDriven holds iff no newer event**
    - **Validates: Requirements 10.3**

  - [ ]* 3.6 Write unit tests for issuance freshness and unreachable/unreadable sources
    - Assert freshly issued tokens hold at issuance; unreadable `Pinnable` rejects; `holds`
      returns false on unreachable sources
    - _Requirements: 9.5, 9.6, 10.5_

- [ ] 4. Implement the Tier 1 evidence cache
  - [ ] 4.1 Define `CacheEntry`, `CacheLookup`, `Freshness` and the `Tier1Cache` trait
    - Define `CacheEntry` (tool, normalized_args, validity_token, evidence_value, stored_at),
      `CacheLookup` (`Hit`/`Stale`/`Miss`), and `Freshness` (`Fresh`/`Stale`)
    - Declare the `Tier1Cache` trait with `get`, `put`, `issue_token`, and `revalidate`
    - _Requirements: 11.1, 12.1, 13.1_

  - [ ] 4.2 Implement the cache read path (`get`)
    - Locate the entry by `tool + normalized_args`, call `holds`, and return
      `Hit`/`Stale`/`Miss`; treat unreachable source as `Stale`; never mutate stored evidence
    - _Requirements: 11.1, 11.2, 11.3, 11.4, 11.5_

  - [ ] 4.3 Implement the cache write path (`put`)
    - Store evidence under a token that holds, replacing any prior entry for the key (exactly
      one entry per key); reject writes whose token does not hold at write time
    - _Requirements: 12.1, 12.2, 12.3_

  - [ ] 4.4 Implement `revalidate`
    - Return `Fresh`/`Stale` for a recorded token without returning or mutating evidence;
      treat unreachable source as `Stale`; keep concrete evidence values owned by Tier 1
    - _Requirements: 13.1, 13.2, 13.3_

  - [ ]* 4.5 Write property test for no stale value served
    - **Property 9: No stale value served**
    - **Validates: Requirements 11.1, 11.2, 11.4, 11.5**

  - [ ]* 4.6 Write property test for write-then-read freshness
    - **Property 10: Write-then-read freshness**
    - **Validates: Requirements 12.1, 12.2**

  - [ ]* 4.7 Write unit tests for cache miss, write rejection, and revalidate
    - Test `Miss` on absent key, `put` rejection on non-fresh token, and `revalidate`
      freshness without value return
    - _Requirements: 11.3, 12.3, 13.1_

- [ ] 5. Checkpoint - Tier 1 complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 6. Implement the Goal Model data model and fold
  - [ ] 6.1 Define `GoalNode`, `Resolution`, `GoalTree`, and `GoalEvent`
    - Define `GoalNode` (id, parent, children, hypothesis, resolution_conditions, resolution,
      intent, tool_calls, subtree_hash), `Resolution` enum, `GoalTree`/`GoalTreeState`, and
      the `GoalEvent` variants (`GoalNodeCreated`, `GoalNodeRevised`, `GoalNodeResolved`,
      `GoalClosed`) with `GoalNodeRevision`
    - _Requirements: 1.2, 2.1, 4.1_

  - [ ] 6.2 Implement the goal fold
    - Implement `apply_event`-style folding: `GoalNodeCreated` inserts an `Open` node and adds
      it to its parent's children; `GoalNodeRevised` layers a delta; `GoalNodeResolved` sets
      resolution and re-opens affected descendants; `GoalClosed` marks the closure boundary
    - Fold a `GoalEvent` sequence in ascending sequence order into a `GoalTree`
    - _Requirements: 1.2, 1.3, 2.3, 2.4, 3.1_

  - [ ]* 6.3 Write property test for fold/replay determinism
    - **Property 4: Goal tree fold/replay determinism**
    - **Validates: Requirements 3.1, 3.2**

  - [ ] 6.4 Implement `subtree_hash`
    - Compute a deterministic, order-insensitive hash over a resolved node's intent-relevant
      fields (hypothesis, resolution_conditions, resolution, intent, canonical tool_calls)
      combined with each resolved child's `subtree_hash` in canonical child order
    - _Requirements: 5.1, 5.2, 5.3, 5.4, 5.5_

  - [ ]* 6.5 Write property test for subtree_hash determinism
    - **Property 1: `subtree_hash` determinism over a resolved subtree**
    - **Validates: Requirements 5.1, 5.2, 5.3, 5.4, 5.5**

- [ ] 7. Implement `IntentSignature` derivation
  - [ ] 7.1 Implement intent signature derivation and attachment
    - Derive all four fields (`intent_type`, `target_type`, `target_ref`, `scope`) on create
      and revise; reject the operation and identify unresolved fields when any cannot resolve
    - Expose each existing node's attached `IntentSignature` for Tier 2 retrieval/merge
    - _Requirements: 7.1, 7.2, 7.3_

  - [ ]* 7.2 Write unit tests for intent signature derivation
    - Test all-fields-populated success and per-field rejection with prior state retained
    - _Requirements: 7.1, 7.2_

- [ ] 8. Implement the `GoalStore` over the session store
  - [ ] 8.1 Define the `GoalStore` trait and `ClosureOutcome`, and wire session-store commit
    - Declare `GoalStore` (`create`, `revise`, `close`, `get`, `get_tree`) and
      `ClosureOutcome { closed, subtree_hash }`
    - Implement the append path that commits `GoalEvent`s through `SessionStore::commit` with
      `expected_head_sequence`, sharing one gap-free monotonic sequence-ordered log
    - _Requirements: 1.1, 3.3, 3.4_

  - [ ] 8.2 Implement `create`
    - Append exactly one `GoalNodeCreated`, return a unique node id; reject non-existent
      parent and empty hypothesis, appending no event and leaving log/tree unchanged
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6_

  - [ ] 8.3 Implement `revise`
    - Append exactly one `GoalNodeRevised` without mutating prior events; re-open affected
      subtrees; reject non-existent node with no event appended
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6_

  - [ ]* 8.4 Write property test for append-only revision semantics
    - **Property 3: Retroactive revision preserves append-only event-log semantics**
    - **Validates: Requirements 2.1, 2.2, 2.3**

  - [ ] 8.5 Implement `close` and closure detection
    - Append `GoalNodeResolved`; detect full subtree closure; append exactly one `GoalClosed`
      carrying the computed `subtree_hash` per distinct `(node_id, subtree_hash)`; return the
      correct `ClosureOutcome`; reject `Open` resolution and non-existent nodes
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5, 4.6_

  - [ ] 8.6 Implement `get` and `get_tree` projection
    - Project a single node or the whole tree by folding replayed `GoalEvent`s
    - _Requirements: 3.1, 3.2_

  - [ ]* 8.7 Write unit tests for GoalStore error paths and concurrency rejection
    - Test parent-not-found, empty-hypothesis, node-not-found, `Open`-resolution rejection,
      and `expected_head_sequence` mismatch leaving log/tree unchanged
    - _Requirements: 1.4, 1.5, 2.6, 3.4, 4.4, 4.5_

- [ ] 9. Implement the goal-closure signal and induction enqueue
  - [ ] 9.1 Dispatch the closure hook through `halter-hooks`
    - On first append of `GoalClosed` for a `(node_id, subtree_hash)`, dispatch exactly one
      closure signal carrying that key; suppress dispatch for already-recorded keys; treat a
      distinct `subtree_hash` for the same `node_id` as a new trigger
    - _Requirements: 6.1, 6.2, 6.4, 6.5_

  - [ ] 9.2 Enqueue the async induction job from the closure signal
    - Enqueue exactly one induction job per distinct `(node_id, subtree_hash)` and return
      control to the caller before induction runs (off the hot path)
    - _Requirements: 6.3, 6.4, 14.5_

  - [ ]* 9.3 Write property test for closure-triggers-induction-exactly-once
    - **Property 2: Closure triggers induction exactly once per `(node_id, subtree_hash)`**
    - **Validates: Requirements 6.1, 6.2, 6.3, 6.4, 6.5**

- [ ] 10. Checkpoint - Goal Model complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 11. Implement the Tier 2 memory model and store
  - [ ] 11.1 Define the `Memory` model and supporting types
    - Define `Memory`, `MemoryKind` (`Fragment`/`Composite`/`Negative`), `ParameterSchema`,
      `Applicability`, `Plan`, `EvidenceContract` (contract items only, no values),
      `OutcomeShape`, `CachedOutcome`, `AnswerMode`, `Provenance`, `Reinforcement`,
      `MemoryVersion`, and `MemoryRecord`
    - _Requirements: 17.1, 17.4_

  - [ ] 11.2 Implement the `MemoryStore` trait with write-time validation
    - Implement `insert`, `reinforce`, `filter`, `ann_recall`; validate `kind` presence/range,
      non-empty `evidence_contract` for `Negative`, non-empty `validity_tokens` when
      `cached_outcome` present, `SoundPinnable` requires only `ContentHash` tokens, and store
      contracts not values
    - _Requirements: 17.1, 17.2, 17.3, 17.4, 17.5, 22.1, 22.2, 22.3_

  - [ ]* 11.3 Write property test for Mode A purity
    - **Property 13: Mode A purity**
    - **Validates: Requirements 22.1, 22.2**

  - [ ]* 11.4 Write unit tests for memory-kind and evidence-contract validation
    - Test invalid/absent kind rejection, `Negative` missing evidence contract rejection, and
      empty `validity_tokens` rejection
    - _Requirements: 17.1, 17.3, 17.5, 22.3_

- [ ] 12. Implement the Tier 2 Induction Engine
  - [ ] 12.1 Implement the recurrence gate
    - Record exactly one occurrence per run before evaluating the gate; author nothing and
      skip Judge while `recurrence_count(intent) < N`; proceed to Judge when `>= N`
    - _Requirements: 14.1, 14.2, 14.3, 14.4_

  - [ ] 12.2 Implement two-step Judge-then-Author authoring in a clean context
    - Invoke Judge and Author as two distinct calls, each in a context free of the original
      goal work's messages/transcript/reasoning; Judge yields verdict+rationale and no memory;
      Author runs only on approve; persist decline rationale; handle author failure by
      inserting nothing, recording the failure, and allowing re-trigger
    - _Requirements: 15.1, 15.2, 15.3, 15.4, 15.5, 15.6_

  - [ ] 12.3 Implement idempotency, dedup, and reinforcement
    - Key on `(node_id, subtree_hash)` for at-most-one memory; reinforce on intent+plan match
      instead of inserting; serialize concurrent inductions on the idempotency key
    - _Requirements: 16.1, 16.2, 16.3_

  - [ ]* 12.4 Write property test for induction idempotency
    - **Property 14: Induction idempotency**
    - **Validates: Requirements 16.1, 16.3**

  - [ ]* 12.5 Write property test for recurrence gating
    - **Property 17: Recurrence gating**
    - **Validates: Requirements 14.2, 14.3, 14.4**

  - [ ]* 12.6 Write property test for two-step and clean-context authoring
    - **Property 15: Two-step induction**
    - **Property 16: Clean-context authoring**
    - **Validates: Requirements 15.1, 15.5**

- [ ] 13. Implement Tier 2 Retrieval
  - [ ] 13.1 Implement `retrieve_memories`
    - Apply the structured filter over all four `IntentSignature` fields first; invoke
      embedding recall only when the head is below `HEAD_MIN`, bounded by `TAIL_LIMIT`, and
      dedup the union by `MemoryId`; exclude candidates failing applicability; order by cheap
      re-rank descending; return compact memories; degrade to head-only if the backend is down
    - _Requirements: 18.1, 18.2, 18.3, 18.4, 18.5, 18.6, 18.7_

  - [ ]* 13.2 Write property test for retrieval order
    - **Property 18: Retrieval order**
    - **Validates: Requirements 18.1, 18.2, 18.3**

  - [ ]* 13.3 Write unit tests for applicability exclusion and backend-unavailable degradation
    - Test candidates failing applicability are excluded and embedding-backend outage returns
      the structured head without error
    - _Requirements: 18.4, 18.7_

- [ ] 14. Implement the Tier 2 Replay Engine
  - [ ] 14.1 Implement `decide_replay` fixed decision order
    - Evaluate the answer-cache check before the procedure-replay fallback; skip the check
      when no `cached_outcome`; fall through to replay when no servable answer
    - _Requirements: 19.1, 19.2, 19.3_

  - [ ] 14.2 Implement Mode A verified serving
    - Return `ServeAnswer { verified = true }` only when a `SoundPinnable` outcome's every
      token holds at serve time; otherwise fall through; never mark verified unless all tokens
      confirmed to hold
    - _Requirements: 20.1, 20.2, 20.3, 20.4_

  - [ ] 14.3 Implement Mode B bounded-volatile serving
    - Serve a `BoundedVolatile` answer only when the intent type opted in and age `<= max_age`,
      always marked believed-unverified; otherwise fall through; never activate Mode B by
      default
    - _Requirements: 21.1, 21.2, 21.3, 21.4, 21.5_

  - [ ] 14.4 Implement procedure-replay evidence re-validation
    - Re-validate every `evidence_contract` item through `Tier1Cache::revalidate` before
      trusting the plan; report `evidence_fully_fresh` only when all items are `Fresh`; on
      Tier 1 unavailability treat all evidence as stale, serve no verified answer, and serve a
      believed-unverified answer only under Mode B opt-in/`max_age`
    - _Requirements: 23.1, 23.2, 23.3, 23.4_

  - [ ]* 14.5 Write property test for answer soundness
    - **Property 11: Answer soundness**
    - **Validates: Requirements 20.1, 20.2, 20.3, 20.4**

  - [ ]* 14.6 Write property test for no-default-unverified serving
    - **Property 12: No default unverified**
    - **Validates: Requirements 21.1, 21.2, 21.3, 21.5**

  - [ ]* 14.7 Write property test for replay decision order
    - **Property 19: Replay decision order**
    - **Validates: Requirements 19.1, 19.2, 19.3**

  - [ ]* 14.8 Write property test for evidence re-validation on replay
    - **Property 20: Evidence re-validation on replay**
    - **Validates: Requirements 23.1, 23.2, 23.3**

- [ ] 15. Checkpoint - Tier 2 complete
  - Ensure all tests pass, ask the user if questions arise.

- [ ] 16. Integration and wiring
  - [ ] 16.1 Wire goal closure to the Tier 2 induction pipeline end to end
    - Connect the closure hook enqueue to `induce_memory` (gate -> Judge -> Author ->
      dedup/reinforce/insert) running async off the hot path
    - _Requirements: 6.3, 14.5, 15.1, 16.1_

  - [ ] 16.2 Wire retrieval and replay to Tier 1 on the hot path
    - Connect `retrieve_memories` output into `decide_replay`, calling `Tier1Cache::revalidate`
      for evidence and `holds` for answer tokens; expose the crate's public API from `lib.rs`
    - _Requirements: 13.2, 19.1, 20.1, 23.1_

  - [ ]* 16.3 Write integration test for cross-backend fold determinism
    - **Property 4: Goal tree fold/replay determinism (cross-backend)**
    - **Validates: Requirements 3.2**

  - [ ]* 16.4 Write integration test for the closure-to-induction-to-replay flow
    - Test closure enqueues induction once, a recurring goal retrieves the memory, and replay
      re-validates evidence through Tier 1
    - _Requirements: 6.3, 18.1, 23.1_

- [ ] 17. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP.
- Each task references specific requirements (granular acceptance-criteria clauses) for
  traceability, and each property test references its numbered Correctness Property.
- All code is Rust in the `halter-goals` crate; it reuses `halter-protocol` `fold`,
  `halter-session` stores, and `halter-hooks` rather than introducing new infrastructure.
- Property tests use `proptest` and encode the 20 Correctness Properties from the design.
- Checkpoints ensure incremental validation after each subsystem.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2"] },
    { "id": 2, "tasks": ["1.3", "2.1"] },
    { "id": 3, "tasks": ["2.2", "2.3", "3.1", "3.2"] },
    { "id": 4, "tasks": ["3.3", "3.4", "3.5", "3.6", "4.1"] },
    { "id": 5, "tasks": ["4.2", "4.3", "4.4"] },
    { "id": 6, "tasks": ["4.5", "4.6", "4.7", "6.1"] },
    { "id": 7, "tasks": ["6.2", "6.4"] },
    { "id": 8, "tasks": ["6.3", "6.5", "7.1"] },
    { "id": 9, "tasks": ["7.2", "8.1"] },
    { "id": 10, "tasks": ["8.2", "8.3", "8.6"] },
    { "id": 11, "tasks": ["8.4", "8.5"] },
    { "id": 12, "tasks": ["8.7", "9.1"] },
    { "id": 13, "tasks": ["9.2"] },
    { "id": 14, "tasks": ["9.3", "11.1"] },
    { "id": 15, "tasks": ["11.2"] },
    { "id": 16, "tasks": ["11.3", "11.4", "12.1", "13.1"] },
    { "id": 17, "tasks": ["12.2", "12.3", "13.2", "13.3"] },
    { "id": 18, "tasks": ["12.4", "12.5", "12.6", "14.1"] },
    { "id": 19, "tasks": ["14.2", "14.3", "14.4"] },
    { "id": 20, "tasks": ["14.5", "14.6", "14.7", "14.8", "16.1", "16.2"] },
    { "id": 21, "tasks": ["16.3", "16.4"] }
  ]
}
```
