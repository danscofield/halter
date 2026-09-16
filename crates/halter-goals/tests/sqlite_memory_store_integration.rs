//! Integration tests for the SQLite-backed `MemoryStore` (Part 1) wired through
//! the public hot-path seams (Part 2), exercising the crate's PUBLIC API only.
//!
//! These tests live outside the crate (`tests/`), so they may only use items
//! reachable through `halter_goals::...`. In particular they define their own
//! minimal [`EvidenceValidator`] and [`EmbeddingSource`] implementations rather
//! than depending on any `#[cfg(test)]`-only fixtures inside the crate.
//!
//! - Test 1 (drop-in wiring, Requirements 7.1, 7.2, 7.3): wires
//!   `SqliteMemoryStore::open_in_memory()` through `MemoryRetrieval` and runs
//!   the UNCHANGED `start_goal`, proving the SQLite store substitutes for
//!   `InMemoryMemoryStore` without wiring changes.
//! - Test 2 (end-to-end reopen + summaries, Requirements 14.1, 14.2, 1.2):
//!   inserts into a FILE-BACKED store, drops to close, reopens at the same
//!   path, wires through `MemoryRetrieval`, and runs
//!   `start_goal_with_summaries` with summaries enabled — confirming the replay
//!   decision equals `start_goal`'s and summaries are attached.

use halter_goals::integration::{start_goal, start_goal_with_summaries, GoalStart};
use halter_goals::tier1::Freshness;
use halter_goals::tier2::{
    Applicability, DenyAllModeB, Embedding, EmbeddingSource, EvidenceContract,
    EvidenceContractItem, Memory, MemoryKind, MemoryRetrieval, MemoryStore, MemoryVersion,
    OutcomeShape, ParameterSchema, Plan, PlanStep, Provenance, Reinforcement, SqliteMemoryStore,
    SummaryConfig, SummaryProvider, TokensHold,
};
use halter_goals::types::{
    GoalNodeId, IntentSignature, IntentType, MemoryId, OutcomeRef, Scope, Sha256, SubtreeHash,
    TargetRef, TargetType, Timestamp, ValidityToken,
};

// ===========================================================================
// Local minimal test doubles (public-API only)
// ===========================================================================

/// A minimal, always-fresh [`EvidenceValidator`].
///
/// It reports every evidence item [`Freshness::Fresh`] and every answer-token
/// set [`TokensHold::AllHold`]. This is deliberately the simplest validator
/// that lets `decide_replay` (via `start_goal`) reach a serving decision on a
/// retrieval hit without depending on any Tier 1 wiring.
struct AlwaysFreshValidator;

impl halter_goals::tier2::EvidenceValidator for AlwaysFreshValidator {
    fn revalidate_item(&self, _item: &EvidenceContractItem) -> Freshness {
        Freshness::Fresh
    }

    fn tokens_hold(&self, _tokens: &[ValidityToken]) -> TokensHold {
        TokensHold::AllHold
    }
}

/// A minimal always-available embedder returning a fixed embedding.
struct FixedEmbedder;

impl EmbeddingSource for FixedEmbedder {
    fn embed(&self, _sig: &IntentSignature) -> Option<Embedding> {
        Some(Embedding(vec![0.1, 0.2, 0.3]))
    }
}

// ===========================================================================
// Shared fixtures
// ===========================================================================

/// The query signature used across both tests.
fn query_sig() -> IntentSignature {
    IntentSignature {
        intent_type: IntentType::from("lookup"),
        target_type: TargetType::from("file"),
        target_ref: TargetRef::from("src/lib.rs"),
        scope: Scope::from("repo"),
    }
}

/// An idempotency key for inserts.
fn key(id: &str) -> (GoalNodeId, SubtreeHash) {
    (GoalNodeId::from(id), SubtreeHash(Sha256::from("h")))
}

/// Build a valid `Memory` whose intent equals `query_sig()` and whose
/// applicability guard is unconstrained (so it applies to the query signature).
///
/// No `cached_outcome`, so `decide_replay` falls through to a
/// `ReplayProcedure` decision — enough to prove a retrieval hit drove the
/// decision. Passes `validate_memory` (a `Fragment` with a non-empty plan and
/// an empty evidence contract is valid).
fn applicable_memory(id: &str) -> Memory {
    Memory {
        id: MemoryId::from(id),
        kind: MemoryKind::Fragment,
        intent: query_sig(),
        parameter_schema: ParameterSchema::default(),
        applicability: Applicability::unconstrained(),
        plan: Plan {
            steps: vec![PlanStep {
                description: "read the file".to_owned(),
                tool: None,
                intent: None,
            }],
        },
        evidence_contract: EvidenceContract::default(),
        outcome_shape: OutcomeShape {
            result_ref: OutcomeRef::from("outcome://1"),
            schema: "file-contents".to_owned(),
        },
        cached_outcome: None,
        provenance: Provenance::default(),
        reinforcement: Reinforcement::default(),
        version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
    }
}

// ===========================================================================
// Task 10.1 — Drop-in wiring integration test
// ===========================================================================

/// Requirements 7.1, 7.2, 7.3: `SqliteMemoryStore` substitutes for
/// `InMemoryMemoryStore` in the retrieval + `start_goal` wiring with no changes
/// to that wiring.
///
/// We open an in-memory SQLite store, insert one applicable memory, wire it
/// through `MemoryRetrieval::new(&store, &embedder)` exactly as an
/// `InMemoryMemoryStore` would be wired, and run the UNCHANGED `start_goal`.
/// Because retrieval finds the applicable memory, `start_goal` returns
/// `GoalStart::Replay(..)` (a hit drives `decide_replay`).
#[test]
fn sqlite_store_is_drop_in_for_start_goal() {
    // SqliteMemoryStore used through the same `&self` MemoryStore contract.
    let store = SqliteMemoryStore::open_in_memory().expect("open in-memory sqlite store");
    store
        .insert(key("n1"), applicable_memory("m1"))
        .expect("valid insert persists");

    // Wire through retrieval UNCHANGED: identical call the in-memory store uses.
    let embedder = FixedEmbedder;
    let retrieval = MemoryRetrieval::new(&store, &embedder);
    let validator = AlwaysFreshValidator;

    // Run the untouched hot-path seam.
    let start = start_goal(
        &query_sig(),
        &retrieval,
        &validator,
        &DenyAllModeB,
        Timestamp(0),
    );

    // A retrieval hit must drive decide_replay -> GoalStart::Replay(..).
    match start {
        GoalStart::Replay(_) => {}
        GoalStart::FromScratch => {
            panic!("expected GoalStart::Replay from a retrieval hit, got FromScratch")
        }
    }
}

// ===========================================================================
// Task 10.2 — End-to-end reopen + summaries integration test
// ===========================================================================

/// A unique temp path for a file-backed SQLite store, cleaned up by the caller.
fn unique_temp_db_path() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    // Nanosecond time + process id makes collisions between concurrent test
    // binaries vanishingly unlikely without pulling in an extra dependency.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.push(format!(
        "halter_sqlite_memory_store_it_{}_{}.db",
        std::process::id(),
        nanos
    ));
    path
}

/// Requirements 14.1, 14.2, 1.2: durability across reopen plus additive
/// summaries on the hot path.
///
/// Insert applicable memories into a FILE-BACKED store, drop it to close the
/// connection, reopen at the same path, wire through `MemoryRetrieval`, and run
/// `start_goal_with_summaries` with summaries ENABLED. Assert:
///   (a) the replay decision equals what `start_goal` returns for the same
///       inputs (summaries are additive — Requirements 14.1, 14.2), and
///   (b) summaries are attached (non-empty when candidates exist).
/// The temp file is removed at the end.
#[test]
fn reopen_then_start_goal_with_summaries_is_additive() {
    let path = unique_temp_db_path();

    // --- 1. Insert into a file-backed store, then close it by dropping. -----
    {
        let store = SqliteMemoryStore::open(&path).expect("open file-backed sqlite store");
        store
            .insert(key("n1"), applicable_memory("m1"))
            .expect("valid insert persists");
        store
            .insert(key("n2"), applicable_memory("m2"))
            .expect("valid insert persists");
        // `store` drops here, closing the connection (Requirement 1.2 setup).
    }

    // --- 2. Reopen at the same path; data must survive the reopen. ----------
    let store = SqliteMemoryStore::open(&path).expect("reopen file-backed sqlite store");

    // Sanity: the previously persisted memories are retrievable after reopen.
    let persisted = store.filter(&query_sig());
    assert_eq!(
        persisted.len(),
        2,
        "both memories must survive close + reopen (Requirement 1.2)"
    );

    // --- 3. Wire through retrieval and run both hot-path seams. -------------
    let embedder = FixedEmbedder;
    let retrieval = MemoryRetrieval::new(&store, &embedder);
    let validator = AlwaysFreshValidator;

    // Baseline decision via the UNTOUCHED start_goal.
    let baseline = start_goal(
        &query_sig(),
        &retrieval,
        &validator,
        &DenyAllModeB,
        Timestamp(0),
    );

    // Summaries ENABLED with a bound.
    let provider = SummaryProvider::new(SummaryConfig {
        enabled: true,
        max_summaries: Some(5),
    });
    let with_summaries = start_goal_with_summaries(
        &query_sig(),
        &retrieval,
        &validator,
        &DenyAllModeB,
        Timestamp(0),
        &provider,
    );

    // (a) Additive: the replay decision is identical to start_goal's.
    assert_eq!(
        with_summaries.start, baseline,
        "start_goal_with_summaries must return the same GoalStart as start_goal (Requirements 14.1, 14.2)"
    );

    // (b) Summaries attached when candidates exist.
    assert!(
        !with_summaries.summaries.is_empty(),
        "summaries must be attached when applicable candidates exist"
    );

    // --- 4. Clean up the temp file. -----------------------------------------
    drop(store);
    let _ = std::fs::remove_file(&path);
}
