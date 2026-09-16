# Requirements Document

## Introduction

**Goal-Oriented Compaction** fully specifies the `GoalOrientedCompaction` `CompactionStrategy`
body that the `runtime-goal-tracking` spec deliberately deferred. That prior spec built the
required seam (`CompactionContext::goal_log()`) and shipped a safe skeleton that delegates every
decision to `ModelSummary`; it captured the target behavior as deferred **Requirement 11** (the
seam) and **Requirement 12** (the strategy behavior), noting that "the strategy body depends on a
later spec that fully specifies the sketch." This is that later spec.

`GoalOrientedCompaction` compresses a session's transcript along the boundaries of its goal tree.
It reads the folded goal tree and the goal-tagged transcript through the existing per-session
`GoalLog` handle, partitions the session's messages by the goal node that owns each event,
preserves the work still in flight (the open frontier) verbatim, and distills the work that is
finished (fully-closed subtrees) down to a compact record of hypothesis, resolution, and
`subtree_hash` (optionally referencing a Tier 2 memory). When the goal tree carries too little
usable structure — it is thin, or the transcript is largely unmapped — the strategy falls back to
`ModelSummary` and is never worse than the default.

This work fills in the `TODO(deferred)` in
`crates/halter-runtime/src/goal_oriented_compaction.rs`. It is additive to the runtime and reuses
the seam, the folded `GoalTree` (`halter-goals` / re-exported through `halter-protocol::goals`),
the goal-tagged `SessionEventPayload` variants (`halter-protocol`), and the `ModelSummary`
fallback that already exists. It changes no persisted format and adds no provider-visible data.

These requirements refine the origin acceptance criteria in
`.kiro/specs/runtime-goal-tracking/requirements.md` Requirements 11 and 12, and are consistent
with that spec's Correctness Properties **P11** (compaction preserves the open frontier) and
**P12** (compaction totality/fallback). Property references appear inline as **(Property N)**.

### Scope and non-goals

- **In scope:** the `GoalOrientedCompaction::compact` body — partitioning messages by owning
  `goal_node`, the open-frontier preservation rule, the closed-subtree distillation rule, the
  optional Tier 2 memory reference in a distillation, the thin/unmapped fallback decision, and the
  totality/no-worse-than-default guarantee relative to `ModelSummary`.
- **Origin seam reused, not re-specified.** `CompactionContext::goal_log()` and the read-only
  `GoalLog` handle (`get_tree`, `goal_events`) already exist and are honored as-is; this spec does
  not redefine them (they are the subject of the prior spec's Requirement 11).
- **No provider/server-side auto-compaction.** The existing prohibition in the `CompactionStrategy`
  docs stands; this strategy runs only under the runtime-owned trigger, exactly like `ModelSummary`.
- **No change to attribution, the active-goal stack, the goal tool, or `subtree_hash` inputs.**
  Those are owned by `runtime-goal-tracking` and `halter-goals` and are consumed unchanged.
- **No redesign of Tier 2.** A distillation may *reference* an existing memory by id; this spec
  does not author, retrieve-rank, or invalidate memories.
- **Off mode is untouched.** When goal tracking resolves to `off`, `goal_log()` returns `None` and
  the strategy takes the byte-identical `ModelSummary` path, exactly as the skeleton does today.

## Glossary

- **GoalOrientedCompaction**: The `CompactionStrategy` (in `halter-runtime`) whose `compact` body
  this spec specifies; it compresses the transcript along goal boundaries and falls back to
  `ModelSummary`.
- **ModelSummary**: The existing default/fallback `CompactionStrategy`. The behavioral baseline
  `GoalOrientedCompaction` must never be worse than.
- **CompactionStrategy**: The shared, per-session-stateless trait (`compact`, `window_policy`,
  `tools`, `prompt_segments`) the runtime invokes on a compaction trigger.
- **CompactionContext**: The per-session context passed to `compact`, exposing `state()`,
  `trigger()`, `goal_log()`, `infer()`, `append`/`record`, and the fallback surface.
- **CompactionEffects**: The result of a compaction pass — the replacement `messages` window plus
  the `compacted_context` prefix — returned as `Option<CompactionEffects>`.
- **goal_log() / GoalLog**: The existing read-only per-session handle over the session's
  `GoalStore` and goal event log. `goal_log()` returns `Some(GoalLog)` under `auto` and `None`
  under `off`. `GoalLog::get_tree()` projects the folded `GoalTree`; `GoalLog::goal_events()`
  replays the goal-tagged `GoalEvent`s in ascending shared-log sequence order.
- **GoalTree**: The folded projection of the session's goal events: nodes keyed by id, a single
  `root`, `children`, per-node `resolution`, and a `subtree_hash` stamped on each fully-closed
  node. Exposes `node`, `root`, `children`, `contains`, `iter`, `len`, `is_empty`.
- **GoalNode**: A node in the `GoalTree` with `id`, `parent`, `children`, `hypothesis`,
  `resolution_conditions`, `resolution`, `intent`, curated `tool_calls`, and optional
  `subtree_hash`.
- **goal_node tag**: The optional `Option<GoalNodeId>` attribution tag stamped by the runtime onto
  each turn-attributable `SessionEventPayload` (`MessageItem` and siblings); the source of truth
  for which message belongs to which node.
- **Owning node**: For a message in `ctx.state().messages`, the `GoalNodeId` carried by the event
  that produced the message (its `goal_node` tag). A message whose tag is `None` or names a node
  absent from the folded tree is **unmapped**.
- **Message window**: `ctx.state().messages` — the ordered live transcript the strategy rewrites.
- **Open frontier**: The set of goal nodes not contained in a fully-closed subtree. A node is on
  the open frontier when it, or any of its descendants, is still open (`resolution` is `Open`, or
  the node carries no `subtree_hash`).
- **Fully-closed subtree**: A subtree rooted at a node whose entire subtree is closed — every node
  in it has a non-`Open` resolution and the subtree root carries a stamped `subtree_hash`.
- **subtree_hash**: The stable, order-insensitive content hash stamped on a node at closure; keyed
  on the semantic contract, not the transcript. Used here only as an identity/reference token in a
  distillation and as the memory-lookup key part.
- **Distillation**: The compact replacement for a fully-closed subtree's events: its `hypothesis`,
  its `resolution`, and its `subtree_hash`, optionally with a reference to a Tier 2 memory.
- **Tier 2 memory reference**: An optional `MemoryId` a distillation may carry when a memory
  already exists for the subtree's `(node_id, subtree_hash)` key.
- **Thin tree**: A goal tree with no usable closed-subtree structure to distill (at minimum: empty
  or root-only, `len() <= 1`).
- **Largely unmapped transcript**: A message window in which the fraction of messages with a
  resolvable owning node in the folded tree falls below the mapped-coverage threshold.
- **GoalTrackingMode**: The `[context].goal_tracking = auto | off` config from
  `runtime-goal-tracking`; `off` is the default and yields `goal_log() == None`.

## Requirements

### Requirement 1: Strategy selection, trigger policy, and off-mode passthrough

**User Story:** As an operator, I want `GoalOrientedCompaction` to run only under the runtime's own
compaction trigger and to reduce to the default when goal tracking is off, so that enabling it
carries no new risk and never invokes provider-side compaction.

*Origin: runtime-goal-tracking Requirement 12 (follow-on), and the standing no-provider-compaction
prohibition.*

#### Acceptance Criteria

1. THE GoalOrientedCompaction SHALL select the same runtime-owned `window_policy` as ModelSummary
   and SHALL NOT set its own compaction thresholds.
2. THE GoalOrientedCompaction SHALL expose the same `tools` and `prompt_segments` as its
   ModelSummary fallback and SHALL add no goal-specific tool or prompt segment.
3. WHERE `goal_log()` returns `None` for a compaction pass, THE GoalOrientedCompaction SHALL
   produce the identical `Option<CompactionEffects>` result that ModelSummary produces for the same
   CompactionContext, and SHALL make no goal-store call. *(Property 12)*
4. WHEN GoalOrientedCompaction compacts a session, THE GoalOrientedCompaction SHALL run only under
   the runtime-owned compaction trigger and SHALL NOT delegate compaction to any provider or
   server-side mechanism.

### Requirement 2: Read the goal tree and tagged transcript through the seam

**User Story:** As the strategy, I want to obtain the folded goal tree and the owning node of each
message through the per-session seam, so that partitioning is a pure read over the committed log.

*Origin: runtime-goal-tracking Requirement 11 (the seam) and Requirement 12.1.*

#### Acceptance Criteria

1. WHEN GoalOrientedCompaction compacts a session under `auto`, THE GoalOrientedCompaction SHALL
   obtain the folded goal tree via `goal_log().get_tree()` and SHALL treat that tree as read-only.
2. WHEN GoalOrientedCompaction determines the owning node of a message in the message window, THE
   GoalOrientedCompaction SHALL use the `goal_node` tag attributed to that message as the sole
   source of truth and SHALL NOT infer ownership from message content.
3. IF `goal_log().get_tree()` returns an error, THEN THE GoalOrientedCompaction SHALL fall back to
   ModelSummary for that pass and SHALL NOT fail the compaction pass with that error. *(Property
   12)*
4. WHILE reading the goal tree and tagged transcript, THE GoalOrientedCompaction SHALL append no
   `Goal` payload and SHALL mutate neither the goal tree nor the goal event log.

### Requirement 3: Partition the message window by owning goal node

**User Story:** As the strategy, I want to partition the live message window by owning goal node,
so that I can decide per node whether to preserve or distill its events.

*Origin: runtime-goal-tracking Requirement 12 and design "Flow 3".*

#### Acceptance Criteria

1. WHEN GoalOrientedCompaction partitions the message window, THE GoalOrientedCompaction SHALL
   assign each message to exactly one partition — the message's owning node when its `goal_node`
   tag names a node present in the folded tree, otherwise the unmapped partition.
2. WHEN GoalOrientedCompaction partitions the message window, THE GoalOrientedCompaction SHALL
   place every message in the window into exactly one partition such that concatenating the
   partitions reproduces the original window with no loss and no duplication.
3. WHILE producing the replacement window, THE GoalOrientedCompaction SHALL preserve the relative
   order of any two retained messages as their relative order in the original message window.
4. WHERE a message's `goal_node` tag is `None` or names a node absent from the folded tree, THE
   GoalOrientedCompaction SHALL treat that message as unmapped and SHALL retain it verbatim in the
   replacement window.

### Requirement 4: Preserve open-frontier events verbatim

**User Story:** As a user, I want work that is still in flight to survive compaction unchanged, so
that the agent never loses context for goals it has not finished.

*Origin: runtime-goal-tracking Requirement 12.1 (Property 11).*

#### Acceptance Criteria

1. WHEN GoalOrientedCompaction produces the replacement window, THE GoalOrientedCompaction SHALL
   retain verbatim every message whose owning node is on the open frontier. *(Property 11)*
2. THE GoalOrientedCompaction SHALL classify a node as on the open frontier WHERE that node, or any
   of its descendants in the folded tree, has a resolution of `Open` or carries no `subtree_hash`.
   *(Property 11)*
3. WHEN GoalOrientedCompaction retains an open-frontier message, THE GoalOrientedCompaction SHALL
   retain that message with byte-identical content to its content in the original message window.
   *(Property 11)*
4. IF a subtree becomes fully closed only after the compaction pass reads the tree, THEN THE
   GoalOrientedCompaction SHALL still have treated that subtree's events as open-frontier for the
   pass that read the earlier tree, so that the pass's decision is a pure function of the tree it
   read. *(Property 11, Property 12)*

### Requirement 5: Distill fully-closed subtrees

**User Story:** As a user, I want finished work compressed to its essential outcome, so that the
transcript stays within budget without losing what each closed goal concluded.

*Origin: runtime-goal-tracking Requirement 12.2 (Property 11) and design "Flow 3".*

#### Acceptance Criteria

1. WHERE the events in the message window are attributed to a fully-closed subtree, THE
   GoalOrientedCompaction SHALL be permitted to replace those events with a single distillation of
   that subtree. *(Property 11)*
2. WHEN GoalOrientedCompaction distills a fully-closed subtree, THE GoalOrientedCompaction SHALL
   include in the distillation the subtree root's `hypothesis`, its `resolution`, and its
   `subtree_hash`. *(Property 11)*
3. WHERE a Tier 2 memory exists for the subtree root's `(node_id, subtree_hash)` key, THE
   GoalOrientedCompaction SHALL be permitted to include a reference to that memory's `MemoryId` in
   the distillation.
4. WHEN GoalOrientedCompaction distills a subtree, THE GoalOrientedCompaction SHALL NOT distill any
   node that is on the open frontier and SHALL leave that open-frontier node's events retained
   verbatim. *(Property 11)*
5. WHEN GoalOrientedCompaction places a distillation into the replacement window, THE
   GoalOrientedCompaction SHALL position the distillation so that the relative order of the
   remaining retained messages is preserved. *(Property 11)*

### Requirement 6: Fall back to ModelSummary when the tree is thin or unmapped

**User Story:** As a user, I want goal-oriented compaction to defer to the default whenever the
goal structure is too sparse to help, so that it is never worse than the default checkpoint.

*Origin: runtime-goal-tracking Requirement 12.3 and 12.4 (Property 12).*

#### Acceptance Criteria

1. IF the folded goal tree is thin, THEN THE GoalOrientedCompaction SHALL produce the same
   `Option<CompactionEffects>` result that ModelSummary produces for the same CompactionContext.
   *(Property 12)*
2. THE GoalOrientedCompaction SHALL classify a goal tree as thin WHERE the tree is empty or holds
   only the root node (`len() <= 1`).
3. IF the fraction of messages in the window with a resolvable owning node in the folded tree is
   below the mapped-coverage threshold, THEN THE GoalOrientedCompaction SHALL classify the
   transcript as largely unmapped and SHALL produce the same result that ModelSummary produces for
   the same CompactionContext. *(Property 12)*
4. THE GoalOrientedCompaction SHALL define the mapped-coverage threshold as a fixed fraction in the
   closed interval [0, 1] and SHALL apply the same threshold on every pass.

### Requirement 7: Totality — never worse than the default

**User Story:** As a maintainer, I want the goal-oriented pass to be provably no worse than
`ModelSummary` on every path, so that adopting it can never lose open work or drop below the
default.

*Origin: runtime-goal-tracking Requirement 12.4 (Property 12).*

#### Acceptance Criteria

1. WHEN GoalOrientedCompaction returns a non-`None` goal-oriented result for a session under
   `auto`, THE GoalOrientedCompaction SHALL include in the replacement window every open-frontier
   message and every unmapped message that the original window contained; when this totality
   guarantee cannot be met, THE GoalOrientedCompaction SHALL fall back to ModelSummary rather than
   return a totality-violating result. *(Property 11, Property 12)*
2. WHEN GoalOrientedCompaction falls back to ModelSummary on any path (off mode, tree-read error,
   thin tree, or largely-unmapped transcript), THE GoalOrientedCompaction SHALL return the same
   `Option<CompactionEffects>` result that ModelSummary returns for the same CompactionContext.
   *(Property 12)*
3. WHEN GoalOrientedCompaction produces a distilled replacement window, THE GoalOrientedCompaction
   SHALL emit a replacement window no larger than the original message window. *(Property 12)*
4. IF GoalOrientedCompaction cannot construct a replacement window that retains every open-frontier
   and unmapped message, THEN THE GoalOrientedCompaction SHALL fall back to ModelSummary rather than
   emit a lossy window. *(Property 11, Property 12)*

### Requirement 8: Determinism and read-only purity

**User Story:** As a maintainer, I want a compaction pass to be a pure function of the message
window and the goal tree it reads, so that behavior is reproducible and free of side effects on
goal state.

*Origin: runtime-goal-tracking Requirement 8.6 (attribution purity) and Requirement 12.*

#### Acceptance Criteria

1. WHEN GoalOrientedCompaction compacts the same CompactionContext twice with the same message
   window and the same folded goal tree, THE GoalOrientedCompaction SHALL produce the same
   `Option<CompactionEffects>` result both times.
2. WHEN GoalOrientedCompaction completes a pass, THE session's folded goal tree and goal event log
   SHALL be unchanged by that pass.
3. WHEN GoalOrientedCompaction reads the owning node of each message, THE result of that reading
   SHALL depend only on the committed `goal_node` tags and the folded tree, and SHALL NOT depend on
   wall-clock time, iteration order of non-deterministic collections, or provider responses.

### Requirement 9: Error handling and graceful degradation

**User Story:** As the runtime, I want goal-oriented compaction failures to degrade to the default
rather than break a turn, so that a goal-store hiccup never blocks agent progress.

*Origin: runtime-goal-tracking Requirement 10 (graceful degradation) and Requirement 12.*

#### Acceptance Criteria

1. IF reading the goal tree or replaying the goal-tagged events fails during a pass, THEN THE
   GoalOrientedCompaction SHALL fall back to ModelSummary for that pass. *(Property 12)*
2. IF a fully-closed subtree is missing its `subtree_hash` when a distillation would require it,
   THEN THE GoalOrientedCompaction SHALL retain that subtree's events verbatim rather than emit a
   distillation without a `subtree_hash`. *(Property 11)*
3. WHEN GoalOrientedCompaction runs under a `Manual` compaction trigger, THE GoalOrientedCompaction
   SHALL propagate a fallback error according to the same `CompactionStrategy` contract ModelSummary
   follows for that trigger, rather than silently swallowing it.
4. WHEN a legacy session log with no `Goal` events is compacted under `auto`, THE
   GoalOrientedCompaction SHALL observe an empty goal tree, SHALL classify it as thin, and SHALL
   fall back to ModelSummary as a degraded compaction that yields the ModelSummary result.
   *(Property 12)*
