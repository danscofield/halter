//! Tier 1 — the Validity Token Service (issuance + re-validation).
//!
//! The Validity Token Service mints volatility-aware [`ValidityToken`]s for a
//! source and later re-validates them. The volatility class of the source picks
//! the token variant, and each variant captures exactly *how* a cached result's
//! validity is re-checked:
//!
//! - `Pinnable` -> [`ValidityToken::ContentHash`]: holds iff the source's current
//!   content still hashes to the recorded digest (Requirements 9.1, 10.1).
//! - `Volatile` -> [`ValidityToken::Ttl`]: holds iff `now() < issued_at + ttl`
//!   (Requirements 9.2, 10.2).
//! - `Signalled` -> [`ValidityToken::EventDriven`]: holds iff no event newer than
//!   `last_seen` has been observed on the subscription (Requirements 9.3, 10.3).
//!
//! ## Source access abstraction
//!
//! Issuance and re-validation both need to observe the *current* state of a
//! source: read a `Pinnable` source's bytes, read the wall clock for a `Ttl`
//! token, and read the latest `EventSeq` on a `Signalled` source's subscription.
//! That observation is factored behind the [`SourceProvider`] trait so the
//! service is deterministic and testable, and so that an unreachable source can
//! be modelled explicitly. Any provider read may fail with a
//! [`SourceUnreachable`] error; when a read needed by [`holds`] fails, the token
//! is treated as **not held** (fail-safe, Requirements 10.5), and when the read
//! needed to issue a `ContentHash` token fails, issuance is rejected
//! (Requirement 9.6).
//!
//! Because a bare [`ValidityToken`] does not itself carry a back-reference to its
//! source (a `ContentHash` records only the digest, a `Ttl` records only the
//! window), [`holds`] is given the original [`SourceDescriptor`] alongside the
//! token so it knows *which* source to re-observe. Callers that stored a token
//! keep the descriptor next to it. Issuance and re-validation never mutate the
//! token, the source, or any stored evidence (Requirement 10.4).

use crate::types::{
    ContentRef, Duration, EventKey, EventSeq, SourceDescriptor, Timestamp, ValidityToken,
};

use sha2::{Digest, Sha256 as Sha256Hasher};

/// The source referenced by a token could not be observed at check time.
///
/// A provider returns this when it cannot read a `Pinnable` source's content or
/// cannot observe the latest `EventSeq` on a subscription. It is pure data; the
/// service converts it into a rejected issuance (Requirement 9.6) or a
/// not-held token (Requirements 10.5) as appropriate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUnreachable {
    /// A human-readable reason the source could not be observed.
    pub reason: String,
}

impl SourceUnreachable {
    /// Construct an unreachable error with the given `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for SourceUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "source unreachable: {}", self.reason)
    }
}

impl std::error::Error for SourceUnreachable {}

/// Error returned when a token cannot be issued for a source.
///
/// The only failure mode at issuance is an unreadable `Pinnable` source
/// (Requirement 9.6); `Volatile` and `Signalled` issuance succeed as long as the
/// provider can report the current time / latest `EventSeq`, which are total for
/// the trait's contract (`now` cannot fail; a failing `latest_event_seq` also
/// surfaces here for `Signalled`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    /// A `Pinnable` source's content was not readable at issuance time, so no
    /// `ContentHash` token could be minted.
    UnreadableContent {
        /// The content reference that could not be read.
        content: ContentRef,
        /// Why the read failed.
        reason: String,
    },
    /// A `Signalled` source's subscription could not be observed at issuance
    /// time, so no `EventDriven` token could be minted.
    UnobservableSubscription {
        /// The subscription that could not be observed.
        subscription: EventKey,
        /// Why the observation failed.
        reason: String,
    },
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnreadableContent { content, reason } => {
                write!(f, "content `{content}` is unreadable at issuance: {reason}")
            }
            Self::UnobservableSubscription {
                subscription,
                reason,
            } => write!(
                f,
                "subscription `{subscription}` is unobservable at issuance: {reason}"
            ),
        }
    }
}

impl std::error::Error for IssueError {}

/// Observes the *current* state of sources for the Validity Token Service.
///
/// This is the seam that makes issuance and re-validation deterministic and
/// testable and that lets an unreachable source be represented explicitly. All
/// three methods are read-only: implementations must never mutate the source,
/// the token, or any stored evidence (Requirement 10.4).
pub trait SourceProvider {
    /// Read the current bytes backing a `Pinnable` source.
    ///
    /// # Errors
    ///
    /// Returns [`SourceUnreachable`] when the content cannot be read. At issuance
    /// this rejects the token (Requirement 9.6); during [`holds`] it makes the
    /// token not hold (Requirement 10.5).
    fn read_content(&self, content: &ContentRef) -> Result<Vec<u8>, SourceUnreachable>;

    /// The current wall-clock instant, used to evaluate `Ttl` tokens.
    ///
    /// Time is always available, so this is total (it cannot report the clock as
    /// unreachable).
    fn now(&self) -> Timestamp;

    /// Observe the latest `EventSeq` seen on `subscription`.
    ///
    /// # Errors
    ///
    /// Returns [`SourceUnreachable`] when the subscription cannot be observed. At
    /// issuance this rejects a `Signalled` token; during [`holds`] it makes an
    /// `EventDriven` token not hold (Requirement 10.5).
    fn latest_event_seq(&self, subscription: &EventKey) -> Result<EventSeq, SourceUnreachable>;
}

/// Compute the lowercase-hex SHA-256 digest of `bytes` as a [`crate::types::Sha256`].
fn hash_content(bytes: &[u8]) -> crate::types::Sha256 {
    let mut hasher = Sha256Hasher::new();
    hasher.update(bytes);
    crate::types::Sha256(format!("{:x}", hasher.finalize()))
}

/// Issue a volatility-aware [`ValidityToken`] for `descriptor`.
///
/// The token variant matches the source's volatility class
/// (`Pinnable -> ContentHash`, `Volatile -> Ttl`, `Signalled -> EventDriven`),
/// per Requirements 9.1–9.4. A freshly issued token always holds at issuance
/// time (Requirement 9.5): a `ContentHash` is minted from the content just read,
/// a `Ttl` starts its window at `now()`, and an `EventDriven` records the latest
/// observed `EventSeq` so that nothing newer than `last_seen` exists yet.
///
/// # Errors
///
/// Returns [`IssueError::UnreadableContent`] when a `Pinnable` source's content
/// is not readable at issuance (Requirement 9.6) and
/// [`IssueError::UnobservableSubscription`] when a `Signalled` source's
/// subscription cannot be observed. No token is returned in either case.
pub fn issue_token(
    descriptor: &SourceDescriptor,
    provider: &impl SourceProvider,
) -> Result<ValidityToken, IssueError> {
    match descriptor {
        SourceDescriptor::Pinnable { content } => {
            let bytes = provider.read_content(content).map_err(|err| {
                IssueError::UnreadableContent {
                    content: content.clone(),
                    reason: err.reason,
                }
            })?;
            Ok(ValidityToken::ContentHash(hash_content(&bytes)))
        }
        SourceDescriptor::Volatile { ttl } => Ok(ValidityToken::Ttl {
            issued_at: provider.now(),
            ttl: *ttl,
        }),
        SourceDescriptor::Signalled { subscription } => {
            let last_seen = provider.latest_event_seq(subscription).map_err(|err| {
                IssueError::UnobservableSubscription {
                    subscription: subscription.clone(),
                    reason: err.reason,
                }
            })?;
            Ok(ValidityToken::EventDriven {
                subscription: subscription.clone(),
                last_seen,
            })
        }
    }
}

/// Re-validate `token` against the current state of its `source`.
///
/// Returns `true` iff the token still holds per its variant rule:
///
/// - `ContentHash(h)` holds iff the source's current content hashes to `h`
///   (Requirement 10.1).
/// - `Ttl { issued_at, ttl }` holds iff `now() < issued_at + ttl`, strictly
///   (Requirement 10.2).
/// - `EventDriven { subscription, last_seen }` holds iff no event with an
///   `EventSeq` strictly greater than `last_seen` has been observed on the
///   subscription (Requirement 10.3).
///
/// If the source cannot be observed (the provider reports it unreachable), the
/// token is treated as **not held** and this returns `false` (Requirement 10.5).
/// `holds` is read-only: it never mutates the token, the source, or any stored
/// evidence (Requirement 10.4).
///
/// The `source` descriptor is supplied because a bare token does not carry a
/// back-reference to the source it was issued for: a `ContentHash` records only
/// the digest and a `Ttl` records only its window. `holds` re-derives what it
/// needs to observe from the descriptor. If the descriptor's volatility class
/// does not match the token variant (a caller error), the token is treated as
/// not held.
#[must_use]
pub fn holds(
    token: &ValidityToken,
    source: &SourceDescriptor,
    provider: &impl SourceProvider,
) -> bool {
    match (token, source) {
        (ValidityToken::ContentHash(expected), SourceDescriptor::Pinnable { content }) => {
            match provider.read_content(content) {
                Ok(bytes) => &hash_content(&bytes) == expected,
                // Unreachable source => not held (fail-safe).
                Err(_) => false,
            }
        }
        (ValidityToken::Ttl { issued_at, ttl }, SourceDescriptor::Volatile { .. }) => {
            ttl_holds(*issued_at, *ttl, provider.now())
        }
        (
            ValidityToken::EventDriven {
                subscription,
                last_seen,
            },
            SourceDescriptor::Signalled {
                subscription: descriptor_subscription,
            },
        ) => {
            // Observe the subscription the token watches. Guard against a
            // descriptor pointing at a different stream than the token.
            if subscription != descriptor_subscription {
                return false;
            }
            match provider.latest_event_seq(subscription) {
                Ok(current) => current <= *last_seen,
                // Unreachable subscription => not held (fail-safe).
                Err(_) => false,
            }
        }
        // Token/descriptor volatility-class mismatch: treat as not held.
        _ => false,
    }
}

/// `Ttl` validity: `now < issued_at + ttl`, strictly, saturating on overflow.
fn ttl_holds(issued_at: Timestamp, ttl: Duration, now: Timestamp) -> bool {
    now < issued_at.plus(ttl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Sha256;
    use std::cell::Cell;
    use std::collections::HashMap;

    /// A deterministic in-memory provider for exercising issuance and `holds`.
    ///
    /// Pinnable content is keyed by `ContentRef`; a missing key models an
    /// unreachable / unreadable source. Subscriptions are keyed by `EventKey`;
    /// a missing key models an unobservable subscription. `now` is a settable
    /// clock.
    struct FakeProvider {
        content: HashMap<String, Vec<u8>>,
        events: HashMap<String, u64>,
        now: Cell<u64>,
    }

    impl FakeProvider {
        fn new() -> Self {
            Self {
                content: HashMap::new(),
                events: HashMap::new(),
                now: Cell::new(0),
            }
        }

        fn with_content(mut self, reference: &str, bytes: &[u8]) -> Self {
            self.content.insert(reference.to_owned(), bytes.to_vec());
            self
        }

        fn with_event_seq(mut self, subscription: &str, seq: u64) -> Self {
            self.events.insert(subscription.to_owned(), seq);
            self
        }

        fn set_content(&mut self, reference: &str, bytes: &[u8]) {
            self.content.insert(reference.to_owned(), bytes.to_vec());
        }

        fn remove_content(&mut self, reference: &str) {
            self.content.remove(reference);
        }

        fn set_event_seq(&mut self, subscription: &str, seq: u64) {
            self.events.insert(subscription.to_owned(), seq);
        }

        fn remove_subscription(&mut self, subscription: &str) {
            self.events.remove(subscription);
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
                .ok_or_else(|| {
                    SourceUnreachable::new(format!("cannot observe `{subscription}`"))
                })
        }
    }

    fn pinnable(reference: &str) -> SourceDescriptor {
        SourceDescriptor::Pinnable {
            content: ContentRef::from(reference),
        }
    }

    fn volatile(ttl: u64) -> SourceDescriptor {
        SourceDescriptor::Volatile {
            ttl: Duration(ttl),
        }
    }

    fn signalled(subscription: &str) -> SourceDescriptor {
        SourceDescriptor::Signalled {
            subscription: EventKey::from(subscription),
        }
    }

    // --- issue_token ------------------------------------------------------

    #[test]
    fn pinnable_issuance_hashes_content_and_holds_at_issuance() {
        // Requirements 9.1, 9.4, 9.5.
        let provider = FakeProvider::new().with_content("file://a", b"hello");
        let source = pinnable("file://a");

        let token = issue_token(&source, &provider).expect("readable pinnable issues");

        // The token is a ContentHash of the exact SHA-256 of the content.
        let mut hasher = Sha256Hasher::new();
        hasher.update(b"hello");
        let expected = Sha256(format!("{:x}", hasher.finalize()));
        assert_eq!(token, ValidityToken::ContentHash(expected));

        // Freshly issued token holds at issuance.
        assert!(holds(&token, &source, &provider));
    }

    #[test]
    fn unreadable_pinnable_issuance_rejects() {
        // Requirement 9.6: no content at the reference => rejected, no token.
        let provider = FakeProvider::new();
        let source = pinnable("file://missing");

        let result = issue_token(&source, &provider);
        assert!(matches!(
            result,
            Err(IssueError::UnreadableContent { .. })
        ));
    }

    #[test]
    fn volatile_issuance_stamps_window_and_holds_at_issuance() {
        // Requirements 9.2, 9.4, 9.5.
        let provider = FakeProvider::new();
        provider.set_now(1_000);
        let source = volatile(500);

        let token = issue_token(&source, &provider).expect("volatile always issues");
        assert_eq!(
            token,
            ValidityToken::Ttl {
                issued_at: Timestamp(1_000),
                ttl: Duration(500),
            }
        );
        assert!(holds(&token, &source, &provider));
    }

    #[test]
    fn signalled_issuance_records_last_seen_and_holds_at_issuance() {
        // Requirements 9.3, 9.4, 9.5.
        let provider = FakeProvider::new().with_event_seq("fs", 7);
        let source = signalled("fs");

        let token = issue_token(&source, &provider).expect("observable subscription issues");
        assert_eq!(
            token,
            ValidityToken::EventDriven {
                subscription: EventKey::from("fs"),
                last_seen: EventSeq(7),
            }
        );
        assert!(holds(&token, &source, &provider));
    }

    // --- holds: ContentHash ----------------------------------------------

    #[test]
    fn content_hash_holds_iff_content_unchanged() {
        // Requirement 10.1.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let token = issue_token(&source, &provider).expect("issue");

        assert!(holds(&token, &source, &provider), "unchanged content holds");

        provider.set_content("file://a", b"v2");
        assert!(
            !holds(&token, &source, &provider),
            "changed content does not hold"
        );
    }

    #[test]
    fn content_hash_unreachable_source_does_not_hold() {
        // Requirement 10.5: unreachable => not held.
        let mut provider = FakeProvider::new().with_content("file://a", b"v1");
        let source = pinnable("file://a");
        let token = issue_token(&source, &provider).expect("issue");

        provider.remove_content("file://a");
        assert!(!holds(&token, &source, &provider));
    }

    // --- holds: Ttl -------------------------------------------------------

    #[test]
    fn ttl_holds_within_window_and_not_after() {
        // Requirement 10.2: strictly `now < issued_at + ttl`.
        let provider = FakeProvider::new();
        provider.set_now(100);
        let source = volatile(50);
        let token = issue_token(&source, &provider).expect("issue"); // window [100, 150)

        provider.set_now(149);
        assert!(holds(&token, &source, &provider), "just inside window holds");

        provider.set_now(150);
        assert!(
            !holds(&token, &source, &provider),
            "at expiry the window is closed (strict)"
        );

        provider.set_now(1_000);
        assert!(!holds(&token, &source, &provider), "well past expiry");
    }

    // --- holds: EventDriven ----------------------------------------------

    #[test]
    fn event_driven_holds_until_newer_event() {
        // Requirement 10.3.
        let mut provider = FakeProvider::new().with_event_seq("fs", 5);
        let source = signalled("fs");
        let token = issue_token(&source, &provider).expect("issue"); // last_seen = 5

        assert!(holds(&token, &source, &provider), "no newer event holds");

        // Same seq still holds (nothing strictly newer).
        provider.set_event_seq("fs", 5);
        assert!(holds(&token, &source, &provider));

        // A strictly newer event invalidates.
        provider.set_event_seq("fs", 6);
        assert!(!holds(&token, &source, &provider), "newer event => not held");
    }

    #[test]
    fn event_driven_unreachable_subscription_does_not_hold() {
        // Requirement 10.5.
        let mut provider = FakeProvider::new().with_event_seq("fs", 5);
        let source = signalled("fs");
        let token = issue_token(&source, &provider).expect("issue");

        provider.remove_subscription("fs");
        assert!(!holds(&token, &source, &provider));
    }

    // --- holds: purity ----------------------------------------------------

    #[test]
    fn holds_does_not_mutate_token() {
        // Requirement 10.4: token unchanged across a holds evaluation.
        let provider = FakeProvider::new().with_content("file://a", b"data");
        let source = pinnable("file://a");
        let token = issue_token(&source, &provider).expect("issue");
        let before = token.clone();

        let _ = holds(&token, &source, &provider);
        assert_eq!(token, before, "holds must not mutate the token");
    }

    #[test]
    fn class_mismatch_does_not_hold() {
        // A token evaluated against a mismatched descriptor is fail-safe.
        let provider = FakeProvider::new().with_content("file://a", b"data");
        let content_token = ValidityToken::ContentHash(Sha256::from("deadbeef"));
        assert!(!holds(&content_token, &volatile(10), &provider));
    }
}
