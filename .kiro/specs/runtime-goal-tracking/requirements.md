# Requirements Document

## Introduction

**Runtime Goal Tracking** wires the already-built **Goal Model** (`halter-goals`) into the
`halter-runtime` turn loop. When enabled, the runtime automatically maintains goal state as
turns execute and **tags every turn-attributable session event with the goal node that owns
it** — the first mechanism in the workspace that maps runtime artifacts (messages, tool calls,
reasoning, usage) to goals. It does two things and defers a third:

1. **Automatic attribution (runtime-driven).** At the single event chokepoint
   (`push_event` / `make_event` in `crates/halter-runtime/src/session.rs`), the runtime stamps
   each emitted `SessionEventPayload` with the **active goal node id**.
2. **Agent-driven goal advancement.** A new agent-facing **goal tool** (mirroring the shape of
   the built-in `TaskTool`) drives the `GoalStore` create/revise/resolve operations and updates
   the active-goal stack. The agent decides *when* a subgoal starts, is resolved, or is closed;
   the runtime only decides *what node owns an event*.
3. **A follow-on consumer (sketched, lower priority).** A `GoalOrientedCompaction`
   `CompactionStrategy` that reads the goal tree plus goal-tagged events to compress the
   transcript along goal boundaries. This feature adds the required compaction seam and captures
   the consumer's target behavior, but the strategy body depends on a later spec that fully
   specifies the sketch.

A per-session **root goal** (lazily created on first use) is the always-present fallback, so
attribution is **total**: every turn event maps to *some* node even when the agent never touches
the goal tool. The feature is **off by default**; the config toggle `[context].goal_tracking =
auto | off` (mirroring `CompactionStrategyKind`) gates all of it, and `off` reproduces today's
behavior exactly with zero per-turn overhead.

These requirements are derived from the approved design at
`.kiro/specs/runtime-goal-tracking/design.md` and are consistent with its 12 numbered
Correctness Properties (P1–P12). Each property maps onto the acceptance criteria that validate
it; property references appear inline as **(Property N)**.

### Backward compatibility and non-goals

- **No redesign of Tier 1 or Tier 2 internals** (induction, memory, retrieval, replay). This
  feature only *triggers* them through the existing `GoalStore`/`ClosureSignal` path in
  `halter-goals`.
- **Additive-only protocol changes.** The attribution tag is an additive optional field on
  turn-attributable event payloads; existing serialized logs must deserialize unchanged.
- **No provider/server-side auto-compaction.** The existing prohibition in `CompactionStrategy`
  docs stands.
- **The flat `TaskList` and the goal tree coexist.** Neither reads nor writes the other.

## Glossary

- **Runtime**: The `halter-runtime` turn loop that owns attribution and the active-goal pointer.
- **Chokepoint**: The single event-emission path (`push_event` / `make_event` in
  `session.rs`) through which every `SessionEventPayload` is created.
- **SessionEventPayload**: The protocol enum (`halter-protocol`) whose variants a turn emits;
  the unit that carries the attribution tag.
- **Turn-attributable event**: A `SessionEventPayload` variant emitted by a turn and owned by a
  goal node (`MessageItem`, `ToolExecutionStarted`, `ToolExecutionCompleted`, `ToolOutput`,
  `TurnCompleted`, `Warning`, `DeltaItem`, `ContextProjectionUpdated`). Lifecycle/aggregate
  markers (`SessionStarted`, `Lagged`, `SessionShutdownComplete`) are not turn-attributable.
- **Attribution tag** (`goal_node`): The additive optional `Option<GoalNodeId>` field on
  turn-attributable payloads, set to the active node id when tracking is on.
- **GoalNodeId**: The identity of a goal node, available to `halter-protocol` for the tag.
- **Active-goal stack** (`ActiveGoalStack`): Session-owned runtime bookkeeping tracking the
  current `GoalNodeId` as a stack with the root at the bottom (index 0); the top is the active
  node.
- **Root goal**: The lazily created, always-present per-session goal node at the bottom of the
  active-goal stack; the total-attribution fallback. Never popped.
- **Active node**: The top of the active-goal stack; the node onto which the chokepoint stamps
  events.
- **Goal tool** (`GoalTool`): The agent-facing tool (in `halter-tools`) with actions
  `create`/`revise`/`resolve`/`focus`/`tree`, mirroring `TaskTool`'s shape, that drives
  `GoalStore` and updates the active-goal stack.
- **GoalStore**: The existing `halter-goals` interface
  (`create`/`revise`/`close`/`get`/`get_tree`) reused unchanged.
- **GoalEvent**: A goal-tree mutation (`GoalNodeCreated`, `GoalNodeRevised`, `GoalNodeResolved`,
  `GoalClosed`) recorded on the log.
- **Goal payload** (`SessionEventPayload::Goal { event }`): The new protocol payload carrying a
  `GoalEvent` so goal events ride the same log as tagged transcript events.
- **SessionStoreGoalEventLog**: The `halter-goals` adapter implementing `GoalEventLog` over
  `SessionStore::commit`/`replay`, so goal events share the single sequence-ordered session log.
- **SessionStore**: The existing event-sourced store providing an append-only, gap-free,
  sequence-ordered log with optimistic concurrency control (OCC) on commit via
  `expected_head_sequence`.
- **Goal fold**: The `halter-goals` fold that consumes `Goal` payloads into a `GoalTree`; a
  no-op for `halter_protocol::fold::apply_event` with respect to `SessionState.messages`.
- **subtree_hash**: The stable, order-insensitive content hash over a resolved subtree, keyed on
  the semantic contract (`hypothesis`, `resolution_conditions`, `resolution`, `intent`,
  normalized `tool_calls`, resolved children), used as the induction versioning/idempotency key.
- **ClosureSignal**: The `halter-goals` signal dispatched on goal closure that enqueues an
  induction job on `(node_id, subtree_hash)`.
- **GoalTrackingMode**: The config enum `{ Off, Auto }` (`halter-config`), default `Off`, added
  to `ContextConfig.goal_tracking`.
- **TaskList / TaskTool**: The existing flat, two-state, user-visible checklist and its tool;
  distinct from and coexisting with the goal tree.
- **CompactionContext**: The per-session context passed to a `CompactionStrategy`, to be extended
  with a `goal_log()` accessor (the seam addition).
- **GoalOrientedCompaction**: The follow-on `CompactionStrategy` (sketched) that compresses the
  transcript along goal boundaries, falling back to `ModelSummary`.
- **ModelSummary**: The existing default compaction strategy used as the fallback.
- **Open frontier**: The set of goal nodes not contained in a fully-closed subtree.

## Requirements

### Requirement 1: Goal-tracking config toggle

**User Story:** As an operator, I want a `[context].goal_tracking` toggle that defaults to off,
so that goal tracking is opt-in and the current behavior is preserved unless I enable it.

#### Acceptance Criteria

1. THE GoalTrackingMode SHALL define exactly the two values `off` and `auto`, and THE
   ContextConfig SHALL expose a `goal_tracking` field of type GoalTrackingMode.
2. WHERE a configuration omits `[context].goal_tracking`, THE ContextConfig SHALL resolve
   `goal_tracking` to `off`.
3. WHEN a configuration with a `goal_tracking` value is serialized and then deserialized, THE
   ContextConfig SHALL resolve the same `goal_tracking` value that was serialized.
4. WHERE `goal_tracking` resolves to `off`, THE Runtime SHALL register no goal tool, SHALL leave
   the Goal Model dormant, and SHALL stamp no attribution tag onto any emitted payload.
5. WHERE `goal_tracking` resolves to `auto`, THE Runtime SHALL register the goal tool, SHALL
   maintain the active-goal stack, and SHALL stamp the attribution tag onto every
   turn-attributable payload. *(Property 5)*

### Requirement 2: Off mode reproduces today's behavior

**User Story:** As an operator, I want `off` mode to reproduce today's behavior exactly with zero
per-turn overhead, so that adopting this feature carries no risk when it is disabled.

#### Acceptance Criteria

1. WHILE `goal_tracking` is `off`, THE Runtime SHALL emit every turn-attributable payload with
   `goal_node = None`. *(Property 5)*
2. WHILE `goal_tracking` is `off`, THE Runtime SHALL produce no `Goal` payloads on the session
   log. *(Property 5)*
3. WHILE `goal_tracking` is `off`, THE Runtime SHALL commit a session log that is byte-identical
   to the log the same sequence of turns produces on the current code path. *(Property 5)*
4. WHILE `goal_tracking` is `off`, THE Runtime SHALL perform no active-goal-stack read or write
   and no goal-store call while emitting a payload at the chokepoint. *(Property 5)*
5. IF, WHILE `goal_tracking` is `off`, any component would commit data that makes the committed
   session log diverge from the current code path's output for the same sequence of turns, THEN
   THE Runtime SHALL fail the session rather than filter the divergent data or continue. *(Property
   5)*

### Requirement 3: Automatic attribution at the chokepoint

**User Story:** As the Runtime, I want to stamp the active goal node onto every turn-attributable
event at the single chokepoint, so that attribution is total and mechanical.

#### Acceptance Criteria

1. WHILE `goal_tracking` is `auto` and the root goal exists, WHEN the chokepoint emits a
   turn-attributable payload, THE Runtime SHALL set that payload's `goal_node` to the active node
   id (the top of the active-goal stack). *(Property 1)*
2. WHILE `goal_tracking` is `auto`, WHEN the chokepoint emits a turn-attributable payload after
   the root goal exists, THE Runtime SHALL set `goal_node` to `Some(n)` for a node `n` that is
   present in the session's folded goal tree. *(Property 1)*
3. WHILE `goal_tracking` is `auto`, THE Runtime SHALL stamp no attribution tag onto lifecycle or
   aggregate markers that are not turn-attributable (`SessionStarted`, `Lagged`,
   `SessionShutdownComplete`).
4. WHILE `goal_tracking` is `auto`, WHEN the turn-attributable events for a session are grouped
   by `goal_node`, THE Runtime SHALL assign each such event to exactly one node such that the
   grouping reconstructs the full turn-event stream with no loss and no duplication. *(Property
   4)*

### Requirement 4: Lazy, always-present root goal as total fallback

**User Story:** As the Runtime, I want a per-session root goal created lazily and never popped,
so that every turn event attributes to some node even when the agent never uses the goal tool.

#### Acceptance Criteria

1. WHILE `goal_tracking` is `auto`, WHEN the first turn-attributable event or the first
   goal-tool call occurs for a session (whichever is first) and no root goal yet exists, THE
   Runtime SHALL create the root goal node via GoalStore and push it as the bottom of the
   active-goal stack.
2. WHILE `goal_tracking` is `auto` and the root goal exists, THE Runtime SHALL return a `Some`
   value from the active node accessor for the remainder of the session. *(Property 6)*
3. WHERE `goal_tracking` is `auto` and a session never calls the goal tool, THE Runtime SHALL set
   `goal_node = Some(root)` on every turn-attributable event emitted after the root exists.
   *(Property 2)*
4. WHILE `goal_tracking` is `auto`, THE Runtime SHALL never pop the root goal from the
   active-goal stack.

### Requirement 5: Additive, backward-compatible attribution tag

**User Story:** As a maintainer, I want the attribution tag to be an additive optional field on
event payloads and never provider-visible, so that legacy logs stay readable and provider
requests stay unchanged.

#### Acceptance Criteria

1. THE SessionEventPayload turn-attributable variants SHALL each carry an optional `goal_node`
   field of type `Option<GoalNodeId>` serialized with `#[serde(default, skip_serializing_if =
   "Option::is_none")]`.
2. THE attribution tag SHALL reside on the SessionEventPayload variants and SHALL NOT be added to
   the provider-visible `Message` struct.
3. WHERE a provider request is constructed from the provider-visible `Message` struct, THE Runtime
   SHALL include no `goal_node` tag in that request, because the tag resides on the
   SessionEventPayload event and not on the `Message` from which the request is built.
4. WHEN a session log serialized before this feature (containing no `goal_node` fields and no
   `Goal` payloads) is deserialized, THE Runtime SHALL deserialize it successfully with
   `goal_node = None` on every event and an empty goal tree. *(Property 7)*
5. WHEN a `MessageItem` payload carrying a `goal_node` tag is folded, THE Goal fold SHALL produce
   the same `SessionState.messages` content as folding the same `MessageItem` without the tag.
   *(Property 3, Property 7)*

### Requirement 6: Active-goal stack advancement semantics

**User Story:** As the Runtime, I want a stack with defined create/resolve/focus semantics that
persists across turns, so that nested subgoal work is modeled directly and attribution resumes at
the right node.

#### Acceptance Criteria

1. WHEN the active-goal stack pushes a freshly created subgoal, THE Runtime SHALL make that
   subgoal the active node.
2. WHEN the goal tool resolves node `x`, THE Runtime SHALL pop `x` and any of its still-active
   descendants from the active-goal stack so that the active node becomes `x`'s parent, or the
   root when `x` was a direct child of the root. *(Property 6)*
3. WHEN the goal tool resolves the root node, THE Runtime SHALL leave the root on the active-goal
   stack and SHALL continue to attribute subsequent turn-attributable events to the root.
   *(Property 6)*
4. WHEN a turn boundary occurs, THE Runtime SHALL preserve the active node pointer so that the
   node the agent last focused or opened remains the active node across the turn boundary and
   continues to own events in the next turn until the agent resolves or refocuses it.
5. WHEN a session is resumed, THE Runtime SHALL rehydrate the active-goal stack by folding the
   session's `Goal` events into a `GoalTree` and reconstructing the root-to-active path, so that
   attribution resumes at the previously active node. *(Property 3)*
6. WHEN the goal tool focuses an existing node, THE Runtime SHALL set the active node to that node
   and SHALL rebuild the active-goal stack as the root-to-node path.

### Requirement 7: Goal tool actions

**User Story:** As the agent, I want a goal tool with create/revise/resolve/focus/tree actions,
so that I can advance the goal tree while the runtime attributes events to it.

#### Acceptance Criteria

1. WHEN the goal tool `create` action is invoked with a non-empty `hypothesis`, an optional
   `parent` (defaulting to the active node), and optional `resolution_conditions`, THE GoalTool
   SHALL call `GoalStore::create`, SHALL push the new subgoal as the active node, and SHALL
   return the new node id.
2. WHEN the goal tool `revise` action is invoked for a node that exists in the folded tree, THE
   GoalTool SHALL call `GoalStore` to append a `GoalNodeRevised` event with the supplied fields.
3. WHEN the goal tool `resolve` action is invoked with an existing `id` and a non-`Open`
   `resolution`, THE GoalTool SHALL call `GoalStore::close`, SHALL pop the active-goal stack to
   the parent, and SHALL return the resolution outcome, including the `subtree_hash` when the
   subtree closes. *(Property 6)*
4. WHEN the goal tool `focus` action is invoked with an `id` that exists in the folded tree, THE
   GoalTool SHALL set the active node to that node without creating a node.
5. WHEN the goal tool `tree` action is invoked, THE GoalTool SHALL return the current projected
   goal tree without mutating any state.
6. IF the goal tool `create` action is invoked with an empty or whitespace-only `hypothesis`,
   THEN THE GoalTool SHALL return a tool error indicating the hypothesis is required, SHALL append
   no event, and SHALL leave the active-goal stack unchanged.
7. IF the goal tool `revise` or `resolve` action is invoked with an `id` that does not exist in
   the folded tree, THEN THE GoalTool SHALL return a tool error indicating the node was not found,
   SHALL append no event, and SHALL leave the active-goal stack unchanged.
8. THE GoalTool SHALL neither read from nor write to the flat `TaskList`, and THE Runtime SHALL
   keep the flat `TaskList` and the goal tree as distinct coexisting surfaces.

### Requirement 8: Shared-log persistence of goal events

**User Story:** As the Runtime, I want goal mutations committed through the same session-store
path as transcript events on one sequence-ordered log, so that attribution and replay are
coherent and survive resume.

#### Acceptance Criteria

1. WHEN the agent performs a goal mutation under `auto`, THE Runtime SHALL commit it as a
   `SessionEventPayload::Goal { event }` through the same `SessionStore::commit` path used for
   transcript events, via the SessionStoreGoalEventLog adapter.
2. WHEN goal events and transcript events belong to one session, THE SessionStore SHALL assign
   them sequence numbers on a single append-only log that is gap-free (each new sequence is
   exactly one greater than the previous appended sequence) and strictly increasing. *(Property
   10)*
3. WHEN the turn-attributable events attributed to a node `n` are collected from the shared log,
   THE Runtime SHALL yield a set ordered by the log's sequence. *(Property 10)*
4. WHEN a `Goal` payload is folded by `halter_protocol::fold::apply_event`, THE fold SHALL leave
   `SessionState.messages` unchanged, and THE Goal fold in `halter-goals` SHALL consume the
   payload into the `GoalTree`. *(Property 3)*
5. WHEN the SessionStoreGoalEventLog `append` is called with an `expected_head_sequence` that
   does not match the current head sequence of the session log, THE SessionStore SHALL reject the
   commit, append no event, and leave the log and folded `GoalTree` unchanged.
6. WHEN a session's committed log is folded twice, or resumed from any checkpoint and re-folded,
   THE Runtime SHALL produce an identical `goal_node` value on every turn-attributable event, so
   that attribution is a pure function of the committed log. *(Property 3)*

### Requirement 9: Goal Model reconciliation constraints

**User Story:** As a maintainer, I want event tags to be the sole attribution source of truth and
`subtree_hash` to stay keyed on the semantic contract, so that tagging never perturbs goal
versioning or closure guarantees.

#### Acceptance Criteria

1. THE Runtime SHALL treat the `goal_node` tags on the shared log as the source of truth for
   which artifacts belong to which node.
2. THE Runtime SHALL NOT auto-populate `GoalNode.tool_calls` from tagged tool events, and SHALL
   treat `GoalNode.tool_calls` as an optional agent-curated convenience projection recorded only
   via the goal tool `revise` action.
3. WHEN `subtree_hash` is computed for a resolved subtree, THE Goal Model SHALL derive it only
   from the semantic contract (`hypothesis`, `resolution_conditions`, `resolution`, `intent`,
   normalized `tool_calls`, and resolved children in canonical order) and SHALL NOT derive it
   from the raw tagged transcript. *(Property 8)*
4. WHERE a resolved subtree's semantic contract and resolved children are fixed, THE Goal Model
   SHALL compute an identical `subtree_hash` regardless of how many turn events were tagged to
   that node, their content, or their ordering. *(Property 8)*
5. WHEN the goal tool resolves the same node repeatedly under one `(node_id, subtree_hash)`, THE
   Goal Model SHALL enqueue exactly one induction job for that key. *(Property 9)*

### Requirement 10: Error handling and graceful degradation

**User Story:** As the Runtime, I want goal-tracking failures to surface as tool errors or
graceful degradation without blocking the turn, so that a goal-store outage never breaks agent
progress.

#### Acceptance Criteria

1. IF a goal-store OCC conflict or unavailability occurs during a goal-tool call, THEN THE
   GoalTool SHALL return a tool error, THE Runtime SHALL continue the turn, and THE Runtime SHALL
   leave the active node unchanged.
2. IF root creation (`ensure_root`) fails while emitting an event, THEN THE Runtime SHALL emit
   that event with `goal_node = None`, SHALL emit a `Warning` event, and SHALL NOT block or fail
   the turn.
3. IF the goal tool `focus` action targets a node that does not exist in the folded tree, THEN
   THE GoalTool SHALL return a focus error and SHALL leave the active node unchanged.
4. WHEN a legacy session log with no `Goal` events is folded, THE Runtime SHALL produce an empty
   goal tree and SHALL keep every enabled strategy operational. *(Property 7)*

### Requirement 11: Compaction seam for goal-oriented consumers (follow-on)

**User Story:** As a future compaction strategy, I want a per-session accessor to the goal tree
and tagged events, so that a goal-oriented consumer can read attribution through the context it is
already given.

> Follow-on / lower priority: this requirement adds the seam and its contract. The
> `GoalOrientedCompaction` strategy body (Requirement 12) depends on the design's sketch being
> fully specified in a later pass.

#### Acceptance Criteria

1. THE CompactionContext SHALL expose a `goal_log()` accessor that returns a per-session handle
   over the session's GoalStore and goal event log.
2. WHEN `goal_log()` is called for a session, THE CompactionContext SHALL return access to that
   session's folded goal tree and the goal-tagged events for that session.
3. THE CompactionContext SHALL scope the `goal_log()` handle to the per-session context rather
   than to the shared, per-session-stateless CompactionStrategy.

### Requirement 12: Goal-oriented compaction behavior (follow-on)

**User Story:** As a future compaction strategy, I want to preserve the open frontier verbatim and
fall back to the default summary when the tree is thin, so that goal-oriented compaction is never
worse than the default.

> Follow-on / lower priority: these acceptance criteria capture the target behavior of the
> sketched `GoalOrientedCompaction`; they depend on the design's sketch being fully specified in a
> later pass before implementation.

#### Acceptance Criteria

1. WHEN GoalOrientedCompaction compacts a session, THE GoalOrientedCompaction SHALL retain
   verbatim every event whose `goal_node` is a node in the open frontier (a node not in a
   fully-closed subtree). *(Property 11)*
2. WHERE events are attributed to a fully-closed subtree, THE GoalOrientedCompaction SHALL be
   permitted to compress only those events. *(Property 11)*
3. IF the goal tree is thin or the transcript is largely unmapped, THEN THE GoalOrientedCompaction
   SHALL produce the same result as ModelSummary. *(Property 12)*
4. WHEN GoalOrientedCompaction falls back to ModelSummary, THE GoalOrientedCompaction SHALL still
   retain every open-frontier event and SHALL produce a result no worse than the default
   strategy. *(Property 12)*
