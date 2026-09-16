//! Tier 2 — the procedural-memory acceleration layer.
//!
//! Tier 2 induces reusable memories from resolved goal work, stores and
//! retrieves them structured-first, replays them (re-validating evidence
//! through Tier 1), and serves two-mode cached answers.
//!
//! This module is populated incrementally by the Tier 2 tasks. The
//! procedural-memory model — [`Memory`] and its supporting types — lives in
//! [`memory`]; the store, induction, retrieval, and replay engines are added by
//! later tasks.

pub mod induction;
pub mod memory;
pub mod replay;
pub mod retrieval;
pub mod sqlite_store;
pub mod store;
pub mod summary;

pub use induction::{
    Author, AuthorError, CleanContext, Decision, DeclineRecord, DistilledNode, FailureRecord,
    Granularity, InMemoryRecurrenceTracker, InductionEngine, InductionLog, InductionOutcome,
    Judge, JudgeVerdict, RecurrenceTracker,
};
pub use memory::{
    AnswerMode, Applicability, CachedOutcome, Embedding, EvidenceContract,
    EvidenceContractItem, Memory, MemoryKind, MemoryRecord, MemoryVersion, OutcomeShape,
    OutcomeValue, ParameterDef, ParameterSchema, Plan, PlanStep, Provenance, Reinforcement,
};
pub use replay::{
    decide_replay, AllowListModeB, DenyAllModeB, EvidenceValidator, ModeBPolicy, ReplayDecision,
    TokensHold,
};
pub use retrieval::{
    cheap_score, EmbeddingSource, MemoryRetrieval, Retrieval, ScoredMemory, HEAD_MIN, TAIL_LIMIT,
};
pub use sqlite_store::{SqliteMemoryStore, SqliteStoreError};
pub use store::{InMemoryMemoryStore, MemoryStore, MemoryStoreError};
pub use summary::{GoalSummary, SummaryConfig, SummaryProvider};
