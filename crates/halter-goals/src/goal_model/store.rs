//! The [`GoalStore`] trait, the [`ClosureOutcome`] result, the [`GoalStoreError`]
//! error space, and the [`GoalEventLog`] append/replay abstraction that realizes
//! the "one gap-free, sequence-ordered log through one commit path" contract
//! (Requirements 1.1, 3.3, 3.4).
//!
//! # Where goal events live (design intent vs. this task's pragmatic shape)
//!
//! The design's intent is that goal-tree [`GoalEvent`]s and transcript events
//! **share one session log**: goal mutations would ride the existing
//! event-sourced `SessionStore` as a `SessionEventPayload` variant, committed
//! through the single [`SessionStore::commit`](halter_session::SessionStore::commit)
//! path so they get gap-free monotonic sequences and optimistic-concurrency
//! control via `expected_head_sequence` (Requirement 3.3, 3.4).
//!
//! `halter_protocol::SessionEventPayload` is, however, a **closed enum** with no
//! variant for a `GoalEvent`, and adding one is a `halter-protocol` change that
//! is out of scope for this crate/task. Rather than fork the protocol here, this
//! task introduces a small [`GoalEventLog`] abstraction **owned by
//! `halter-goals`** that preserves exactly the contract the `SessionStore` log
//! guarantees:
//!
//! - append assigns **gap-free monotonic** sequences starting at `head + 1`,
//! - [`replay`](GoalEventLog::replay) returns events in ascending sequence order,
//! - an append with a non-matching `expected_head_sequence` is rejected as a
//!   [`conflict`](GoalStoreError::Conflict), appending nothing and leaving the
//!   log unchanged (optimistic concurrency control, Requirement 3.4).
//!
//! An in-memory implementation ([`InMemoryGoalEventLog`]) backs the log with a
//! locked `Vec` now, so the `GoalStore` and its tests run without any protocol
//! change. The future protocol integration is localized to **one adapter**: a
//! `SessionStore`-backed `GoalEventLog` impl that maps `append`/`replay` onto
//! `commit`/`replay` once a `SessionEventPayload::Goal(GoalEvent)` variant
//! exists. Nothing above the trait — the `GoalStore` and every Tier that folds
//! goal events — changes when that swap happens, because the contract modeled
//! here is identical to the session store's.
//!
//! # What this task delivers
//!
//! - The [`GoalStore`] trait (`create`, `revise`, `close`, `get`, `get_tree`),
//!   the [`ClosureOutcome`] result, and the [`GoalStoreError`] error space that
//!   the method tasks (8.2, 8.3, 8.5, 8.6) fill in.
//! - The [`GoalEventLog`] trait plus [`InMemoryGoalEventLog`], with unit tests
//!   locking in gap-free monotonic sequencing, in-order replay, and
//!   `expected_head_sequence` conflict rejection (Requirement 3.4).
//! - A concrete [`EventLogGoalStore`] skeleton wired to a [`GoalEventLog`], with
//!   the **shared append+fold helpers fully implemented** (they realize the
//!   single-ordered-log append path of Requirements 3.3/3.4): [`load_tree`]
//!   folds replayed events into a [`GoalTree`], and [`append_with_occ`] appends
//!   under optimistic concurrency at the log head. The public `create`/`revise`/
//!   `close`/`get`/`get_tree` bodies are left for tasks 8.2/8.3/8.5/8.6.
//!
//! [`load_tree`]: EventLogGoalStore::load_tree
//! [`append_with_occ`]: EventLogGoalStore::append_with_occ

use async_trait::async_trait;
use halter_protocol::SessionId;
use thiserror::Error;

use crate::types::{GoalNodeId, IntentSignature, SubtreeHash};

use super::closure::{ClosureSignal, NoopInductionQueue, SharedInductionQueue};
use super::fold::fold_goal_events;
use super::model::{GoalEvent, GoalNode, GoalNodeRevision, GoalTree, Resolution};
use super::subtree_hash::subtree_hash;

/// The outcome of a [`GoalStore::close`] call.
///
/// When the resolution closes the node's whole resolved subtree, `closed` is
/// `true` and `subtree_hash` carries the computed hash of that resolved subtree
/// (Requirement 4.3). When the subtree is not yet fully closed, `closed` is
/// `false` and `subtree_hash` is `None` (Requirement 4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureOutcome {
    /// Whether the node's resolved subtree became fully closed.
    pub closed: bool,
    /// The computed subtree hash when `closed` is `true`, else `None`.
    pub subtree_hash: Option<SubtreeHash>,
}

impl ClosureOutcome {
    /// The outcome for a node whose subtree is not yet fully closed.
    #[must_use]
    pub fn not_closed() -> Self {
        Self {
            closed: false,
            subtree_hash: None,
        }
    }

    /// The outcome for a node whose subtree became fully closed under `hash`.
    #[must_use]
    pub fn closed(hash: SubtreeHash) -> Self {
        Self {
            closed: true,
            subtree_hash: Some(hash),
        }
    }
}

/// The error space of the [`GoalStore`] and its backing [`GoalEventLog`].
///
/// Variants cover every rejection path the method tasks (8.2/8.3/8.5/8.6) need:
/// a missing parent on create ([`ParentNotFound`](Self::ParentNotFound)), an
/// empty hypothesis ([`EmptyHypothesis`](Self::EmptyHypothesis)), a missing node
/// on revise/close/get ([`NodeNotFound`](Self::NodeNotFound)), a request to
/// close with an `Open` resolution ([`OpenResolution`](Self::OpenResolution)),
/// an optimistic-concurrency clash on append ([`Conflict`](Self::Conflict)), a
/// failed intent derivation ([`IntentDerivation`](Self::IntentDerivation)), and
/// a backend/log failure ([`Log`](Self::Log)).
#[derive(Debug, Error)]
pub enum GoalStoreError {
    /// `create` named a `parent` absent from the session's folded tree
    /// (Requirement 1.4). No event is appended and the log/tree are unchanged.
    #[error("parent node not found: {0}")]
    ParentNotFound(GoalNodeId),

    /// `create` was called with an empty (or blank) hypothesis (Requirement
    /// 1.5). No event is appended and the log/tree are unchanged.
    #[error("hypothesis is required and must be non-empty")]
    EmptyHypothesis,

    /// `revise`/`close`/`get` referenced a node absent from the session's
    /// folded tree (Requirements 2.6, 4.5). No event is appended.
    #[error("goal node not found: {0}")]
    NodeNotFound(GoalNodeId),

    /// `close` was called with `resolution = Open`; closure requires a
    /// non-`Open` resolution (Requirement 4.4). No event is appended.
    #[error("closure requires a non-Open resolution")]
    OpenResolution,

    /// A commit was attempted with an `expected_head_sequence` that did not
    /// match the current log head — the log advanced concurrently (Requirement
    /// 3.4). No event is appended and the log/tree are unchanged.
    #[error(
        "goal event log advanced concurrently (expected head {expected}, found {actual})"
    )]
    Conflict {
        /// The head sequence the caller expected.
        expected: u64,
        /// The head sequence the log actually held.
        actual: u64,
    },

    /// Deriving the node's [`IntentSignature`] on create/revise could not
    /// resolve one or more fields (Requirement 7.2). No event is appended.
    #[error("cannot derive intent signature: {0}")]
    IntentDerivation(#[from] super::intent::IntentDerivationError),

    /// The backing [`GoalEventLog`] (or, in the future, the `SessionStore`)
    /// failed for a reason other than an optimistic-concurrency conflict.
    #[error("goal event log backend error: {0}")]
    Log(String),
}

/// A single committed goal event together with the gap-free monotonic sequence
/// the log assigned it.
pub type SequencedGoalEvent = (u64, GoalEvent);

/// The append/replay abstraction over the shared, append-only, gap-free,
/// sequence-ordered goal-event log (Requirements 3.3, 3.4).
///
/// This trait models the exact contract the event-sourced `SessionStore`
/// guarantees for the single session log, so that a future adapter can back it
/// with [`SessionStore::commit`](halter_session::SessionStore::commit) without
/// changing any caller (see the module docs). Implementations MUST:
///
/// - assign **gap-free monotonic** sequences on append, each exactly one greater
///   than the previous committed sequence, starting at `head + 1`;
/// - return events from [`replay`](Self::replay) in ascending sequence order;
/// - when `expected_head_sequence` is `Some(n)` and the current head is not `n`,
///   reject the append with [`GoalStoreError::Conflict`], appending nothing and
///   leaving the log unchanged (Requirement 3.4).
#[async_trait]
pub trait GoalEventLog: Send + Sync {
    /// Append `events` for `session` under optimistic concurrency control.
    ///
    /// When `expected_head_sequence` is `Some(n)`, the append is rejected with
    /// [`GoalStoreError::Conflict`] unless the current head sequence equals `n`.
    /// On success, the events receive gap-free monotonic sequences starting at
    /// `head + 1`, and the assigned `(sequence, event)` pairs are returned in
    /// ascending sequence order.
    ///
    /// # Errors
    ///
    /// Returns [`GoalStoreError::Conflict`] on an `expected_head_sequence`
    /// mismatch, or [`GoalStoreError::Log`] on a backend failure.
    async fn append(
        &self,
        session: &SessionId,
        events: Vec<GoalEvent>,
        expected_head_sequence: Option<u64>,
    ) -> Result<Vec<SequencedGoalEvent>, GoalStoreError>;

    /// Replay all committed goal events for `session` in ascending sequence
    /// order.
    ///
    /// # Errors
    ///
    /// Returns [`GoalStoreError::Log`] on a backend failure.
    async fn replay(
        &self,
        session: &SessionId,
    ) -> Result<Vec<SequencedGoalEvent>, GoalStoreError>;

    /// The current head (highest committed) sequence for `session`, or `0` when
    /// the session's goal log is empty.
    ///
    /// # Errors
    ///
    /// Returns [`GoalStoreError::Log`] on a backend failure.
    async fn head_sequence(&self, session: &SessionId) -> Result<u64, GoalStoreError>;
}

/// The owned goal-tree store (Requirement 1.1, design § "Component: Goal
/// Model").
///
/// Every mutation appends [`GoalEvent`]s through the backing [`GoalEventLog`]
/// (which preserves the `SessionStore` log contract), and the tree is
/// reconstructed by folding replayed events. The concrete method bodies for
/// `create`/`revise`/`close`/`get`/`get_tree` are implemented by tasks
/// 8.2/8.3/8.5/8.6 on [`EventLogGoalStore`]; this trait fixes their signatures.
#[async_trait]
pub trait GoalStore: Send + Sync {
    /// Create a new `Open` node (optionally under `parent`) by appending exactly
    /// one [`GoalEvent::GoalNodeCreated`], returning the assigned node id
    /// (Requirement 1.1). Rejects a non-existent parent
    /// ([`GoalStoreError::ParentNotFound`]) and an empty hypothesis
    /// ([`GoalStoreError::EmptyHypothesis`]) without appending any event
    /// (Requirements 1.4, 1.5).
    ///
    /// # Errors
    ///
    /// See [`GoalStoreError`].
    async fn create(
        &self,
        session: &SessionId,
        parent: Option<GoalNodeId>,
        hypothesis: String,
        intent: IntentSignature,
    ) -> Result<GoalNodeId, GoalStoreError>;

    /// Retroactively revise a node by appending exactly one
    /// [`GoalEvent::GoalNodeRevised`] without mutating any prior event
    /// (Requirement 2.1). Rejects a non-existent node
    /// ([`GoalStoreError::NodeNotFound`]) without appending any event
    /// (Requirement 2.6).
    ///
    /// # Errors
    ///
    /// See [`GoalStoreError`].
    async fn revise(
        &self,
        session: &SessionId,
        id: &GoalNodeId,
        revision: GoalNodeRevision,
    ) -> Result<(), GoalStoreError>;

    /// Record a resolution for a node by appending [`GoalEvent::GoalNodeResolved`];
    /// when the resolution fully closes the node's subtree, also append exactly
    /// one [`GoalEvent::GoalClosed`] carrying the computed `subtree_hash` and
    /// return a closed [`ClosureOutcome`] (Requirements 4.1, 4.3). Rejects an
    /// `Open` resolution ([`GoalStoreError::OpenResolution`]) and a non-existent
    /// node ([`GoalStoreError::NodeNotFound`]) without appending any event
    /// (Requirements 4.4, 4.5).
    ///
    /// # Errors
    ///
    /// See [`GoalStoreError`].
    async fn close(
        &self,
        session: &SessionId,
        id: &GoalNodeId,
        resolution: Resolution,
    ) -> Result<ClosureOutcome, GoalStoreError>;

    /// Project a single node by folding replayed [`GoalEvent`]s (Requirement
    /// 3.1). Returns `None` when no node with that id exists.
    ///
    /// # Errors
    ///
    /// See [`GoalStoreError`].
    async fn get(
        &self,
        session: &SessionId,
        id: &GoalNodeId,
    ) -> Result<Option<GoalNode>, GoalStoreError>;

    /// Project the whole tree by folding replayed [`GoalEvent`]s (Requirement
    /// 3.1).
    ///
    /// # Errors
    ///
    /// See [`GoalStoreError`].
    async fn get_tree(&self, session: &SessionId) -> Result<GoalTree, GoalStoreError>;
}

// --- In-memory GoalEventLog ------------------------------------------------

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// An in-memory [`GoalEventLog`] backed by a locked per-session `Vec`.
///
/// Enforces the same contract the `SessionStore` log guarantees so the
/// `GoalStore` and its tests run without a protocol change (see module docs):
/// gap-free monotonic sequences starting at `head + 1`, in-order replay, and
/// `expected_head_sequence` optimistic-concurrency rejection that leaves the log
/// unchanged (Requirement 3.4). Sequences start at `1`, so a head of `0` denotes
/// an empty log.
#[derive(Debug, Default)]
pub struct InMemoryGoalEventLog {
    /// Per-session committed events, each paired with its assigned sequence.
    /// A single `Mutex` serializes appends so the optimistic-concurrency check
    /// and the append are atomic with respect to one another.
    sessions: Mutex<HashMap<SessionId, Vec<SequencedGoalEvent>>>,
}

impl InMemoryGoalEventLog {
    /// Create an empty in-memory goal event log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GoalEventLog for InMemoryGoalEventLog {
    async fn append(
        &self,
        session: &SessionId,
        events: Vec<GoalEvent>,
        expected_head_sequence: Option<u64>,
    ) -> Result<Vec<SequencedGoalEvent>, GoalStoreError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| GoalStoreError::Log("goal event log lock poisoned".to_owned()))?;
        let log = sessions.entry(session.clone()).or_default();

        // Current head is the last assigned sequence, or 0 for an empty log.
        let head = log.last().map_or(0, |(seq, _)| *seq);

        // Optimistic concurrency: reject and leave the log untouched on mismatch.
        if let Some(expected) = expected_head_sequence
            && expected != head
        {
            return Err(GoalStoreError::Conflict {
                expected,
                actual: head,
            });
        }

        // Assign gap-free monotonic sequences starting at head + 1.
        let mut assigned = Vec::with_capacity(events.len());
        let mut next = head;
        for event in events {
            next += 1;
            assigned.push((next, event));
        }
        log.extend(assigned.iter().cloned());
        Ok(assigned)
    }

    async fn replay(
        &self,
        session: &SessionId,
    ) -> Result<Vec<SequencedGoalEvent>, GoalStoreError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| GoalStoreError::Log("goal event log lock poisoned".to_owned()))?;
        // Stored in append order, which is ascending sequence order by
        // construction; clone out so the lock is not held by the caller.
        Ok(sessions.get(session).cloned().unwrap_or_default())
    }

    async fn head_sequence(&self, session: &SessionId) -> Result<u64, GoalStoreError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| GoalStoreError::Log("goal event log lock poisoned".to_owned()))?;
        Ok(sessions
            .get(session)
            .and_then(|log| log.last())
            .map_or(0, |(seq, _)| *seq))
    }
}

// --- EventLogGoalStore -----------------------------------------------------

/// A concrete [`GoalStore`] over any [`GoalEventLog`].
///
/// The shared append+fold helpers ([`load_tree`](Self::load_tree),
/// [`load_events`](Self::load_events), [`append_with_occ`](Self::append_with_occ))
/// are fully implemented here — they realize the single-ordered-log append path
/// of Requirements 3.3/3.4. The public `create`/`revise`/`close`/`get`/`get_tree`
/// method bodies are left as `todo!()` for tasks 8.2/8.3/8.5/8.6, which build on
/// these helpers.
///
/// The store also holds an [`InductionQueue`] seam: on the **first** append of a
/// `GoalClosed` for a `(node_id, subtree_hash)` key, `close` dispatches exactly
/// one [`ClosureSignal`] and enqueues exactly one induction job through this
/// queue off the hot path (Requirements 6.2, 6.3, 6.4, 6.5, 14.5). By default
/// ([`new`](Self::new)) the queue is a [`NoopInductionQueue`], so existing
/// callers and tests see unchanged behavior; [`with_induction_queue`] supplies a
/// real one. See [`closure`](super::closure) for the design rationale (the
/// signal is owned by `halter-goals` because `halter_hooks::HookEventName` is a
/// closed enum with no goal-closure variant).
///
/// [`with_induction_queue`]: Self::with_induction_queue
pub struct EventLogGoalStore<L: GoalEventLog> {
    log: L,
    /// The async induction-enqueue seam. Shared/dynamically dispatched so the
    /// queue can be a no-op default or any caller-supplied implementation
    /// without a second generic parameter.
    induction_queue: SharedInductionQueue,
}

impl<L: GoalEventLog> std::fmt::Debug for EventLogGoalStore<L>
where
    L: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The induction queue is a trait object without a Debug bound; identify
        // it by name only so the struct stays Debug for tests/diagnostics.
        f.debug_struct("EventLogGoalStore")
            .field("log", &self.log)
            .field("induction_queue", &"<dyn InductionQueue>")
            .finish()
    }
}

impl<L: GoalEventLog> EventLogGoalStore<L> {
    /// Wrap a [`GoalEventLog`] as a goal store with a no-op induction queue.
    ///
    /// Closure detection and the single `GoalClosed` append per distinct
    /// `(node_id, subtree_hash)` still happen; the closure signal is simply
    /// dispatched into a [`NoopInductionQueue`]. Use [`with_induction_queue`]
    /// to route induction to a real queue.
    ///
    /// [`with_induction_queue`]: Self::with_induction_queue
    #[must_use]
    pub fn new(log: L) -> Self {
        Self {
            log,
            induction_queue: Arc::new(NoopInductionQueue),
        }
    }

    /// Wrap a [`GoalEventLog`] as a goal store that enqueues induction through
    /// `queue` on first closure of each distinct `(node_id, subtree_hash)`
    /// (Requirements 6.2, 6.3, 6.5).
    #[must_use]
    pub fn with_induction_queue(log: L, queue: SharedInductionQueue) -> Self {
        Self {
            log,
            induction_queue: queue,
        }
    }

    /// Borrow the backing log (used by the method tasks and by tests).
    #[must_use]
    pub fn log(&self) -> &L {
        &self.log
    }

    /// Replay the session's goal events in ascending sequence order, dropping the
    /// sequence numbers. Shared read helper for the projection and mutation
    /// paths.
    ///
    /// # Errors
    ///
    /// Propagates [`GoalStoreError::Log`] from the backing log.
    pub async fn load_events(
        &self,
        session: &SessionId,
    ) -> Result<Vec<GoalEvent>, GoalStoreError> {
        Ok(self
            .log
            .replay(session)
            .await?
            .into_iter()
            .map(|(_seq, event)| event)
            .collect())
    }

    /// Load and fold the session's goal events into the projected [`GoalTree`]
    /// (Requirement 3.1). Shared read helper the mutation methods use to check
    /// preconditions (parent exists, node exists, subtree closed) against the
    /// current folded state before appending.
    ///
    /// # Errors
    ///
    /// Propagates [`GoalStoreError::Log`] from the backing log.
    pub async fn load_tree(&self, session: &SessionId) -> Result<GoalTree, GoalStoreError> {
        let events = self.load_events(session).await?;
        Ok(fold_goal_events(GoalTree::new(), &events))
    }

    /// Append `events` at the current log head under optimistic concurrency
    /// control — the shared append path that gives goal events gap-free
    /// monotonic sequences on one ordered log (Requirements 3.3, 3.4).
    ///
    /// Reads the current head, then appends with that head as the
    /// `expected_head_sequence`, so a concurrent writer that advanced the log
    /// between the read and the append loses the race with
    /// [`GoalStoreError::Conflict`], appending nothing (Requirement 3.4).
    ///
    /// # Errors
    ///
    /// Returns [`GoalStoreError::Conflict`] on a concurrent advance, or
    /// [`GoalStoreError::Log`] on a backend failure.
    pub async fn append_with_occ(
        &self,
        session: &SessionId,
        events: Vec<GoalEvent>,
    ) -> Result<Vec<SequencedGoalEvent>, GoalStoreError> {
        let head = self.log.head_sequence(session).await?;
        self.log.append(session, events, Some(head)).await
    }
}

#[async_trait]
impl<L: GoalEventLog> GoalStore for EventLogGoalStore<L> {
    async fn create(
        &self,
        session: &SessionId,
        parent: Option<GoalNodeId>,
        hypothesis: String,
        intent: IntentSignature,
    ) -> Result<GoalNodeId, GoalStoreError> {
        // Reject an empty (or blank/whitespace-only) hypothesis before touching
        // the log, so no event is appended (Requirement 1.5).
        if hypothesis.trim().is_empty() {
            return Err(GoalStoreError::EmptyHypothesis);
        }

        // Fold the current log to check the parent precondition against the
        // projected tree. A named parent that is absent rejects with
        // ParentNotFound and appends nothing (Requirement 1.4).
        let tree = self.load_tree(session).await?;
        if let Some(parent_id) = &parent
            && !tree.contains(parent_id)
        {
            return Err(GoalStoreError::ParentNotFound(parent_id.clone()));
        }

        // Generate a unique id. GoalNodeId::new() is UUID-backed and thus
        // effectively unique; double-check against the current tree so a
        // (vanishingly unlikely) collision cannot alias an existing node.
        let mut id = GoalNodeId::new();
        while tree.contains(&id) {
            id = GoalNodeId::new();
        }

        // Append exactly one GoalNodeCreated (Requirements 1.1, 1.2, 1.3). The
        // Goal Model never touches the flat TaskList (Requirement 1.6).
        self.append_with_occ(
            session,
            vec![GoalEvent::GoalNodeCreated {
                id: id.clone(),
                parent,
                hypothesis,
                intent,
            }],
        )
        .await?;

        Ok(id)
    }

    async fn revise(
        &self,
        session: &SessionId,
        id: &GoalNodeId,
        revision: GoalNodeRevision,
    ) -> Result<(), GoalStoreError> {
        // A revision for an absent node rejects with NodeNotFound and appends
        // nothing (Requirement 2.6).
        let tree = self.load_tree(session).await?;
        if !tree.contains(id) {
            return Err(GoalStoreError::NodeNotFound(id.clone()));
        }

        // Append exactly one GoalNodeRevised (Requirement 2.1). The fold already
        // layers the delta, re-opens affected subtrees, and preserves
        // append-only semantics (Requirements 2.2-2.5); we just append.
        self.append_with_occ(
            session,
            vec![GoalEvent::GoalNodeRevised {
                id: id.clone(),
                revision,
            }],
        )
        .await?;

        Ok(())
    }

    async fn close(
        &self,
        session: &SessionId,
        id: &GoalNodeId,
        resolution: Resolution,
    ) -> Result<ClosureOutcome, GoalStoreError> {
        // Closure requires a non-Open resolution; reject Open before appending
        // anything (Requirement 4.4).
        if resolution.is_open() {
            return Err(GoalStoreError::OpenResolution);
        }

        // A resolution for an absent node rejects with NodeNotFound and appends
        // nothing (Requirement 4.5).
        let tree = self.load_tree(session).await?;
        if !tree.contains(id) {
            return Err(GoalStoreError::NodeNotFound(id.clone()));
        }

        // Append the resolution event (Requirement 4.1).
        self.append_with_occ(
            session,
            vec![GoalEvent::GoalNodeResolved {
                id: id.clone(),
                resolution,
            }],
        )
        .await?;

        // Re-fold with the resolution applied to determine subtree closure over
        // the current projected state.
        let tree = self.load_tree(session).await?;

        // Not fully closed: no GoalClosed, closed = false (Requirement 4.2).
        if !subtree_fully_closed(&tree, id) {
            return Ok(ClosureOutcome::not_closed());
        }

        // Fully closed: compute the subtree hash over the resolved subtree.
        let node = tree
            .node(id)
            .ok_or_else(|| GoalStoreError::NodeNotFound(id.clone()))?;
        let hash = subtree_hash(node, &tree);

        // Idempotency per distinct (node_id, subtree_hash): append GoalClosed
        // only if no GoalClosed for this exact id + hash is already recorded on
        // the log (Requirements 4.3, 4.6). A distinct hash for the same id is a
        // new closure and is appended.
        let already_closed = self.load_events(session).await?.iter().any(|event| {
            matches!(
                event,
                GoalEvent::GoalClosed { id: closed_id, subtree_hash: closed_hash }
                    if closed_id == id && *closed_hash == hash
            )
        });
        if !already_closed {
            self.append_with_occ(
                session,
                vec![GoalEvent::GoalClosed {
                    id: id.clone(),
                    subtree_hash: hash.clone(),
                }],
            )
            .await?;

            // First append of GoalClosed for this (node_id, subtree_hash):
            // dispatch exactly one closure signal and enqueue exactly one
            // induction job for the key (Requirements 6.2, 6.3). A re-closed
            // (already-recorded) key takes the other branch and enqueues
            // nothing (Requirement 6.4); a distinct subtree_hash for the same
            // node_id is a different key and reaches here again as a new trigger
            // (Requirement 6.5). The enqueue records the job and returns before
            // induction runs, keeping closure off the hot path (14.5).
            self.induction_queue
                .enqueue(ClosureSignal::new(session.clone(), id.clone(), hash.clone()))
                .await;
        }

        Ok(ClosureOutcome::closed(hash))
    }

    async fn get(
        &self,
        session: &SessionId,
        id: &GoalNodeId,
    ) -> Result<Option<GoalNode>, GoalStoreError> {
        // Project a single node by folding replayed events (Requirement 3.1).
        Ok(self.load_tree(session).await?.node(id).cloned())
    }

    async fn get_tree(&self, session: &SessionId) -> Result<GoalTree, GoalStoreError> {
        // Project the whole tree by folding replayed events (Requirements 3.1, 3.2).
        self.load_tree(session).await
    }
}

/// Whether `id`'s subtree is fully closed: `id` and every transitive descendant
/// have a non-`Open` resolution (design § `close` loop invariant, Requirement
/// 4.3). Walks the subtree via `tree.children`; an absent node is treated as not
/// closed.
fn subtree_fully_closed(tree: &GoalTree, id: &GoalNodeId) -> bool {
    let Some(node) = tree.node(id) else {
        return false;
    };
    if node.is_open() {
        return false;
    }
    node.children
        .iter()
        .all(|child_id| subtree_fully_closed(tree, child_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{IntentType, Scope, TargetRef, TargetType};

    fn session(id: &str) -> SessionId {
        SessionId::from(id)
    }

    fn intent(target: &str) -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from(target),
            scope: Scope::from("repo"),
        }
    }

    fn created(id: &str, parent: Option<&str>) -> GoalEvent {
        GoalEvent::GoalNodeCreated {
            id: GoalNodeId::from(id),
            parent: parent.map(GoalNodeId::from),
            hypothesis: format!("hypothesis {id}"),
            intent: intent(id),
        }
    }

    #[tokio::test]
    async fn append_assigns_gap_free_monotonic_sequences() {
        let log = InMemoryGoalEventLog::new();
        let s = session("s1");

        // First append of two events -> sequences 1, 2.
        let first = log
            .append(&s, vec![created("a", None), created("b", Some("a"))], None)
            .await
            .expect("first append succeeds");
        assert_eq!(first.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![1, 2]);

        // Second append of one event -> sequence 3 (exactly head + 1, no gap).
        let second = log
            .append(&s, vec![created("c", Some("a"))], Some(2))
            .await
            .expect("second append succeeds");
        assert_eq!(second.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![3]);

        assert_eq!(log.head_sequence(&s).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn replay_returns_events_in_sequence_order() {
        let log = InMemoryGoalEventLog::new();
        let s = session("s1");
        log.append(&s, vec![created("a", None)], None).await.unwrap();
        log.append(&s, vec![created("b", Some("a"))], Some(1)).await.unwrap();
        log.append(&s, vec![created("c", Some("a"))], Some(2)).await.unwrap();

        let replayed = log.replay(&s).await.expect("replay succeeds");
        // Sequences are strictly ascending and gap-free.
        assert_eq!(
            replayed.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // Events come back in the order they were appended.
        let ids: Vec<GoalNodeId> = replayed
            .iter()
            .map(|(_, event)| match event {
                GoalEvent::GoalNodeCreated { id, .. } => id.clone(),
                _ => panic!("unexpected event variant"),
            })
            .collect();
        assert_eq!(
            ids,
            vec![GoalNodeId::from("a"), GoalNodeId::from("b"), GoalNodeId::from("c")]
        );
    }

    #[tokio::test]
    async fn expected_head_mismatch_rejects_and_leaves_log_unchanged() {
        let log = InMemoryGoalEventLog::new();
        let s = session("s1");
        log.append(&s, vec![created("a", None)], None).await.unwrap();
        log.append(&s, vec![created("b", Some("a"))], Some(1)).await.unwrap();

        let before = log.replay(&s).await.unwrap();
        assert_eq!(log.head_sequence(&s).await.unwrap(), 2);

        // Append expecting a stale head (1) while the real head is 2: rejected.
        let err = log
            .append(&s, vec![created("c", Some("a"))], Some(1))
            .await
            .expect_err("stale expected head must reject");
        match err {
            GoalStoreError::Conflict { expected, actual } => {
                assert_eq!(expected, 1);
                assert_eq!(actual, 2);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // The log is unchanged: same events, same head (Requirement 3.4).
        let after = log.replay(&s).await.unwrap();
        assert_eq!(before, after, "rejected append must leave the log unchanged");
        assert_eq!(log.head_sequence(&s).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn sessions_have_independent_logs() {
        let log = InMemoryGoalEventLog::new();
        let s1 = session("s1");
        let s2 = session("s2");
        log.append(&s1, vec![created("a", None)], None).await.unwrap();

        // A fresh session's head is 0 and its first append starts at 1.
        assert_eq!(log.head_sequence(&s2).await.unwrap(), 0);
        let appended = log.append(&s2, vec![created("x", None)], Some(0)).await.unwrap();
        assert_eq!(appended.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![1]);
        assert_eq!(log.head_sequence(&s1).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn append_with_occ_appends_at_current_head() {
        let store = EventLogGoalStore::new(InMemoryGoalEventLog::new());
        let s = session("s1");
        store
            .append_with_occ(&s, vec![created("a", None)])
            .await
            .expect("first append");
        let sequenced = store
            .append_with_occ(&s, vec![created("b", Some("a"))])
            .await
            .expect("second append at current head");
        assert_eq!(sequenced.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![2]);
    }

    #[tokio::test]
    async fn load_tree_folds_replayed_events() {
        let store = EventLogGoalStore::new(InMemoryGoalEventLog::new());
        let s = session("s1");
        store.append_with_occ(&s, vec![created("root", None)]).await.unwrap();
        store
            .append_with_occ(&s, vec![created("child", Some("root"))])
            .await
            .unwrap();

        let tree = store.load_tree(&s).await.expect("fold replayed events");
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.root(), Some(&GoalNodeId::from("root")));
        assert_eq!(
            tree.children(&GoalNodeId::from("root")),
            &[GoalNodeId::from("child")]
        );
    }

    // --- GoalStore method tests (tasks 8.2/8.3/8.5/8.6) --------------------

    fn store() -> EventLogGoalStore<InMemoryGoalEventLog> {
        EventLogGoalStore::new(InMemoryGoalEventLog::new())
    }

    #[tokio::test]
    async fn create_returns_open_node_with_parent_linkage() {
        let store = store();
        let s = session("s1");

        let root = store
            .create(&s, None, "root hypothesis".to_owned(), intent("root"))
            .await
            .expect("create root");
        let child = store
            .create(&s, Some(root.clone()), "child".to_owned(), intent("child"))
            .await
            .expect("create child");

        // The returned ids are distinct.
        assert_ne!(root, child, "each create returns a unique id");

        let tree = store.get_tree(&s).await.unwrap();
        // Root is Open in the folded tree, and the child is linked under it (1.2, 1.3).
        assert!(tree.node(&root).unwrap().is_open());
        assert!(tree.node(&child).unwrap().is_open());
        assert_eq!(tree.children(&root), std::slice::from_ref(&child));
        assert_eq!(tree.node(&child).unwrap().parent, Some(root.clone()));
        // Exactly one event per create.
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn create_rejects_empty_hypothesis_without_appending() {
        let store = store();
        let s = session("s1");

        for blank in ["", "   ", "\t\n"] {
            let err = store
                .create(&s, None, blank.to_owned(), intent("x"))
                .await
                .expect_err("blank hypothesis rejects");
            assert!(matches!(err, GoalStoreError::EmptyHypothesis));
        }

        // No event was appended (1.5): the log is still empty.
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 0);
        assert!(store.get_tree(&s).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_rejects_nonexistent_parent_without_appending() {
        let store = store();
        let s = session("s1");
        let ghost = GoalNodeId::from("ghost");

        let err = store
            .create(&s, Some(ghost.clone()), "hyp".to_owned(), intent("x"))
            .await
            .expect_err("missing parent rejects");
        match err {
            GoalStoreError::ParentNotFound(id) => assert_eq!(id, ghost),
            other => panic!("expected ParentNotFound, got {other:?}"),
        }

        // No event appended (1.4).
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 0);
        assert!(store.get_tree(&s).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revise_appends_and_reflects_delta() {
        let store = store();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        store
            .revise(
                &s,
                &id,
                GoalNodeRevision {
                    resolution_conditions: Some(vec!["cond".to_owned()]),
                    tool_calls: None,
                    children: None,
                },
            )
            .await
            .expect("revise succeeds");

        // Exactly one revision event appended on top of the create.
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 2);
        let node = store.get(&s, &id).await.unwrap().unwrap();
        assert_eq!(node.resolution_conditions, vec!["cond".to_owned()]);
    }

    #[tokio::test]
    async fn revise_rejects_nonexistent_node_without_appending() {
        let store = store();
        let s = session("s1");
        let ghost = GoalNodeId::from("ghost");

        let err = store
            .revise(&s, &ghost, GoalNodeRevision::default())
            .await
            .expect_err("missing node rejects");
        match err {
            GoalStoreError::NodeNotFound(id) => assert_eq!(id, ghost),
            other => panic!("expected NodeNotFound, got {other:?}"),
        }
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn close_leaf_returns_closed_with_hash_and_stamps_it() {
        let store = store();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let outcome = store
            .close(&s, &id, Resolution::Accepted)
            .await
            .expect("close leaf");
        assert!(outcome.closed, "a fully-resolved leaf is closed");
        let hash = outcome.subtree_hash.expect("closed carries a hash");

        // The folded tree stamps the same subtree_hash on the node.
        let node = store.get(&s, &id).await.unwrap().unwrap();
        assert_eq!(node.subtree_hash.as_ref(), Some(&hash));
        // create + GoalNodeResolved + GoalClosed = 3 events.
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn close_rejects_open_resolution() {
        let store = store();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let err = store
            .close(&s, &id, Resolution::Open)
            .await
            .expect_err("Open resolution rejects");
        assert!(matches!(err, GoalStoreError::OpenResolution));
        // Only the create event exists; no resolution appended (4.4).
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn close_rejects_nonexistent_node() {
        let store = store();
        let s = session("s1");
        let ghost = GoalNodeId::from("ghost");

        let err = store
            .close(&s, &ghost, Resolution::Accepted)
            .await
            .expect_err("missing node rejects");
        match err {
            GoalStoreError::NodeNotFound(id) => assert_eq!(id, ghost),
            other => panic!("expected NodeNotFound, got {other:?}"),
        }
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn close_parent_with_open_child_is_not_closed() {
        let store = store();
        let s = session("s1");
        let root = store
            .create(&s, None, "root".to_owned(), intent("root"))
            .await
            .unwrap();
        let _child = store
            .create(&s, Some(root.clone()), "child".to_owned(), intent("child"))
            .await
            .unwrap();

        // Resolving the parent while the child is still Open does not close.
        let outcome = store
            .close(&s, &root, Resolution::Accepted)
            .await
            .expect("close parent");
        assert!(!outcome.closed, "parent with an Open child is not closed (4.2)");
        assert!(outcome.subtree_hash.is_none());
        // create root + create child + GoalNodeResolved = 3; no GoalClosed.
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn reclose_under_same_key_does_not_append_duplicate() {
        let store = store();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let first = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        assert!(first.closed);
        // create + resolved + closed = 3.
        let head_after_first = store.log().head_sequence(&s).await.unwrap();
        assert_eq!(head_after_first, 3);

        // Re-closing under the same resolution yields the same (id, hash);
        // GoalClosed must not be appended again (4.3/4.6). The GoalNodeResolved
        // still appends (it is the idempotent close event), so exactly one new
        // event is added and no second GoalClosed.
        let second = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        assert_eq!(second.subtree_hash, first.subtree_hash);
        assert!(second.closed);

        let closed_count = store
            .load_events(&s)
            .await
            .unwrap()
            .iter()
            .filter(|e| matches!(e, GoalEvent::GoalClosed { .. }))
            .count();
        assert_eq!(closed_count, 1, "no duplicate GoalClosed for the same (id, hash)");
    }

    #[tokio::test]
    async fn get_returns_node_and_none_for_absent() {
        let store = store();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let node = store.get(&s, &id).await.unwrap();
        assert!(node.is_some());
        assert_eq!(node.unwrap().id, id);

        assert!(
            store.get(&s, &GoalNodeId::from("absent")).await.unwrap().is_none(),
            "get for an absent id returns None"
        );
    }

    // --- Closure signal / induction enqueue tests (tasks 9.1/9.2) ---------

    use super::super::closure::RecordingInductionQueue;

    /// Build a store wired to a recording induction queue, returning both the
    /// store and a handle on the queue for assertions.
    fn store_with_queue() -> (
        EventLogGoalStore<InMemoryGoalEventLog>,
        RecordingInductionQueue,
    ) {
        let queue = RecordingInductionQueue::new();
        let store = EventLogGoalStore::with_induction_queue(
            InMemoryGoalEventLog::new(),
            Arc::new(queue.clone()),
        );
        (store, queue)
    }

    #[tokio::test]
    async fn close_leaf_enqueues_exactly_one_induction_job() {
        let (store, queue) = store_with_queue();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let outcome = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        let hash = outcome.subtree_hash.expect("closed carries a hash");

        // Exactly one induction job for (id, hash) (Requirements 6.2, 6.3).
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.count_for(&id, &hash), 1);
        assert_eq!(
            queue.jobs(),
            vec![ClosureSignal::new(s.clone(), id.clone(), hash)]
        );
    }

    #[tokio::test]
    async fn reclose_under_same_key_enqueues_no_additional_job() {
        let (store, queue) = store_with_queue();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let first = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        let hash = first.subtree_hash.clone().unwrap();
        assert_eq!(queue.len(), 1);

        // Re-closing under the same resolution reproduces the same (id, hash);
        // no additional closure signal / induction job is enqueued (6.4).
        let second = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        assert_eq!(second.subtree_hash, first.subtree_hash);
        assert_eq!(queue.len(), 1, "no additional induction job for the same key");
        assert_eq!(queue.count_for(&id, &hash), 1);
    }

    #[tokio::test]
    async fn reclose_under_distinct_subtree_hash_enqueues_a_new_job() {
        let (store, queue) = store_with_queue();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        // First closure under the original resolved subtree.
        let first = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        let hash1 = first.subtree_hash.clone().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.count_for(&id, &hash1), 1);

        // Revise an intent-relevant field (resolution_conditions), which
        // supersedes the prior closure and changes the recomputed subtree_hash.
        store
            .revise(
                &s,
                &id,
                GoalNodeRevision {
                    resolution_conditions: Some(vec!["a new condition".to_owned()]),
                    tool_calls: None,
                    children: None,
                },
            )
            .await
            .unwrap();

        // Re-close under the revised (distinct) subtree_hash: a new trigger that
        // enqueues exactly one new induction job for (id, hash2) (Requirement
        // 6.5), while (id, hash1) still has exactly one job.
        let second = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        let hash2 = second.subtree_hash.clone().unwrap();
        assert_ne!(hash1, hash2, "a revised resolved subtree has a distinct hash");

        assert_eq!(queue.len(), 2, "the distinct-hash closure is a new trigger");
        assert_eq!(queue.count_for(&id, &hash1), 1);
        assert_eq!(queue.count_for(&id, &hash2), 1);
        assert_eq!(queue.distinct_keys().len(), 2);
    }

    #[tokio::test]
    async fn enqueue_returns_before_induction_runs() {
        // The recording queue models "enqueue, don't await": close() returns
        // with the job merely recorded (no Judge/Author executed inline), so
        // control returns to the caller before induction runs (6.3, 14.5).
        let (store, queue) = store_with_queue();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();

        let outcome = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        let hash = outcome.subtree_hash.unwrap();

        // close() has returned; the job is recorded and awaiting an async
        // induction worker — the closure path did not block on induction.
        let jobs = queue.jobs();
        assert_eq!(jobs, vec![ClosureSignal::new(s.clone(), id, hash)]);
    }

    #[tokio::test]
    async fn not_closed_close_enqueues_no_job() {
        let (store, queue) = store_with_queue();
        let s = session("s1");
        let root = store
            .create(&s, None, "root".to_owned(), intent("root"))
            .await
            .unwrap();
        let _child = store
            .create(&s, Some(root.clone()), "child".to_owned(), intent("child"))
            .await
            .unwrap();

        // Resolving the parent while its child is Open does not close, so no
        // closure signal / induction job is enqueued (Requirement 4.2 / 6.4).
        let outcome = store.close(&s, &root, Resolution::Accepted).await.unwrap();
        assert!(!outcome.closed);
        assert!(queue.is_empty(), "an unclosed subtree enqueues no induction job");
    }

    #[tokio::test]
    async fn default_store_keeps_working_without_a_queue() {
        // EventLogGoalStore::new stays API-stable: closure still appends exactly
        // one GoalClosed with a no-op queue behind the scenes.
        let store = store();
        let s = session("s1");
        let id = store
            .create(&s, None, "hyp".to_owned(), intent("n"))
            .await
            .unwrap();
        let outcome = store.close(&s, &id, Resolution::Accepted).await.unwrap();
        assert!(outcome.closed);
        assert_eq!(store.log().head_sequence(&s).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn get_tree_returns_whole_tree() {
        let store = store();
        let s = session("s1");
        let root = store
            .create(&s, None, "root".to_owned(), intent("root"))
            .await
            .unwrap();
        let child = store
            .create(&s, Some(root.clone()), "child".to_owned(), intent("child"))
            .await
            .unwrap();

        let tree = store.get_tree(&s).await.unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.root(), Some(&root));
        assert_eq!(tree.children(&root), &[child]);
    }
}
