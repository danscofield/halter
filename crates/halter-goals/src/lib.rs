//! Goal Pace — the Goal Model and its two procedural-memory acceleration layers.
//!
//! This crate owns three subsystems, each exposed as a module:
//!
//! - [`goal_model`]: the hypothesis-shaped `GoalNode` tree, its resolution
//!   lifecycle, `subtree_hash` computation, `IntentSignature` derivation,
//!   persistence on the event-sourced session store, and the goal-closure signal.
//! - [`tier1`]: the deterministic exact-match result cache, argument
//!   normalization, and the volatility-aware Validity Token Service.
//! - [`tier2`]: procedural-memory induction, store/retrieval, replay, and
//!   two-mode answer caching.
//!
//! Shared identifier and value types common to all three subsystems live in
//! [`types`] and are re-exported at the crate root so `goal_model`, `tier1`, and
//! `tier2` can speak one vocabulary.
//!
//! The [`integration`] module wires the subsystems into the two end-to-end flows
//! the design shows in § "Example Usage": goal closure enqueuing induction
//! asynchronously off the hot path ([`EngineInductionQueue`]), and the hot-path
//! retrieval -> replay -> Tier 1 re-validation path ([`start_goal`]).
//!
//! # Public API
//!
//! The key public types and functions of each subsystem are re-exported at the
//! crate root so callers can `use halter_goals::{...}` without reaching into the
//! `goal_model`/`tier1`/`tier2`/`integration` module paths. The modules remain
//! public for callers that prefer the fully-qualified paths.

pub mod types;

pub use types::{
    CanonicalJson, ContentRef, Duration, EventKey, EventSeq, EvidenceValue, GoalNodeId,
    IntentSignature, IntentType, MemoryId, OutcomeRef, Scope, Sha256, SourceDescriptor,
    SubtreeHash, TargetRef, TargetType, Timestamp, ToolCall, ToolName, ValidityToken,
};

pub mod goal_model;

pub mod tier1;

pub mod tier2;

pub mod integration;

// --- Crate-root public API re-exports --------------------------------------
//
// One flat vocabulary for callers of the crate. Grouped by subsystem; the
// module-level re-exports (goal_model::*, tier1::*, tier2::*) remain the source
// of truth, this simply surfaces the most-used items at the root.

/// Goal Model — the goal tree, its store, resolution lifecycle, and closure seam.
pub use goal_model::{
    ClosureOutcome, ClosureSignal, EventLogGoalStore, GoalEvent, GoalEventLog, GoalNode,
    GoalNodeRevision, GoalStore, GoalStoreError, GoalTree, InMemoryGoalEventLog, InductionQueue,
    NoopInductionQueue, RecordingInductionQueue, Resolution, SessionStoreGoalEventLog,
    SharedInductionQueue,
};

/// Tier 1 — the exact-match evidence cache, normalization, and token service.
pub use tier1::{
    holds, issue_token, normalize_args, CacheEntry, CacheLookup, Freshness, InMemoryTier1Cache,
    IssueError, SourceProvider, SourceUnreachable, Tier1Cache,
};

/// Tier 2 — induction, the memory store/retrieval, and replay.
pub use tier2::{
    decide_replay, insert_memory_with_writer, AllowListModeB, Author, CachedOutcome, DenyAllModeB,
    EmbeddingSource, EvidenceContract, EvidenceContractItem, EvidenceValidator,
    GoalResolutionAuthor, GoalResolutionJudge, InMemoryMemoryStore, InductionEngine,
    InductionOutcome, InMemoryRecurrenceTracker, Judge, Memory, MemoryEmbeddingWriter, MemoryStore,
    MemoryRetrieval, ModeBPolicy, NoEmbeddingClient, OpenAiEmbeddingSource,
    ResolvedEmbeddingSettings, Retrieval, ReplayDecision, ScoredMemory, SqliteMemoryStore,
    SqliteStoreError, TokensHold,
};

/// Integration — the end-to-end wiring for the two design flows.
pub use integration::{
    retrieve_advisory, start_goal, AdvisorySummary, EngineInductionQueue, GoalStart,
};
