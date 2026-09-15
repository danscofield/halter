//! Tier 1 — the deterministic exact-match evidence cache.
//!
//! Tier 1 stores the concrete result of a tool call ([`crate::types::EvidenceValue`])
//! alongside the volatility-aware [`ValidityToken`] it was captured under, keyed by
//! `tool + normalized_args`. Tier 2 never duplicates these values: it holds only the
//! *contract* (`tool + normalized_args + validity_token`) and reads/re-validates through
//! Tier 1. This module defines the cache's data model — [`CacheEntry`], [`CacheLookup`],
//! and [`Freshness`] — and the [`Tier1Cache`] trait. The concrete read/write/revalidate
//! behavior is implemented by tasks 4.2–4.4.
//!
//! ## Re-validation needs a source and a provider
//!
//! Freshness is decided by [`tokens::holds`], which re-observes the *current* state of the
//! source a token was issued for. A bare [`ValidityToken`] does not carry a back-reference
//! to its source (a `ContentHash` records only the digest, a `Ttl` only its window), so
//! re-validation needs both the original [`SourceDescriptor`] and a [`SourceProvider`] to
//! observe it — exactly as [`tokens::holds`] requires. Accordingly:
//!
//! - every [`CacheEntry`] records the [`SourceDescriptor`] of the source that produced its
//!   evidence, so the cache can re-observe that source when [`Tier1Cache::get`] is called;
//!   and
//! - the [`Tier1Cache`] trait methods that must evaluate freshness ([`Tier1Cache::get`],
//!   [`Tier1Cache::revalidate`], [`Tier1Cache::put`]) are given a [`SourceProvider`],
//!   mirroring [`tokens::holds`] and [`tokens::issue_token`].

use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::tier1::tokens::{self, SourceProvider};
use crate::types::{
    CanonicalJson, EvidenceValue, SourceDescriptor, Timestamp, ToolName, ValidityToken,
};

/// A stored Tier 1 cache entry: a concrete evidence value plus the token gating its
/// validity and the source that produced it.
///
/// The `tool + normalized_args` pair is the exact-match key: logically-equal tool calls
/// canonicalize to byte-equal `normalized_args` (see [`crate::tier1::normalize`]) and so
/// address the same entry. `validity_token` records *how* the value's freshness is
/// re-checked, and `source` records *which* source to re-observe when doing so (a bare
/// token does not carry that back-reference). `evidence_value` is the concrete result,
/// which Tier 1 alone owns. `stored_at` records when the entry was written.
///
/// There is at most one entry per `(tool, normalized_args)` key: a [`Tier1Cache::put`]
/// replaces any prior entry for the same key (task 4.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// The tool whose invocation this entry caches.
    pub tool: ToolName,
    /// The canonicalized arguments; together with `tool` this is the cache key.
    pub normalized_args: CanonicalJson,
    /// The volatility-aware token whose `holds` decides this entry's freshness.
    pub validity_token: ValidityToken,
    /// The concrete evidence value. Tier 1 owns concrete values; Tier 2 stores only
    /// contracts and reads through Tier 1.
    pub evidence_value: EvidenceValue,
    /// The descriptor of the source that produced the evidence, re-observed via a
    /// [`SourceProvider`] to evaluate `validity_token`'s `holds`.
    pub source: SourceDescriptor,
    /// When this entry was written.
    pub stored_at: Timestamp,
}

/// The outcome of a cache read ([`Tier1Cache::get`]).
///
/// A read never serves a value whose token does not currently hold: a present-but-expired
/// entry surfaces as [`CacheLookup::Stale`], not a stale [`CacheLookup::Hit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheLookup {
    /// An entry exists for the key **and** its token still holds. Carries the evidence
    /// value that is safe to serve.
    Hit(EvidenceValue),
    /// An entry exists for the key but its token no longer holds (or its source is
    /// unreachable, treated fail-safe as stale).
    Stale,
    /// No entry exists for the key.
    Miss,
}

/// The freshness of a specific recorded token, as reported by
/// [`Tier1Cache::revalidate`] for Tier 2 replay.
///
/// Unlike [`CacheLookup`], `Freshness` never carries an evidence value: revalidation only
/// answers "does this token still hold?" so that concrete values stay owned by Tier 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Freshness {
    /// The token still holds against the current state of its source.
    Fresh,
    /// The token no longer holds (or its source is unreachable, treated fail-safe as
    /// stale).
    Stale,
}

/// A [`Tier1Cache::put`] was rejected because its token did not hold at write time.
///
/// Requirement 12.3: a write whose token does not hold is rejected, any prior entry for the
/// key is left unchanged, and the caller receives this indication. It carries no evidence —
/// it only signals that the write did not happen — so a rejected `put` never leaks or mutates
/// a stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutRejected;

impl std::fmt::Display for PutRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cache write rejected: validity token did not hold at write time")
    }
}

impl std::error::Error for PutRejected {}

/// The owned Tier 1 result cache and its validity-token operations.
///
/// An implementation deterministically caches concrete tool-call evidence under
/// volatility-aware tokens and re-validates them. All four methods are declared here;
/// their behavior is implemented by tasks 4.2–4.4.
///
/// Freshness decisions are delegated to [`tokens::holds`], which must observe the current
/// state of a source. That observation is supplied by a [`SourceProvider`] parameter,
/// mirroring the free functions in [`crate::tier1::tokens`]. Implementations must keep
/// concrete evidence values owned by Tier 1 and must never mutate a token, a source, or
/// stored evidence while evaluating freshness.
///
/// [`tokens::holds`]: crate::tier1::tokens::holds
pub trait Tier1Cache {
    /// Read the cached evidence for `(tool, args)`.
    ///
    /// Locates the entry, evaluates its stored token via `holds` against the current state
    /// of its source (observed through `provider`), and returns:
    ///
    /// - [`CacheLookup::Hit`] with the value iff an entry exists **and** its token holds;
    /// - [`CacheLookup::Stale`] iff an entry exists but its token no longer holds, or the
    ///   source is unreachable (fail-safe);
    /// - [`CacheLookup::Miss`] iff no entry exists for the key.
    ///
    /// Never returns a value whose token does not currently hold, and never mutates stored
    /// evidence. `args` must already be canonicalized via
    /// [`normalize_args`](crate::tier1::normalize::normalize_args).
    ///
    /// Implemented by task 4.2.
    fn get(
        &self,
        tool: &ToolName,
        args: &CanonicalJson,
        provider: &impl SourceProvider,
    ) -> CacheLookup;

    /// Store `value` for `(tool, args)` under `token`, replacing any prior entry.
    ///
    /// `token` must have been issued for `source` (the source that produced `value`) and
    /// must hold at write time. On success there is exactly one entry for the key, and a
    /// subsequent [`Tier1Cache::get`] returns [`CacheLookup::Hit`] while `token` continues
    /// to hold; the previous entry (if any) is replaced (Requirements 12.1, 12.2).
    ///
    /// # Errors
    ///
    /// Returns [`PutRejected`] when `token` does not hold at write time (checked via
    /// `provider`); in that case any prior entry for the key is left unchanged and no new
    /// entry is stored (Requirement 12.3).
    ///
    /// Implemented by task 4.3.
    fn put(
        &self,
        tool: &ToolName,
        args: &CanonicalJson,
        source: &SourceDescriptor,
        token: ValidityToken,
        value: EvidenceValue,
        provider: &impl SourceProvider,
    ) -> Result<(), PutRejected>;

    /// Issue a volatility-aware token for `source` (used before [`Tier1Cache::put`]).
    ///
    /// Delegates to the Validity Token Service
    /// ([`issue_token`](crate::tier1::tokens::issue_token)); the token variant matches the
    /// source's volatility class and holds at issuance.
    ///
    /// # Errors
    ///
    /// Returns an [`IssueError`](crate::tier1::tokens::IssueError) when the source cannot
    /// be observed at issuance (e.g. an unreadable `Pinnable` source).
    fn issue_token(
        &self,
        source: &SourceDescriptor,
        provider: &impl SourceProvider,
    ) -> Result<ValidityToken, crate::tier1::tokens::IssueError>;

    /// Re-validate a specific recorded `token` for `(tool, args)`: is it still fresh?
    ///
    /// This is Tier 2 replay's re-validation entry point. It evaluates `token` via `holds`
    /// against the current state of `source` (observed through `provider`) and returns
    /// [`Freshness::Fresh`] iff the token holds, otherwise [`Freshness::Stale`]; an
    /// unreachable source is treated fail-safe as [`Freshness::Stale`]. It never returns
    /// or mutates the evidence value — concrete values stay owned by Tier 1.
    ///
    /// Implemented by task 4.4.
    fn revalidate(
        &self,
        tool: &ToolName,
        args: &CanonicalJson,
        source: &SourceDescriptor,
        token: &ValidityToken,
        provider: &impl SourceProvider,
    ) -> Freshness;
}

/// An in-memory, thread-safe [`Tier1Cache`].
///
/// Storage is a map keyed by `(tool, normalized_args)` to a single [`CacheEntry`]; a
/// [`Self::put`] replaces any prior entry for a key, so there is at most one entry per key
/// (Requirement 12.1). Because the [`Tier1Cache`] trait methods take `&self`, the map lives
/// behind a [`RwLock`] for interior mutability and thread-safe sharing: reads
/// ([`Self::get`], [`Self::revalidate`]) take a read guard, writes ([`Self::put`]) take a
/// write guard.
///
/// All freshness decisions are delegated to [`tokens::holds`], which re-observes the current
/// state of the entry's source through the supplied [`SourceProvider`]. An unreachable source
/// makes `holds` return `false`, which this cache surfaces fail-safe as
/// [`CacheLookup::Stale`] / [`Freshness::Stale`] (Requirements 11.5, 13.3). Concrete evidence
/// values are owned here and never handed out by [`Self::revalidate`]; reads clone the stored
/// value and never mutate it (Requirements 11.4, 13.1, 13.2).
#[derive(Debug, Default)]
pub struct InMemoryTier1Cache {
    entries: RwLock<HashMap<(ToolName, CanonicalJson), CacheEntry>>,
}

impl InMemoryTier1Cache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Build the map key for `(tool, args)` without borrowing issues.
    fn key(tool: &ToolName, args: &CanonicalJson) -> (ToolName, CanonicalJson) {
        (tool.clone(), args.clone())
    }
}

impl Tier1Cache for InMemoryTier1Cache {
    fn get(
        &self,
        tool: &ToolName,
        args: &CanonicalJson,
        provider: &impl SourceProvider,
    ) -> CacheLookup {
        let key = Self::key(tool, args);
        // A poisoned lock would only occur if another thread panicked mid-write; recovering
        // the guard keeps reads available and never mutates evidence.
        let guard = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match guard.get(&key) {
            None => CacheLookup::Miss,
            Some(entry) => {
                // Re-observe the entry's source; `holds` is fail-safe on an unreachable
                // source (returns false), which we surface as `Stale` (Requirement 11.5).
                if tokens::holds(&entry.validity_token, &entry.source, provider) {
                    // Clone the stored value to serve it; the stored entry is untouched
                    // (Requirement 11.4).
                    CacheLookup::Hit(entry.evidence_value.clone())
                } else {
                    // Entry present but token no longer holds: never serve the value
                    // (Requirement 11.2).
                    CacheLookup::Stale
                }
            }
        }
    }

    fn put(
        &self,
        tool: &ToolName,
        args: &CanonicalJson,
        source: &SourceDescriptor,
        token: ValidityToken,
        value: EvidenceValue,
        provider: &impl SourceProvider,
    ) -> Result<(), PutRejected> {
        // Reject writes whose token does not hold at write time, leaving the cache
        // unchanged (Requirement 12.3). The check runs before acquiring the write lock so a
        // rejected write never touches stored state.
        if !tokens::holds(&token, source, provider) {
            return Err(PutRejected);
        }

        let entry = CacheEntry {
            tool: tool.clone(),
            normalized_args: args.clone(),
            validity_token: token,
            evidence_value: value,
            source: source.clone(),
            stored_at: provider.now(),
        };

        let key = Self::key(tool, args);
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `insert` replaces any prior entry for the key, so exactly one entry remains
        // (Requirement 12.1).
        guard.insert(key, entry);
        Ok(())
    }

    fn issue_token(
        &self,
        source: &SourceDescriptor,
        provider: &impl SourceProvider,
    ) -> Result<ValidityToken, crate::tier1::tokens::IssueError> {
        tokens::issue_token(source, provider)
    }

    fn revalidate(
        &self,
        tool: &ToolName,
        args: &CanonicalJson,
        source: &SourceDescriptor,
        token: &ValidityToken,
        provider: &impl SourceProvider,
    ) -> Freshness {
        // Revalidation answers only "does this token still hold?" for the recorded token; it
        // reads no evidence and returns none (Requirements 13.1, 13.2). We do not even need
        // the stored entry: the recorded `token` and `source` are supplied by the caller
        // (Tier 2), and `holds` is fail-safe on an unreachable source (Requirement 13.3).
        let _ = (tool, args);
        if tokens::holds(token, source, provider) {
            Freshness::Fresh
        } else {
            Freshness::Stale
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier1::tokens::SourceUnreachable;
    use crate::types::{ContentRef, Duration, EventKey, EventSeq, Sha256};
    use std::cell::Cell;
    use std::collections::HashMap as StdHashMap;

    /// A deterministic in-memory provider mirroring the one used in `tokens` tests: a missing
    /// content key models an unreachable `Pinnable` source, a settable clock drives `Ttl`,
    /// and a missing subscription models an unobservable `Signalled` source.
    struct FakeProvider {
        content: StdHashMap<String, Vec<u8>>,
        events: StdHashMap<String, u64>,
        now: Cell<u64>,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                content: StdHashMap::new(),
                events: StdHashMap::new(),
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

        fn remove_content(&mut self, reference: &str) {
            self.content.remove(reference);
        }

        fn set_now(&self, now: u64) {
            self.now.set(now);
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
            self.events
                .get(&subscription.0)
                .copied()
                .map(EventSeq)
                .ok_or_else(|| SourceUnreachable::new(format!("cannot observe `{subscription}`")))
        }
    }

    fn pinnable(reference: &str) -> SourceDescriptor {
        SourceDescriptor::Pinnable {
            content: ContentRef::from(reference),
        }
    }

    fn volatile(ttl: u64) -> SourceDescriptor {
        SourceDescriptor::Volatile { ttl: Duration(ttl) }
    }

    fn tool() -> ToolName {
        ToolName::from("read_file")
    }

    fn args(canon: &str) -> CanonicalJson {
        CanonicalJson(canon.to_owned())
    }

    fn value(v: &str) -> EvidenceValue {
        EvidenceValue(CanonicalJson(v.to_owned()))
    }

    // --- get: Hit / Stale / Miss -----------------------------------------

    #[test]
    fn get_returns_miss_on_absent_key() {
        // Requirement 11.3.
        let provider = FakeProvider::new();
        let cache = InMemoryTier1Cache::new();
        assert_eq!(
            cache.get(&tool(), &args("{}"), &provider),
            CacheLookup::Miss
        );
    }

    #[test]
    fn get_returns_hit_when_token_holds() {
        // Requirement 11.1: entry exists and its token holds => Hit(value).
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");

        cache
            .put(
                &tool(),
                &args("{\"p\":1}"),
                &source,
                token,
                value("\"result\""),
                &provider,
            )
            .expect("token holds at write");

        assert_eq!(
            cache.get(&tool(), &args("{\"p\":1}"), &provider),
            CacheLookup::Hit(value("\"result\""))
        );
    }

    #[test]
    fn get_returns_stale_when_token_expired() {
        // Requirement 11.2: entry exists but token no longer holds => Stale, no value.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");

        cache
            .put(&tool(), &args("{}"), &source, token, value("\"r\""), &provider)
            .expect("write");

        // Mutate the source so the ContentHash token no longer holds.
        provider.set_content("file://a", b"v2");
        assert_eq!(cache.get(&tool(), &args("{}"), &provider), CacheLookup::Stale);
    }

    #[test]
    fn get_treats_unreachable_source_as_stale() {
        // Requirement 11.5: unreachable source => holds fails => Stale.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");
        cache
            .put(&tool(), &args("{}"), &source, token, value("\"r\""), &provider)
            .expect("write");

        provider.remove_content("file://a");
        assert_eq!(cache.get(&tool(), &args("{}"), &provider), CacheLookup::Stale);
    }

    #[test]
    fn get_does_not_mutate_stored_evidence() {
        // Requirement 11.4: repeated reads keep serving the same stored value untouched.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");
        cache
            .put(&tool(), &args("{}"), &source, token, value("\"r\""), &provider)
            .expect("write");

        let first = cache.get(&tool(), &args("{}"), &provider);
        let second = cache.get(&tool(), &args("{}"), &provider);
        assert_eq!(first, CacheLookup::Hit(value("\"r\"")));
        assert_eq!(first, second, "reads are idempotent and non-mutating");
    }

    // --- put: rejection, replacement, write-then-read --------------------

    #[test]
    fn put_rejects_non_holding_token_and_leaves_prior_entry_unchanged() {
        // Requirement 12.3.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();

        // Seed a valid prior entry.
        let good = cache.issue_token(&source, &provider).expect("issue");
        cache
            .put(&tool(), &args("{}"), &source, good, value("\"prior\""), &provider)
            .expect("prior write holds");

        // A stale token: hash of content that no longer matches the source.
        let stale_token = ValidityToken::ContentHash(Sha256::from("deadbeef"));
        let rejected = cache.put(
            &tool(),
            &args("{}"),
            &source,
            stale_token,
            value("\"new\""),
            &provider,
        );
        assert_eq!(rejected, Err(PutRejected));

        // Prior entry unchanged: still the original value.
        assert_eq!(
            cache.get(&tool(), &args("{}"), &provider),
            CacheLookup::Hit(value("\"prior\"")),
            "rejected put must not overwrite the prior entry"
        );
    }

    #[test]
    fn put_replaces_prior_entry_keeping_one_entry_per_key() {
        // Requirement 12.1: replacing leaves exactly one entry, serving the newest value.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();

        let t1 = cache.issue_token(&source, &provider).expect("issue");
        cache
            .put(&tool(), &args("{}"), &source, t1, value("\"first\""), &provider)
            .expect("write");
        let t2 = cache.issue_token(&source, &provider).expect("issue");
        cache
            .put(&tool(), &args("{}"), &source, t2, value("\"second\""), &provider)
            .expect("write");

        assert_eq!(
            cache.get(&tool(), &args("{}"), &provider),
            CacheLookup::Hit(value("\"second\"")),
            "the latest write wins"
        );
        assert_eq!(
            cache.entries.read().unwrap().len(),
            1,
            "exactly one entry per key"
        );
    }

    #[test]
    fn write_then_read_returns_hit_then_stale_after_expiry() {
        // Requirement 12.2: Hit while token holds, Stale once it stops holding.
        let provider = FakeProvider::new();
        provider.set_now(100);
        let source = volatile(50); // window [100, 150)
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");
        cache
            .put(&tool(), &args("{}"), &source, token, value("\"r\""), &provider)
            .expect("token holds at write");

        provider.set_now(149);
        assert_eq!(
            cache.get(&tool(), &args("{}"), &provider),
            CacheLookup::Hit(value("\"r\"")),
            "hit while ttl holds"
        );

        provider.set_now(150);
        assert_eq!(
            cache.get(&tool(), &args("{}"), &provider),
            CacheLookup::Stale,
            "stale once ttl expires"
        );
    }

    // --- revalidate: Fresh / Stale, no evidence --------------------------

    #[test]
    fn revalidate_returns_fresh_when_token_holds() {
        // Requirement 13.1.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");

        assert_eq!(
            cache.revalidate(&tool(), &args("{}"), &source, &token, &provider),
            Freshness::Fresh
        );
    }

    #[test]
    fn revalidate_returns_stale_when_token_does_not_hold() {
        // Requirement 13.1.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");

        provider.set_content("file://a", b"v2");
        assert_eq!(
            cache.revalidate(&tool(), &args("{}"), &source, &token, &provider),
            Freshness::Stale
        );
    }

    #[test]
    fn revalidate_treats_unreachable_source_as_stale() {
        // Requirement 13.3.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");

        provider.remove_content("file://a");
        assert_eq!(
            cache.revalidate(&tool(), &args("{}"), &source, &token, &provider),
            Freshness::Stale
        );
    }

    #[test]
    fn revalidate_does_not_require_or_return_stored_evidence() {
        // Requirement 13.2: revalidation works from the recorded token/source alone, never
        // touching stored evidence, and its result type carries no value.
        let provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let cache = InMemoryTier1Cache::new();
        let token = cache.issue_token(&source, &provider).expect("issue");

        // Never put anything: revalidate still answers Fresh for a holding token.
        let freshness = cache.revalidate(&tool(), &args("{}"), &source, &token, &provider);
        assert_eq!(freshness, Freshness::Fresh);
        assert_eq!(
            cache.entries.read().unwrap().len(),
            0,
            "revalidate does not read or create any entry"
        );
    }
}
