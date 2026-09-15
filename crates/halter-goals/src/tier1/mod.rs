//! Tier 1 — the deterministic exact-match result cache and its supporting
//! services (argument normalization, the Validity Token Service, and evidence
//! storage).
//!
//! This module is populated incrementally by the Tier 1 tasks. The argument
//! normalization layer lives in [`normalize`]; the Validity Token Service
//! (issuance + re-validation) lives in [`tokens`]; the exact-match evidence
//! cache lives in [`cache`].

pub mod cache;
pub mod normalize;
pub mod tokens;

pub use cache::{
    CacheEntry, CacheLookup, Freshness, InMemoryTier1Cache, PutRejected, Tier1Cache,
};
pub use normalize::{normalize_args, NormalizationError};
pub use tokens::{holds, issue_token, IssueError, SourceProvider, SourceUnreachable};
