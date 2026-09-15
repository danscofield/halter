//! Tier 2 — the procedural-memory store with write-time validation.
//!
//! The [`MemoryStore`] persists [`Memory`] objects and surfaces applicable ones
//! for a new goal, structured-first. Its defining responsibility beyond storage
//! is **write-time validation**: every write ([`MemoryStore::insert`] and the
//! candidate side of [`MemoryStore::reinforce`]) is gated by [`validate_memory`]
//! so that no invalid memory is ever persisted (Requirements 17.1–17.5, 22.1–22.3).
//!
//! The invariants enforced here are:
//!
//! - **Kind is present and in-set** (Requirement 17.1). Because [`MemoryKind`]
//!   is a closed Rust enum, "absent or outside the set" is unrepresentable — the
//!   type system enforces it. The [`MemoryStoreError::InvalidMemoryKind`] variant
//!   is retained for completeness / API stability, but validation cannot observe
//!   an invalid kind for an already-typed [`Memory`].
//! - **Negative memories carry evidence** (Requirements 17.2, 17.3): a `Negative`
//!   memory must have a non-empty `evidence_contract`, else the write is rejected
//!   with [`MemoryStoreError::MissingEvidenceContract`].
//! - **Contracts, not values** (Requirements 17.4, 22.x): the [`EvidenceContract`]
//!   type structurally excludes concrete values, so storing the memory as-is
//!   preserves the "contracts only" invariant with no extra work.
//! - **Answer caches carry tokens** (Requirements 17.5, 22.3): a present
//!   `cached_outcome` must have a non-empty `validity_tokens`, else the write is
//!   rejected with [`MemoryStoreError::EmptyValidityTokens`].
//! - **Mode A purity** (Requirements 22.1, 22.2): a `SoundPinnable` cached
//!   outcome's `validity_tokens` must be *only* pinnable `ContentHash` tokens;
//!   any volatile (`Ttl`/`EventDriven`) token rejects the write with
//!   [`MemoryStoreError::UnsoundPinnableTokens`].
//!
//! The [`InMemoryMemoryStore`] is a thread-safe implementation backed by a
//! [`RwLock`]-guarded map of [`MemoryRecord`], mirroring the interior-mutability
//! pattern of the Tier 1 cache. Idempotency/dedup on `(node_id, subtree_hash)`
//! is out of scope here (task 12.3); `insert` validates and stores.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::tier2::memory::{
    AnswerMode, Embedding, Memory, MemoryKind, MemoryRecord, Plan,
};
use crate::types::{
    GoalNodeId, IntentSignature, MemoryId, SubtreeHash, Timestamp, ValidityToken,
};

/// A write-time validation failure raised by the [`MemoryStore`].
///
/// Every variant corresponds to an acceptance-criterion rule that must hold
/// before a memory is persisted. A returned error guarantees the store was left
/// unchanged (no partial write): validation runs before any mutation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemoryStoreError {
    /// The memory's `kind` is absent or outside {`Fragment`, `Composite`,
    /// `Negative`} (Requirement 17.1).
    ///
    /// Because [`MemoryKind`] is a closed enum, this condition is unrepresentable
    /// for a well-typed [`Memory`] and is enforced by the type system; the
    /// variant exists for completeness and API stability.
    #[error("invalid memory kind: kind is absent or outside the permitted set")]
    InvalidMemoryKind,

    /// A `Negative` memory was written without a non-empty `evidence_contract`
    /// (Requirements 17.2, 17.3).
    #[error("missing evidence contract: a Negative memory requires a non-empty evidence contract")]
    MissingEvidenceContract,

    /// A `cached_outcome` was present but its `validity_tokens` was empty
    /// (Requirements 17.5, 22.3).
    #[error("empty validity tokens: a cached outcome requires at least one validity token")]
    EmptyValidityTokens,

    /// A `SoundPinnable` (Mode A) cached outcome carried a volatile token
    /// (`Ttl` or `EventDriven`) among its `validity_tokens` (Requirements 22.1,
    /// 22.2).
    #[error(
        "unsound pinnable tokens: a SoundPinnable answer must depend only on pinnable ContentHash tokens"
    )]
    UnsoundPinnableTokens,
}

/// Validate a memory against the Tier 2 write-time rules.
///
/// Returns `Ok(())` iff the memory is safe to persist. This is the single
/// gate applied by [`MemoryStore::insert`] and by [`MemoryStore::reinforce`] to
/// its candidate. The checks, in order:
///
/// 1. **Kind in-set** (Requirement 17.1) — matched exhaustively; the closed
///    [`MemoryKind`] enum makes every value valid, so this documents the
///    type-level enforcement rather than rejecting anything at runtime.
/// 2. **Negative ⇒ non-empty evidence contract** (Requirements 17.2, 17.3).
/// 3. **Present `cached_outcome` ⇒ non-empty `validity_tokens`** (Requirements
///    17.5, 22.3).
/// 4. **`SoundPinnable` ⇒ only `ContentHash` tokens** (Requirements 22.1, 22.2).
///
/// # Errors
///
/// Returns the matching [`MemoryStoreError`] variant for the first rule violated.
pub fn validate_memory(mem: &Memory) -> Result<(), MemoryStoreError> {
    // (1) Kind must be one of the permitted set. `MemoryKind` is a closed enum,
    // so this match is exhaustive and every arm is valid — the type system
    // enforces Requirement 17.1. We keep the exhaustive match so a future new
    // variant forces a compile-time decision here.
    match mem.kind {
        MemoryKind::Fragment | MemoryKind::Composite | MemoryKind::Negative => {}
    }

    // (2) A Negative memory must carry a non-empty evidence contract
    // (Requirements 17.2, 17.3).
    if mem.kind == MemoryKind::Negative && mem.evidence_contract.is_empty() {
        return Err(MemoryStoreError::MissingEvidenceContract);
    }

    // (3)/(4) Rules that only apply when an answer is cached.
    if let Some(cached) = &mem.cached_outcome {
        // (3) A present cached outcome must depend on at least one token
        // (Requirements 17.5, 22.3).
        if cached.validity_tokens.is_empty() {
            return Err(MemoryStoreError::EmptyValidityTokens);
        }

        // (4) Mode A purity: a SoundPinnable answer must depend only on pinnable
        // ContentHash tokens (Requirements 22.1, 22.2).
        if cached.mode == AnswerMode::SoundPinnable
            && cached
                .validity_tokens
                .iter()
                .any(|t| !matches!(t, ValidityToken::ContentHash(_)))
        {
            return Err(MemoryStoreError::UnsoundPinnableTokens);
        }
    }

    Ok(())
}

/// Persist memories and surface applicable ones for a new goal, structured-first.
///
/// Writes are validated at write time (see [`validate_memory`]); an invalid
/// memory is never persisted. Reads are split into the **structured head**
/// ([`Self::filter`], the primary applicability filter over indexed signature
/// fields) and the **ANN tail** ([`Self::ann_recall`], embedding recall used
/// only when the structured head is thin).
///
/// The trait is synchronous with interior mutability (methods take `&self`),
/// matching the Tier 1 cache: the design shows a sync `MemoryStore`, and a
/// sync API keeps writes off any async runtime.
pub trait MemoryStore {
    /// Validate and store `mem` under the idempotency key `(node_id, subtree_hash)`.
    ///
    /// On success the memory is retrievable via [`Self::filter`] /
    /// [`Self::ann_recall`] and its [`MemoryId`] is returned.
    ///
    /// # Errors
    ///
    /// Returns a [`MemoryStoreError`] when `mem` violates a write-time rule (see
    /// [`validate_memory`]); in that case nothing is persisted.
    ///
    /// Full dedup/idempotency on the key is handled separately (task 12.3); this
    /// method validates and stores.
    fn insert(
        &self,
        key: (GoalNodeId, SubtreeHash),
        mem: Memory,
    ) -> Result<MemoryId, MemoryStoreError>;

    /// Reinforce the memory identified by `id` using a fresh `candidate`.
    ///
    /// The `candidate` is validated with the same rules as [`Self::insert`]. On
    /// success the existing memory's reinforcement bookkeeping is updated (its
    /// `hits`/`confirms` counters advance and `last_updated` is bumped) and its
    /// [`MemoryId`] is returned.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryStoreError`] if the `candidate` fails validation. The
    /// existing memory is left unchanged in that case. If `id` is unknown the
    /// candidate is validated and, when valid, inserted under `id` (the store
    /// treats reinforce of an absent id as a first observation).
    fn reinforce(
        &self,
        id: &MemoryId,
        candidate: Memory,
    ) -> Result<MemoryId, MemoryStoreError>;

    /// Structured primary filter: memories applicable to `sig`.
    ///
    /// Returns every stored memory whose [`crate::tier2::Applicability`] guard is
    /// satisfied by `sig` (`applies_to(sig)`). This is the structured head of
    /// retrieval; the ANN tail ([`Self::ann_recall`]) supplements it only when
    /// the head is thin.
    fn filter(&self, sig: &IntentSignature) -> Vec<Memory>;

    /// ANN tail recall: up to `limit` memories nearest `embedding`.
    ///
    /// This is the embedding-recall seam used to widen a thin structured head.
    /// The in-memory implementation ranks stored memories by a cheap cosine
    /// distance to `embedding` and returns the closest `limit`.
    fn ann_recall(&self, embedding: &Embedding, limit: usize) -> Vec<Memory>;

    /// The [`MemoryId`] of the memory stored under the idempotency key
    /// `(node_id, subtree_hash)`, if any (Requirement 16.1).
    ///
    /// This is the idempotency lookup the Induction Engine consults before
    /// authoring: if a memory already exists for the closure key, the engine
    /// reinforces it instead of authoring a duplicate. Returns `None` when no
    /// memory has been stored for the key.
    fn has_memory_for(&self, key: &(GoalNodeId, SubtreeHash)) -> Option<MemoryId>;

    /// The [`MemoryId`] of an existing memory that matches a candidate by
    /// `intent` **and** `plan` (Requirement 16.2), if any.
    ///
    /// The Induction Engine calls this after authoring: an intent+plan match
    /// means the candidate is a duplicate of an already-stored memory, so the
    /// engine reinforces the match rather than inserting. Returns `None` when no
    /// stored memory matches on both fields.
    fn find_duplicate(&self, intent: &IntentSignature, plan: &Plan) -> Option<MemoryId>;
}

/// An in-memory, thread-safe [`MemoryStore`].
///
/// Storage is a [`RwLock`]-guarded map from [`MemoryId`] to [`MemoryRecord`].
/// Because the trait methods take `&self`, the map lives behind a lock for
/// interior mutability and thread-safe sharing (mirroring `InMemoryTier1Cache`):
/// reads take a read guard, writes take a write guard.
///
/// Timestamps use a simple monotonic counter seeded at [`Timestamp::default`]
/// so the store is deterministic without a wall clock; `created_at` is fixed at
/// insert and `updated_at` advances on reinforcement.
#[derive(Debug, Default)]
pub struct InMemoryMemoryStore {
    records: RwLock<HashMap<MemoryId, MemoryRecord>>,
}

impl InMemoryMemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            records: RwLock::new(HashMap::new()),
        }
    }

    /// Build a [`MemoryRecord`] from a validated `mem`, its key, and timestamps.
    ///
    /// The indexed fields mirror `mem.intent`/`mem.kind` for the structured
    /// filter; `embedding` defaults to empty (populated by later indexing tasks).
    fn record_from(
        mem: Memory,
        node_id: GoalNodeId,
        subtree_hash: SubtreeHash,
        created_at: Timestamp,
        updated_at: Timestamp,
    ) -> MemoryRecord {
        MemoryRecord {
            id: mem.id.clone(),
            intent_type: mem.intent.intent_type.clone(),
            target_type: mem.intent.target_type.clone(),
            target_ref: mem.intent.target_ref.clone(),
            scope: mem.intent.scope.clone(),
            kind: mem.kind,
            embedding: Embedding::default(),
            body: mem,
            node_id,
            subtree_hash,
            created_at,
            updated_at,
        }
    }
}

/// Cosine distance between two embeddings (`1 - cosine_similarity`).
///
/// Returns a value in `[0, 2]` where `0` is identical direction. Zero-magnitude
/// vectors (including empty ones) are treated as maximally distant (`2.0`) so
/// they sort last, keeping ranking well-defined without panicking.
fn cosine_distance(a: &Embedding, b: &Embedding) -> f32 {
    let len = a.0.len().min(b.0.len());
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for i in 0..len {
        dot += a.0[i] * b.0[i];
        norm_a += a.0[i] * a.0[i];
        norm_b += b.0[i] * b.0[i];
    }
    // Also account for magnitude of any tail beyond the shared prefix so
    // differently-sized vectors do not appear artificially similar.
    for &x in a.0.iter().skip(len) {
        norm_a += x * x;
    }
    for &x in b.0.iter().skip(len) {
        norm_b += x * x;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 2.0;
    }
    1.0 - dot / (norm_a.sqrt() * norm_b.sqrt())
}

impl MemoryStore for InMemoryMemoryStore {
    fn insert(
        &self,
        key: (GoalNodeId, SubtreeHash),
        mem: Memory,
    ) -> Result<MemoryId, MemoryStoreError> {
        // Validate before touching stored state so a rejected write leaves the
        // store unchanged (Requirements 17.x, 22.x).
        validate_memory(&mem)?;

        let (node_id, subtree_hash) = key;
        let id = mem.id.clone();
        // Deterministic timestamp: created == updated at insert time.
        let now = Timestamp::default();
        let record = Self::record_from(mem, node_id, subtree_hash, now, now);

        let mut guard = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Dedup/idempotency on (node_id, subtree_hash) is task 12.3; here we
        // simply store keyed by MemoryId.
        guard.insert(id.clone(), record);
        Ok(id)
    }

    fn reinforce(
        &self,
        id: &MemoryId,
        candidate: Memory,
    ) -> Result<MemoryId, MemoryStoreError> {
        // The candidate is subject to the same write-time validation as insert.
        validate_memory(&candidate)?;

        let mut guard = self
            .records
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match guard.get_mut(id) {
            Some(record) => {
                // Update reinforcement bookkeeping on the existing memory: count
                // the observation as a hit + confirm and advance updated_at.
                let r = &mut record.body.reinforcement;
                r.hits = r.hits.saturating_add(1);
                r.confirms = r.confirms.saturating_add(1);
                let next = Timestamp(record.updated_at.0.saturating_add(1));
                r.last_updated = next;
                record.updated_at = next;
                Ok(id.clone())
            }
            None => {
                // Reinforcing an unknown id is treated as a first observation:
                // store the validated candidate under its own key.
                let node_id = candidate
                    .provenance
                    .origins
                    .first()
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default();
                let subtree_hash = candidate.version.0.clone();
                let new_id = candidate.id.clone();
                let now = Timestamp::default();
                let record =
                    Self::record_from(candidate, node_id, subtree_hash, now, now);
                guard.insert(new_id.clone(), record);
                Ok(new_id)
            }
        }
    }

    fn filter(&self, sig: &IntentSignature) -> Vec<Memory> {
        let guard = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .values()
            .filter(|r| r.body.applicability.applies_to(sig))
            .map(|r| r.body.clone())
            .collect()
    }

    fn ann_recall(&self, embedding: &Embedding, limit: usize) -> Vec<Memory> {
        if limit == 0 {
            return Vec::new();
        }
        let guard = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut scored: Vec<(f32, Memory)> = guard
            .values()
            .map(|r| (cosine_distance(&r.embedding, embedding), r.body.clone()))
            .collect();
        // Sort ascending by distance; NaN-safe via total_cmp.
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, mem)| mem)
            .collect()
    }

    fn has_memory_for(&self, key: &(GoalNodeId, SubtreeHash)) -> Option<MemoryId> {
        let (node_id, subtree_hash) = key;
        let guard = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .values()
            .find(|r| &r.node_id == node_id && &r.subtree_hash == subtree_hash)
            .map(|r| r.id.clone())
    }

    fn find_duplicate(&self, intent: &IntentSignature, plan: &Plan) -> Option<MemoryId> {
        let guard = self
            .records
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .values()
            .find(|r| &r.body.intent == intent && &r.body.plan == plan)
            .map(|r| r.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier2::memory::{
        Applicability, CachedOutcome, EvidenceContract, EvidenceContractItem, MemoryVersion,
        OutcomeShape, OutcomeValue, Plan, Provenance, Reinforcement,
    };
    use crate::types::{
        CanonicalJson, Duration, IntentType, OutcomeRef, Scope, Sha256, TargetRef, TargetType,
        ToolName,
    };

    fn sig() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn contract_item() -> EvidenceContractItem {
        EvidenceContractItem {
            tool: ToolName::from("read_file"),
            normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
            validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
        }
    }

    /// A valid `Fragment` memory whose applicability requires the `lookup`
    /// intent type, with a sound (`ContentHash`-only) cached outcome.
    fn valid_memory(id: &str) -> Memory {
        Memory {
            id: MemoryId::from(id),
            kind: MemoryKind::Fragment,
            intent: sig(),
            parameter_schema: crate::tier2::memory::ParameterSchema::default(),
            applicability: Applicability {
                required_intent_type: Some(IntentType::from("lookup")),
                ..Applicability::default()
            },
            plan: Plan::default(),
            evidence_contract: EvidenceContract {
                items: vec![contract_item()],
            },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            cached_outcome: Some(CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{\"lines\":42}".to_owned())),
                mode: AnswerMode::SoundPinnable,
                validity_tokens: vec![ValidityToken::ContentHash(Sha256::from("abc123"))],
                issued_at: Timestamp(1_000),
            }),
            provenance: Provenance {
                origins: vec![(
                    GoalNodeId::from("node-1"),
                    SubtreeHash(Sha256::from("hash-1")),
                )],
            },
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("hash-1"))),
        }
    }

    fn key() -> (GoalNodeId, SubtreeHash) {
        (
            GoalNodeId::from("node-1"),
            SubtreeHash(Sha256::from("hash-1")),
        )
    }

    #[test]
    fn valid_insert_succeeds_and_is_retrievable_via_filter() {
        let store = InMemoryMemoryStore::new();
        let mem = valid_memory("mem-1");

        let id = store.insert(key(), mem).expect("valid insert should succeed");
        assert_eq!(id, MemoryId::from("mem-1"));

        let found = store.filter(&sig());
        assert_eq!(found.len(), 1, "the inserted memory must be retrievable");
        assert_eq!(found[0].id, MemoryId::from("mem-1"));
    }

    #[test]
    fn negative_memory_without_evidence_contract_is_rejected() {
        let store = InMemoryMemoryStore::new();
        let mut mem = valid_memory("neg-1");
        mem.kind = MemoryKind::Negative;
        mem.evidence_contract = EvidenceContract::default(); // empty
        // Remove the cached outcome so we isolate the evidence-contract rule.
        mem.cached_outcome = None;

        let err = store.insert(key(), mem).expect_err("empty contract must reject");
        assert_eq!(err, MemoryStoreError::MissingEvidenceContract);
        // Nothing persisted.
        assert!(store.filter(&sig()).is_empty());
    }

    #[test]
    fn negative_memory_with_evidence_contract_succeeds() {
        let store = InMemoryMemoryStore::new();
        let mut mem = valid_memory("neg-2");
        mem.kind = MemoryKind::Negative;
        // keeps the non-empty contract from valid_memory
        mem.cached_outcome = None;

        store.insert(key(), mem).expect("negative with contract should succeed");
    }

    #[test]
    fn cached_outcome_with_empty_validity_tokens_is_rejected() {
        let store = InMemoryMemoryStore::new();
        let mut mem = valid_memory("mem-2");
        if let Some(cached) = mem.cached_outcome.as_mut() {
            cached.validity_tokens.clear();
        }

        let err = store
            .insert(key(), mem)
            .expect_err("empty validity tokens must reject");
        assert_eq!(err, MemoryStoreError::EmptyValidityTokens);
        assert!(store.filter(&sig()).is_empty());
    }

    #[test]
    fn sound_pinnable_with_volatile_token_is_rejected() {
        let store = InMemoryMemoryStore::new();

        // A Ttl token is volatile — illegal under SoundPinnable.
        let mut ttl_mem = valid_memory("mem-ttl");
        if let Some(cached) = ttl_mem.cached_outcome.as_mut() {
            cached.validity_tokens = vec![ValidityToken::Ttl {
                issued_at: Timestamp(0),
                ttl: Duration(1_000),
            }];
        }
        let err = store
            .insert(key(), ttl_mem)
            .expect_err("SoundPinnable + Ttl must reject");
        assert_eq!(err, MemoryStoreError::UnsoundPinnableTokens);

        // An EventDriven token is also volatile.
        let mut ev_mem = valid_memory("mem-ev");
        if let Some(cached) = ev_mem.cached_outcome.as_mut() {
            cached.validity_tokens = vec![ValidityToken::EventDriven {
                subscription: crate::types::EventKey::from("fs"),
                last_seen: crate::types::EventSeq(1),
            }];
        }
        let err = store
            .insert(key(), ev_mem)
            .expect_err("SoundPinnable + EventDriven must reject");
        assert_eq!(err, MemoryStoreError::UnsoundPinnableTokens);

        assert!(store.filter(&sig()).is_empty());
    }

    #[test]
    fn sound_pinnable_with_only_content_hash_tokens_succeeds() {
        let store = InMemoryMemoryStore::new();
        let mut mem = valid_memory("mem-pure");
        if let Some(cached) = mem.cached_outcome.as_mut() {
            cached.validity_tokens = vec![
                ValidityToken::ContentHash(Sha256::from("h1")),
                ValidityToken::ContentHash(Sha256::from("h2")),
            ];
        }
        store
            .insert(key(), mem)
            .expect("SoundPinnable with only ContentHash tokens should succeed");
    }

    #[test]
    fn filter_returns_only_applicable_memories() {
        let store = InMemoryMemoryStore::new();

        // Applicable to `lookup`.
        store.insert(key(), valid_memory("mem-applies")).unwrap();

        // Not applicable: requires a different intent type.
        let mut other = valid_memory("mem-other");
        other.applicability = Applicability {
            required_intent_type: Some(IntentType::from("mutate")),
            ..Applicability::default()
        };
        store.insert(key(), other).unwrap();

        let found = store.filter(&sig());
        assert_eq!(found.len(), 1, "only the applicable memory is returned");
        assert_eq!(found[0].id, MemoryId::from("mem-applies"));
    }

    #[test]
    fn ann_recall_respects_limit() {
        let store = InMemoryMemoryStore::new();
        for i in 0..5 {
            store
                .insert(key(), valid_memory(&format!("mem-{i}")))
                .unwrap();
        }

        let recalled = store.ann_recall(&Embedding(vec![0.1, 0.2, 0.3]), 3);
        assert_eq!(recalled.len(), 3, "ann_recall must not exceed the limit");

        assert!(
            store.ann_recall(&Embedding(vec![0.0]), 0).is_empty(),
            "a zero limit yields no results"
        );
    }

    #[test]
    fn reinforce_updates_bookkeeping_on_existing_memory() {
        let store = InMemoryMemoryStore::new();
        let id = store.insert(key(), valid_memory("mem-r")).unwrap();

        let returned = store
            .reinforce(&id, valid_memory("mem-r"))
            .expect("reinforce of a valid candidate should succeed");
        assert_eq!(returned, id);

        let found = store.filter(&sig());
        let mem = found
            .iter()
            .find(|m| m.id == id)
            .expect("reinforced memory present");
        assert_eq!(mem.reinforcement.hits, 1);
        assert_eq!(mem.reinforcement.confirms, 1);
    }

    #[test]
    fn has_memory_for_finds_by_idempotency_key() {
        let store = InMemoryMemoryStore::new();
        assert!(
            store.has_memory_for(&key()).is_none(),
            "empty store has no memory for the key"
        );

        let id = store.insert(key(), valid_memory("mem-k")).unwrap();
        assert_eq!(
            store.has_memory_for(&key()),
            Some(id),
            "the memory is found by its (node_id, subtree_hash) key"
        );

        // A different key does not match.
        let other_key = (
            GoalNodeId::from("node-2"),
            SubtreeHash(Sha256::from("hash-2")),
        );
        assert!(store.has_memory_for(&other_key).is_none());
    }

    #[test]
    fn find_duplicate_matches_on_intent_and_plan() {
        let store = InMemoryMemoryStore::new();
        let mem = valid_memory("mem-d");
        let intent = mem.intent.clone();
        let plan = mem.plan.clone();
        assert!(
            store.find_duplicate(&intent, &plan).is_none(),
            "empty store has no duplicate"
        );

        let id = store.insert(key(), mem).unwrap();
        assert_eq!(
            store.find_duplicate(&intent, &plan),
            Some(id),
            "an intent+plan match is found"
        );

        // A different plan does not match even with the same intent.
        let other_plan = Plan {
            steps: vec![crate::tier2::memory::PlanStep {
                description: "different step".to_owned(),
                tool: None,
                intent: None,
            }],
        };
        assert!(store.find_duplicate(&intent, &other_plan).is_none());
    }

    #[test]
    fn reinforce_rejects_invalid_candidate() {
        let store = InMemoryMemoryStore::new();
        let id = store.insert(key(), valid_memory("mem-rv")).unwrap();

        let mut bad = valid_memory("mem-rv");
        if let Some(cached) = bad.cached_outcome.as_mut() {
            cached.validity_tokens.clear();
        }
        let err = store
            .reinforce(&id, bad)
            .expect_err("invalid candidate must be rejected");
        assert_eq!(err, MemoryStoreError::EmptyValidityTokens);
    }
}
