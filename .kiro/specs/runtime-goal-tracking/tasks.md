# Implementation Plan: Runtime Goal Tracking

## Overview

This plan wires the already-built Goal Model (`halter-goals`) into the `halter-runtime` turn
loop so that, when `[context].goal_tracking = auto`, the runtime maintains an active-goal
pointer and tags every turn-attributable session event with the goal node that owns it. The
feature is off by default, and `off` reproduces today's behavior byte-for-byte with zero
per-turn overhead.

The implementation is incremental and bottom-up, matching the crate dependency direction:

1. Foundations first — the additive, backward-compatible `halter-protocol` tag and `Goal`
   payload; the `halter-config` toggle; and the `SessionStore`-backed `GoalEventLog` adapter
   in `halter-goals`.
2. Runtime state and attribution — the `ActiveGoalStack` and the chokepoint stamping at
   `push_event`/`make_event`.
3. Agent surface and wiring — the `GoalTool` in `halter-tools`, then `HalterBuilder`/
   `RuntimeServices` wiring that honors the mode and rehydrates the stack on resume.
4. Reconciliation guards on `halter-goals` (`tool_calls` role, `subtree_hash` invariance).
5. Follow-on compaction: the required `CompactionContext::goal_log()` seam (buildable now)
   and a deferred `GoalOrientedCompaction` strategy skeleton (depends on a later design pass).

All code is Rust, matching the workspace. It reuses `halter-goals` (`GoalStore`,
`EventLogGoalStore`, `GoalEventLog`, `GoalEvent`, `subtree_hash`, `ClosureSignal`),
`halter-session` (`SessionStore::commit`/`replay`), `halter-protocol` `fold`, and the
existing `TaskTool` shape. Property tests use `proptest` (the workspace standard) and encode
the 12 Correctness Properties (P1–P12) from the design.

## Tasks

- [x] 1. Protocol foundations: attribution tag and `Goal` payload (`halter-protocol`)
  - [x] 1.1 Make `GoalNodeId` available to `halter-protocol` and add the `Goal` payload
    - Re-export or relocate `GoalNodeId` so `halter-protocol` can reference the id type
      without a heavy dependency; add the `halter-protocol` → `halter-goals` dependency for
      the richer `GoalEvent` used by the new variant
    - Add `SessionEventPayload::Goal { event: halter_goals::GoalEvent }` to the
      `#[serde(tag = "kind", rename_all = "snake_case")]` enum
    - _Requirements: 5.1, 8.1_

  - [x] 1.2 Add the optional `goal_node` tag to turn-attributable payloads
    - Add `#[serde(default, skip_serializing_if = "Option::is_none")] goal_node:
      Option<GoalNodeId>` to `MessageItem`, `ToolExecutionStarted`, `ToolExecutionCompleted`,
      `ToolOutput`, `TurnCompleted`, `Warning`, `DeltaItem`, and `ContextProjectionUpdated`
    - Leave lifecycle/aggregate markers (`SessionStarted`, `Lagged`,
      `SessionShutdownComplete`) untouched; keep the tag off the provider-visible `Message`
    - _Requirements: 5.1, 5.2, 5.3, 3.3_

  - [x] 1.3 Fold the tag and the `Goal` payload in `halter_protocol::fold::apply_event`
    - Treat `SessionEventPayload::Goal` as a no-op for `SessionState.messages`
    - Fold a tagged `MessageItem` to the identical `SessionState.messages` content as an
      untagged one (fold reads `message`, ignores `goal_node`)
    - _Requirements: 5.5, 8.4_

  - [ ]* 1.4 Write property test for backward-compatible deserialization
    - **Property 7: Backward-compatible deserialization**
    - **Validates: Requirements 5.4, 10.4**

  - [ ]* 1.5 Write property test for fold stability of the tag
    - **Property 3: Attribution stability under replay (tagged vs untagged fold equality)**
    - **Validates: Requirements 5.5, 8.4**

- [x] 2. Config toggle (`halter-config`)
  - [x] 2.1 Add `GoalTrackingMode` and `ContextConfig.goal_tracking`
    - Define `GoalTrackingMode { Off, Auto }` (`#[serde(rename_all = "snake_case")]`,
      `#[default] Off`) mirroring `CompactionStrategyKind`
    - Add `#[serde(default)] pub goal_tracking: GoalTrackingMode` to `ContextConfig`
    - _Requirements: 1.1, 1.2_

  - [ ]* 2.2 Write unit tests for the config default and round-trip
    - Test omission resolves to `off`, and serialize→deserialize preserves the value
    - _Requirements: 1.2, 1.3_

- [x] 3. `SessionStore`-backed `GoalEventLog` adapter (`halter-goals`)
  - [x] 3.1 Implement `SessionStoreGoalEventLog` over `SessionStore`
    - Implement `append` (wrap each `GoalEvent` as `SessionEventPayload::Goal { event }` and
      `SessionStore::commit` with `expected_head_sequence` for OCC, yielding gap-free
      monotonic shared sequences), `replay` (filter `Goal` payloads in ascending sequence
      order), and `head_sequence` (shared log head)
    - Keep the `GoalEventLog` contract identical so `EventLogGoalStore` is unchanged
    - _Requirements: 8.1, 8.2, 8.5_

  - [ ]* 3.2 Run the existing `GoalEventLog` conformance suite against the adapter
    - Reuse the in-memory conformance expectations (gap-free sequences, in-order replay, OCC
      conflict on `expected_head_sequence` mismatch leaving log and tree unchanged)
    - _Requirements: 8.2, 8.5_

  - [ ]* 3.3 Write property test for shared-log ordering
    - **Property 10: Shared-log ordering**
    - **Validates: Requirements 8.2, 8.3**

- [x] 4. Checkpoint - Protocol, config, and adapter foundations
  - Ensure all tests pass, ask the user if questions arise.

- [x] 5. Active-goal stack (`halter-runtime`)
  - [x] 5.1 Implement `ActiveGoalStack` state and operations
    - Define the stack (root at index 0, top is active) with `active()`, `ensure_root(store,
      session)` (lazy root creation via `GoalStore`), `push(node)`, `resolve_to_parent(node)`
      (never pops the root; pops the node and still-active descendants back to its parent),
      and `focus(node)` (rebuild the root→node path)
    - Carry it as session-owned runtime bookkeeping on the checkpoint (like
      `pending_tool_calls`), not on fold-covered `SessionState` domain fields
    - _Requirements: 4.4, 6.1, 6.2, 6.3, 6.6_

  - [ ]* 5.2 Write unit tests for advancement semantics
    - Test push-makes-active, resolve→parent (and resolve-root leaves root active),
      root-never-popped, focus-rebuilds-path, and cross-turn persistence of the active pointer
    - _Requirements: 4.4, 6.1, 6.2, 6.3, 6.4, 6.6_

  - [ ]* 5.3 Write property test for advancement discipline
    - **Property 6: Advancement discipline (resolve → parent, root always Some)**
    - **Validates: Requirements 4.2, 6.2, 6.3**

- [x] 6. Attribution at the chokepoint (`halter-runtime`)
  - [x] 6.1 Stamp `goal_node` at `push_event`/`make_event`
    - Give the chokepoint access to the session's `ActiveGoalStack`; when `auto`, stamp
      `goal_node = active()` onto turn-attributable payloads only; leave non-attributable
      markers untagged
    - When `off`, skip stamping entirely — no stack read, no goal-store call, payloads emitted
      with `goal_node = None` exactly as today
    - _Requirements: 1.4, 1.5, 2.1, 2.4, 3.1, 3.2, 3.3, 9.1_

  - [x] 6.2 Lazy root and graceful degradation
    - Call `ensure_root` lazily on the first turn-attributable event or first goal-tool call;
      after it, `active()` is always `Some`
    - On `ensure_root` failure, emit the event with `goal_node = None`, emit a `Warning`, and
      never block or fail the turn
    - _Requirements: 4.1, 4.2, 4.3, 10.2_

  - [ ]* 6.3 Write property test for total attribution
    - **Property 1: Total attribution**
    - **Validates: Requirements 3.1, 3.2**

  - [ ]* 6.4 Write property test for root as total fallback
    - **Property 2: Root is total fallback**
    - **Validates: Requirements 4.3**

  - [ ]* 6.5 Write property test for partitionability
    - **Property 4: Partitionability**
    - **Validates: Requirements 3.4**

  - [ ]* 6.6 Write property test for attribution stability under replay
    - **Property 3: Attribution stability under replay**
    - **Validates: Requirements 8.6, 6.5**

- [x] 7. Checkpoint - Runtime attribution and active-goal stack
  - Ensure all tests pass, ask the user if questions arise.

- [x] 8. Goal tool (`halter-tools`)
  - [x] 8.1 Implement `GoalTool` actions driving `GoalStore` and the stack
    - Implement `create` (non-empty hypothesis, optional parent defaulting to active, optional
      resolution_conditions → `GoalStore::create`, push active, return id), `revise`
      (`GoalNodeRevised`), `resolve` (`GoalStore::close`, pop to parent, return outcome incl.
      `subtree_hash` on closure), `focus` (set active to an existing node), and `tree`
      (read-only projection), mirroring the `TaskTool` shape
    - Never read or write the flat `TaskList`
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5, 7.8, 10.1_

  - [x] 8.2 Implement `GoalTool` input validation and error handling
    - Reject empty/whitespace hypothesis on `create`, and unknown id on `revise`/`resolve`/
      `focus`, appending no event and leaving the stack unchanged; surface `GoalStore` OCC/
      unavailability as a tool error while the turn continues and the active node is unchanged
    - _Requirements: 7.6, 7.7, 10.1, 10.3_

  - [ ]* 8.3 Write unit tests for goal tool actions and validation
    - Mirror `TaskTool`'s tests: blank/missing hypothesis, unknown id on revise/resolve/focus,
      idempotent resolve, and tree read-only
    - _Requirements: 7.1, 7.4, 7.5, 7.6, 7.7, 10.3_

- [x] 9. Wiring the mode (`halter` / `halter-runtime`)
  - [x] 9.1 Honor `goal_tracking` in `HalterBuilder`/`RuntimeServices`
    - When `auto`, register the `GoalTool`, install the `SessionStore`-backed `GoalStore`, and
      create the per-session `ActiveGoalStack`; when `off`, register no goal tool, leave the
      Goal Model dormant, and stamp no tag
    - _Requirements: 1.4, 1.5, 2.1, 2.2_

  - [x] 9.2 Rehydrate the active-goal stack on resume
    - On resume, fold the session's `Goal` events into a `GoalTree` and reconstruct the
      root→active path so attribution resumes at the previously active node
    - _Requirements: 6.5_

  - [ ]* 9.3 Write the off-mode byte-identical golden test (R2.5 divergence guard)
    - **Property 5: Off ≡ today**
    - Assert `off` produces a session log byte-identical to the current code path and that any
      divergent commit fails the session rather than filtering or continuing
    - **Validates: Requirements 2.1, 2.2, 2.3, 2.4, 2.5, 1.5**

- [x] 10. Checkpoint - Goal tool and wiring (core complete)
  - Ensure all tests pass, ask the user if questions arise.

- [x] 11. Goal Model reconciliation guards (`halter-goals`)
  - [x] 11.1 Enforce agent-curated `tool_calls` and confirm `subtree_hash` keying
    - Ensure `GoalNode.tool_calls` is never auto-populated from tagged tool events (stays
      curated via the `revise` action only); update the documented role; add a guard/test that
      confirms `subtree_hash` stays keyed on the semantic contract (no `subtree_hash` code
      change expected)
    - _Requirements: 9.1, 9.2, 9.3_

  - [ ]* 11.2 Write property test for `subtree_hash` invariance under incidental transcript changes
    - **Property 8: `subtree_hash` invariance under incidental transcript changes**
    - **Validates: Requirements 9.3, 9.4**

  - [ ]* 11.3 Write property test for closure-exactly-once preservation
    - **Property 9: Closure-exactly-once is preserved**
    - **Validates: Requirements 9.5**

- [x] 12. Compaction seam (`halter-runtime`) — follow-on, buildable now
  - [x] 12.1 Add the `CompactionContext::goal_log()` accessor
    - Add a per-session `goal_log()` accessor returning a handle over the session's
      `GoalStore` and goal event log (folded tree + goal-tagged events), scoped to the
      per-session context rather than the shared, stateless `CompactionStrategy`
    - _Requirements: 11.1, 11.2, 11.3_

- [x] 13. `GoalOrientedCompaction` strategy skeleton (`halter-runtime`) — DEFERRED
  - [x] 13.1 Sketch the `GoalOrientedCompaction` strategy (deferred to a later design pass)
    - Deferred: the strategy body depends on the design's sketch being fully specified later.
      Provide only a skeleton `CompactionStrategy` that reads `ctx.goal_log()`, preserves the
      open-frontier events verbatim, and falls back to `ModelSummary` when the tree is thin or
      the transcript is largely unmapped
    - **Property 11: Compaction preserves the open frontier (consumer)**
    - **Property 12: Compaction totality/fallback (consumer)**
    - **Validates: Requirements 12.1, 12.2, 12.3, 12.4**

- [x] 14. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP; core implementation
  tasks are never optional.
- Each non-optional task references specific requirements (granular acceptance-criteria
  clauses) for traceability, and each property test references its numbered Correctness
  Property (P1–P12).
- Task 13 (`GoalOrientedCompaction` body) is explicitly deferred: it depends on a later spec
  fully specifying the design's sketch. Task 12 (the `goal_log()` seam) is buildable now.
- All changes to `halter-protocol` and `halter-goals` are additive and backward-compatible;
  legacy logs deserialize with `goal_node = None` and an empty goal tree.
- Property tests use `proptest` over random interleavings of turn events and goal-tool
  actions, matching the `halter-goals` and `halter-protocol::fold` conventions.
- Checkpoints ensure incremental validation after the foundations, after runtime attribution,
  and after the core (tool + wiring) is complete.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "2.1", "5.1"] },
    { "id": 1, "tasks": ["1.2", "2.2", "5.2", "5.3"] },
    { "id": 2, "tasks": ["1.3", "3.1"] },
    { "id": 3, "tasks": ["1.4", "1.5", "3.2", "3.3", "6.1"] },
    { "id": 4, "tasks": ["6.2", "11.1"] },
    { "id": 5, "tasks": ["6.3", "6.4", "6.5", "6.6", "8.1", "11.2", "11.3"] },
    { "id": 6, "tasks": ["8.2"] },
    { "id": 7, "tasks": ["8.3", "9.1"] },
    { "id": 8, "tasks": ["9.2"] },
    { "id": 9, "tasks": ["9.3", "12.1"] },
    { "id": 10, "tasks": ["13.1"] }
  ]
}
```
