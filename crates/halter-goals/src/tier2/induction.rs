//! The Tier 2 Induction Engine — recurrence gate, two-step Judge/Author
//! authoring in a clean context, and idempotent dedup/reinforce/insert
//! (Requirements 14, 15, 16).
//!
//! Induction is the process that distills a resolved goal subtree into a
//! reusable [`Memory`]. It runs asynchronously off the hot path when a subtree
//! closes (see [`crate::goal_model::closure`]), and follows the design's
//! `induceMemory` pseudocode exactly:
//!
//! ```text
//! ASSERT node.resolution != Open
//! recordOccurrence(node.intent)
//! IF recurrenceCount(node.intent) < N THEN RETURN Skipped
//! key <- (node.id, node.subtree_hash)
//! IF store.hasMemoryFor(key) THEN RETURN Reinforced(store.reinforce(key))
//! cleanCtx <- freshContext()
//! verdict <- Judge.evaluate(node, cleanCtx)   // Judge only judges
//! IF verdict.decision = Decline THEN RETURN Rejected(verdict.rationale)
//! candidate <- Author.write(node, verdict.granularity, cleanCtx)  // Author only authors
//! match <- store.findDuplicate(candidate.intent, candidate.plan)
//! IF match != NULL THEN RETURN Reinforced(store.reinforce(match.id, candidate))
//! ELSE RETURN Inserted(store.insert(key, candidate))
//! ```
//!
//! # The clean-context guarantee (Requirement 15.5)
//!
//! Judge and Author never see the original goal work's messages, tool-call
//! transcript, or reasoning state. This is enforced *structurally*: both steps
//! receive a [`CleanContext`], a type that deliberately carries **only** a small
//! set of distilled node fields ([`DistilledNode`]) and has **no field** for a
//! transcript, message list, or reasoning trace. Because a [`CleanContext`] is
//! constructed solely from [`CleanContext::from_node`] — which reads only the
//! node's structural fields — it is impossible to smuggle residual reasoning
//! into a Judge or Author invocation. Each step receives a **freshly
//! constructed** context (two distinct constructions), satisfying the
//! "two distinct invocations, each in a clean context" requirement (15.1, 15.5).
//!
//! # Recurrence gating (Requirement 14)
//!
//! A [`RecurrenceTracker`] records exactly one occurrence per run **before** the
//! gate is evaluated (14.1), regardless of outcome. While the count is below the
//! threshold `N`, the engine authors nothing and never invokes Judge (14.2,
//! 14.4); at or above `N` it proceeds to Judge (14.3).
//!
//! # Idempotency, dedup, reinforcement (Requirement 16)
//!
//! The engine keys on `(node_id, subtree_hash)`: if the store already holds a
//! memory for the key it reinforces rather than authoring a duplicate (16.1). A
//! freshly authored candidate that matches an existing memory by intent+plan
//! reinforces that match instead of inserting (16.2). Concurrent inductions for
//! the same key are serialized on a per-key lock so exactly one insert happens
//! and the rest reinforce (16.3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use halter_providers::{
    EmbeddingClient, EmbeddingClientError, EmbeddingRequest, EmbeddingResponse,
};
use tokio_util::sync::CancellationToken;

use crate::goal_model::{GoalNode, Resolution};
use crate::tier2::embedding::settings::ResolvedEmbeddingSettings;
use crate::tier2::embedding::{insert_memory_with_writer, MemoryEmbeddingWriter};
use crate::tier2::memory::{
    Applicability, EvidenceContract, Memory, MemoryKind, MemoryVersion, OutcomeShape,
    ParameterSchema, Plan, PlanStep, Provenance, Reinforcement,
};
use crate::tier2::store::MemoryStore;
use crate::types::{GoalNodeId, IntentSignature, MemoryId, OutcomeRef, SubtreeHash};

// ---------------------------------------------------------------------------
// Recurrence tracking (Requirement 14)
// ---------------------------------------------------------------------------

/// Records and reports how many times a given intent has recurred.
///
/// The Induction Engine records **exactly one** occurrence per run before
/// evaluating the recurrence gate (Requirement 14.1) and then compares
/// [`recurrence_count`](Self::recurrence_count) against the threshold `N`.
pub trait RecurrenceTracker: Send + Sync {
    /// Record exactly one occurrence of `intent`.
    fn record_occurrence(&self, intent: &IntentSignature);

    /// The number of occurrences recorded for `intent` so far.
    fn recurrence_count(&self, intent: &IntentSignature) -> u64;
}

/// An in-memory [`RecurrenceTracker`] backed by an [`RwLock`]-guarded map.
///
/// Occurrence counts live behind a lock for interior mutability so the tracker
/// can be shared (`&self`) across concurrent induction jobs.
#[derive(Debug, Default)]
pub struct InMemoryRecurrenceTracker {
    counts: RwLock<HashMap<IntentSignature, u64>>,
}

impl InMemoryRecurrenceTracker {
    /// Create an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: RwLock::new(HashMap::new()),
        }
    }
}

impl RecurrenceTracker for InMemoryRecurrenceTracker {
    fn record_occurrence(&self, intent: &IntentSignature) {
        let mut guard = self
            .counts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.entry(intent.clone()).or_insert(0);
        *entry = entry.saturating_add(1);
    }

    fn recurrence_count(&self, intent: &IntentSignature) -> u64 {
        let guard = self
            .counts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(intent).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Clean context (Requirement 15.5)
// ---------------------------------------------------------------------------

/// The distilled, transcript-free view of a resolved node handed to Judge and
/// Author.
///
/// This struct carries **only** the node's structural fields needed to judge
/// worth and author a memory. It intentionally has no field for messages,
/// tool-call transcripts, or reasoning traces — the fields simply do not exist,
/// so the original goal work's reasoning cannot leak through it (Requirement
/// 15.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistilledNode {
    /// The node's id.
    pub id: GoalNodeId,
    /// The node's resolution (never `Open` for induction).
    pub resolution: Resolution,
    /// The node's structured intent signature.
    pub intent: IntentSignature,
    /// The stable content hash of the resolved subtree, if closed.
    pub subtree_hash: Option<SubtreeHash>,
    /// The hypothesis the node tested.
    pub hypothesis: String,
    /// The conditions under which the node resolved.
    pub resolution_conditions: Vec<String>,
}

/// A context that provably contains **none** of the original goal work's
/// messages, transcript, or reasoning (Requirement 15.5).
///
/// A `CleanContext` can only be built via [`CleanContext::from_node`], which
/// reads exclusively the node's distilled structural fields into a
/// [`DistilledNode`]. There is no constructor and no field that accepts a
/// message list, transcript, or reasoning state, so it is structurally
/// impossible to carry residual reasoning into Judge or Author. Judge and Author
/// each receive their own freshly constructed `CleanContext` (Requirement 15.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanContext {
    node: DistilledNode,
}

impl CleanContext {
    /// Construct a fresh clean context from a resolved node.
    ///
    /// Only the node's distilled structural fields are copied in; no transcript
    /// or reasoning is (or can be) carried over.
    #[must_use]
    pub fn from_node(node: &GoalNode) -> Self {
        Self {
            node: DistilledNode {
                id: node.id.clone(),
                resolution: node.resolution,
                intent: node.intent.clone(),
                subtree_hash: node.subtree_hash.clone(),
                hypothesis: node.hypothesis.clone(),
                resolution_conditions: node.resolution_conditions.clone(),
            },
        }
    }

    /// The distilled node this context exposes.
    #[must_use]
    pub fn node(&self) -> &DistilledNode {
        &self.node
    }
}

// ---------------------------------------------------------------------------
// Judge (Requirement 15.1, 15.2)
// ---------------------------------------------------------------------------

/// A coarse hint of how large a memory to author, produced by the Judge and
/// consumed by the Author.
///
/// This mirrors the memory kinds the Author may produce. The Judge decides the
/// granularity; the Author writes accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// Author a single-procedure [`MemoryKind::Fragment`].
    Fragment,
    /// Author a composed [`MemoryKind::Composite`].
    Composite,
    /// Author a dead-end [`MemoryKind::Negative`].
    Negative,
}

impl Granularity {
    /// The [`MemoryKind`] this granularity hint maps to.
    #[must_use]
    pub fn kind(self) -> MemoryKind {
        match self {
            Granularity::Fragment => MemoryKind::Fragment,
            Granularity::Composite => MemoryKind::Composite,
            Granularity::Negative => MemoryKind::Negative,
        }
    }
}

/// The Judge's decision on whether a resolved subtree is worth a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The subtree is worth authoring a memory for.
    Approve,
    /// The subtree is not worth a memory.
    Decline,
}

/// The Judge's verdict: a decision, a rationale, and (on approve) a granularity.
///
/// The Judge **never** produces a [`Memory`] (Requirement 15.2); it yields only
/// this verdict. The `rationale` is persisted on decline (Requirement 15.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeVerdict {
    /// Approve or decline.
    pub decision: Decision,
    /// Human-readable rationale for the decision.
    pub rationale: String,
    /// The granularity to author at; meaningful only on approve.
    pub granularity: Granularity,
}

impl JudgeVerdict {
    /// An approve verdict at the given `granularity`.
    #[must_use]
    pub fn approve(granularity: Granularity, rationale: impl Into<String>) -> Self {
        Self {
            decision: Decision::Approve,
            rationale: rationale.into(),
            granularity,
        }
    }

    /// A decline verdict carrying a `rationale`.
    #[must_use]
    pub fn decline(rationale: impl Into<String>) -> Self {
        Self {
            decision: Decision::Decline,
            rationale: rationale.into(),
            // Granularity is irrelevant on decline; pick a harmless default.
            granularity: Granularity::Fragment,
        }
    }
}

/// Step 1 of induction: judge whether a subtree is worth a memory.
///
/// A `Judge` **only judges** — it produces a [`JudgeVerdict`] and never a
/// [`Memory`] (Requirement 15.2). It runs in a [`CleanContext`] (Requirement
/// 15.5).
#[async_trait]
pub trait Judge: Send + Sync {
    /// Evaluate the resolved subtree described by `ctx` and yield a verdict.
    async fn evaluate(&self, ctx: &CleanContext) -> JudgeVerdict;
}

// ---------------------------------------------------------------------------
// Author (Requirement 15.4, 15.6)
// ---------------------------------------------------------------------------

/// A failure to author a well-formed [`Memory`] (Requirement 15.6).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("author failure: {0}")]
pub struct AuthorError(pub String);

impl AuthorError {
    /// Build an author error from a reason.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// Step 2 of induction: author the candidate [`Memory`].
///
/// An `Author` **only authors** — it runs only after the Judge approves
/// (Requirement 15.4), in a [`CleanContext`] (Requirement 15.5), and may fail
/// with an [`AuthorError`] (Requirement 15.6).
#[async_trait]
pub trait Author: Send + Sync {
    /// Author a candidate memory at `granularity` from the clean `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`AuthorError`] when a well-formed memory cannot be produced; the
    /// engine then inserts nothing and records the failure (Requirement 15.6).
    async fn write(
        &self,
        granularity: Granularity,
        ctx: &CleanContext,
    ) -> Result<Memory, AuthorError>;
}

// ---------------------------------------------------------------------------
// Decline / failure logs (Requirements 15.3, 15.6)
// ---------------------------------------------------------------------------

/// A persisted record that the Judge declined to author a memory (Requirement
/// 15.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclineRecord {
    /// The intent whose induction was declined.
    pub intent: IntentSignature,
    /// The idempotency key of the declined subtree.
    pub key: (GoalNodeId, SubtreeHash),
    /// The Judge's rationale for declining.
    pub rationale: String,
}

/// A persisted record that the Author failed to produce a memory (Requirement
/// 15.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRecord {
    /// The intent whose authoring failed.
    pub intent: IntentSignature,
    /// The idempotency key of the subtree whose authoring failed.
    pub key: (GoalNodeId, SubtreeHash),
    /// The author failure reason.
    pub reason: String,
}

/// A thread-safe append-only log for decline and author-failure records.
///
/// Persisting decline rationales (15.3) and author failures (15.6) is modeled as
/// appending to this in-memory log. Because authoring failure records the
/// failure but does **not** mark the idempotency key as done, a later recurrence
/// can re-trigger induction for the same intent (15.6).
#[derive(Debug, Default, Clone)]
pub struct InductionLog {
    declines: Arc<Mutex<Vec<DeclineRecord>>>,
    failures: Arc<Mutex<Vec<FailureRecord>>>,
}

impl InductionLog {
    /// Create an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn record_decline(&self, record: DeclineRecord) {
        self.declines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }

    fn record_failure(&self, record: FailureRecord) {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }

    /// All persisted decline records, in order.
    #[must_use]
    pub fn declines(&self) -> Vec<DeclineRecord> {
        self.declines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// All persisted author-failure records, in order.
    #[must_use]
    pub fn failures(&self) -> Vec<FailureRecord> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Induction outcome
// ---------------------------------------------------------------------------

/// The result of one [`InductionEngine::induce_memory`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InductionOutcome {
    /// The run terminated before authoring (e.g. below the recurrence gate, or
    /// the node was not resolved). Carries a human-readable reason.
    Skipped(String),
    /// The Judge declined; carries the persisted rationale (Requirement 15.3).
    Rejected(String),
    /// An existing memory was reinforced instead of inserting a duplicate
    /// (Requirements 16.1, 16.2). Carries the reinforced memory's id.
    Reinforced(MemoryId),
    /// A new memory was inserted under the idempotency key. Carries its id.
    Inserted(MemoryId),
    /// The Judge approved but the Author failed; nothing was inserted and the
    /// failure was recorded (Requirement 15.6). Carries the failure reason.
    AuthorFailed(String),
}

// ---------------------------------------------------------------------------
// Per-key serialization (Requirement 16.3)
// ---------------------------------------------------------------------------

/// Serializes the check-then-insert critical section per idempotency key so
/// concurrent inductions for the same `(node_id, subtree_hash)` produce exactly
/// one insert and the rest reinforce (Requirement 16.3).
///
/// Each key is associated with its own async [`tokio::sync::Mutex`]; a run holds
/// the key's mutex across its whole `hasMemoryFor -> Judge -> Author -> dedup ->
/// insert` critical section (which spans `.await` points), so no two runs for
/// the same key interleave. An async mutex is required precisely because the
/// guard is held across `.await`. Different keys use different mutexes and never
/// block one another. The registry of per-key mutexes is guarded by a short-held
/// std [`Mutex`] taken only to look up/create the per-key mutex.
/// The idempotency key an induction is serialized on: `(node_id, subtree_hash)`.
type IdempotencyKey = (GoalNodeId, SubtreeHash);

/// A per-key async mutex; its guard is held across the Judge/Author `.await`s.
type KeyLock = Arc<tokio::sync::Mutex<()>>;

#[derive(Debug, Default)]
struct KeyLocks {
    locks: Mutex<HashMap<IdempotencyKey, KeyLock>>,
}

impl KeyLocks {
    fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// The async mutex guarding `key`, creating it on first use.
    fn lock_for(&self, key: &IdempotencyKey) -> KeyLock {
        let mut guard = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            guard
                .entry(key.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

// ---------------------------------------------------------------------------
// Disabled-writer embedding client (backward-compat for `new`)
// ---------------------------------------------------------------------------

/// A zero-sized [`EmbeddingClient`] whose [`embed_once`](EmbeddingClient::embed_once)
/// is never reached.
///
/// It backs [`InductionEngine::new`]'s backward-compatible disabled-writer
/// path: `new` builds a [`MemoryEmbeddingWriter`] over this client from a
/// disabled, credential-less [`ResolvedEmbeddingSettings`], so the writer
/// short-circuits every `embed_for_write` to `Unavailable` *before* any client
/// call — induced memories store an empty embedding exactly as they did before
/// the write path was wired in (Req 6.4, 10.4). The transport is therefore
/// dead code; it returns a retryable [`EmbeddingClientError::Transport`] rather
/// than panicking so the type stays a total, panic-free `EmbeddingClient` even
/// if some future path reached it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoEmbeddingClient;

#[async_trait]
impl EmbeddingClient for NoEmbeddingClient {
    async fn embed_once(
        &self,
        _request: &EmbeddingRequest,
        _cancel: CancellationToken,
    ) -> Result<EmbeddingResponse, EmbeddingClientError> {
        // Never reached: the disabled writer degrades to `Unavailable` before
        // any client call. Degrade rather than panic if ever reached.
        Err(EmbeddingClientError::Transport)
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Ties together a [`MemoryStore`], [`RecurrenceTracker`], [`Judge`],
/// [`Author`], and threshold `N` to run the induction pipeline.
///
/// See the module docs for the algorithm. The engine is generic over the store
/// so it composes with any [`MemoryStore`]; Judge/Author/tracker are
/// trait objects so callers can supply model-backed or test implementations.
pub struct InductionEngine<S: MemoryStore, C: EmbeddingClient> {
    store: Arc<S>,
    tracker: Arc<dyn RecurrenceTracker>,
    judge: Arc<dyn Judge>,
    author: Arc<dyn Author>,
    threshold: u64,
    log: InductionLog,
    key_locks: Arc<KeyLocks>,
    /// The write-path embedding producer. The single non-dedup insert routes
    /// through [`insert_memory_with_writer`] so induced memories store real
    /// embeddings (Req 6.1, 6.2), degrading to an empty embedding when the
    /// backend is unavailable/disabled (Req 6.3).
    writer: MemoryEmbeddingWriter<C>,
    /// The query-path source's configured dimension, passed to
    /// [`insert_memory_with_writer`] so the writer/source dimension-consistency
    /// check runs before each insert (Req 8.2).
    source_dimension: Option<u32>,
}

impl<S: MemoryStore> InductionEngine<S, NoEmbeddingClient> {
    /// Build an engine on the backward-compatible **disabled-writer** path
    /// (unchanged params).
    ///
    /// This is the drop-in replacement for the original `new`: it constructs a
    /// [`MemoryEmbeddingWriter`] over a zero-sized [`NoEmbeddingClient`] from a
    /// disabled, credential-less [`ResolvedEmbeddingSettings`], with
    /// `source_dimension = None`. Because the writer is disabled, its
    /// `embed_for_write` short-circuits to `Unavailable` with no network call,
    /// so induced memories store an empty embedding exactly as they did before
    /// the write path was wired in (Req 6.4, 10.4). Callers that want real
    /// write-time embeddings use [`InductionEngine::with_writer`] instead.
    #[must_use]
    pub fn new(
        store: Arc<S>,
        tracker: Arc<dyn RecurrenceTracker>,
        judge: Arc<dyn Judge>,
        author: Arc<dyn Author>,
        threshold: u64,
    ) -> Self {
        let writer =
            MemoryEmbeddingWriter::new(NoEmbeddingClient, ResolvedEmbeddingSettings::disabled());
        Self::with_writer(store, tracker, judge, author, threshold, writer, None)
    }
}

impl<S: MemoryStore, C: EmbeddingClient> InductionEngine<S, C> {
    /// Build an engine that inserts through [`insert_memory_with_writer`] using
    /// the given write-path `writer` and query-path `source_dimension`
    /// (Req 6, 8). This is the runtime path; embeddings are produced async,
    /// outside the store lock, and handed to the synchronous insert (Req 9).
    #[must_use]
    pub fn with_writer(
        store: Arc<S>,
        tracker: Arc<dyn RecurrenceTracker>,
        judge: Arc<dyn Judge>,
        author: Arc<dyn Author>,
        threshold: u64,
        writer: MemoryEmbeddingWriter<C>,
        source_dimension: Option<u32>,
    ) -> Self {
        Self {
            store,
            tracker,
            judge,
            author,
            threshold,
            log: InductionLog::new(),
            key_locks: Arc::new(KeyLocks::new()),
            writer,
            source_dimension,
        }
    }

    /// The induction log recording declines (15.3) and author failures (15.6).
    #[must_use]
    pub fn log(&self) -> &InductionLog {
        &self.log
    }

    /// Run induction for a resolved `node`, following the design pseudocode.
    ///
    /// Records exactly one occurrence, gates on `N`, and — when past the gate —
    /// runs the idempotency check, Judge, Author, and dedup/insert steps under a
    /// per-key lock so concurrent runs for the same key stay serialized.
    pub async fn induce_memory(&self, node: &GoalNode) -> InductionOutcome {
        // ASSERT node.resolution != Open. The closure path only enqueues closed
        // subtrees, but we treat an Open node defensively as a no-op skip rather
        // than panicking, so a stray enqueue can never author from open work.
        if node.resolution.is_open() {
            return InductionOutcome::Skipped("node is not resolved".to_owned());
        }

        // (14.1) Record exactly one occurrence BEFORE evaluating the gate,
        // regardless of the gate outcome.
        self.tracker.record_occurrence(&node.intent);

        // (14.2/14.3/14.4) Gate on the recurrence threshold. Below N: author
        // nothing, do NOT invoke Judge, terminate.
        if self.tracker.recurrence_count(&node.intent) < self.threshold {
            return InductionOutcome::Skipped(
                "below recurrence threshold".to_owned(),
            );
        }

        // The idempotency key. A closed subtree always carries a subtree_hash;
        // if it is somehow absent we cannot key the memory, so skip defensively.
        let Some(subtree_hash) = node.subtree_hash.clone() else {
            return InductionOutcome::Skipped(
                "resolved node has no subtree_hash".to_owned(),
            );
        };
        let key = (node.id.clone(), subtree_hash);

        // (16.3) Serialize the check-then-insert critical section per key so
        // exactly one concurrent run inserts and the rest reinforce. We hold the
        // async key lock across the whole critical section, which spans the
        // Judge/Author `.await` points; a `tokio::sync::Mutex` guard is `Send`,
        // so the future stays `Send` and this works across worker threads.
        let key_lock = self.key_locks.lock_for(&key);
        let _guard = key_lock.lock().await;

        // (16.1) Idempotency: if a memory already exists for the key, reinforce
        // it and author no duplicate.
        if let Some(existing) = self.store.has_memory_for(&key) {
            return match self.reinforce_existing(&existing) {
                Ok(id) => InductionOutcome::Reinforced(id),
                Err(reason) => InductionOutcome::AuthorFailed(reason),
            };
        }

        // (15.1/15.5) Step 1: Judge, in a freshly constructed clean context.
        let judge_ctx = CleanContext::from_node(node);
        let verdict = self.judge.evaluate(&judge_ctx).await;

        if verdict.decision == Decision::Decline {
            // (15.3) Persist the decline rationale; author nothing.
            self.log.record_decline(DeclineRecord {
                intent: node.intent.clone(),
                key: key.clone(),
                rationale: verdict.rationale.clone(),
            });
            return InductionOutcome::Rejected(verdict.rationale);
        }

        // (15.1/15.4/15.5) Step 2: Author, in a *separate* freshly constructed
        // clean context (a distinct invocation from Judge).
        let author_ctx = CleanContext::from_node(node);
        let candidate = match self.author.write(verdict.granularity, &author_ctx).await {
            Ok(mem) => mem,
            Err(err) => {
                // (15.6) Insert nothing, record the failure, leave the store
                // unchanged, and do NOT mark the key done — a later recurrence
                // re-triggers induction (we simply return without inserting).
                self.log.record_failure(FailureRecord {
                    intent: node.intent.clone(),
                    key: key.clone(),
                    reason: err.0.clone(),
                });
                return InductionOutcome::AuthorFailed(err.0);
            }
        };

        // (16.2) Dedup on intent+plan: reinforce a matching memory instead of
        // inserting a duplicate.
        if let Some(dup) =
            self.store.find_duplicate(&candidate.intent, &candidate.plan)
        {
            return match self.store.reinforce(&dup, candidate) {
                Ok(id) => InductionOutcome::Reinforced(id),
                Err(err) => InductionOutcome::AuthorFailed(err.to_string()),
            };
        }

        // Otherwise insert under the idempotency key, routing through the
        // writer so the memory stores a real embedding. The embedding is
        // produced async, UP FRONT and OUTSIDE the store lock, then handed to
        // the synchronous `insert_with_embedding` (Req 6.1, 6.2, 8.2, 9.1,
        // 9.2). `source_dimension` drives the writer/source consistency check.
        match insert_memory_with_writer(
            &self.writer,
            self.source_dimension,
            self.store.as_ref(),
            key,
            candidate,
        )
        .await
        {
            Ok(id) => InductionOutcome::Inserted(id),
            Err(err) => InductionOutcome::AuthorFailed(err.to_string()),
        }
    }

    /// Reinforce the existing memory found by idempotency key (Requirement
    /// 16.1), authoring no duplicate.
    ///
    /// The idempotent path deliberately runs **no** Judge/Author step — the
    /// design reinforces the existing memory in place. [`MemoryStore::reinforce`]
    /// for a known id bumps its reinforcement bookkeeping; it still validates the
    /// candidate it is given, so we hand it the memory's own current body (which
    /// is already valid, since it was validated at insert time).
    fn reinforce_existing(&self, existing: &MemoryId) -> Result<MemoryId, String> {
        let candidate = self
            .stored_memory_by_id(existing)
            .ok_or_else(|| "existing memory not found for reinforcement".to_owned())?;
        self.store
            .reinforce(existing, candidate)
            .map_err(|e| e.to_string())
    }

    /// Locate a stored memory by id.
    ///
    /// The [`MemoryStore`] trait offers no direct get-by-id, so we recall every
    /// stored memory (the in-memory store returns all of them for an
    /// unconstrained recall) and pick the one whose id matches. This is only used
    /// on the idempotent reinforcement path, which is off the hot path.
    fn stored_memory_by_id(&self, id: &MemoryId) -> Option<Memory> {
        self.store
            .ann_recall(&crate::tier2::memory::Embedding::default(), usize::MAX)
            .into_iter()
            .find(|m| &m.id == id)
    }
}

// ---------------------------------------------------------------------------
// Deterministic, agent-data-backed Judge + Author (runtime production path)
// ---------------------------------------------------------------------------

/// The runtime [`Judge`] that decides worth from the agent-supplied resolution
/// alone — no model call.
///
/// The agent already supplies structured goal data (its [`IntentSignature`],
/// hypothesis, and resolution conditions) at create/resolve time, and that data
/// flows into induction through [`CleanContext`]/[`DistilledNode`]. A goal the
/// agent *solved* is worth remembering, so this Judge approves at
/// [`Granularity::Fragment`] exactly when the node's resolution is
/// [`Resolution::Accepted`], and declines otherwise (a rejected or inconclusive
/// goal is not authored). The decision is fully deterministic and reads only the
/// distilled node.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalResolutionJudge;

impl GoalResolutionJudge {
    /// Create the Judge. Stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Judge for GoalResolutionJudge {
    async fn evaluate(&self, ctx: &CleanContext) -> JudgeVerdict {
        match ctx.node().resolution {
            Resolution::Accepted => JudgeVerdict::approve(
                Granularity::Fragment,
                "goal resolved as accepted; the solved procedure is worth remembering",
            ),
            Resolution::Rejected => {
                JudgeVerdict::decline("goal was rejected; a rejected goal is not authored")
            }
            Resolution::Inconclusive => JudgeVerdict::decline(
                "goal was inconclusive; an inconclusive goal is not authored",
            ),
            Resolution::Open => {
                JudgeVerdict::decline("goal is still open; an open goal is not authored")
            }
        }
    }
}

/// The runtime [`Author`] that builds a [`Memory`] deterministically from the
/// agent-supplied node data — no model call, no served answer.
///
/// It authors a minimal, valid [`MemoryKind::Fragment`] from the
/// [`DistilledNode`]: the node's [`IntentSignature`] becomes the memory intent,
/// the [`Applicability`] guard requires the same `intent_type`, and the [`Plan`]
/// steps are derived from the node's `hypothesis` and each of its
/// `resolution_conditions`. `cached_outcome` is left `None` — this feature
/// serves no cached answers — and the remaining bookkeeping fields take their
/// defaults. The memory `version` and a stable [`MemoryId`] are derived from the
/// node's `subtree_hash` (falling back to the node id when the hash carries no
/// content); authoring fails cleanly only when no stable version key can be
/// derived (a closed node with no `subtree_hash`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalResolutionAuthor;

impl GoalResolutionAuthor {
    /// Create the Author. Stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Author for GoalResolutionAuthor {
    async fn write(
        &self,
        granularity: Granularity,
        ctx: &CleanContext,
    ) -> Result<Memory, AuthorError> {
        let node = ctx.node();

        // A stable version key is required. A closed node always carries a
        // `subtree_hash`; if it is somehow absent we cannot version the memory,
        // so fail cleanly (the engine records the failure and inserts nothing).
        let subtree_hash = node.subtree_hash.clone().ok_or_else(|| {
            AuthorError::new("cannot author a memory: resolved node has no subtree_hash")
        })?;

        // Author only the granularity the Judge approved. This deterministic
        // Author approves solved goals as `Fragment`s; any other granularity is
        // not something this Author knows how to write.
        if granularity != Granularity::Fragment {
            return Err(AuthorError::new(format!(
                "unsupported granularity {granularity:?}; this author writes fragments only"
            )));
        }

        // A stable id derived from the subtree hash keeps re-authoring the same
        // subtree idempotent at the id level; the node id disambiguates.
        let memory_id = MemoryId::from(format!("mem-{}-{}", node.id, subtree_hash));

        // Derive plan steps from the agent-supplied hypothesis and resolution
        // conditions. The hypothesis is the leading step (what the goal tested);
        // each resolution condition follows (the conditions under which it
        // resolved). Skip empty descriptions so the plan carries only content.
        let mut steps: Vec<PlanStep> = Vec::new();
        if !node.hypothesis.trim().is_empty() {
            steps.push(PlanStep {
                description: node.hypothesis.clone(),
                tool: None,
                intent: None,
            });
        }
        for condition in &node.resolution_conditions {
            if condition.trim().is_empty() {
                continue;
            }
            steps.push(PlanStep {
                description: condition.clone(),
                tool: None,
                intent: None,
            });
        }

        let intent = node.intent.clone();
        let applicability = Applicability {
            required_intent_type: Some(intent.intent_type.clone()),
            ..Applicability::default()
        };

        Ok(Memory {
            id: memory_id,
            kind: MemoryKind::Fragment,
            intent,
            parameter_schema: ParameterSchema::default(),
            applicability,
            plan: Plan { steps },
            evidence_contract: EvidenceContract::default(),
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from(format!("outcome://{}", node.id)),
                schema: String::new(),
            },
            // This feature serves no cached answers.
            cached_outcome: None,
            provenance: Provenance {
                origins: vec![(node.id.clone(), subtree_hash.clone())],
            },
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(subtree_hash),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::goal_model::GoalNode;
    use crate::tier2::memory::{
        AnswerMode, Applicability, CachedOutcome, EvidenceContract, EvidenceContractItem,
        MemoryVersion, OutcomeShape, OutcomeValue, ParameterSchema, Plan, PlanStep,
        Provenance, Reinforcement,
    };
    use crate::tier2::store::InMemoryMemoryStore;
    use crate::types::{
        CanonicalJson, IntentType, OutcomeRef, Scope, Sha256, TargetRef, TargetType, Timestamp,
        ToolName, ValidityToken,
    };

    // --- fixtures ---------------------------------------------------------

    fn intent() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn subtree_hash(seed: &str) -> SubtreeHash {
        SubtreeHash(Sha256::from(seed))
    }

    /// A resolved node with a subtree hash — the induction input.
    fn resolved_node(id: &str, hash: &str) -> GoalNode {
        GoalNode {
            id: GoalNodeId::from(id),
            parent: None,
            children: Vec::new(),
            hypothesis: "the file exists".to_owned(),
            resolution_conditions: vec!["file readable".to_owned()],
            resolution: Resolution::Accepted,
            intent: intent(),
            tool_calls: Vec::new(),
            subtree_hash: Some(subtree_hash(hash)),
        }
    }

    /// A valid `Fragment` memory the Author emits.
    fn authored_memory(id: &str) -> Memory {
        Memory {
            id: MemoryId::from(id),
            kind: MemoryKind::Fragment,
            intent: intent(),
            parameter_schema: ParameterSchema::default(),
            applicability: Applicability {
                required_intent_type: Some(IntentType::from("lookup")),
                ..Applicability::default()
            },
            plan: Plan {
                steps: vec![PlanStep {
                    description: "read the file".to_owned(),
                    tool: Some(ToolName::from("read_file")),
                    intent: None,
                }],
            },
            evidence_contract: EvidenceContract {
                items: vec![EvidenceContractItem {
                    tool: ToolName::from("read_file"),
                    normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
                    validity_token: ValidityToken::ContentHash(Sha256::from("abc")),
                }],
            },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            cached_outcome: Some(CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{\"lines\":42}".to_owned())),
                mode: AnswerMode::SoundPinnable,
                validity_tokens: vec![ValidityToken::ContentHash(Sha256::from("abc"))],
                issued_at: Timestamp(0),
            }),
            provenance: Provenance::default(),
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(subtree_hash("v1")),
        }
    }

    // --- test doubles -----------------------------------------------------

    /// A Judge that always approves, counting its invocations.
    struct SpyJudge {
        decision: Decision,
        calls: Arc<AtomicUsize>,
    }

    impl SpyJudge {
        fn approving() -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    decision: Decision::Approve,
                    calls: Arc::clone(&calls),
                }),
                calls,
            )
        }

        fn declining() -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    decision: Decision::Decline,
                    calls: Arc::clone(&calls),
                }),
                calls,
            )
        }
    }

    #[async_trait]
    impl Judge for SpyJudge {
        async fn evaluate(&self, ctx: &CleanContext) -> JudgeVerdict {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Structural clean-context check: the context exposes only a
            // distilled node, never a transcript. Touch it to prove we work off
            // distilled fields alone.
            let _distilled: &DistilledNode = ctx.node();
            match self.decision {
                Decision::Approve => {
                    JudgeVerdict::approve(Granularity::Fragment, "worth caching")
                }
                Decision::Decline => JudgeVerdict::decline("one-off, not worth caching"),
            }
        }
    }

    /// An Author that emits a fixed valid memory, counting invocations.
    struct FixedAuthor {
        mem_id: String,
        calls: Arc<AtomicUsize>,
    }

    impl FixedAuthor {
        fn new(mem_id: &str) -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    mem_id: mem_id.to_owned(),
                    calls: Arc::clone(&calls),
                }),
                calls,
            )
        }
    }

    #[async_trait]
    impl Author for FixedAuthor {
        async fn write(
            &self,
            _granularity: Granularity,
            ctx: &CleanContext,
        ) -> Result<Memory, AuthorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _distilled: &DistilledNode = ctx.node();
            Ok(authored_memory(&self.mem_id))
        }
    }

    /// An Author that always fails.
    struct FailingAuthor {
        calls: Arc<AtomicUsize>,
    }

    impl FailingAuthor {
        fn new() -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    calls: Arc::clone(&calls),
                }),
                calls,
            )
        }
    }

    #[async_trait]
    impl Author for FailingAuthor {
        async fn write(
            &self,
            _granularity: Granularity,
            _ctx: &CleanContext,
        ) -> Result<Memory, AuthorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(AuthorError::new("could not synthesize a well-formed memory"))
        }
    }

    fn engine_with(
        store: Arc<InMemoryMemoryStore>,
        tracker: Arc<InMemoryRecurrenceTracker>,
        judge: Arc<dyn Judge>,
        author: Arc<dyn Author>,
        n: u64,
    ) -> InductionEngine<InMemoryMemoryStore, NoEmbeddingClient> {
        InductionEngine::new(store, tracker, judge, author, n)
    }

    // --- 14: recurrence gate ---------------------------------------------

    #[tokio::test]
    async fn below_threshold_skips_and_does_not_invoke_judge() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, judge_calls) = SpyJudge::approving();
        let (author, author_calls) = FixedAuthor::new("mem-1");
        // N = 3, so a single run is below threshold.
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 3);

        let node = resolved_node("n1", "h1");
        let outcome = engine.induce_memory(&node).await;

        assert!(
            matches!(outcome, InductionOutcome::Skipped(_)),
            "below N must Skip, got {outcome:?}"
        );
        // (14.1) exactly one occurrence recorded before the gate.
        assert_eq!(tracker.recurrence_count(&node.intent), 1);
        // (14.2) Judge not invoked; Author not invoked.
        assert_eq!(judge_calls.load(Ordering::SeqCst), 0, "Judge must not run below N");
        assert_eq!(author_calls.load(Ordering::SeqCst), 0);
        // Nothing inserted.
        assert!(store.filter(&node.intent).is_empty());
    }

    #[tokio::test]
    async fn at_threshold_proceeds_to_judge_and_inserts() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, judge_calls) = SpyJudge::approving();
        let (author, _) = FixedAuthor::new("mem-1");
        // N = 1, so the first run is at threshold and proceeds.
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 1);

        let node = resolved_node("n1", "h1");
        let outcome = engine.induce_memory(&node).await;

        assert!(
            matches!(outcome, InductionOutcome::Inserted(_)),
            "at/above N must proceed and insert, got {outcome:?}"
        );
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1, "Judge must run at N");
        assert_eq!(store.filter(&node.intent).len(), 1);
    }

    #[tokio::test]
    async fn occurrence_recorded_even_when_gated() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, _) = SpyJudge::approving();
        let (author, _) = FixedAuthor::new("mem-1");
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 5);

        let node = resolved_node("n1", "h1");
        // Three below-threshold runs each record exactly one occurrence.
        engine.induce_memory(&node).await;
        engine.induce_memory(&node).await;
        engine.induce_memory(&node).await;
        assert_eq!(tracker.recurrence_count(&node.intent), 3);
    }

    // --- 15: two-step Judge/Author ---------------------------------------

    #[tokio::test]
    async fn judge_decline_rejects_persists_rationale_and_skips_author() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, judge_calls) = SpyJudge::declining();
        let (author, author_calls) = FixedAuthor::new("mem-1");
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 1);

        let node = resolved_node("n1", "h1");
        let outcome = engine.induce_memory(&node).await;

        match outcome {
            InductionOutcome::Rejected(rationale) => {
                assert_eq!(rationale, "one-off, not worth caching");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1);
        // (15.3) Author never runs on decline.
        assert_eq!(author_calls.load(Ordering::SeqCst), 0);
        // Nothing inserted.
        assert!(store.filter(&node.intent).is_empty());
        // (15.3) The decline rationale is persisted.
        let declines = engine.log().declines();
        assert_eq!(declines.len(), 1);
        assert_eq!(declines[0].rationale, "one-off, not worth caching");
        assert_eq!(declines[0].intent, node.intent);
    }

    #[tokio::test]
    async fn approve_then_author_ok_inserts_and_store_has_memory() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, _) = SpyJudge::approving();
        let (author, author_calls) = FixedAuthor::new("mem-ins");
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 1);

        let node = resolved_node("n1", "h1");
        let outcome = engine.induce_memory(&node).await;

        assert_eq!(outcome, InductionOutcome::Inserted(MemoryId::from("mem-ins")));
        assert_eq!(author_calls.load(Ordering::SeqCst), 1);
        let found = store.filter(&node.intent);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, MemoryId::from("mem-ins"));
    }

    #[tokio::test]
    async fn author_failure_inserts_nothing_records_failure_and_allows_retrigger() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, _) = SpyJudge::approving();
        let (author, author_calls) = FailingAuthor::new();
        let engine = engine_with(
            Arc::clone(&store),
            Arc::clone(&tracker),
            judge,
            author,
            1,
        );

        let node = resolved_node("n1", "h1");
        let outcome = engine.induce_memory(&node).await;

        assert!(
            matches!(outcome, InductionOutcome::AuthorFailed(_)),
            "expected AuthorFailed, got {outcome:?}"
        );
        // (15.6) Nothing inserted.
        assert!(store.filter(&node.intent).is_empty());
        // (15.6) Failure recorded.
        assert_eq!(engine.log().failures().len(), 1);
        assert_eq!(engine.log().failures()[0].intent, node.intent);

        // (15.6) A subsequent recurrence can re-trigger: since the key was not
        // marked done, re-running reaches Judge and Author again.
        let outcome2 = engine.induce_memory(&node).await;
        assert!(
            matches!(outcome2, InductionOutcome::AuthorFailed(_)),
            "re-trigger reaches Author again, got {outcome2:?}"
        );
        assert_eq!(
            author_calls.load(Ordering::SeqCst),
            2,
            "Author was re-invoked on re-trigger"
        );
    }

    #[tokio::test]
    async fn clean_context_carries_only_distilled_node_no_transcript() {
        // Structural guarantee (15.5): a CleanContext exposes only a
        // DistilledNode, whose fields are the node's distilled structure. There
        // is no messages/transcript/reasoning field to populate.
        let node = resolved_node("n1", "h1");
        let ctx = CleanContext::from_node(&node);
        let distilled = ctx.node();
        assert_eq!(distilled.id, node.id);
        assert_eq!(distilled.intent, node.intent);
        assert_eq!(distilled.hypothesis, node.hypothesis);
        assert_eq!(distilled.resolution, node.resolution);
        // The type has no transcript accessor; this test compiles precisely
        // because no such field/method exists.
    }

    // --- 16: idempotency, dedup, reinforcement ---------------------------

    #[tokio::test]
    async fn same_key_again_reinforces_without_duplicate() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, judge_calls) = SpyJudge::approving();
        let (author, author_calls) = FixedAuthor::new("mem-1");
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 1);

        let node = resolved_node("n1", "h1");
        let first = engine.induce_memory(&node).await;
        assert!(matches!(first, InductionOutcome::Inserted(_)));

        // Second run for the same (node_id, subtree_hash): reinforce, no new
        // author, store count stays 1.
        let second = engine.induce_memory(&node).await;
        assert!(
            matches!(second, InductionOutcome::Reinforced(_)),
            "same key must reinforce, got {second:?}"
        );
        assert_eq!(store.filter(&node.intent).len(), 1, "no duplicate memory");
        // (16.1) Judge/Author ran only once (the first time); the idempotent
        // path authored nothing.
        assert_eq!(judge_calls.load(Ordering::SeqCst), 1);
        assert_eq!(author_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dedup_on_intent_and_plan_reinforces_instead_of_inserting() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, _) = SpyJudge::approving();
        // Author emits the same memory body (same intent+plan) each time.
        let (author, _) = FixedAuthor::new("mem-dup");
        let engine =
            engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 1);

        // First closure under key (n1, h1) inserts.
        let node1 = resolved_node("n1", "h1");
        assert!(matches!(
            engine.induce_memory(&node1).await,
            InductionOutcome::Inserted(_)
        ));

        // A DIFFERENT key (n2, h2) but the Author produces a memory with the
        // same intent+plan -> dedup reinforces instead of inserting (16.2).
        let node2 = resolved_node("n2", "h2");
        let outcome = engine.induce_memory(&node2).await;
        assert!(
            matches!(outcome, InductionOutcome::Reinforced(_)),
            "intent+plan match must reinforce, got {outcome:?}"
        );
        assert_eq!(
            store.filter(&intent()).len(),
            1,
            "dedup keeps a single memory"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_inductions_for_same_key_insert_exactly_once() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, _) = SpyJudge::approving();
        let (author, _) = FixedAuthor::new("mem-conc");
        let engine = Arc::new(engine_with(
            Arc::clone(&store),
            Arc::clone(&tracker),
            judge,
            author,
            1,
        ));

        // Fire many concurrent inductions for the SAME (node_id, subtree_hash).
        let mut handles = Vec::new();
        for _ in 0..8 {
            let engine = Arc::clone(&engine);
            handles.push(tokio::spawn(async move {
                let node = resolved_node("n1", "h1");
                engine.induce_memory(&node).await
            }));
        }

        let mut inserts = 0;
        let mut reinforces = 0;
        for h in handles {
            match h.await.expect("task panicked") {
                InductionOutcome::Inserted(_) => inserts += 1,
                InductionOutcome::Reinforced(_) => reinforces += 1,
                other => panic!("unexpected outcome under concurrency: {other:?}"),
            }
        }

        // (16.3) Exactly one insert; every other run reinforced.
        assert_eq!(inserts, 1, "exactly one insert under concurrency");
        assert_eq!(reinforces, 7, "all other runs reinforce");
        assert_eq!(store.filter(&intent()).len(), 1, "at most one memory for the key");
    }

    #[tokio::test]
    async fn open_node_is_skipped_without_recording_or_judging() {
        let store = Arc::new(InMemoryMemoryStore::new());
        let tracker = Arc::new(InMemoryRecurrenceTracker::new());
        let (judge, judge_calls) = SpyJudge::approving();
        let (author, _) = FixedAuthor::new("mem-1");
        let engine = engine_with(Arc::clone(&store), Arc::clone(&tracker), judge, author, 1);

        let mut node = resolved_node("n1", "h1");
        node.resolution = Resolution::Open;

        let outcome = engine.induce_memory(&node).await;
        assert!(
            matches!(outcome, InductionOutcome::Skipped(_)),
            "an Open node must be skipped, got {outcome:?}"
        );
        assert_eq!(judge_calls.load(Ordering::SeqCst), 0, "Judge never runs for Open");
        assert!(store.filter(&node.intent).is_empty());
    }
}
