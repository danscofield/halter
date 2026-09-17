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

pub mod embedding;
pub mod induction;
pub mod memory;
pub mod replay;
pub mod retrieval;
pub mod sqlite_store;
pub mod store;
pub mod summary;

pub use embedding::{embedding_input_text, EMPTY_FIELD_PLACEHOLDER, FIELD_SEPARATOR};
pub use embedding::{EmbeddingCache, EmbeddingCacheKey};
pub use embedding::{
    insert_memory_with_writer, EmbeddingWriteError, MemoryEmbeddingWriter, OpenAiEmbeddingSource,
    ResolvedEmbeddingSettings, WriteEmbedding, DEFAULT_EMBEDDING_BASE_URL,
    MAX_EMBEDDING_MAX_ATTEMPTS, MIN_EMBEDDING_MAX_ATTEMPTS,
};
pub use induction::{
    Author, AuthorError, CleanContext, Decision, DeclineRecord, DistilledNode, FailureRecord,
    GoalResolutionAuthor, GoalResolutionJudge, Granularity, InMemoryRecurrenceTracker,
    InductionEngine, InductionLog, InductionOutcome, Judge, JudgeVerdict, NoEmbeddingClient,
    RecurrenceTracker,
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
