# Requirements Document

## Introduction

**Goal Pace — hit your goals faster.** Goal Pace is a system for reaching goals faster in the
`halter` agentic analysis harness. Its headline subsystem is the **Goal Model** — the thing
that structures and tracks goals — and beneath it sit two acceleration layers (Tier 1 and
Tier 2 memory) whose whole job is to make reaching those goals faster. All three subsystems
are owned by this feature and ship in the `halter-goals` crate:

1. **Goal Model (the headline subsystem)** — the `GoalNode` subsystem that structures and
   tracks goals: a hypothesis-shaped, hierarchical, retroactively revisable goal tree with a
   resolution lifecycle (`Open | Accepted | Rejected | Inconclusive`), `subtree_hash`
   computation over resolved subtrees, `IntentSignature` derivation, persistence as
   append-only `GoalEvent`s folded into state on the existing event-sourced session store,
   and a goal-closure signal (via `halter-hooks`) that enqueues induction. It is distinct
   from and coexists with the flat `TaskList`.
2. **Tier 1 (acceleration layer)** — a deterministic exact-match result cache keyed by `tool +
   normalized_args + validity_token`, with argument normalization, a volatility-aware
   Validity Token Service (`ContentHash | Ttl | EventDriven`), per-volatility-class
   invalidation, and evidence-value storage. It makes goals faster by letting the agent skip
   redundant tool calls.
3. **Tier 2 (acceleration layer)** — procedural memory that makes goals faster by skipping
   rediscovery and re-planning: induction at goal closure (async, recurrence-gated, two-step
   Judge-then-Author, dedup/reinforce, idempotent on `(node_id, subtree_hash)`); store and
   retrieval (structured filter first, embedding tail recall, cheap re-rank); replay with a
   fixed decision order (answer-cache check then procedure replay with Tier 1 evidence
   re-validation); memory kinds (fragment/composite/negative); and two-mode answer caching
   (Mode A sound/pinnable, Mode B bounded-volatile opt-in).

The single non-negotiable guarantee is that an answer may be served from cache as verified
only when all of its validity tokens still hold; otherwise it must be explicitly marked
`believed-unverified`, opt-in per intent type, and never the default.

These requirements are derived from the approved design at
`.kiro/specs/halter-goals/design.md` and are consistent with its 20 numbered
Correctness Properties (Goal Model 1–4, Tier 1 5–10, Tier 2 11–20).

## Glossary

- **Goal_Model**: The headline subsystem of Goal Pace (in the `halter-goals` crate) that
  structures and tracks goals — maintains the goal tree, runs the resolution lifecycle,
  computes `subtree_hash`, derives `IntentSignature`s, persists the tree on the session store,
  and fires the goal-closure signal.
- **Goal_Store**: The Goal_Model interface (`create`/`revise`/`close`/`get`/`get_tree`)
  backed by the event-sourced `SessionStore`.
- **GoalNode**: A hypothesis-shaped unit of goal work with `id`, `parent`, `children`,
  `hypothesis`, `resolution_conditions`, `resolution`, `intent`, `tool_calls`, and
  `subtree_hash`.
- **GoalEvent**: An append-only session event payload recording a goal-tree mutation
  (`GoalNodeCreated`, `GoalNodeRevised`, `GoalNodeResolved`, `GoalClosed`).
- **Goal_Fold**: The deterministic function that folds a sequence of `GoalEvent`s into a
  `GoalTree`, mirroring `halter_protocol::fold`.
- **Session_Store**: The existing event-sourced store (`SqliteSessionStore` /
  `InMemorySessionStore`) providing an append-only, gap-free, sequence-ordered log with
  optimistic concurrency control on commit.
- **Resolution**: The state of a node: `Open`, `Accepted`, `Rejected`, or `Inconclusive`.
- **Closure**: The condition where a node's `resolution != Open` and every descendant is
  also closed; **goal closure** is the transition from not-closed to closed.
- **subtree_hash**: A stable, deterministic, order-insensitive content hash over a resolved
  node and its resolved subtree, used as the induction versioning and idempotency key.
- **IntentSignature**: The structured retrieval/merge key with `intent_type`, `target_type`,
  `target_ref`, and `scope`.
- **Tier_1**: The owned acceleration layer (in the `halter-goals` crate) providing the
  exact-match result cache and Validity Token Service, so the agent can skip redundant tool
  calls.
- **normalize_args**: The pure, deterministic, total argument-canonicalization function
  owned by Tier_1, producing `CanonicalJson`.
- **Validity_Token_Service**: The Tier_1 service that issues and re-validates tokens.
- **ValidityToken**: A volatility-aware token: `ContentHash(Sha256)`,
  `Ttl { issued_at, ttl }`, or `EventDriven { subscription, last_seen }`.
- **holds**: The per-variant re-validation predicate for a `ValidityToken`.
- **CacheEntry / EvidenceValue**: A stored Tier_1 entry holding the concrete result value
  under the token gating its validity.
- **Tier_2**: The owned procedural memory acceleration layer (in the `halter-goals` crate)
  covering induction, store/retrieval, replay, and answer caching, so the agent can skip
  rediscovery and re-planning.
- **Induction_Engine**: The Tier_2 subsystem that induces memories at goal closure.
- **Judge / Author**: The two distinct induction steps — Judge evaluates worthiness, Author
  writes the `Memory`.
- **Memory**: The durable, replayable distillation of how a class of goal was solved, of
  kind `Fragment`, `Composite`, or `Negative`.
- **Memory_Store**: The Tier_2 persistent store of memories (evidence contracts, not values).
- **Retrieval**: The Tier_2 subsystem that surfaces applicable memories for a new goal.
- **Replay_Engine**: The Tier_2 subsystem that decides, in fixed order, whether to serve a
  cached answer or replay a procedure.
- **CachedOutcome / AnswerMode**: A memory's optional goal-level answer and its serving mode
  (`SoundPinnable` = Mode A, `BoundedVolatile { max_age }` = Mode B).
- **believed-unverified**: The mandatory marker on any answer served without all validity
  tokens holding.
- **N**: The recurrence threshold at or above which induction may author a memory.
- **HEAD_MIN**: The threshold below which the structured filter head is "thin" and embedding
  recall is invoked.

## Requirements

### Requirement 1: Goal tree creation and hierarchy

**User Story:** As the Goal_Model, I want to create hypothesis-shaped goal nodes in a
parent/child hierarchy, so that goal work is represented as a structured tree distinct from
the flat task list.

#### Acceptance Criteria

1. WHEN `create` is called with an existing `session`, an optional `parent`, a non-empty
   `hypothesis`, and an `intent`, THE Goal_Store SHALL append exactly one `GoalNodeCreated`
   event through `SessionStore::commit`, SHALL receive a gap-free monotonic sequence number
   for that event, and SHALL return the assigned node id, which SHALL be unique among all
   node ids in the session's folded tree.
2. WHEN a `GoalNodeCreated` event is folded, THE Goal_Fold SHALL produce exactly one node
   whose id equals the returned id, whose `resolution` equals `Open`, and whose `hypothesis`
   and `intent` equal the supplied values, with an empty `children` set.
3. WHERE the `create` call supplied a `parent`, THE Goal_Fold SHALL add the new node id to
   that parent's `children` set exactly once and SHALL leave all other nodes' fields
   unchanged.
4. IF `create` is called with a `parent` that does not exist in the session's folded tree,
   THEN THE Goal_Store SHALL return an error result indicating the parent was not found,
   SHALL append no event, and SHALL leave the folded tree and the event log unchanged.
5. IF `create` is called with an empty `hypothesis`, THEN THE Goal_Store SHALL return an
   error result indicating the hypothesis is required, SHALL append no event, and SHALL leave
   the folded tree and the event log unchanged.
6. THE Goal_Model SHALL maintain the goal tree independently of the flat `TaskList` and SHALL
   neither read from nor write to `TaskList`.

### Requirement 2: Retroactive revision with append-only semantics

**User Story:** As the Goal_Model, I want to retroactively revise goal nodes as new evidence
arrives, so that the projected tree stays accurate while the event log remains append-only.

#### Acceptance Criteria

1. WHEN `revise` is called for a node that exists in the session's folded tree, THE
   Goal_Store SHALL append exactly one `GoalNodeRevised` event through `SessionStore::commit`
   and SHALL NOT mutate or remove any event already present in the log.
2. WHEN goal-tree events are appended, THE Session_Store SHALL assign sequence numbers that
   are gap-free (each new sequence is exactly one greater than the previous appended
   sequence) and monotonically increasing.
3. WHEN a revision changes a node's resolution conditions, tool calls, or child set, THE
   Goal_Fold SHALL produce a projected tree that is equal to the fold of all appended events
   in sequence order.
4. WHERE a revision re-opens a node or any of its descendants, THE Goal_Fold SHALL set each
   affected node's `resolution` to `Open` and SHALL supersede any previously computed closure
   for that subtree such that the subtree is no longer reported as closed.
5. WHERE a revision leaves the resolved subtree of a node structurally unchanged, THE
   Goal_Fold SHALL compute the same `subtree_hash` for that node as before the revision.
6. IF `revise` is called for a node that does not exist in the session's folded tree, THEN
   THE Goal_Store SHALL return an error result indicating the node was not found, SHALL append
   no event, and SHALL leave the folded tree and the event log unchanged.

### Requirement 3: Deterministic goal tree fold and replay

**User Story:** As the Goal_Model, I want folding a session's goal events to deterministically
reconstruct the goal tree, so that replay yields the same tree on every store backend.

#### Acceptance Criteria

1. WHEN a session's `GoalEvent`s are folded in ascending sequence order, THE Goal_Fold SHALL
   reconstruct a `GoalTree` whose node set, parent/child hierarchy, per-node `resolution`, and
   per-node `subtree_hash` are identical on every replay of that same event sequence.
2. WHEN the same `GoalEvent` sequence for a session is folded on `InMemorySessionStore` and on
   `SqliteSessionStore`, THE Goal_Fold SHALL reconstruct `GoalTree`s that are equal in node
   set, hierarchy, per-node `resolution`, and per-node `subtree_hash`.
3. WHEN goal-tree `GoalEvent`s and transcript events belong to one session, THE Session_Store
   SHALL append and commit them to a single append-only, gap-free, strictly increasing
   sequence-ordered log through one `commit` path.
4. IF a `commit` of `GoalEvent`s is attempted with an `expected_head_sequence` that does not
   match the current head sequence of the session log, THEN THE Session_Store SHALL reject the
   commit, append no event, and leave the log and folded `GoalTree` unchanged.

### Requirement 4: Resolution lifecycle and goal closure

**User Story:** As the Goal_Model, I want to record node resolutions and detect goal closure
over resolved subtrees, so that closure can trigger induction at the right boundary.

#### Acceptance Criteria

1. WHEN `close` is called with a `resolution` of `Accepted`, `Rejected`, or `Inconclusive`
   for a node that exists in the folded tree, THE Goal_Store SHALL append exactly one
   `GoalNodeResolved` event recording that `resolution`.
2. WHILE a node's `resolution` is `Open`, OR at least one node in its subtree has
   `resolution = Open`, THE Goal_Model SHALL treat the node as not closed, SHALL append no
   `GoalClosed` event for it, and SHALL return a `ClosureOutcome` with `closed = false` and no
   `subtree_hash`.
3. WHEN a node's `resolution` becomes non-`Open` and every node in its subtree has
   `resolution` non-`Open`, THE Goal_Model SHALL treat the node as closed, SHALL append
   exactly one `GoalClosed` event carrying the computed `subtree_hash`, and SHALL return a
   `ClosureOutcome` with `closed = true` and that `subtree_hash`.
4. IF `close` is called with `resolution = Open`, THEN THE Goal_Store SHALL return an error
   indicating that closure requires a non-`Open` resolution, and SHALL append no
   `GoalNodeResolved` or `GoalClosed` event.
5. IF `close` is called for a node that does not exist in the folded tree, THEN THE Goal_Store
   SHALL return an error indicating the node is not found, and SHALL append no event.
6. WHERE a `GoalNodeRevised` event re-opens a previously closed node or any of its descendants
   and the node's subtree subsequently closes again under a `subtree_hash` distinct from a
   prior closure, THE Goal_Model SHALL treat each distinct `(node_id, subtree_hash)` closure
   as a separate closure and SHALL dispatch the closure signal exactly once per distinct
   `(node_id, subtree_hash)`.

### Requirement 5: Deterministic subtree_hash over resolved subtrees

**User Story:** As the Goal_Model, I want `subtree_hash` to be a deterministic,
order-insensitive content hash of a resolved subtree, so that it can serve as the induction
versioning and idempotency key.

#### Acceptance Criteria

1. WHEN `subtree_hash` is computed for a subtree in which every node has `resolution`
   non-`Open`, THE Goal_Model SHALL derive the hash only from the node's `hypothesis`,
   `resolution_conditions`, `resolution`, `intent`, and canonically normalized `tool_calls`,
   combined with each resolved child's `subtree_hash` in canonical child order.
2. WHEN two resolved subtrees are structurally equal, THE Goal_Model SHALL compute equal
   `subtree_hash` values regardless of child insertion order or incidental encoding of the
   intent-relevant fields.
3. WHEN `subtree_hash` is recomputed for the same resolved subtree, THE Goal_Model SHALL
   compute a value identical to every prior computation for that subtree.
4. WHEN a revision leaves a resolved subtree structurally unchanged, THE Goal_Model SHALL
   compute a `subtree_hash` equal to the value computed before the revision.
5. WHEN a revision changes the resolved subtree in any intent-relevant field or in any
   resolved child's `subtree_hash`, THE Goal_Model SHALL compute a `subtree_hash` different
   from the value computed before the revision.

### Requirement 6: Goal-closure signal enqueues induction exactly once

**User Story:** As the Goal_Model, I want goal closure to dispatch a hook signal that enqueues
induction exactly once per distinct `(node_id, subtree_hash)`, so that recurring or duplicate
closure signals never author duplicate work.

#### Acceptance Criteria

1. WHEN a node's subtree transitions from not-closed to fully closed (the node and every
   descendant have `resolution != Open`) under a `subtree_hash` value that has not previously
   been recorded for that `node_id`, THE Goal_Model SHALL append exactly one `GoalClosed`
   event carrying `(node_id, subtree_hash)`.
2. WHEN a `GoalClosed` event carrying `(node_id, subtree_hash)` is appended for the first
   time for that key, THE Goal_Model SHALL dispatch exactly one closure signal carrying
   `(node_id, subtree_hash)` through `halter-hooks`.
3. WHEN a closure signal carrying `(node_id, subtree_hash)` is dispatched, THE Goal_Model
   SHALL enqueue exactly one induction job carrying that same `(node_id, subtree_hash)` key,
   such that the total count of enqueued induction jobs for that distinct key is exactly one.
4. IF a `GoalClosed` event is re-fired for a `(node_id, subtree_hash)` that has already been
   recorded, THEN THE Goal_Model SHALL append no additional `GoalClosed` event, dispatch no
   additional closure signal, and enqueue no additional induction job for that key.
5. WHEN a revision produces a `subtree_hash` for a `node_id` that differs from every
   previously recorded `subtree_hash` for that `node_id` and the node's subtree becomes fully
   closed under that differing hash, THE Goal_Model SHALL treat that closure as a distinct
   induction trigger and enqueue exactly one induction job for the new `(node_id,
   subtree_hash)` key.

### Requirement 7: IntentSignature derivation

**User Story:** As the Goal_Model, I want each goal node to carry a structured
`IntentSignature`, so that Tier_2 can use it as the retrieval and merge key.

#### Acceptance Criteria

1. WHEN a goal node is created or revised, THE Goal_Model SHALL derive and attach to that
   node exactly one `IntentSignature` populated with all four fields: `intent_type`,
   `target_type`, `target_ref`, and `scope`.
2. IF deriving the `IntentSignature` for a created or revised node cannot resolve a value for
   any of `intent_type`, `target_type`, `target_ref`, or `scope`, THEN THE Goal_Model SHALL
   reject the create or revise operation, retain the node's prior state unchanged, and return
   an error identifying the unresolved field or fields.
3. WHILE a goal node exists in the folded tree, THE Goal_Model SHALL expose that node's
   attached `IntentSignature`, with all four fields readable, to Tier_2 as the retrieval and
   merge key.

### Requirement 8: Argument normalization / canonicalization

**User Story:** As Tier_1, I want tool arguments canonicalized deterministically, so that
semantically equal calls key the same cache entry.

#### Acceptance Criteria

1. WHEN `normalize_args` is called with a tool and raw arguments, THE Tier_1 SHALL return a
   `CanonicalJson` value whose byte sequence is identical across repeated invocations with the
   same tool and logically equal arguments within the same process and across processes.
2. WHEN `normalize_args` is called twice with a given tool and two argument sets that are
   logically equal for that tool, THE Tier_1 SHALL return two `CanonicalJson` values whose
   byte sequences are equal.
3. WHEN two argument sets for a given tool differ only by object-key ordering, by
   insignificant whitespace, or by equivalent scalar encodings of the same value, THE Tier_1
   SHALL return byte-equal `CanonicalJson` for both.
4. WHEN two argument sets for a given tool are NOT logically equal, THE Tier_1 SHALL return
   `CanonicalJson` values whose byte sequences differ.
5. WHEN two argument sets produce byte-equal `CanonicalJson` for a given tool, THE Tier_1
   SHALL resolve both to the same cache entry, and WHEN they produce differing `CanonicalJson`,
   THE Tier_1 SHALL resolve them to distinct cache entries.
6. IF `normalize_args` receives raw arguments that cannot be canonicalized for the given
   tool, THEN THE Tier_1 SHALL reject the request with an error indicating the arguments are
   not canonicalizable, and SHALL NOT create or read any cache entry for that request.

### Requirement 9: Validity token issuance

**User Story:** As Tier_1, I want to issue volatility-aware validity tokens for a source, so
that each cached result carries the correct way to check whether it is still valid.

#### Acceptance Criteria

1. WHEN `issue_token` is called for a `Pinnable` source whose content is readable at
   issuance, THE Validity_Token_Service SHALL return a `ContentHash` token whose hash equals
   the SHA-256 of the source content read at issuance time.
2. WHEN `issue_token` is called for a `Volatile` source, THE Validity_Token_Service SHALL
   return a `Ttl` token whose `issued_at` equals the issuance time and whose `ttl` equals the
   descriptor's `ttl`.
3. WHEN `issue_token` is called for a `Signalled` source, THE Validity_Token_Service SHALL
   return an `EventDriven` token whose `subscription` equals the descriptor's subscription and
   whose `last_seen` equals the current `EventSeq` observed on that subscription at issuance
   time.
4. WHEN a token is returned by `issue_token`, THE Validity_Token_Service SHALL return a token
   whose variant matches the source's volatility class (`Pinnable` yields `ContentHash`,
   `Volatile` yields `Ttl`, `Signalled` yields `EventDriven`).
5. WHEN a token is returned by `issue_token`, THE Validity_Token_Service SHALL ensure that
   evaluating `holds` on that token at issuance time returns true.
6. IF `issue_token` is called for a `Pinnable` source whose content is not readable at
   issuance, THEN THE Validity_Token_Service SHALL reject the request with an error indicating
   the source content is unreadable, and SHALL NOT return a token.

### Requirement 10: Token re-validation semantics (holds)

**User Story:** As Tier_1, I want `holds` to decide token validity exactly per variant, so
that cache validity is precise for each volatility class.

#### Acceptance Criteria

1. WHEN `holds` is evaluated for a `ContentHash(h)` token and the source is reachable, THE
   Validity_Token_Service SHALL return true if and only if the SHA-256 of the source's current
   content equals `h`.
2. WHEN `holds` is evaluated for a `Ttl { issued_at, ttl }` token, THE Validity_Token_Service
   SHALL return true if and only if `now()` is strictly less than `issued_at + ttl`.
3. WHEN `holds` is evaluated for an `EventDriven { subscription, last_seen }` token, THE
   Validity_Token_Service SHALL return true if and only if no event with `EventSeq` strictly
   greater than `last_seen` has been observed on `subscription`.
4. WHEN `holds` is evaluated for any token, THE Validity_Token_Service SHALL NOT modify the
   token, the source content, or any stored evidence.
5. IF the source referenced by a token is unreachable at the time `holds` is evaluated, THEN
   THE Validity_Token_Service SHALL return false for that token.

### Requirement 11: Cache read path and no stale value served

**User Story:** As Tier_1, I want cache reads to return a value only when its token still
holds, so that a stale value is never served.

#### Acceptance Criteria

1. WHEN `get` is called with arguments already canonicalized by `normalize_args` and an entry
   exists for the same `tool + normalized_args` whose validity token holds per its variant
   rule (ContentHash: current content hashes to the stored hash; Ttl: `now() < issued_at +
   ttl`; EventDriven: no event newer than `last_seen` observed on the subscription), THE
   Tier_1 SHALL return `Hit(value)` carrying that entry's stored evidence value.
2. WHEN `get` is called with canonicalized arguments and an entry exists for the same `tool +
   normalized_args` but its validity token does not hold per its variant rule, THE Tier_1
   SHALL return `Stale` and SHALL NOT return the entry's evidence value.
3. WHEN `get` is called with canonicalized arguments and no entry exists for the same `tool +
   normalized_args`, THE Tier_1 SHALL return `Miss`.
4. WHEN `get` returns `Hit(value)`, THE Tier_1 SHALL guarantee the entry's validity token
   holds at read time and SHALL NOT mutate any stored evidence value during the read.
5. IF `get` is called with canonicalized arguments, an entry exists, and the source
   referenced by the entry's validity token is unreachable so that its `holds` check cannot be
   completed, THEN THE Tier_1 SHALL treat the token as not held and return `Stale`.

### Requirement 12: Cache write path and write-then-read freshness

**User Story:** As Tier_1, I want writing fresh evidence under a freshly issued token to make
it immediately readable, so that results are cached reproducibly and stale entries are
replaced.

#### Acceptance Criteria

1. WHEN `put` is called with arguments canonicalized by `normalize_args`, a validity token
   that holds at write time, and an evidence value, THE Tier_1 SHALL store the evidence value
   under that token and SHALL replace any prior entry for the same `tool + normalized_args`,
   leaving exactly one entry for that key.
2. WHEN a `put` stores an evidence value under a token that holds, THE Tier_1 SHALL return
   `Hit(value)` for an immediately following `get` of the same `tool + normalized_args` for as
   long as that token continues to hold, and SHALL return `Stale` for any `get` of that key
   issued after the token stops holding.
3. IF `put` is called with a validity token that does not hold at write time, THEN THE Tier_1
   SHALL reject the write, SHALL leave any prior entry for the same `tool + normalized_args`
   unchanged, and SHALL return an indication that the write was rejected because the token was
   not fresh.

### Requirement 13: Evidence re-validation entry point for replay

**User Story:** As Tier_1, I want to expose a re-validation entry point, so that Tier_2 replay
can check whether a recorded token still holds without necessarily returning the value.

#### Acceptance Criteria

1. WHEN `revalidate` is called with a tool, arguments canonicalized by `normalize_args`, and
   a recorded validity token, THE Tier_1 SHALL return `Fresh` if that token holds for the
   current source per its variant rule and `Stale` otherwise, without returning any evidence
   value and without mutating any stored evidence value.
2. THE Tier_1 SHALL own all concrete evidence values, and Tier_2 SHALL obtain evidence
   freshness only through Tier_1.
3. IF `revalidate` is called and the source referenced by the recorded validity token is
   unreachable so that its `holds` check cannot be completed, THEN THE Tier_1 SHALL treat the
   token as not held and return `Stale`.

### Requirement 14: Recurrence-gated induction

**User Story:** As the Induction_Engine, I want induction to be lazily gated on recurrence, so
that cold or one-off goals incur no authoring cost.

#### Acceptance Criteria

1. WHEN induction runs for a resolved node, THE Induction_Engine SHALL record exactly one
   occurrence of that node's intent before evaluating the recurrence gate, regardless of the
   gate outcome.
2. WHILE `recurrence_count(intent)` is strictly less than the configured threshold `N`, THE
   Induction_Engine SHALL author no memory for that intent and SHALL NOT invoke the Judge
   step.
3. WHEN `recurrence_count(intent)` is greater than or equal to the configured threshold `N`,
   THE Induction_Engine SHALL proceed to the Judge step.
4. IF `recurrence_count(intent)` is strictly less than the configured threshold `N` after the
   occurrence is recorded, THEN THE Induction_Engine SHALL terminate the induction run without
   authoring a memory.
5. WHEN induction is enqueued at goal closure, THE Induction_Engine SHALL run it as an
   asynchronous job that returns control to the goal-closure caller before the Judge step
   executes, so that goal closure does not block on induction.

### Requirement 15: Two-step Judge-then-Author authoring in a clean context

**User Story:** As the Induction_Engine, I want authoring to run as two distinct steps in a
clean context, so that memories are judged before being written and carry no residual
reasoning from the original goal work.

#### Acceptance Criteria

1. WHEN induction proceeds past the recurrence gate, THE Induction_Engine SHALL invoke a
   Judge step and a separate Author step as two distinct invocations and SHALL NOT combine
   judging and authoring into a single invocation.
2. WHEN the Judge step runs, THE Induction_Engine SHALL obtain from it a verdict of approve or
   decline together with a rationale, and the Judge step SHALL produce no `Memory`.
3. IF the Judge step returns a decline verdict, THEN THE Induction_Engine SHALL author no
   memory and SHALL persist the decline rationale.
4. WHEN the Judge step returns an approve verdict, THE Induction_Engine SHALL run the Author
   step to produce a candidate `Memory`.
5. WHEN the Judge step and the Author step run, THE Induction_Engine SHALL run each in a
   context that contains none of the messages, tool-call transcript, or reasoning state
   carried over from the original goal work.
6. IF the Judge step approves but the Author step cannot produce a well-formed `Memory`, THEN
   THE Induction_Engine SHALL insert no memory, SHALL persist a record of the failure, SHALL
   leave the existing contents of the Memory_Store unchanged, and SHALL allow a subsequent
   recurrence to re-trigger induction for the same intent.

### Requirement 16: Induction idempotency, dedup, and versioning

**User Story:** As the Induction_Engine, I want induction to be idempotent and versioned on
`(node_id, subtree_hash)`, so that re-inducing the same resolved subtree never creates a
duplicate memory.

#### Acceptance Criteria

1. WHEN induction runs any number of times for the same `(node_id, subtree_hash)`, THE
   Memory_Store SHALL contain at most one memory for that key.
2. WHEN a candidate memory matches an existing memory by intent and plan, THE Induction_Engine
   SHALL reinforce the existing memory by updating its reinforcement bookkeeping and SHALL NOT
   insert a new memory.
3. WHEN two closures concurrently enqueue induction for the same `(node_id, subtree_hash)`,
   THE Memory_Store SHALL serialize the operations on that idempotency key so that exactly one
   operation performs an insert and every other operation performs a reinforce.

### Requirement 17: Memory kinds

**User Story:** As Tier_2, I want memories classified as fragment, composite, or negative, so
that both successful procedures and known dead-ends prune search on recurrence.

#### Acceptance Criteria

1. THE Memory_Store SHALL persist every memory with exactly one `kind` value drawn from the
   set {`Fragment`, `Composite`, `Negative`}, and SHALL reject on write any memory whose
   `kind` is absent or outside that set with an error indicating an invalid memory kind.
2. WHERE a memory is of kind `Negative`, THE Tier_2 SHALL record a non-empty
   `evidence_contract` on it so that the dead-end applies only while that evidence holds.
3. IF a memory of kind `Negative` is written without a non-empty `evidence_contract`, THEN
   THE Memory_Store SHALL reject the write with an error indicating a missing evidence
   contract and SHALL NOT persist the memory.
4. WHEN Tier_2 records an `evidence_contract` on any memory, THE Tier_2 SHALL store only the
   contract fields (`tool`, `normalized_args`, `validity_token`) and SHALL NOT store concrete
   evidence values.
5. IF a memory is written with a present `cached_outcome` whose `validity_tokens` is empty,
   THEN THE Memory_Store SHALL reject the write with an error indicating missing validity
   tokens and SHALL NOT persist the memory.

### Requirement 18: Structured-first retrieval with embedding tail recall

**User Story:** As Retrieval, I want to filter memories by structured signature first and use
embedding recall only for a thin head, so that retrieval is context-frugal and structured
matches take precedence.

#### Acceptance Criteria

1. WHEN `retrieve_memories` is called with a fully populated `IntentSignature`, THE Retrieval
   SHALL apply the structured filter over `intent_type`, `target_type`, `target_ref`, and
   `scope` to assemble the structured head before invoking any embedding recall.
2. WHILE the structured head holds at least `HEAD_MIN` candidates, THE Retrieval SHALL NOT
   invoke embedding recall and SHALL use the structured head as the candidate set.
3. WHEN the structured head holds fewer than `HEAD_MIN` candidates, THE Retrieval SHALL invoke
   embedding recall for the tail bounded by `TAIL_LIMIT` and SHALL form the candidate set as
   the deduplicated union of the head and the tail, retaining exactly one entry per distinct
   `MemoryId`.
4. WHEN the candidate set is assembled, THE Retrieval SHALL exclude every candidate whose
   `applicability` guard is not satisfied by the supplied `IntentSignature`.
5. WHEN Retrieval returns results, THE Retrieval SHALL order the returned memories by cheap
   re-rank score in descending order.
6. THE Retrieval SHALL return only compact `Memory` objects and SHALL NOT return raw
   transcripts.
7. IF the embedding recall backend is unavailable when tail recall would otherwise be
   invoked, THEN THE Retrieval SHALL return the structured-filter-only candidate set without
   an error.

### Requirement 19: Fixed replay decision order

**User Story:** As the Replay_Engine, I want a fixed decision order that checks the answer
cache before procedure replay, so that the cheapest sound path is always tried first.

#### Acceptance Criteria

1. WHEN `decide_replay` runs for a memory whose applicability matches the request, THE
   Replay_Engine SHALL evaluate the answer-cache check before evaluating the procedure-replay
   fallback.
2. IF the answer-cache check yields no answer that can be served (no sound cached answer and
   no eligible Mode B answer), THEN THE Replay_Engine SHALL fall through to the
   procedure-replay fallback.
3. IF a memory has no `cached_outcome`, THEN THE Replay_Engine SHALL skip the answer-cache
   check and SHALL proceed directly to the procedure-replay fallback.

### Requirement 20: Answer soundness for verified serving (non-negotiable)

**User Story:** As the Replay_Engine, I want a cached answer served as verified only when all
its validity tokens hold, so that a stale answer is never served as verified.

#### Acceptance Criteria

1. WHEN `decide_replay` finds a `SoundPinnable` cached outcome and every token in its
   `validity_tokens` holds at serve time, THE Replay_Engine SHALL return `ServeAnswer` with
   `verified = true`.
2. IF a cached outcome is `SoundPinnable` and at least one token in its `validity_tokens` does
   not hold at serve time, THEN THE Replay_Engine SHALL NOT return that answer as verified and
   SHALL fall through to the procedure-replay fallback.
3. WHEN the Replay_Engine returns an answer with `verified = true`, THE Replay_Engine SHALL
   guarantee that every token in the answer's `validity_tokens` held at serve time.
4. IF the number of tokens confirmed to hold is fewer than the total count of the answer's
   `validity_tokens`, THEN THE Replay_Engine SHALL NOT return that answer as verified.

### Requirement 21: Mode B bounded-volatile serving is opt-in and always marked

**User Story:** As the Replay_Engine, I want bounded-volatile answers served only when opted
in and within `max_age`, and always marked believed-unverified, so that unverified answers
are never the default.

#### Acceptance Criteria

1. WHEN `decide_replay` finds a `BoundedVolatile` cached outcome, the request's intent type
   has opted in, and the answer age is less than or equal to the outcome's configured
   `max_age`, THE Replay_Engine SHALL return `ServeAnswer` with `verified = false` marked
   believed-unverified.
2. WHEN the Replay_Engine returns an answer with `verified = false`, THE Replay_Engine SHALL
   mark it believed-unverified and SHALL have confirmed the answer's intent type has opted in
   before returning it.
3. IF a `BoundedVolatile` intent type has not opted in, THEN THE Replay_Engine SHALL NOT serve
   the answer and SHALL fall through to the procedure-replay fallback.
4. IF a `BoundedVolatile` answer's age exceeds the outcome's configured `max_age`, THEN THE
   Replay_Engine SHALL NOT serve the answer and SHALL fall through to the procedure-replay
   fallback.
5. WHERE no intent type has opted into Mode B, THE Replay_Engine SHALL NOT activate Mode B.

### Requirement 22: Mode A purity

**User Story:** As Tier_2, I want Mode A answers to depend only on pinnable tokens, so that a
sound answer is always re-verifiable by content hash.

#### Acceptance Criteria

1. WHERE a memory's `cached_outcome.mode` is `SoundPinnable`, THE Memory_Store SHALL require
   every token in its `validity_tokens` to be a pinnable `ContentHash` token.
2. IF at least one token in a candidate cached answer's `validity_tokens` is volatile (`Ttl`
   or `EventDriven`), THEN THE Memory_Store SHALL require the mode to be `BoundedVolatile` or
   SHALL store no cached answer.
3. WHEN a `cached_outcome` is present, THE Memory_Store SHALL require its `validity_tokens` to
   contain at least one token.

### Requirement 23: Evidence re-validation on procedure replay

**User Story:** As the Replay_Engine, I want every evidence item re-validated through Tier_1
before a plan is trusted, so that a replay's freshness reflects the current source state.

#### Acceptance Criteria

1. WHEN the Replay_Engine performs a procedure replay, THE Replay_Engine SHALL re-validate
   every evidence item in the memory's `evidence_contract` through `Tier1Cache::revalidate`
   before trusting the plan.
2. WHEN a procedure replay reports `evidence_fully_fresh`, THE Replay_Engine SHALL guarantee
   every evidence item returned `Fresh` from `Tier1Cache::revalidate`.
3. IF any evidence item is re-validated as `Stale`, THEN THE Replay_Engine SHALL NOT report
   `evidence_fully_fresh` for that replay.
4. IF Tier_1 is unavailable during replay, THEN THE Replay_Engine SHALL treat all evidence as
   stale, SHALL NOT serve a verified answer, and SHALL serve a believed-unverified answer only
   where Mode B opt-in and `max_age` conditions hold.
