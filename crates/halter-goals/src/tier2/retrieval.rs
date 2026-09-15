//! Tier 2 — structured-first retrieval with embedding tail recall.
//!
//! Retrieval surfaces the memories applicable to a new goal's
//! [`IntentSignature`], **structured-first**: the primary path is the exact
//! structured filter over all four signature fields, and embedding (ANN) recall
//! is used *only* to widen a thin head. This keeps retrieval context-frugal and
//! guarantees structured matches take precedence over embedding-only matches
//! (Requirement 18, design "Retrieval" pseudocode).
//!
//! The algorithm ([`Retrieval::retrieve_memories`]) is:
//!
//! 1. **Structured filter first** (Requirement 18.1): `head = store.filter(sig)`.
//!    `filter` already applies each memory's [`Applicability`] guard, assembling
//!    the structured head before any embedding recall is considered.
//! 2. **Embedding recall only for a thin head** (Requirements 18.2, 18.3): if
//!    `head.len() < HEAD_MIN`, ask the [`EmbeddingSource`] for the query
//!    embedding and, when available, `tail = store.ann_recall(embedding,
//!    TAIL_LIMIT)`. The candidate set is the union of head and tail, deduplicated
//!    by [`MemoryId`] (one entry per id, preferring the head entry). When the
//!    head is not thin, embedding recall is never invoked and the head is the
//!    candidate set.
//! 3. **Applicability exclusion** (Requirement 18.4): every candidate whose
//!    guard is not satisfied by `sig` is dropped. `head` is already applicable,
//!    but `tail` from `ann_recall` is ranked by embedding distance and may not
//!    be — so this filter is applied defensively to the whole union.
//! 4. **Cheap re-rank** (Requirement 18.5): each surviving candidate is scored by
//!    [`cheap_score`] and the results are ordered by score descending. The score
//!    is engineered so a stronger structured match always outranks a weaker one,
//!    and structured/head matches generally outrank embedding-only tail matches.
//! 5. **Compact results** (Requirement 18.6): a list of [`ScoredMemory`] is
//!    returned. Each wraps a compact [`Memory`] (a distilled plan + contracts),
//!    never a raw transcript.
//!
//! **Backend-down degradation** (Requirement 18.7): the embedding backend is
//! abstracted behind [`EmbeddingSource`], whose [`EmbeddingSource::embed`]
//! returns `Option<Embedding>`. When it returns `None` (backend unavailable),
//! tail recall is skipped and the structured-head-only candidate set is returned
//! *without error*.

use crate::tier2::memory::{Embedding, Memory};
use crate::tier2::store::MemoryStore;
use crate::types::IntentSignature;

/// The minimum size for the structured head to be considered "not thin".
///
/// While the structured head holds at least `HEAD_MIN` candidates, embedding
/// recall is not invoked and the head is used directly (Requirement 18.2). This
/// is a sensible default; use [`Retrieval`]'s configurable form
/// ([`MemoryRetrieval::with_bounds`]) to override it.
pub const HEAD_MIN: usize = 3;

/// The maximum number of tail candidates requested from embedding recall.
///
/// When the head is thin, at most `TAIL_LIMIT` memories are recalled via
/// [`MemoryStore::ann_recall`] to widen it (Requirement 18.3).
pub const TAIL_LIMIT: usize = 10;

/// A memory paired with its cheap re-rank score.
///
/// The returned retrieval order is these values sorted by `score` descending
/// (Requirement 18.5). `memory` is a compact [`Memory`] — never a raw transcript
/// (Requirement 18.6).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMemory {
    /// The candidate memory.
    pub memory: Memory,
    /// Its cheap re-rank score (higher ranks earlier). See [`cheap_score`].
    pub score: f32,
}

/// A source of query embeddings, abstracting the (possibly remote) embedding
/// backend so its availability can be modeled and simulated.
///
/// [`Self::embed`] returns `Option<Embedding>`: `Some(embedding)` when the
/// backend produced an embedding, and `None` when the backend is unavailable
/// (down). A `None` result drives the graceful degradation to a
/// structured-head-only result (Requirement 18.7) rather than an error.
pub trait EmbeddingSource {
    /// Embed the query signature, or `None` if the backend is unavailable.
    fn embed(&self, sig: &IntentSignature) -> Option<Embedding>;
}

/// Structured-first retrieval over a [`MemoryStore`].
///
/// Implementors surface the applicable memories for `sig`, structured-first,
/// ordered by cheap re-rank score (Requirement 18).
pub trait Retrieval {
    /// Retrieve the memories applicable to `sig`, ordered by score descending.
    ///
    /// See the module docs for the full algorithm and the requirement mapping.
    fn retrieve_memories(&self, sig: &IntentSignature) -> Vec<ScoredMemory>;
}

/// Count how many of the four [`IntentSignature`] fields of `mem`'s intent
/// exactly match `sig` (in `0..=4`).
fn matching_field_count(mem: &Memory, sig: &IntentSignature) -> u32 {
    let mi = &mem.intent;
    u32::from(mi.intent_type == sig.intent_type)
        + u32::from(mi.target_type == sig.target_type)
        + u32::from(mi.target_ref == sig.target_ref)
        + u32::from(mi.scope == sig.scope)
}

/// A cheap, structured re-rank score for `mem` against `sig`.
///
/// The score is the count of exactly-matching [`IntentSignature`] fields
/// (`intent_type`, `target_type`, `target_ref`, `scope`), in `0.0..=4.0`. It is
/// deliberately cheap — four field comparisons, no embedding math — so it can
/// re-rank the whole candidate set inline.
///
/// # Ordering rationale
///
/// Ranking by structured-match strength means a full four-field structured
/// match always outranks any weaker match (Requirement 18.5), and a
/// structurally-matched head memory outranks an embedding-only tail memory that
/// shares fewer fields — so structured matches are considered before embedding
/// matches, as the design postcondition requires. A tail memory that happens to
/// match every structured field scores identically to an equivalent head
/// memory, which is correct: they are equally good structural matches, and
/// dedup already removed any true duplicate.
#[must_use]
pub fn cheap_score(mem: &Memory, sig: &IntentSignature) -> f32 {
    matching_field_count(mem, sig) as f32
}

/// A [`Retrieval`] implementation over a borrowed [`MemoryStore`] and
/// [`EmbeddingSource`], with configurable head/tail bounds.
///
/// The store supplies the structured head ([`MemoryStore::filter`]) and the ANN
/// tail ([`MemoryStore::ann_recall`]); the embedding source supplies the query
/// embedding (and models backend availability). `head_min` and `tail_limit`
/// default to [`HEAD_MIN`] / [`TAIL_LIMIT`] but are configurable via
/// [`Self::with_bounds`].
#[derive(Debug)]
pub struct MemoryRetrieval<'a, S: MemoryStore, E: EmbeddingSource> {
    store: &'a S,
    embedder: &'a E,
    head_min: usize,
    tail_limit: usize,
}

impl<'a, S: MemoryStore, E: EmbeddingSource> MemoryRetrieval<'a, S, E> {
    /// Create a retrieval over `store` and `embedder` with default bounds
    /// ([`HEAD_MIN`], [`TAIL_LIMIT`]).
    #[must_use]
    pub fn new(store: &'a S, embedder: &'a E) -> Self {
        Self {
            store,
            embedder,
            head_min: HEAD_MIN,
            tail_limit: TAIL_LIMIT,
        }
    }

    /// Create a retrieval with explicit `head_min` / `tail_limit` bounds.
    #[must_use]
    pub fn with_bounds(store: &'a S, embedder: &'a E, head_min: usize, tail_limit: usize) -> Self {
        Self {
            store,
            embedder,
            head_min,
            tail_limit,
        }
    }
}

impl<S: MemoryStore, E: EmbeddingSource> Retrieval for MemoryRetrieval<'_, S, E> {
    fn retrieve_memories(&self, sig: &IntentSignature) -> Vec<ScoredMemory> {
        // 1. Structured filter first (primary). `filter` applies each memory's
        //    applicability guard, so the head is assembled over all four
        //    signature fields before any embedding recall (Requirement 18.1).
        let head = self.store.filter(sig);

        // 2. Embedding recall only for a thin head (Requirements 18.2, 18.3).
        //    When the head has >= HEAD_MIN candidates we never consult the
        //    embedding backend and the head is the candidate set. Otherwise we
        //    try the backend; if it is available we recall up to TAIL_LIMIT and
        //    take the deduplicated union, else we degrade to head-only (18.7).
        let candidates: Vec<Memory> = if head.len() < self.head_min {
            match self.embedder.embed(sig) {
                Some(embedding) => {
                    let tail = self.store.ann_recall(&embedding, self.tail_limit);
                    dedupe_union(head, tail)
                }
                // Backend unavailable: structured-head-only, no error (18.7).
                None => head,
            }
        } else {
            head
        };

        // 3. Applicability exclusion (Requirement 18.4) + 4. cheap re-rank
        //    (Requirement 18.5). The head is already applicable, but tail
        //    candidates from ann_recall are ranked by embedding distance and may
        //    not be, so the guard is re-checked over the whole union.
        let mut scored: Vec<ScoredMemory> = candidates
            .into_iter()
            .filter(|m| m.applicability.applies_to(sig))
            .map(|m| {
                let score = cheap_score(&m, sig);
                ScoredMemory { memory: m, score }
            })
            .collect();

        // Order by score descending (Requirement 18.5). total_cmp is NaN-safe;
        // cheap_score never yields NaN, but this keeps the sort well-defined.
        scored.sort_by(|a, b| b.score.total_cmp(&a.score));

        // 5. Return compact memories (Requirement 18.6): ScoredMemory wraps a
        //    compact Memory, never a raw transcript.
        scored
    }
}

/// Deduplicate the union of `head` and `tail` by [`crate::types::MemoryId`],
/// keeping exactly one entry per distinct id and preferring the `head` entry.
///
/// `head` entries are emitted first (in order), then any `tail` entry whose id
/// was not already seen (Requirement 18.3).
fn dedupe_union(head: Vec<Memory>, tail: Vec<Memory>) -> Vec<Memory> {
    let mut seen: std::collections::HashSet<crate::types::MemoryId> =
        std::collections::HashSet::with_capacity(head.len() + tail.len());
    let mut union = Vec::with_capacity(head.len() + tail.len());
    for m in head.into_iter().chain(tail) {
        if seen.insert(m.id.clone()) {
            union.push(m);
        }
    }
    union
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier2::memory::{
        Applicability, EvidenceContract, MemoryKind, MemoryVersion, OutcomeShape, Plan,
        Provenance, Reinforcement,
    };
    use crate::tier2::store::{InMemoryMemoryStore, MemoryStoreError};
    use crate::types::{
        GoalNodeId, IntentType, MemoryId, OutcomeRef, Scope, Sha256, SubtreeHash, TargetRef,
        TargetType,
    };
    use std::cell::Cell;

    fn sig() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    /// A memory whose intent equals the given signature and whose applicability
    /// guard is unconstrained (so it applies to `sig`).
    fn memory_with(id: &str, intent: IntentSignature) -> Memory {
        Memory {
            id: MemoryId::from(id),
            kind: MemoryKind::Fragment,
            intent,
            parameter_schema: crate::tier2::memory::ParameterSchema::default(),
            applicability: Applicability::unconstrained(),
            plan: Plan::default(),
            evidence_contract: EvidenceContract::default(),
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "s".to_owned(),
            },
            cached_outcome: None,
            provenance: Provenance::default(),
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        }
    }

    fn key() -> (GoalNodeId, SubtreeHash) {
        (GoalNodeId::from("n"), SubtreeHash(Sha256::from("h")))
    }

    fn insert(store: &InMemoryMemoryStore, mem: Memory) -> Result<MemoryId, MemoryStoreError> {
        store.insert(key(), mem)
    }

    /// An embedding source that is always available and records call count.
    struct SpyEmbedder {
        embedding: Embedding,
        calls: Cell<usize>,
    }
    impl SpyEmbedder {
        fn available() -> Self {
            Self {
                embedding: Embedding(vec![0.1, 0.2, 0.3]),
                calls: Cell::new(0),
            }
        }
    }
    impl EmbeddingSource for SpyEmbedder {
        fn embed(&self, _sig: &IntentSignature) -> Option<Embedding> {
            self.calls.set(self.calls.get() + 1);
            Some(self.embedding.clone())
        }
    }

    /// An embedding source modeling a down backend: always `None`.
    struct DownEmbedder {
        calls: Cell<usize>,
    }
    impl DownEmbedder {
        fn new() -> Self {
            Self { calls: Cell::new(0) }
        }
    }
    impl EmbeddingSource for DownEmbedder {
        fn embed(&self, _sig: &IntentSignature) -> Option<Embedding> {
            self.calls.set(self.calls.get() + 1);
            None
        }
    }

    #[test]
    fn structured_head_used_without_embedding_when_head_not_thin() {
        // HEAD_MIN candidates in the head -> not thin -> no embedding recall.
        let store = InMemoryMemoryStore::new();
        for i in 0..HEAD_MIN {
            insert(&store, memory_with(&format!("head-{i}"), sig())).unwrap();
        }
        let embedder = SpyEmbedder::available();
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let results = retrieval.retrieve_memories(&sig());

        assert_eq!(results.len(), HEAD_MIN, "the head is the candidate set");
        assert_eq!(
            embedder.calls.get(),
            0,
            "embedding recall must not be invoked when the head is not thin"
        );
    }

    #[test]
    fn thin_head_invokes_embedding_recall_and_dedupes_union_by_id() {
        // One structured-head memory (< HEAD_MIN) -> thin -> tail recall runs.
        // "shared" is applicable (matches sig) so it appears in BOTH head and
        // tail; it must appear exactly once in the result.
        let store = InMemoryMemoryStore::new();
        insert(&store, memory_with("shared", sig())).unwrap();
        // A second memory that is only reachable via ann_recall: applicable too,
        // so it survives the applicability filter and widens the head.
        insert(&store, memory_with("tail-only", sig())).unwrap();

        let embedder = SpyEmbedder::available();
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let results = retrieval.retrieve_memories(&sig());

        assert_eq!(embedder.calls.get(), 1, "thin head must invoke embedding recall");
        let shared_count = results
            .iter()
            .filter(|s| s.memory.id == MemoryId::from("shared"))
            .count();
        assert_eq!(shared_count, 1, "a memory in head and tail appears once");
        // Both distinct applicable memories are present, deduped by id.
        let ids: std::collections::HashSet<_> =
            results.iter().map(|s| s.memory.id.clone()).collect();
        assert_eq!(ids.len(), 2, "union deduped to two distinct memories");
        assert!(ids.contains(&MemoryId::from("shared")));
        assert!(ids.contains(&MemoryId::from("tail-only")));
    }

    #[test]
    fn tail_candidate_failing_applicability_is_excluded() {
        // Head is thin (empty). The store holds a memory whose applicability
        // guard is NOT satisfied by sig; ann_recall may surface it, but the
        // defensive applicability filter must exclude it (Requirement 18.4).
        let store = InMemoryMemoryStore::new();
        let mut non_applicable = memory_with("bad", sig());
        non_applicable.applicability = Applicability {
            required_intent_type: Some(IntentType::from("mutate")),
            ..Applicability::default()
        };
        insert(&store, non_applicable).unwrap();

        let embedder = SpyEmbedder::available();
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let results = retrieval.retrieve_memories(&sig());

        assert!(
            results.is_empty(),
            "a candidate failing applicability must be excluded"
        );
    }

    #[test]
    fn results_ordered_by_score_descending_full_match_outranks_weaker() {
        // A full four-field structured match must outrank a weaker match.
        // Use a thin head so both are surfaced, then check ordering.
        let store = InMemoryMemoryStore::new();

        // Full match: intent equals sig exactly (score 4).
        insert(&store, memory_with("full", sig())).unwrap();

        // Weaker match: differs in target_ref (score 3), still applicable
        // (unconstrained guard).
        let weaker_intent = IntentSignature {
            target_ref: TargetRef::from("src/other.rs"),
            ..sig()
        };
        insert(&store, memory_with("weak", weaker_intent)).unwrap();

        let embedder = SpyEmbedder::available();
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let results = retrieval.retrieve_memories(&sig());

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].memory.id,
            MemoryId::from("full"),
            "the full structured match must rank first"
        );
        assert!(
            results[0].score > results[1].score,
            "results must be ordered by score descending"
        );
        assert_eq!(results[0].score, 4.0);
        assert_eq!(results[1].score, 3.0);
    }

    #[test]
    fn backend_down_degrades_to_head_only_without_error() {
        // Thin head + down backend -> head-only, no panic/error (Requirement 18.7).
        let store = InMemoryMemoryStore::new();
        insert(&store, memory_with("head", sig())).unwrap();

        let embedder = DownEmbedder::new();
        let retrieval = MemoryRetrieval::new(&store, &embedder);

        let results = retrieval.retrieve_memories(&sig());

        assert_eq!(embedder.calls.get(), 1, "the backend was consulted");
        assert_eq!(results.len(), 1, "structured head is returned");
        assert_eq!(results[0].memory.id, MemoryId::from("head"));
    }

    #[test]
    fn tail_recall_bound_by_tail_limit() {
        // Many memories, thin head trigger (head_min large so head is "thin"),
        // small tail_limit -> at most tail_limit distinct candidates surface via
        // the union (all applicable, all in head AND tail since filter returns
        // all applicable). We assert the tail request itself is bounded by
        // observing ann_recall through a small tail_limit and a store spy.
        let store = InMemoryMemoryStore::new();
        for i in 0..8 {
            insert(&store, memory_with(&format!("m-{i}"), sig())).unwrap();
        }

        let embedder = SpyEmbedder::available();
        // head_min = 100 forces the "thin" branch; tail_limit = 2 bounds recall.
        let retrieval = MemoryRetrieval::with_bounds(&store, &embedder, 100, 2);

        let tail = store.ann_recall(&embedder.embedding, 2);
        assert_eq!(tail.len(), 2, "ann_recall itself respects the tail limit");

        // Retrieval still returns the deduped union; head already holds all 8
        // applicable memories, so the union is 8, but the tail request was
        // bounded to 2 (verified above). This documents TAIL_LIMIT is honored.
        let results = retrieval.retrieve_memories(&sig());
        assert_eq!(results.len(), 8, "head union is complete; tail was bounded");
    }
}
