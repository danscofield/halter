# Implementation Plan: Goal-Oriented Compaction

## Overview

This plan fills in the `TODO(deferred)` body of `GoalOrientedCompaction::compact` in
`crates/halter-runtime/src/goal_oriented_compaction.rs`, replacing the conservative
`thin_or_unmapped` skeleton with the full partition → classify → preserve → distill → assemble
algorithm from the design. It also enforces the totality/never-worse-than-default guarantee by
falling back to `ModelSummary` on every non-goal-oriented path.

The work is entirely additive within `halter-runtime`. It reuses the existing seam
(`CompactionContext::goal_log()`, `GoalLog::get_tree`, `GoalLog::goal_events`, `ctx.trigger()`),
the folded `GoalTree`/`GoalNode`/`Resolution` (`halter-goals`, re-exported via
`halter-protocol::goals`), the goal-tagged `SessionEventPayload::MessageItem { message, goal_node }`
tags, the Tier 2 `MemoryStore::has_memory_for((node_id, subtree_hash))` lookup, and the
`ModelSummary` fallback. It changes no persisted format and adds no provider-visible data.

Implementation is bottom-up so each step builds on the last and nothing is orphaned:

1. Data models and pure fixed constants (`Owner`, `Partition`/`PartitionEntry`, `Distillation`,
   `render_distillation`, `MAPPED_COVERAGE_THRESHOLD`, `THIN_TREE_MAX_LEN`).
2. Pure tree/window readers (`owning_nodes`, `partition_window`, `on_open_frontier`,
   `closed_subtree_roots`).
3. The distiller, the assembler, and the totality/size gate.
4. The `compact` body wiring every gate in order and delegating to `ModelSummary`.
5. The optional Tier 2 `memory_ref` lookup.
6. Property tests (the 7 design properties) and unit/example/edge tests.

All code is Rust, matching the workspace. Property tests use `proptest` (the workspace standard),
run a minimum of 100 iterations, and are tagged
`Feature: goal-oriented-compaction, Property N: ...`.

## Tasks

- [x] 1. Data models, fixed constants, and distillation rendering
  - [x] 1.1 Add the ownership and result data models plus fixed constants
    - In `crates/halter-runtime/src/goal_oriented_compaction.rs`, add `enum Owner { Node(GoalNodeId), Unmapped }`,
      `struct PartitionEntry { index: usize, owner: Owner }`, `struct Partition { entries: Vec<PartitionEntry>, mapped_count: usize }`
      with `Partition::mapped_coverage() -> f64` (empty window ⇒ `1.0`), and
      `struct Distillation { node_id, hypothesis, resolution, subtree_hash, memory: Option<MemoryId>, anchor_index }`
    - Add `const MAPPED_COVERAGE_THRESHOLD: f64 = 0.5;` and `const THIN_TREE_MAX_LEN: usize = 1;`
    - _Requirements: 3.1, 3.2, 3.3, 3.4, 5.2, 6.2, 6.4_

  - [x] 1.2 Implement `render_distillation` (deterministic, user-role message)
    - Render a `Distillation` into exactly one user-role `Message` with the stable text layout from
      the design (goal marker + hypothesis, `resolution:`, `subtree:`, and `memory:` only when
      `memory` is `Some`); no timestamps, no iteration-order dependence
    - _Requirements: 5.2, 5.3, 7.3, 8.1_

  - [ ]* 1.3 Write unit tests for `mapped_coverage` and `render_distillation` determinism
    - Test empty-window coverage `== 1.0`, fractional coverage math, and that two renders of the
      same `Distillation` are byte-identical (with and without a `memory` id)
    - _Requirements: 5.3, 6.3, 8.1_

- [x] 2. Owning-node reader (message → `goal_node`)
  - [x] 2.1 Implement `owning_nodes` over the shared-log replay
    - Add `async fn owning_nodes(goal_log: &GoalLog<'_>) -> Result<HashMap<MessageId, GoalNodeId>, GoalStoreError>`
      that replays the session log's `MessageItem { message, goal_node: Some(n) }` payloads (the
      same read the seam performs) and records `message.id() -> n`; messages with `goal_node = None`
      or no `MessageItem` event are absent from the map (⇒ unmapped)
    - Resolution depends only on committed tags, never on message content
    - _Requirements: 2.2, 8.3_

  - [ ]* 2.2 Write unit tests for owning-node resolution
    - Test that a tagged message resolves to its node, an untagged message is absent, and content
      never affects the mapping
    - _Requirements: 2.2, 8.3_

- [x] 3. Partitioning
  - [x] 3.1 Implement `partition_window`
    - Add `fn partition_window(window: &[Message], owning: &HashMap<MessageId, GoalNodeId>, tree: &GoalTree) -> Partition`
      that emits one `PartitionEntry` per message in original order; owner is `Node(n)` only when
      `owning.get(id)` is `Some(n)` and `tree.contains(&n)`, otherwise `Unmapped`; increment
      `mapped_count` per mapped entry
    - _Requirements: 2.2, 3.1, 3.2, 3.3, 3.4_

  - [ ]* 3.2 Write property test for the partition
    - **Property 1: Total, order-preserving, tag-only partition**
    - **Validates: Requirements 2.2, 3.1, 3.2, 3.3, 3.4, 5.5**

- [x] 4. Frontier classification
  - [x] 4.1 Implement `on_open_frontier` and `closed_subtree_roots`
    - Add `fn on_open_frontier(tree: &GoalTree, n: &GoalNodeId) -> bool` (true iff `n` or any
      descendant has `resolution == Open` or lacks `subtree_hash`) and
      `fn closed_subtree_roots(tree: &GoalTree) -> Vec<GoalNodeId>` returning maximal fully-closed
      roots (highest closed ancestor); both pure over the folded tree, memoized via a single
      post-order walk
    - _Requirements: 4.2, 4.4, 5.1, 8.1_

  - [ ]* 4.2 Write unit tests for classification edges
    - Test open descendant forces ancestor open, closed leaf under open parent is not a maximal
      root, and a closed parent subsumes closed children into one root
    - _Requirements: 4.2, 5.1_

- [x] 5. Distiller, assembler, and totality gate
  - [x] 5.1 Implement the distiller (closed subtree → `Distillation`)
    - For each maximal closed root, read `hypothesis`, `resolution`, and `subtree_hash` from the
      tree node; if `subtree_hash` is absent, skip (retain verbatim, no hash-less distillation);
      compute `anchor_index` as the subtree's first owned message index; skip roots that own no
      messages
    - _Requirements: 5.1, 5.2, 5.4, 9.2_

  - [ ]* 5.2 Write property test for distillation completeness
    - **Property 3: Distillation is complete and drawn only from closed subtrees**
    - **Validates: Requirements 5.1, 5.2**

  - [x] 5.3 Implement `assemble`
    - Add `fn assemble(window, partition, tree, distillations) -> Vec<Message>` that walks the
      window once: emit each distillation once at its subtree's first-member position (dropping the
      remaining members), and emit open-frontier and unmapped messages verbatim, preserving relative
      order
    - _Requirements: 3.3, 4.1, 4.3, 5.4, 5.5_

  - [ ]* 5.4 Write property test for verbatim preservation
    - **Property 2: Open-frontier and unmapped messages are preserved verbatim**
    - **Validates: Requirements 4.1, 4.3, 5.4, 3.4**

  - [x] 5.5 Implement the totality/size gate helpers
    - Add `retains_all_open_and_unmapped(...)` and a size check so the caller can fall back when
      `distillations.is_empty()`, `replacement.len() > window.len()`, or any open-frontier/unmapped
      message is missing from the replacement
    - _Requirements: 7.1, 7.3, 7.4_

  - [ ]* 5.6 Write property tests for the totality gate and size bound
    - **Property 4: Totality gate — retain everything or fall back**
    - **Property 5: The replacement window is never larger than the original**
    - **Validates: Requirements 7.1, 7.3, 7.4**

- [x] 6. Checkpoint - Data models and pure helpers
  - Ensure all tests pass, ask the user if questions arise.

- [x] 7. Wire the `compact` body
  - [x] 7.1 Implement the ordered gate pipeline in `compact`
    - Replace the deferred body: (1) off-mode gate (`goal_log()` `None` ⇒ fallback, no goal-store
      call); (2) `get_tree()` error ⇒ fallback for the pass; (3) thin gate `tree.len() <= THIN_TREE_MAX_LEN`
      ⇒ fallback; (4) `owning_nodes` replay error ⇒ fallback; (5) `partition_window` then
      unmapped gate `mapped_coverage() < MAPPED_COVERAGE_THRESHOLD` ⇒ fallback; (6) classify +
      distill + `assemble`; (7) totality/size gate ⇒ fallback; else return
      `Some(CompactionEffects { messages, compacted_context: default, result, usage: default })`
    - Delete the obsolete `thin_or_unmapped` helper (and update/replace its unit test) now that the
      thin gate lives inline
    - _Requirements: 1.3, 2.1, 2.3, 2.4, 6.1, 6.3, 7.1, 7.2, 7.4, 9.1, 9.4_

  - [ ]* 7.2 Write property test for fallback equivalence
    - **Property 6: Fallback equivalence — never worse than the default**
    - **Validates: Requirements 1.3, 2.3, 6.1, 6.3, 7.2, 9.1, 9.4**

  - [ ]* 7.3 Write property test for determinism and read-only purity
    - **Property 7: Determinism and read-only purity**
    - **Validates: Requirements 2.1, 2.4, 4.4, 8.1, 8.2, 8.3**

- [x] 8. Optional Tier 2 memory reference
  - [x] 8.1 Implement `memory_ref` and wire it into the distiller
    - Add `fn memory_ref(&self, goal_log: &GoalLog<'_>, node: &GoalNodeId, hash: &SubtreeHash) -> Option<MemoryId>`
      that calls `has_memory_for((node, hash))` when a memory store is wired, returning `None`
      otherwise; populate `Distillation.memory` from it (best-effort, read-only, never authors)
    - _Requirements: 5.3_

  - [ ]* 8.2 Write unit test for the Tier 2 reference
    - With a stub store returning `Some(MemoryId)` assert the distillation carries the id and the
      rendered message shows it; with `None` assert it does not
    - _Requirements: 5.3_

- [ ] 9. Delegation and edge/example tests
  - [ ]* 9.1 Write unit tests for surface delegation and no-provider guarantee
    - Assert `window_policy`, `tools`, `prompt_segments` equal `ModelSummary`'s and add nothing; and
      that the goal-oriented path issues no provider request (provider stub panics if called)
    - _Requirements: 1.1, 1.2, 1.4_

  - [ ]* 9.2 Write unit/edge tests for threshold bounds, missing hash, manual propagation, legacy log
    - Boundary tests at `tree.len()` 0/1/2 and mapped-coverage just below/at/above the threshold;
      assert the threshold constant is in `[0, 1]`; a closed subtree with no `subtree_hash` retains
      its messages verbatim (9.2); under `CompactionTrigger::Manual` a fallback `Err` propagates
      unchanged (9.3); a legacy no-`Goal` log folds to an empty tree, classifies thin, and yields the
      `ModelSummary` result (9.4)
    - _Requirements: 6.2, 6.4, 9.2, 9.3, 9.4_

- [x] 10. Final checkpoint - Ensure all tests pass
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP; core implementation tasks
  are never optional.
- Each non-optional task references specific granular acceptance criteria for traceability; each
  property test references its numbered Correctness Property (P1–P7 in the design, refining origin
  P11/P12).
- Property tests use `proptest`, run a minimum of 100 iterations, and are tagged
  `Feature: goal-oriented-compaction, Property N: {property text}`. A single composite generator
  produces a random `GoalTree` + random message window + random tag assignment, with a stub
  `GoalLog`/context, a stub `MemoryStore`, and a `ModelSummary` baseline for equivalence.
- Property sub-tasks are placed next to the implementation they validate so errors surface early.
- The pass is a pure, read-only function: it appends no `Goal` payload and mutates neither the goal
  tree nor the goal event log. No persisted format changes and no other crates are touched.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "2.1", "4.1"] },
    { "id": 1, "tasks": ["1.2", "1.3", "2.2", "3.1", "4.2"] },
    { "id": 2, "tasks": ["3.2", "5.1", "5.3"] },
    { "id": 3, "tasks": ["5.2", "5.4", "5.5"] },
    { "id": 4, "tasks": ["5.6", "7.1"] },
    { "id": 5, "tasks": ["7.2", "7.3", "8.1"] },
    { "id": 6, "tasks": ["8.2", "9.1", "9.2"] }
  ]
}
```
