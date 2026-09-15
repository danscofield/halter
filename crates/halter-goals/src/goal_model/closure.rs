//! The goal-closure signal and the async induction-enqueue seam (Requirements
//! 6.1, 6.2, 6.3, 6.4, 6.5, 14.5).
//!
//! When a node's subtree becomes fully closed for the **first time** under a
//! given `(node_id, subtree_hash)` key, the Goal Model dispatches exactly one
//! [`ClosureSignal`] carrying that key and enqueues exactly one asynchronous
//! induction job for it. A re-fired closure for an already-recorded key
//! dispatches and enqueues nothing; a *distinct* `subtree_hash` for the same
//! `node_id` is a new trigger that enqueues a new job (Requirement 6.5).
//!
//! # Where the signal is routed (design intent vs. this task's pragmatic shape)
//!
//! The design's intent (§ "Goal closure -> induction via the hooks system") is
//! that goal closure dispatches a `GoalClosed`-style hook signal **through the
//! existing `halter-hooks` system**, reusing its lifecycle so closure is
//! observable and composes with `PostToolUse`/`Stop`/`SessionStart`/… without
//! new plumbing.
//!
//! `halter_hooks::HookEventName` is, however, a **closed enum** of harness
//! lifecycle events (`SessionStart`, `PostToolUse`, `Stop`, `TaskCompleted`, …)
//! with **no goal-closure variant**, and adding one is a `halter-hooks` change
//! that is out of scope for this crate/task. This mirrors exactly the situation
//! task 8.1 faced with `SessionEventPayload` being a closed enum: rather than
//! fork `halter-hooks`, this task introduces a small closure-signal +
//! induction-enqueue abstraction **owned by `halter-goals`** that preserves the
//! required semantics:
//!
//! - the closure boundary is detected at the single point the design specifies
//!   — the `GoalStore::close` path, where `GoalClosed` is appended **iff** the
//!   `(node_id, subtree_hash)` key is not already on the log (the log-based
//!   idempotency implemented in task 8.5);
//! - on that first append, exactly one [`ClosureSignal`] is dispatched and
//!   exactly one induction job is enqueued through an [`InductionQueue`];
//! - the enqueue **returns control to the caller before induction runs** — the
//!   queue only *records* the job (it does not execute it), modeling the
//!   "enqueue, don't await" hot-path guarantee (Requirements 6.3, 14.5).
//!
//! The future `halter-hooks` integration is localized to **one adapter**: an
//! [`InductionQueue`] implementation whose `enqueue` dispatches a
//! `HookEventName::GoalClosed` (once that variant exists) and lets the hooks
//! engine enqueue the induction job. Nothing above the [`InductionQueue`] trait
//! — the `GoalStore` and its `close` path — changes when that swap happens,
//! because the contract modeled here (exactly-once enqueue per distinct key,
//! off the hot path) is identical to what the hook dispatch would provide.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use halter_protocol::SessionId;

use crate::types::{GoalNodeId, SubtreeHash};

/// The goal-closure signal carrying the closure key `(node_id, subtree_hash)`
/// (Requirements 6.2, 6.3).
///
/// This is the `GoalClosed`-style payload the design routes through the hooks
/// system: a node's subtree closed for the first time under this exact
/// `subtree_hash`, and induction should be enqueued for it. A distinct
/// `subtree_hash` for the same `node_id` is a distinct signal (Requirement 6.5).
///
/// The signal also carries the [`SessionId`] the closure occurred in. The
/// closure key that governs exactly-once semantics remains `(node_id,
/// subtree_hash)` (see [`key`](Self::key)); the session is *addressing* data an
/// [`InductionQueue`] needs to resolve the resolved [`GoalNode`] back out of the
/// store when it runs `induce_memory` (the induction worker only receives the
/// key, so it must be told which session's log to project the node from — see
/// [`crate::integration`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClosureSignal {
    /// The session whose goal log recorded the closure. Used by an
    /// [`InductionQueue`] to resolve the closed node via the store.
    pub session: SessionId,
    /// The node whose resolved subtree closed.
    pub node_id: GoalNodeId,
    /// The stable content hash of that resolved subtree at closure.
    pub subtree_hash: SubtreeHash,
}

impl ClosureSignal {
    /// Build a closure signal for a `(node_id, subtree_hash)` key in `session`.
    #[must_use]
    pub fn new(session: SessionId, node_id: GoalNodeId, subtree_hash: SubtreeHash) -> Self {
        Self {
            session,
            node_id,
            subtree_hash,
        }
    }

    /// The `(node_id, subtree_hash)` key this signal carries.
    ///
    /// This is the exactly-once idempotency key; it deliberately excludes the
    /// session, matching the log-based idempotency in
    /// [`GoalStore::close`](super::store::GoalStore::close).
    #[must_use]
    pub fn key(&self) -> (GoalNodeId, SubtreeHash) {
        (self.node_id.clone(), self.subtree_hash.clone())
    }
}

/// The seam through which goal closure enqueues the **asynchronous** induction
/// job off the hot path (Requirements 6.3, 14.5).
///
/// [`enqueue`](Self::enqueue) MUST return control to the caller **before
/// induction runs** — it records/hands off the job and returns, it does not
/// execute Judge/Author inline. An implementation backed by the future
/// `halter-hooks` adapter would dispatch the closure hook here and let the hooks
/// engine schedule the induction job; the in-memory [`RecordingInductionQueue`]
/// simply records the job so the exactly-once semantics can be asserted.
#[async_trait]
pub trait InductionQueue: Send + Sync {
    /// Enqueue exactly one induction job for `signal`'s `(node_id,
    /// subtree_hash)` key and return **before** induction runs (Requirements
    /// 6.3, 14.5).
    async fn enqueue(&self, signal: ClosureSignal);
}

/// A no-op [`InductionQueue`] that drops every signal.
///
/// This is the default queue for [`EventLogGoalStore::new`], so the existing
/// `new(log)` API and its tests keep working unchanged: closure still appends
/// exactly one `GoalClosed` per distinct key, it just enqueues into a sink.
///
/// [`EventLogGoalStore::new`]: super::store::EventLogGoalStore::new
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopInductionQueue;

#[async_trait]
impl InductionQueue for NoopInductionQueue {
    async fn enqueue(&self, _signal: ClosureSignal) {
        // Intentionally does nothing: the closure boundary is still detected and
        // GoalClosed still appended; this queue simply has no induction backend.
    }
}

/// An in-memory [`InductionQueue`] that **records** every enqueued job behind a
/// lock so tests can assert exactly-once enqueue per distinct `(node_id,
/// subtree_hash)` (Requirements 6.3, 6.4, 6.5).
///
/// Recording (rather than executing) is exactly the "return before induction
/// runs" guarantee (Requirements 6.3, 14.5): [`enqueue`](InductionQueue::enqueue)
/// pushes the signal and returns; no Judge/Author work happens on the caller's
/// thread. Cloning shares the same underlying record (an [`Arc`]), so a queue
/// handed to a store and a handle kept for assertions observe the same jobs.
#[derive(Debug, Default, Clone)]
pub struct RecordingInductionQueue {
    jobs: Arc<Mutex<Vec<ClosureSignal>>>,
}

impl RecordingInductionQueue {
    /// Create an empty recording queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All enqueued induction jobs in the order they were enqueued.
    #[must_use]
    pub fn jobs(&self) -> Vec<ClosureSignal> {
        self.jobs
            .lock()
            .expect("induction queue lock poisoned")
            .clone()
    }

    /// The total number of enqueued induction jobs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs
            .lock()
            .expect("induction queue lock poisoned")
            .len()
    }

    /// Whether no induction job has been enqueued yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of enqueued jobs whose key equals `(node_id, subtree_hash)`.
    ///
    /// Exactly-once per distinct key means this is `1` for every key that has
    /// closed and `0` for keys that never closed (Requirements 6.3, 6.4).
    #[must_use]
    pub fn count_for(&self, node_id: &GoalNodeId, subtree_hash: &SubtreeHash) -> usize {
        self.jobs
            .lock()
            .expect("induction queue lock poisoned")
            .iter()
            .filter(|signal| &signal.node_id == node_id && &signal.subtree_hash == subtree_hash)
            .count()
    }

    /// The set of distinct `(node_id, subtree_hash)` keys enqueued so far.
    #[must_use]
    pub fn distinct_keys(&self) -> HashSet<(GoalNodeId, SubtreeHash)> {
        self.jobs
            .lock()
            .expect("induction queue lock poisoned")
            .iter()
            .map(ClosureSignal::key)
            .collect()
    }
}

#[async_trait]
impl InductionQueue for RecordingInductionQueue {
    async fn enqueue(&self, signal: ClosureSignal) {
        // Record the job and return immediately — induction is not run here, so
        // control returns to the closing caller before any Judge/Author work
        // (Requirements 6.3, 14.5).
        self.jobs
            .lock()
            .expect("induction queue lock poisoned")
            .push(signal);
    }
}

/// A shared, dynamically-dispatched [`InductionQueue`] handle.
///
/// The store holds one of these so the queue can be a no-op by default or any
/// caller-supplied implementation, without threading a second generic parameter
/// through [`EventLogGoalStore`](super::store::EventLogGoalStore).
pub type SharedInductionQueue = Arc<dyn InductionQueue>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Sha256;

    fn hash(seed: &str) -> SubtreeHash {
        SubtreeHash(Sha256::from(seed))
    }

    fn session() -> SessionId {
        SessionId::from("s1")
    }

    #[tokio::test]
    async fn recording_queue_records_in_order_and_counts_per_key() {
        let queue = RecordingInductionQueue::new();
        assert!(queue.is_empty());

        let a = ClosureSignal::new(session(), GoalNodeId::from("n1"), hash("h1"));
        let b = ClosureSignal::new(session(), GoalNodeId::from("n1"), hash("h2"));

        queue.enqueue(a.clone()).await;
        queue.enqueue(b.clone()).await;

        assert_eq!(queue.len(), 2);
        assert_eq!(queue.jobs(), vec![a.clone(), b.clone()]);
        assert_eq!(queue.count_for(&GoalNodeId::from("n1"), &hash("h1")), 1);
        assert_eq!(queue.count_for(&GoalNodeId::from("n1"), &hash("h2")), 1);
        assert_eq!(queue.distinct_keys().len(), 2);
    }

    #[tokio::test]
    async fn recording_queue_clone_shares_the_same_record() {
        let queue = RecordingInductionQueue::new();
        let handle = queue.clone();
        queue
            .enqueue(ClosureSignal::new(session(), GoalNodeId::from("n1"), hash("h1")))
            .await;
        // The clone observes the job enqueued through the original handle.
        assert_eq!(handle.len(), 1);
    }

    #[tokio::test]
    async fn noop_queue_drops_signals() {
        let queue = NoopInductionQueue;
        queue
            .enqueue(ClosureSignal::new(session(), GoalNodeId::from("n1"), hash("h1")))
            .await;
        // Nothing to assert beyond "did not panic / did not run induction".
    }

    #[test]
    fn closure_signal_key_roundtrips() {
        let signal = ClosureSignal::new(session(), GoalNodeId::from("n1"), hash("h1"));
        assert_eq!(signal.key(), (GoalNodeId::from("n1"), hash("h1")));
    }
}
