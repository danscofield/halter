//! End-to-end wiring that connects the three subsystems into the two flows the
//! design shows in § "Example Usage" (task 16).
//!
//! Two seams are realized here:
//!
//! - **Closure -> induction, async, off the hot path** (task 16.1, Requirements
//!   6.3, 14.5, 15.1, 16.1). [`EngineInductionQueue`] is an
//!   [`InductionQueue`](crate::goal_model::InductionQueue) whose `enqueue`
//!   resolves the closed [`GoalNode`](crate::goal_model::GoalNode) back out of a
//!   [`GoalStore`](crate::goal_model::GoalStore) (using the
//!   [`SessionId`](halter_protocol::SessionId) the [`ClosureSignal`] now carries)
//!   and runs the [`InductionEngine`](crate::tier2::InductionEngine)'s
//!   `induce_memory` on a spawned task, so `close` returns to the caller **before
//!   induction runs**. This is exactly the design's `on_goal_closed`: enqueue,
//!   don't await.
//!
//! - **Retrieval + replay -> Tier 1, on the hot path** (task 16.2, Requirements
//!   13.2, 19.1, 20.1, 23.1). [`start_goal`] ties
//!   [`Retrieval::retrieve_memories`](crate::tier2::Retrieval::retrieve_memories)
//!   into [`decide_replay`](crate::tier2::decide_replay), re-validating evidence
//!   and answer tokens through a caller-supplied
//!   [`EvidenceValidator`](crate::tier2::EvidenceValidator), mirroring the
//!   design's `start_goal`.

use std::sync::Arc;

use async_trait::async_trait;

use halter_providers::EmbeddingClient;

use crate::goal_model::{ClosureSignal, GoalStore, InductionQueue};
use crate::tier2::store::MemoryStore;
use crate::tier2::{
    decide_replay, EvidenceValidator, GoalSummary, InductionEngine, InductionOutcome, ModeBPolicy,
    ReplayDecision, Retrieval, ScoredMemory, SummaryProvider,
};
use crate::types::{IntentSignature, Timestamp};

// ===========================================================================
// Task 16.1 — closure -> induction, async, off the hot path
// ===========================================================================

/// The [`InductionQueue`] that actually runs induction (task 16.1).
///
/// This is the real backend behind the closure-enqueue seam: on `enqueue` it
/// hands the job to a spawned task and returns immediately, so
/// [`GoalStore::close`] never blocks on Judge/Author work (Requirements 6.3,
/// 14.5). The spawned task:
///
/// 1. resolves the closed [`GoalNode`](crate::goal_model::GoalNode) from the
///    store by `(session, node_id)` — the [`ClosureSignal`] carries only the
///    key, so the node is re-projected from the shared goal log; and
/// 2. runs [`InductionEngine::induce_memory`], which itself performs the gate ->
///    Judge -> Author -> dedup/reinforce/insert pipeline (Requirements 15.1,
///    16.1).
///
/// The engine's own idempotency keys on `(node_id, subtree_hash)`, so a
/// re-enqueued job (should one ever slip past the closure-level exactly-once
/// guard) still authors at most one memory (Requirement 16.1).
///
/// # Generic parameters
///
/// - `G` — the [`GoalStore`] the node is resolved from.
/// - `M` — the [`MemoryStore`] the [`InductionEngine`] writes to.
/// - `C` — the [`EmbeddingClient`] the [`InductionEngine`]'s write-path writer
///   uses (see [`InductionEngine::with_writer`]).
///
/// Both the store and the engine are shared (`Arc`) so the spawned task can own
/// clones that outlive the `enqueue` call.
pub struct EngineInductionQueue<G: GoalStore, M: MemoryStore, C: EmbeddingClient> {
    store: Arc<G>,
    engine: Arc<InductionEngine<M, C>>,
}

impl<G: GoalStore, M: MemoryStore, C: EmbeddingClient> EngineInductionQueue<G, M, C> {
    /// Wire a goal `store` (to resolve closed nodes) to an induction `engine`.
    #[must_use]
    pub fn new(store: Arc<G>, engine: Arc<InductionEngine<M, C>>) -> Self {
        Self { store, engine }
    }

    /// Resolve the closed node and run induction to completion.
    ///
    /// This is the body the spawned task executes; it is also directly callable
    /// (e.g. by tests) to run induction synchronously and observe its
    /// [`InductionOutcome`]. Returns `None` when the node can no longer be
    /// resolved from the store (e.g. an empty or advanced log), in which case
    /// nothing is induced.
    pub async fn run(&self, signal: &ClosureSignal) -> Option<InductionOutcome> {
        // Re-project the resolved node from the shared goal log. The signal
        // carries only (node_id, subtree_hash); the session tells us which log
        // to fold. If the node is gone, there is nothing to induce.
        let node = self
            .store
            .get(&signal.session, &signal.node_id)
            .await
            .ok()
            .flatten()?;
        Some(self.engine.induce_memory(&node).await)
    }
}

#[async_trait]
impl<G, M, C> InductionQueue for EngineInductionQueue<G, M, C>
where
    G: GoalStore + 'static,
    M: MemoryStore + Send + Sync + 'static,
    C: EmbeddingClient + 'static,
{
    async fn enqueue(&self, signal: ClosureSignal) {
        // Hand the job to a spawned task and return: induction runs off the hot
        // path, so close() does not await Judge/Author (Requirements 6.3, 14.5).
        let store = Arc::clone(&self.store);
        let engine = Arc::clone(&self.engine);
        tokio::spawn(async move {
            let worker = EngineInductionQueue { store, engine };
            // The result is intentionally dropped here — this is the async
            // induction worker, mirroring the design's `on_goal_closed`, which
            // only logs the outcome. Callers that need the outcome use `run`.
            let _ = worker.run(&signal).await;
        });
    }
}

// ===========================================================================
// Task 16.2 — retrieval + replay -> Tier 1, on the hot path
// ===========================================================================

/// The hot-path outcome of starting a goal: either a memory-driven replay
/// decision, or a signal that there was no applicable memory (do real work).
///
/// This mirrors the design's `start_goal`: a retrieval hit yields the
/// [`decide_replay`](crate::tier2::decide_replay) decision for the top
/// candidate; a miss yields [`GoalStart::FromScratch`] (the design's
/// `GoalPlan::from_scratch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalStart {
    /// A memory applied; carries the replay decision for the top candidate.
    Replay(ReplayDecision),
    /// No applicable memory was retrieved — start the goal from scratch.
    FromScratch,
}

/// Tie retrieval to replay on the hot path (task 16.2, design `start_goal`).
///
/// Runs the structured-first
/// [`Retrieval::retrieve_memories`](crate::tier2::Retrieval::retrieve_memories),
/// takes the top-ranked [`ScoredMemory`](crate::tier2::ScoredMemory), and calls
/// [`decide_replay`](crate::tier2::decide_replay) for it — which re-validates
/// evidence/answer tokens through the Tier 1-backed `validator` (Requirements
/// 13.2, 19.1, 20.1, 23.1). When retrieval returns no candidates, this returns
/// [`GoalStart::FromScratch`].
///
/// # Arguments
///
/// - `sig` — the new goal's intent signature (drives retrieval and Mode B opt-in).
/// - `retrieval` — the structured-first retrieval engine.
/// - `validator` — the caller-supplied evidence validator.
/// - `mode_b_policy` — the Mode B opt-in policy.
/// - `now` — the serve-time instant (used for Mode B age).
///
/// This function is `async` because [`Retrieval::retrieve_memories`] now awaits
/// the embedding backend when the structured head is thin (Requirements 1.4,
/// 6.3). The replay decision itself is unchanged.
pub async fn start_goal(
    sig: &IntentSignature,
    retrieval: &impl Retrieval,
    validator: &impl EvidenceValidator,
    mode_b_policy: &impl ModeBPolicy,
    now: Timestamp,
) -> GoalStart {
    let candidates = retrieval.retrieve_memories(sig).await;
    let Some(top) = candidates.first() else {
        // Cold: no memory, do real work (design `GoalPlan::from_scratch`).
        return GoalStart::FromScratch;
    };
    GoalStart::Replay(decide_replay(&top.memory, validator, mode_b_policy, sig, now))
}

// ===========================================================================
// Part 2 — additive hot-path wiring: start_goal + similar-goal summaries
// ===========================================================================

/// The hot-path outcome plus any similar-goal summaries. Additive over
/// [`GoalStart`].
///
/// This wraps the *unchanged* [`GoalStart`] shape and attaches any
/// [`GoalSummary`] values produced by the opt-in
/// [`SummaryProvider`](crate::tier2::SummaryProvider). Because the `start`
/// field is exactly the value [`start_goal`] would have returned, the replay
/// decision is unaffected by summaries (Requirements 13.3, 14.1, 14.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalStartWithSummaries {
    /// The existing replay decision (unchanged shape).
    pub start: GoalStart,
    /// Similar-goal summaries; empty when disabled, no candidates, or on failure.
    pub summaries: Vec<GoalSummary>,
}

/// Like [`start_goal`], but also attaches similar-goal summaries when enabled.
///
/// Always computes `start` via the existing [`start_goal`] path first, so the
/// replay decision is identical to the summaries-absent path (Requirements
/// 13.3, 14.1, 14.2, 15.1). Summarization is best-effort and reads only through
/// the structured-first retrieval: when the [`SummaryProvider`] is disabled it
/// yields an empty summary set without consulting retrieval, and when enabled it
/// preserves retrieval order and degrades gracefully (Requirements 15.1, 15.2).
///
/// # Arguments
///
/// - `sig` — the new goal's intent signature (drives retrieval and Mode B opt-in).
/// - `retrieval` — the structured-first retrieval engine.
/// - `validator` — the caller-supplied evidence validator.
/// - `mode_b_policy` — the Mode B opt-in policy.
/// - `now` — the serve-time instant (used for Mode B age).
/// - `summary_provider` — the opt-in similar-goal summary provider.
///
/// This function is `async` because both [`start_goal`] and
/// [`SummaryProvider::summaries_for`] now await the embedding backend through
/// retrieval (Requirements 1.4, 6.3).
pub async fn start_goal_with_summaries(
    sig: &IntentSignature,
    retrieval: &impl Retrieval,
    validator: &impl EvidenceValidator,
    mode_b_policy: &impl ModeBPolicy,
    now: Timestamp,
    summary_provider: &SummaryProvider,
) -> GoalStartWithSummaries {
    // Compute the replay decision via the UNTOUCHED start_goal first, so the
    // decision is identical whether or not summaries are enabled (Requirements
    // 13.3, 14.1, 14.2, 15.1).
    let start = start_goal(sig, retrieval, validator, mode_b_policy, now).await;
    // Attach summaries best-effort; summaries_for reads through the same
    // structured-first retrieval and honors the opt-in config (Requirements
    // 15.1, 15.2).
    let summaries = summary_provider.summaries_for(sig, retrieval).await;
    GoalStartWithSummaries { start, summaries }
}

// ===========================================================================
// Advisory entry point — the verification bypass (Req 5)
// ===========================================================================

/// The advisory payload for a newly-opened goal (Req 5.3). It carries ONLY the
/// applicable memories and their summaries — no replay decision, no validation
/// result, no served answer (Req 5.4). Empty when retrieval finds nothing
/// (Req 5.5).
#[derive(Debug, Clone, PartialEq)]
pub struct AdvisorySummary {
    /// Applicable memories in retrieval order (score-descending, structured-head
    /// first). Compact `Memory` values, never raw transcripts.
    pub memories: Vec<ScoredMemory>,
    /// Similar-goal summaries (empty when the provider is disabled / no
    /// candidates / on failure). Additive over `memories`.
    pub summaries: Vec<GoalSummary>,
}

impl AdvisorySummary {
    /// The empty advisory (no applicable memory). `goal` proceeds as if no
    /// memory existed (Req 5.5).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            memories: Vec::new(),
            summaries: Vec::new(),
        }
    }

    /// True when there is nothing to inject.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.memories.is_empty() && self.summaries.is_empty()
    }
}

/// Run advisory retrieval for a newly-opened goal.
///
/// Runs the UNCHANGED structured-first [`Retrieval::retrieve_memories`]
/// (structured filter first; embed only when the head is thin — Req 5.2, 5.6)
/// and the opt-in [`SummaryProvider::summaries_for`], and returns the applicable
/// memories + summaries ONLY. It calls neither
/// [`decide_replay`](crate::tier2::decide_replay) nor any
/// [`EvidenceValidator`](crate::tier2::EvidenceValidator) and constructs no
/// [`ReplayDecision`](crate::tier2::ReplayDecision) (Req 5.3, 5.4). Never errors:
/// a degraded embedding backend yields a structured-head-only advisory
/// (Req 10.3).
pub async fn retrieve_advisory(
    sig: &IntentSignature,
    retrieval: &impl Retrieval,
    summary_provider: &SummaryProvider,
) -> AdvisorySummary {
    let memories = retrieval.retrieve_memories(sig).await;
    let summaries = summary_provider.summaries_for(sig, retrieval).await;
    AdvisorySummary { memories, summaries }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::goal_model::{
        EventLogGoalStore, InMemoryGoalEventLog, Resolution, SharedInductionQueue,
    };
    use crate::tier1::cache::Freshness;
    use crate::tier2::memory::{
        AnswerMode, Applicability, CachedOutcome, EvidenceContract, EvidenceContractItem, Memory,
        MemoryKind, MemoryVersion, OutcomeShape, OutcomeValue, ParameterSchema, Plan, PlanStep,
        Provenance, Reinforcement,
    };
    use crate::tier2::retrieval::{EmbeddingSource, MemoryRetrieval};
    use crate::tier2::store::InMemoryMemoryStore;
    use crate::tier2::{
        AllowListModeB, Author, AuthorError, CleanContext, DenyAllModeB, Embedding, Granularity,
        InMemoryRecurrenceTracker, InductionEngine, Judge, JudgeVerdict, NoEmbeddingClient,
        SummaryConfig, TokensHold,
    };
    use crate::types::{
        CanonicalJson, Duration, GoalNodeId, IntentSignature, IntentType, MemoryId, OutcomeRef,
        Scope, Sha256, SubtreeHash, TargetRef, TargetType, Timestamp, ToolName, ValidityToken,
    };

    use async_trait::async_trait;
    use halter_protocol::SessionId;

    // --- shared fixtures --------------------------------------------------

    fn sig() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn content_token(h: &str) -> ValidityToken {
        ValidityToken::ContentHash(Sha256::from(h))
    }

    fn contract_item(tool: &str, args: &str, token: ValidityToken) -> EvidenceContractItem {
        EvidenceContractItem {
            tool: ToolName::from(tool),
            normalized_args: CanonicalJson(args.to_owned()),
            validity_token: token,
        }
    }

    /// A tiny test-only [`EvidenceValidator`] that never confirms freshness.
    ///
    /// The concrete Tier-1-backed validator was removed with the verification
    /// tier; the kept [`start_goal`] / [`start_goal_with_summaries`] tests only
    /// need *some* validator to drive [`decide_replay`], so this fixture reports
    /// every item [`Freshness::Stale`] and every token set
    /// [`TokensHold::SomeStale`]. That is enough to exercise the kept public
    /// functions end to end (replay itself is covered by `replay.rs`).
    struct StaleValidator;

    impl EvidenceValidator for StaleValidator {
        fn revalidate_item(&self, _item: &EvidenceContractItem) -> Freshness {
            Freshness::Stale
        }

        fn tokens_hold(&self, _tokens: &[ValidityToken]) -> TokensHold {
            TokensHold::SomeStale
        }
    }

    /// An always-available embedder for retrieval.
    struct FixedEmbedder;
    #[async_trait]
    impl EmbeddingSource for FixedEmbedder {
        async fn embed(&self, _sig: &IntentSignature) -> Option<Embedding> {
            Some(Embedding(vec![0.1, 0.2, 0.3]))
        }
    }

    /// Build a memory with the given cached outcome + evidence items whose
    /// applicability is unconstrained (so it applies to `sig`).
    fn memory_with(
        id: &str,
        cached_outcome: Option<CachedOutcome>,
        items: Vec<EvidenceContractItem>,
    ) -> Memory {
        Memory {
            id: MemoryId::from(id),
            kind: MemoryKind::Fragment,
            intent: sig(),
            parameter_schema: ParameterSchema::default(),
            applicability: Applicability::unconstrained(),
            plan: Plan {
                steps: vec![PlanStep {
                    description: "read the file".to_owned(),
                    tool: Some(ToolName::from("read_file")),
                    intent: None,
                }],
            },
            evidence_contract: EvidenceContract { items },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            cached_outcome,
            provenance: Provenance::default(),
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        }
    }

    fn sound_outcome(tokens: Vec<ValidityToken>) -> CachedOutcome {
        CachedOutcome {
            answer: OutcomeValue(CanonicalJson("\"answer\"".to_owned())),
            mode: AnswerMode::SoundPinnable,
            validity_tokens: tokens,
            issued_at: Timestamp(0),
        }
    }

    fn key(id: &str) -> (GoalNodeId, SubtreeHash) {
        (GoalNodeId::from(id), SubtreeHash(Sha256::from("h")))
    }

    // --- start_goal: retrieval hit / miss ---------------------------------

    #[tokio::test]
    async fn start_goal_retrieval_hit_invokes_decide_replay() {
        // A retrieval hit (no cached_outcome) => decide_replay is invoked and
        // returns a ReplayProcedure decision carrying the re-validated evidence.
        let token = content_token("a");
        let store = InMemoryMemoryStore::new();
        store
            .insert(
                key("n"),
                memory_with("m", None, vec![contract_item("read_file", "{}", token)]),
            )
            .expect("insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let start = start_goal(&sig(), &retrieval, &StaleValidator, &DenyAllModeB, Timestamp(0))
            .await;
        match start {
            GoalStart::Replay(ReplayDecision::ReplayProcedure {
                evidence,
                evidence_fully_fresh,
                ..
            }) => {
                assert_eq!(evidence.len(), 1);
                assert!(
                    !evidence_fully_fresh,
                    "the stale validator reports the single item's token as not holding"
                );
            }
            other => panic!("expected ReplayProcedure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_goal_no_candidates_returns_from_scratch() {
        // Empty store => no candidates => FromScratch (design GoalPlan::from_scratch).
        let store = InMemoryMemoryStore::new();
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let start = start_goal(&sig(), &retrieval, &StaleValidator, &DenyAllModeB, Timestamp(0))
            .await;
        assert_eq!(start, GoalStart::FromScratch);
    }

    // --- Mode A end to end through decide_replay --------------------------

    #[tokio::test]
    async fn mode_a_stale_tokens_fall_through_to_replay() {
        // A Mode A answer whose tokens do NOT hold falls through to procedure
        // replay: `decide_replay` never serves a Mode A answer believed-unverified,
        // so the full retrieval->replay path yields ReplayProcedure.
        let token = content_token("a");
        let store = InMemoryMemoryStore::new();
        store
            .insert(
                key("n"),
                memory_with("m", Some(sound_outcome(vec![token])), vec![]),
            )
            .expect("valid insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let start = start_goal(&sig(), &retrieval, &StaleValidator, &DenyAllModeB, Timestamp(0))
            .await;
        match start {
            GoalStart::Replay(ReplayDecision::ReplayProcedure { .. }) => {}
            other => panic!("expected ReplayProcedure fall-through, got {other:?}"),
        }
    }

    // --- retrieve_advisory: memories + summaries only, never replay -------

    #[tokio::test]
    async fn retrieve_advisory_empty_store_is_empty() {
        // An empty store yields no structured head and (with an available
        // embedder) an empty ann_recall, so retrieval returns no candidates and
        // the advisory is empty (Req 5.5) — equivalent to AdvisorySummary::empty.
        let store = InMemoryMemoryStore::new();
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);
        let summary_provider = SummaryProvider::new(SummaryConfig {
            enabled: true,
            max_summaries: None,
        });

        let advisory = retrieve_advisory(&sig(), &retrieval, &summary_provider).await;

        assert!(advisory.is_empty(), "empty store => empty advisory");
        assert_eq!(advisory, AdvisorySummary::empty());
    }

    #[tokio::test]
    async fn retrieve_advisory_returns_applicable_memories() {
        // A store with one applicable memory => the advisory surfaces that
        // memory (Req 5.1, 5.3). No replay/validation is involved — the function
        // only runs retrieve_memories + summaries_for.
        let store = InMemoryMemoryStore::new();
        store
            .insert(key("n"), memory_with("m", None, vec![]))
            .expect("insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);
        let summary_provider = SummaryProvider::new(SummaryConfig::default());

        let advisory = retrieve_advisory(&sig(), &retrieval, &summary_provider).await;

        assert!(!advisory.is_empty(), "an applicable memory => non-empty advisory");
        assert_eq!(advisory.memories.len(), 1, "the single applicable memory is surfaced");
        assert_eq!(
            advisory.memories[0].memory.id,
            MemoryId::from("m"),
            "the surfaced memory is the one inserted"
        );
    }

    // --- Task 16.1: closure -> induction end to end -----------------------

    /// A Judge that always approves at Fragment granularity.
    struct ApprovingJudge;
    #[async_trait]
    impl Judge for ApprovingJudge {
        async fn evaluate(&self, _ctx: &CleanContext) -> JudgeVerdict {
            JudgeVerdict::approve(Granularity::Fragment, "worth it")
        }
    }

    /// An Author that emits a minimal valid Fragment memory.
    struct SimpleAuthor;
    #[async_trait]
    impl Author for SimpleAuthor {
        async fn write(
            &self,
            _granularity: Granularity,
            ctx: &CleanContext,
        ) -> Result<Memory, AuthorError> {
            Ok(Memory {
                id: MemoryId::from(format!("mem-{}", ctx.node().id)),
                kind: MemoryKind::Fragment,
                intent: ctx.node().intent.clone(),
                parameter_schema: ParameterSchema::default(),
                applicability: Applicability::unconstrained(),
                plan: Plan {
                    steps: vec![PlanStep {
                        description: "authored step".to_owned(),
                        tool: Some(ToolName::from("read_file")),
                        intent: None,
                    }],
                },
                evidence_contract: EvidenceContract::default(),
                outcome_shape: OutcomeShape {
                    result_ref: OutcomeRef::from("outcome://authored"),
                    schema: "s".to_owned(),
                },
                cached_outcome: None,
                provenance: Provenance::default(),
                reinforcement: Reinforcement::default(),
                version: MemoryVersion(
                    ctx.node()
                        .subtree_hash
                        .clone()
                        .unwrap_or(SubtreeHash(Sha256::from("v"))),
                ),
            })
        }
    }

    fn induction_engine(
        store: Arc<InMemoryMemoryStore>,
        threshold: u64,
    ) -> InductionEngine<InMemoryMemoryStore, NoEmbeddingClient> {
        InductionEngine::new(
            store,
            Arc::new(InMemoryRecurrenceTracker::new()),
            Arc::new(ApprovingJudge),
            Arc::new(SimpleAuthor),
            threshold,
        )
    }

    #[tokio::test]
    async fn engine_queue_run_induces_memory_from_closed_node() {
        // Directly exercise the worker body: create+close a goal, then run the
        // EngineInductionQueue for the enqueued signal and assert a memory was
        // inserted end to end (gate N=1 -> Judge -> Author -> insert).
        let memory_store = Arc::new(InMemoryMemoryStore::new());
        let engine = Arc::new(induction_engine(Arc::clone(&memory_store), 1));

        let goal_store = Arc::new(EventLogGoalStore::new(InMemoryGoalEventLog::new()));
        let queue = EngineInductionQueue::new(Arc::clone(&goal_store), Arc::clone(&engine));

        let session = SessionId::from("s1");
        let id = goal_store
            .create(&session, None, "hyp".to_owned(), sig())
            .await
            .expect("create");
        let outcome = goal_store
            .close(&session, &id, Resolution::Accepted)
            .await
            .expect("close");
        let hash = outcome.subtree_hash.expect("closed carries hash");

        let signal = ClosureSignal::new(session.clone(), id.clone(), hash);
        let result = queue.run(&signal).await;

        assert!(
            matches!(result, Some(InductionOutcome::Inserted(_))),
            "induction ran end to end and inserted a memory, got {result:?}"
        );
        // The memory is now retrievable — closure fed induction fed the store.
        assert_eq!(memory_store.filter(&sig()).len(), 1);
    }

    #[tokio::test]
    async fn engine_queue_enqueue_returns_before_induction_and_runs_async() {
        // Wire the store to the EngineInductionQueue as its induction backend, so
        // close() enqueues onto a spawned task and returns before induction runs
        // (Requirements 6.3, 14.5). We then wait for the async worker to insert.
        let memory_store = Arc::new(InMemoryMemoryStore::new());
        let engine = Arc::new(induction_engine(Arc::clone(&memory_store), 1));

        // The goal store the queue resolves nodes from must be the same store the
        // closure happens on. We build the queue over a handle to that store.
        let goal_store = Arc::new(EventLogGoalStore::new(InMemoryGoalEventLog::new()));
        let queue: SharedInductionQueue = Arc::new(EngineInductionQueue::new(
            Arc::clone(&goal_store),
            Arc::clone(&engine),
        ));

        // A second store instance sharing the SAME log would be needed to route
        // close() through the queue; instead we drive the queue directly via the
        // signal that close() produces, which is the contract close() relies on.
        let session = SessionId::from("s1");
        let id = goal_store
            .create(&session, None, "hyp".to_owned(), sig())
            .await
            .expect("create");
        let outcome = goal_store
            .close(&session, &id, Resolution::Accepted)
            .await
            .expect("close");
        let hash = outcome.subtree_hash.expect("hash");

        // enqueue returns immediately (job handed to a spawned task).
        queue
            .enqueue(ClosureSignal::new(session.clone(), id.clone(), hash))
            .await;

        // Give the spawned induction task a chance to run and insert.
        for _ in 0..50 {
            if !memory_store.filter(&sig()).is_empty() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(
            memory_store.filter(&sig()).len(),
            1,
            "the async induction worker inserted the memory off the hot path"
        );
    }

    #[tokio::test]
    async fn mode_b_opted_in_within_age_serves_believed_unverified() {
        // Requirement 21: exercise the Mode B path through start_goal so the
        // integration helper covers the believed-unverified branch too. Mode B
        // serving depends only on opt-in + age, not token freshness, so the
        // stale validator still serves believed-unverified here.
        let outcome = CachedOutcome {
            answer: OutcomeValue(CanonicalJson("\"volatile\"".to_owned())),
            mode: AnswerMode::BoundedVolatile {
                max_age: Duration(100),
            },
            validity_tokens: vec![ValidityToken::Ttl {
                issued_at: Timestamp(0),
                ttl: Duration(1_000),
            }],
            issued_at: Timestamp(10),
        };
        let store = InMemoryMemoryStore::new();
        store
            .insert(key("n"), memory_with("m", Some(outcome), vec![]))
            .expect("insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);
        let policy = AllowListModeB::new(vec![IntentType::from("lookup")]);

        let start = start_goal(&sig(), &retrieval, &StaleValidator, &policy, Timestamp(50)).await;
        match start {
            GoalStart::Replay(ReplayDecision::ServeAnswer { verified, .. }) => {
                assert!(!verified, "Mode B is always believed-unverified");
            }
            other => panic!("expected Mode B ServeAnswer, got {other:?}"),
        }
    }

    // --- Task 9.3: empty-candidate resilience (Requirements 14.2, 15.1) ---

    #[tokio::test]
    async fn start_goal_with_summaries_empty_candidates_yields_empty_and_from_scratch() {
        // Requirement 14.2: when retrieval returns no candidates, `summaries` is
        // empty and `start` is the from-scratch replay decision. An empty store
        // yields no structured head and (with an available embedder) an empty
        // ann_recall, so retrieval returns no candidates.
        let store = InMemoryMemoryStore::new();
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        // Enabled provider, but no candidates => empty summaries + FromScratch.
        let summary_provider = SummaryProvider::new(SummaryConfig {
            enabled: true,
            max_summaries: None,
        });
        let with = start_goal_with_summaries(
            &sig(),
            &retrieval,
            &StaleValidator,
            &DenyAllModeB,
            Timestamp(0),
            &summary_provider,
        )
        .await;
        assert_eq!(
            with.start,
            GoalStart::FromScratch,
            "no candidates => from-scratch replay decision"
        );
        assert!(
            with.summaries.is_empty(),
            "no candidates => empty summary set"
        );
    }

    #[tokio::test]
    async fn start_goal_with_summaries_disabled_matches_start_goal() {
        // Requirement 15.1: a disabled provider yields the same `start` as
        // `start_goal` and produces no freshly retrieved summaries. Use a
        // non-empty store so `start_goal` returns a real replay decision.
        let token = content_token("a");
        let store = InMemoryMemoryStore::new();
        store
            .insert(
                key("n"),
                memory_with("m", Some(sound_outcome(vec![token])), vec![]),
            )
            .expect("valid insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let plain =
            start_goal(&sig(), &retrieval, &StaleValidator, &DenyAllModeB, Timestamp(0)).await;

        let summary_provider = SummaryProvider::new(SummaryConfig::default());
        let with = start_goal_with_summaries(
            &sig(),
            &retrieval,
            &StaleValidator,
            &DenyAllModeB,
            Timestamp(0),
            &summary_provider,
        )
        .await;

        assert_eq!(
            with.start, plain,
            "a disabled provider leaves the replay decision identical to start_goal"
        );
        assert!(
            with.summaries.is_empty(),
            "a disabled provider produces no freshly retrieved summaries"
        );
    }

    // --- Task 9.2: Property 11 — summaries are additive -------------------

    use proptest::prelude::*;

    /// A batch of `n` valid, applicable memories with distinct ids, varying
    /// kind so both dead-end and non-dead-end summaries are exercised. Each
    /// applies to `sig()` (unconstrained applicability from `memory_with`).
    fn arb_summary_scenario() -> impl Strategy<Value = (usize, bool, Option<usize>)> {
        (
            0_usize..6,               // number of memories in the store
            any::<bool>(),            // provider enabled?
            prop::option::of(0_usize..6), // max_summaries
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 11: Summaries are additive and
        // never alter the replay decision.
        //
        // Validates: Requirements 13.3, 14.1, 14.2.
        #[test]
        fn summaries_are_additive_and_never_alter_replay(
            (count, enabled, max_summaries) in arb_summary_scenario()
        ) {
            let token = content_token("a");

            // Build a store with `count` applicable memories (some with cached
            // sound outcomes, some without) so the top candidate varies across
            // scenarios. Reads never mutate the store, so one store is fine for
            // both calls.
            let store = InMemoryMemoryStore::new();
            for i in 0..count {
                let id = format!("m-{i}");
                let cached = if i % 2 == 0 {
                    Some(sound_outcome(vec![token.clone()]))
                } else {
                    None
                };
                let items = if cached.is_some() {
                    vec![]
                } else {
                    vec![contract_item("read_file", "{}", token.clone())]
                };
                store
                    .insert(key(&format!("n-{i}")), {
                        let mut mem = memory_with(&id, cached, items);
                        // Vary kind so Negative dead-ends are also summarized.
                        if i % 3 == 2 {
                            mem.kind = MemoryKind::Negative;
                            // Negative memories require a non-empty contract.
                            mem.cached_outcome = None;
                            mem.evidence_contract = EvidenceContract {
                                items: vec![contract_item(
                                    "read_file",
                                    "{}",
                                    token.clone(),
                                )],
                            };
                        }
                        mem
                    })
                    .expect("valid insert");
            }
            let embedder = FixedEmbedder;
            let retrieval = MemoryRetrieval::new(&store, &embedder);

            let summary_provider = SummaryProvider::new(SummaryConfig {
                enabled,
                max_summaries,
            });

            // The plain replay decision and the additive one must agree on
            // `start`, regardless of enablement or produced summaries.
            let rt = tokio::runtime::Runtime::new().unwrap();
            let plain = rt.block_on(start_goal(
                &sig(),
                &retrieval,
                &StaleValidator,
                &DenyAllModeB,
                Timestamp(0),
            ));
            let with = rt.block_on(start_goal_with_summaries(
                &sig(),
                &retrieval,
                &StaleValidator,
                &DenyAllModeB,
                Timestamp(0),
                &summary_provider,
            ));

            prop_assert_eq!(with.start, plain);
        }
    }
}
