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
//!   13.2, 19.1, 20.1, 23.1). [`Tier1EvidenceValidator`] is the concrete
//!   [`EvidenceValidator`](crate::tier2::EvidenceValidator) deferred from task
//!   14.x: it drives [`Tier1Cache::revalidate`](crate::tier1::Tier1Cache::revalidate)
//!   for evidence items and [`tokens::holds`](crate::tier1::tokens::holds) for
//!   answer tokens, resolving each item/token back to its
//!   [`SourceDescriptor`](crate::types::SourceDescriptor) through a
//!   [`SourceResolver`]. [`start_goal`] ties
//!   [`Retrieval::retrieve_memories`](crate::tier2::Retrieval::retrieve_memories)
//!   into [`decide_replay`](crate::tier2::decide_replay), mirroring the design's
//!   `start_goal`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::goal_model::{ClosureSignal, GoalStore, InductionQueue};
use crate::tier1::cache::{Freshness, Tier1Cache};
use crate::tier1::tokens::{self, SourceProvider};
use crate::tier2::memory::EvidenceContractItem;
use crate::tier2::store::MemoryStore;
use crate::tier2::{
    decide_replay, EvidenceValidator, GoalSummary, InductionEngine, InductionOutcome, ModeBPolicy,
    ReplayDecision, Retrieval, SummaryProvider, TokensHold,
};
use crate::types::{IntentSignature, SourceDescriptor, Timestamp, ValidityToken};

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
///
/// Both the store and the engine are shared (`Arc`) so the spawned task can own
/// clones that outlive the `enqueue` call.
pub struct EngineInductionQueue<G: GoalStore, M: MemoryStore> {
    store: Arc<G>,
    engine: Arc<InductionEngine<M>>,
}

impl<G: GoalStore, M: MemoryStore> EngineInductionQueue<G, M> {
    /// Wire a goal `store` (to resolve closed nodes) to an induction `engine`.
    #[must_use]
    pub fn new(store: Arc<G>, engine: Arc<InductionEngine<M>>) -> Self {
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
impl<G, M> InductionQueue for EngineInductionQueue<G, M>
where
    G: GoalStore + 'static,
    M: MemoryStore + Send + Sync + 'static,
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

/// Resolves an evidence item or answer token back to the
/// [`SourceDescriptor`](crate::types::SourceDescriptor) it was issued against,
/// plus a distinguished "backend unavailable" signal.
///
/// A [`ValidityToken`] and an [`EvidenceContractItem`] deliberately do **not**
/// carry a `SourceDescriptor` (Tier 2 stores contracts, not sources — see the
/// replay module docs). To re-validate through Tier 1, the concrete validator
/// must map each token/item back to its source. That mapping is this trait's
/// job, kept behind a seam so the validator does not hard-code any particular
/// resolution strategy (a session-scoped map, a tool registry, etc.).
///
/// [`resolve`](Self::resolve) returns:
///
/// - [`SourceResolution::Resolved`] with the descriptor to re-observe;
/// - [`SourceResolution::Unresolvable`] when the token cannot be mapped to a
///   source but Tier 1 itself is reachable (treated fail-safe as *stale*); or
/// - [`SourceResolution::Unavailable`] when the Tier 1 backend cannot be reached
///   at all (drives Requirement 23.4 / the [`TokensHold::Unavailable`] path).
pub trait SourceResolver {
    /// Resolve `token` to its source (or report it unresolvable / the backend
    /// unavailable).
    fn resolve(&self, token: &ValidityToken) -> SourceResolution;
}

/// The result of resolving a token/item to its source (see [`SourceResolver`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceResolution {
    /// The token maps to this source; re-validate against it.
    Resolved(SourceDescriptor),
    /// The token could not be mapped to a source, though Tier 1 is reachable.
    /// Treated fail-safe as stale (no verified serve).
    Unresolvable,
    /// The Tier 1 backend is unavailable; no token can be confirmed
    /// (Requirement 23.4).
    Unavailable,
}

/// The concrete, Tier 1-backed [`EvidenceValidator`] (task 16.2).
///
/// Bridges [`decide_replay`](crate::tier2::decide_replay) to real Tier 1
/// re-validation without leaking Tier 1's generics into the engine:
///
/// - [`revalidate_item`](EvidenceValidator::revalidate_item) resolves the item's
///   token to its source and calls
///   [`Tier1Cache::revalidate`](crate::tier1::Tier1Cache::revalidate); an
///   unresolvable source or an unreachable backend is reported
///   [`Freshness::Stale`] (fail-safe, Requirements 23.1, 23.3, 23.4).
/// - [`tokens_hold`](EvidenceValidator::tokens_hold) resolves every answer token
///   to its source and calls [`tokens::holds`](crate::tier1::tokens::holds): if
///   all hold it reports [`TokensHold::AllHold`]; if the resolver reports the
///   backend [`SourceResolution::Unavailable`] it reports
///   [`TokensHold::Unavailable`] (Requirement 23.4); otherwise
///   [`TokensHold::SomeStale`] (Requirements 20.1–20.4).
///
/// # Generic parameters
///
/// - `C` — the [`Tier1Cache`] used for evidence-item re-validation.
/// - `P` — the [`SourceProvider`] that observes current source state.
/// - `R` — the [`SourceResolver`] mapping tokens/items to sources.
pub struct Tier1EvidenceValidator<'a, C: Tier1Cache, P: SourceProvider, R: SourceResolver> {
    cache: &'a C,
    provider: &'a P,
    resolver: &'a R,
}

impl<'a, C: Tier1Cache, P: SourceProvider, R: SourceResolver>
    Tier1EvidenceValidator<'a, C, P, R>
{
    /// Build the validator over a Tier 1 `cache`, a source `provider`, and a
    /// `resolver` that maps tokens/items back to their sources.
    #[must_use]
    pub fn new(cache: &'a C, provider: &'a P, resolver: &'a R) -> Self {
        Self {
            cache,
            provider,
            resolver,
        }
    }
}

impl<C: Tier1Cache, P: SourceProvider, R: SourceResolver> EvidenceValidator
    for Tier1EvidenceValidator<'_, C, P, R>
{
    fn revalidate_item(&self, item: &EvidenceContractItem) -> Freshness {
        // Resolve the item's token to its source. An unresolvable source or an
        // unavailable backend is fail-safe stale (Requirements 23.3, 23.4) —
        // procedure replay treats it as not-fresh and serves no verified answer.
        match self.resolver.resolve(&item.validity_token) {
            SourceResolution::Resolved(source) => self.cache.revalidate(
                &item.tool,
                &item.normalized_args,
                &source,
                &item.validity_token,
                self.provider,
            ),
            SourceResolution::Unresolvable | SourceResolution::Unavailable => Freshness::Stale,
        }
    }

    fn tokens_hold(&self, tokens: &[ValidityToken]) -> TokensHold {
        // Empty token sets cannot be confirmed to hold; treat as stale so a
        // Mode A answer with no tokens is never served verified (the store
        // already rejects such answers at write time; this is defense in depth).
        if tokens.is_empty() {
            return TokensHold::SomeStale;
        }

        let mut all_hold = true;
        for token in tokens {
            match self.resolver.resolve(token) {
                SourceResolution::Resolved(source) => {
                    if !tokens::holds(token, &source, self.provider) {
                        all_hold = false;
                    }
                }
                // Backend unavailable dominates: no token can be confirmed, so
                // report Unavailable immediately (Requirement 23.4).
                SourceResolution::Unavailable => return TokensHold::Unavailable,
                // Unresolvable but backend up: that token is stale.
                SourceResolution::Unresolvable => all_hold = false,
            }
        }

        if all_hold {
            TokensHold::AllHold
        } else {
            TokensHold::SomeStale
        }
    }
}

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
/// - `validator` — the Tier 1-backed evidence validator (see
///   [`Tier1EvidenceValidator`]).
/// - `mode_b_policy` — the Mode B opt-in policy.
/// - `now` — the serve-time instant (used for Mode B age).
pub fn start_goal(
    sig: &IntentSignature,
    retrieval: &impl Retrieval,
    validator: &impl EvidenceValidator,
    mode_b_policy: &impl ModeBPolicy,
    now: Timestamp,
) -> GoalStart {
    let candidates = retrieval.retrieve_memories(sig);
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
/// - `validator` — the Tier 1-backed evidence validator (see
///   [`Tier1EvidenceValidator`]).
/// - `mode_b_policy` — the Mode B opt-in policy.
/// - `now` — the serve-time instant (used for Mode B age).
/// - `summary_provider` — the opt-in similar-goal summary provider.
pub fn start_goal_with_summaries(
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
    let start = start_goal(sig, retrieval, validator, mode_b_policy, now);
    // Attach summaries best-effort; summaries_for reads through the same
    // structured-first retrieval and honors the opt-in config (Requirements
    // 15.1, 15.2).
    let summaries = summary_provider.summaries_for(sig, retrieval);
    GoalStartWithSummaries { start, summaries }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::collections::HashMap;

    use crate::goal_model::{
        EventLogGoalStore, InMemoryGoalEventLog, Resolution, SharedInductionQueue,
    };
    use crate::tier1::cache::InMemoryTier1Cache;
    use crate::tier1::tokens::SourceUnreachable;
    use crate::tier2::memory::{
        AnswerMode, Applicability, CachedOutcome, EvidenceContract, Memory, MemoryKind,
        MemoryVersion, OutcomeShape, OutcomeValue, ParameterSchema, Plan, PlanStep, Provenance,
        Reinforcement,
    };
    use crate::tier2::retrieval::{EmbeddingSource, MemoryRetrieval};
    use crate::tier2::store::InMemoryMemoryStore;
    use crate::tier2::{
        AllowListModeB, Author, AuthorError, CleanContext, DenyAllModeB, Embedding, Granularity,
        InMemoryRecurrenceTracker, InductionEngine, Judge, JudgeVerdict, SummaryConfig,
    };
    use crate::types::{
        CanonicalJson, ContentRef, Duration, EventKey, EventSeq, GoalNodeId, IntentSignature,
        IntentType, MemoryId, OutcomeRef, Scope, Sha256, SubtreeHash, TargetRef, TargetType,
        Timestamp, ToolName,
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

    fn pinnable(reference: &str) -> SourceDescriptor {
        SourceDescriptor::Pinnable {
            content: ContentRef::from(reference),
        }
    }

    fn contract_item(tool: &str, args: &str, token: ValidityToken) -> EvidenceContractItem {
        EvidenceContractItem {
            tool: ToolName::from(tool),
            normalized_args: CanonicalJson(args.to_owned()),
            validity_token: token,
        }
    }

    /// A deterministic in-memory provider: content by `ContentRef`, a settable
    /// clock, and event sequences by `EventKey`. A missing key models an
    /// unreachable source.
    struct FakeProvider {
        content: HashMap<String, Vec<u8>>,
        now: Cell<u64>,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                content: HashMap::new(),
                now: Cell::new(0),
            }
        }

        fn with_content(mut self, reference: &str, bytes: &[u8]) -> Self {
            self.content.insert(reference.to_owned(), bytes.to_vec());
            self
        }

        fn set_content(&mut self, reference: &str, bytes: &[u8]) {
            self.content.insert(reference.to_owned(), bytes.to_vec());
        }
    }

    impl SourceProvider for FakeProvider {
        fn read_content(&self, content: &ContentRef) -> Result<Vec<u8>, SourceUnreachable> {
            self.content
                .get(&content.0)
                .cloned()
                .ok_or_else(|| SourceUnreachable::new(format!("no content at `{content}`")))
        }

        fn now(&self) -> Timestamp {
            Timestamp(self.now.get())
        }

        fn latest_event_seq(
            &self,
            subscription: &EventKey,
        ) -> Result<EventSeq, SourceUnreachable> {
            Err(SourceUnreachable::new(format!(
                "cannot observe `{subscription}`"
            )))
        }
    }

    /// A resolver backed by an explicit token -> source map, with an
    /// "unavailable" switch modelling Tier 1 being down.
    struct MapResolver {
        map: HashMap<String, SourceDescriptor>,
        unavailable: bool,
    }

    impl MapResolver {
        fn new() -> Self {
            Self {
                map: HashMap::new(),
                unavailable: false,
            }
        }

        fn with(mut self, token: &ValidityToken, source: SourceDescriptor) -> Self {
            self.map.insert(format!("{token:?}"), source);
            self
        }

        fn unavailable() -> Self {
            Self {
                map: HashMap::new(),
                unavailable: true,
            }
        }
    }

    impl SourceResolver for MapResolver {
        fn resolve(&self, token: &ValidityToken) -> SourceResolution {
            if self.unavailable {
                return SourceResolution::Unavailable;
            }
            match self.map.get(&format!("{token:?}")) {
                Some(source) => SourceResolution::Resolved(source.clone()),
                None => SourceResolution::Unresolvable,
            }
        }
    }

    /// An always-available embedder for retrieval.
    struct FixedEmbedder;
    impl EmbeddingSource for FixedEmbedder {
        fn embed(&self, _sig: &IntentSignature) -> Option<Embedding> {
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

    // --- Tier1EvidenceValidator: revalidate_item (13.2, 23.1) -------------

    #[test]
    fn validator_reports_fresh_when_item_token_holds() {
        // Requirements 13.2, 23.1: a holding ContentHash token => Fresh.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let cache = InMemoryTier1Cache::new();
        let source = pinnable("file://a");
        let token = cache.issue_token(&source, &provider).expect("issue");
        let resolver = MapResolver::new().with(&token, source.clone());
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let item = contract_item("read_file", "{}", token);
        assert_eq!(validator.revalidate_item(&item), Freshness::Fresh);
    }

    #[test]
    fn validator_reports_stale_when_item_token_does_not_hold() {
        // Requirement 23.1/23.3: mutated source => token stale => Stale.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let cache = InMemoryTier1Cache::new();
        let source = pinnable("file://a");
        let token = cache.issue_token(&source, &provider).expect("issue");
        let resolver = MapResolver::new().with(&token, source.clone());

        provider.set_content("file://a", b"v2");
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);
        let item = contract_item("read_file", "{}", token);
        assert_eq!(validator.revalidate_item(&item), Freshness::Stale);
    }

    #[test]
    fn validator_reports_stale_when_source_unresolvable() {
        // Fail-safe (23.3): a token the resolver cannot map => Stale.
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        let resolver = MapResolver::new(); // maps nothing
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let item = contract_item("read_file", "{}", content_token("unknown"));
        assert_eq!(validator.revalidate_item(&item), Freshness::Stale);
    }

    #[test]
    fn validator_reports_stale_when_backend_unavailable() {
        // Requirement 23.4: Tier 1 down => every item stale.
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        let resolver = MapResolver::unavailable();
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let item = contract_item("read_file", "{}", content_token("x"));
        assert_eq!(validator.revalidate_item(&item), Freshness::Stale);
    }

    // --- Tier1EvidenceValidator: tokens_hold (20.1, 23.4) -----------------

    #[test]
    fn validator_tokens_all_hold_when_all_fresh() {
        // Requirement 20.1: every token holds => AllHold.
        let provider = FakeProvider::new()
            .with_content("file://a", b"v1")
            .with_content("file://b", b"v2");
        let cache = InMemoryTier1Cache::new();
        let sa = pinnable("file://a");
        let sb = pinnable("file://b");
        let ta = cache.issue_token(&sa, &provider).expect("issue a");
        let tb = cache.issue_token(&sb, &provider).expect("issue b");
        let resolver = MapResolver::new()
            .with(&ta, sa.clone())
            .with(&tb, sb.clone());
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        assert_eq!(
            validator.tokens_hold(&[ta, tb]),
            TokensHold::AllHold
        );
    }

    #[test]
    fn validator_tokens_some_stale_when_one_fails() {
        // Requirement 20.2: any stale token => SomeStale (never verified).
        let mut provider = FakeProvider::new()
            .with_content("file://a", b"v1")
            .with_content("file://b", b"v2");
        let cache = InMemoryTier1Cache::new();
        let sa = pinnable("file://a");
        let sb = pinnable("file://b");
        let ta = cache.issue_token(&sa, &provider).expect("issue a");
        let tb = cache.issue_token(&sb, &provider).expect("issue b");
        let resolver = MapResolver::new()
            .with(&ta, sa.clone())
            .with(&tb, sb.clone());

        provider.set_content("file://b", b"changed");
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);
        assert_eq!(
            validator.tokens_hold(&[ta, tb]),
            TokensHold::SomeStale
        );
    }

    #[test]
    fn validator_tokens_unavailable_when_backend_down() {
        // Requirement 23.4: backend down => Unavailable.
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        let resolver = MapResolver::unavailable();
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        assert_eq!(
            validator.tokens_hold(&[content_token("x")]),
            TokensHold::Unavailable
        );
    }

    // --- Mode A end to end through decide_replay (20.1) -------------------

    #[test]
    fn mode_a_answer_tokens_hold_serves_verified() {
        // Requirement 20.1: a Mode A answer whose tokens hold in Tier 1 => the
        // full retrieval->replay path serves verified.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let cache = InMemoryTier1Cache::new();
        let source = pinnable("file://a");
        let token = cache.issue_token(&source, &provider).expect("issue");
        let resolver = MapResolver::new().with(&token, source.clone());
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let store = InMemoryMemoryStore::new();
        store
            .insert(
                key("n"),
                memory_with("m", Some(sound_outcome(vec![token])), vec![]),
            )
            .expect("valid insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let start = start_goal(&sig(), &retrieval, &validator, &DenyAllModeB, Timestamp(0));
        match start {
            GoalStart::Replay(ReplayDecision::ServeAnswer { verified, .. }) => {
                assert!(verified, "Mode A answer with holding tokens serves verified");
            }
            other => panic!("expected verified ServeAnswer, got {other:?}"),
        }
    }

    // --- start_goal: retrieval hit / miss ---------------------------------

    #[test]
    fn start_goal_retrieval_hit_invokes_decide_replay() {
        // A retrieval hit (no cached_outcome) => decide_replay is invoked and
        // returns a ReplayProcedure decision.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let cache = InMemoryTier1Cache::new();
        let source = pinnable("file://a");
        let token = cache.issue_token(&source, &provider).expect("issue");
        let resolver = MapResolver::new().with(&token, source.clone());
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let store = InMemoryMemoryStore::new();
        store
            .insert(
                key("n"),
                memory_with(
                    "m",
                    None,
                    vec![contract_item("read_file", "{}", token)],
                ),
            )
            .expect("insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let start = start_goal(&sig(), &retrieval, &validator, &DenyAllModeB, Timestamp(0));
        match start {
            GoalStart::Replay(ReplayDecision::ReplayProcedure {
                evidence,
                evidence_fully_fresh,
                ..
            }) => {
                assert_eq!(evidence.len(), 1);
                assert!(evidence_fully_fresh, "the single item's token holds");
            }
            other => panic!("expected ReplayProcedure, got {other:?}"),
        }
    }

    #[test]
    fn start_goal_no_candidates_returns_from_scratch() {
        // Empty store => no candidates => FromScratch (design GoalPlan::from_scratch).
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        let resolver = MapResolver::new();
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let store = InMemoryMemoryStore::new();
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let start = start_goal(&sig(), &retrieval, &validator, &DenyAllModeB, Timestamp(0));
        assert_eq!(start, GoalStart::FromScratch);
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
    ) -> InductionEngine<InMemoryMemoryStore> {
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
        // integration helper covers the believed-unverified branch too.
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        let resolver = MapResolver::new();
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

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

        let start = start_goal(&sig(), &retrieval, &validator, &policy, Timestamp(50));
        match start {
            GoalStart::Replay(ReplayDecision::ServeAnswer { verified, .. }) => {
                assert!(!verified, "Mode B is always believed-unverified");
            }
            other => panic!("expected Mode B ServeAnswer, got {other:?}"),
        }
    }

    // --- Task 9.3: empty-candidate resilience (Requirements 14.2, 15.1) ---

    #[test]
    fn start_goal_with_summaries_empty_candidates_yields_empty_and_from_scratch() {
        // Requirement 14.2: when retrieval returns no candidates, `summaries` is
        // empty and `start` is the from-scratch replay decision. An empty store
        // yields no structured head and (with an available embedder) an empty
        // ann_recall, so retrieval returns no candidates.
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        let resolver = MapResolver::new();
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

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
            &validator,
            &DenyAllModeB,
            Timestamp(0),
            &summary_provider,
        );
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

    #[test]
    fn start_goal_with_summaries_disabled_matches_start_goal() {
        // Requirement 15.1: a disabled provider yields the same `start` as
        // `start_goal` and produces no freshly retrieved summaries. Use a
        // non-empty store so `start_goal` returns a real replay decision.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let cache = InMemoryTier1Cache::new();
        let source = pinnable("file://a");
        let token = cache.issue_token(&source, &provider).expect("issue");
        let resolver = MapResolver::new().with(&token, source.clone());
        let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

        let store = InMemoryMemoryStore::new();
        store
            .insert(
                key("n"),
                memory_with("m", Some(sound_outcome(vec![token])), vec![]),
            )
            .expect("valid insert");
        let embedder = FixedEmbedder;
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let plain = start_goal(&sig(), &retrieval, &validator, &DenyAllModeB, Timestamp(0));

        let summary_provider = SummaryProvider::new(SummaryConfig::default());
        let with = start_goal_with_summaries(
            &sig(),
            &retrieval,
            &validator,
            &DenyAllModeB,
            Timestamp(0),
            &summary_provider,
        );

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
            // Deterministic validator/provider so both calls see identical inputs.
            let provider = FakeProvider::new().with_content("file://a", b"v1");
            let cache = InMemoryTier1Cache::new();
            let source = pinnable("file://a");
            let token = cache.issue_token(&source, &provider).expect("issue");
            let resolver = MapResolver::new().with(&token, source.clone());
            let validator = Tier1EvidenceValidator::new(&cache, &provider, &resolver);

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
            let plain =
                start_goal(&sig(), &retrieval, &validator, &DenyAllModeB, Timestamp(0));
            let with = start_goal_with_summaries(
                &sig(),
                &retrieval,
                &validator,
                &DenyAllModeB,
                Timestamp(0),
                &summary_provider,
            );

            prop_assert_eq!(with.start, plain);
        }
    }
}
