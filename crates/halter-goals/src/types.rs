//! Shared identifier and value types used across the Goal Model, Tier 1, and
//! Tier 2 subsystems.
//!
//! These types are the common vocabulary the three subsystems speak. Identifier
//! newtypes wrap `String` (matching the `halter-protocol` `id_type!` convention),
//! and value newtypes wrap the smallest primitive that captures their meaning.
//! Every type derives `serde::Serialize`/`Deserialize` so it can ride on the
//! event-sourced session store and be persisted inside memories.

use serde::{Deserialize, Serialize};

/// Generate an opaque, UUID-backed identifier newtype over `String`.
///
/// Mirrors the `halter-protocol` `id_type!` convention: identifiers are
/// randomly generated, `Display`-able, and convertible from string types.
macro_rules! id_newtype {
    ($name:ident, $doc:expr) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[doc = $doc]
        pub struct $name(pub String);

        impl $name {
            /// Generate a new random identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

/// Generate a plain string newtype (non-generated, e.g. externally-supplied
/// names). `Display`-able and convertible from string types.
macro_rules! string_newtype {
    ($name:ident, $doc:expr) => {
        #[derive(
            Debug,
            Clone,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Default,
            Serialize,
            Deserialize,
        )]
        #[doc = $doc]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

// --- Identifiers ----------------------------------------------------------

id_newtype!(GoalNodeId, "Opaque identifier for a node in the goal tree.");
id_newtype!(MemoryId, "Opaque identifier for a Tier 2 procedural memory.");

// --- Structured value newtypes over `String` ------------------------------

string_newtype!(
    IntentType,
    "The kind of intent a goal expresses (e.g. `lookup`, `mutate`), forming one field of an `IntentSignature`."
);
string_newtype!(
    TargetType,
    "The category of thing a goal acts on (e.g. `file`, `service`), forming one field of an `IntentSignature`."
);
string_newtype!(
    TargetRef,
    "A concrete reference to the target a goal acts on, forming one field of an `IntentSignature`."
);
string_newtype!(
    Scope,
    "The bounding context in which a goal applies (e.g. repository, session), forming one field of an `IntentSignature`."
);
string_newtype!(ToolName, "The name of a tool whose calls Tier 1 caches.");

// --- Content / hash value newtypes ----------------------------------------

/// Byte-stable canonical JSON encoding of tool arguments.
///
/// Produced by Tier 1 argument normalization: logically-equal arguments for a
/// tool yield byte-equal `CanonicalJson`, so they key the same cache entry. The
/// wrapped `String` is the canonical UTF-8 byte sequence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CanonicalJson(pub String);

impl CanonicalJson {
    /// Borrow the canonical bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Display for CanonicalJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A lowercase-hex SHA-256 digest.
///
/// Used both as a subtree version key and as the content hash inside Tier 1
/// `ContentHash` validity tokens.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Sha256(pub String);

impl std::fmt::Display for Sha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Sha256 {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for Sha256 {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Stable content hash of a resolved goal subtree.
///
/// `subtree_hash(n)` is the versioning and idempotency key for induction:
/// structurally-equal resolved subtrees hash equally regardless of insertion
/// order or incidental encoding. Wraps a [`Sha256`] digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SubtreeHash(pub Sha256);

impl std::fmt::Display for SubtreeHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The concrete result of a tool call, owned and stored only by Tier 1.
///
/// Tier 2 never stores an `EvidenceValue`; it stores evidence *contracts* and
/// calls Tier 1 to fetch and re-validate the underlying value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceValue(pub CanonicalJson);

// --- Time value newtypes --------------------------------------------------

/// A UTC instant expressed as whole milliseconds since the Unix epoch.
///
/// Used for token issuance times and memory timestamps. Kept as a plain integer
/// so it is byte-stable across processes without a datetime dependency.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Timestamp(pub u64);

/// A span of time expressed in whole milliseconds.
///
/// Used for TTL windows on `Ttl` validity tokens and `max_age` bounds on
/// Mode B answer caching.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Duration(pub u64);

impl Timestamp {
    /// The instant `duration` after this timestamp (saturating on overflow).
    #[must_use]
    pub fn plus(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration.0))
    }
}

// --- Event log coordinate newtypes ----------------------------------------

/// A monotonic sequence position in an append-only event log.
///
/// Mirrors the session store's gap-free monotonic sequence; used by
/// `EventDriven` validity tokens to remember the last-seen event position.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct EventSeq(pub u64);

/// An opaque key identifying a logical event stream that an `EventDriven`
/// validity token subscribes to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct EventKey(pub String);

impl std::fmt::Display for EventKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for EventKey {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for EventKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

// --- IntentSignature ------------------------------------------------------

/// The structured retrieval and merge key attached to every goal node.
///
/// The Goal Model derives all four fields on create and revise (Requirement
/// 7.1); Tier 2 uses them as the structured filter for retrieval and as the
/// dedup/merge key for induction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IntentSignature {
    /// The kind of intent the goal expresses.
    pub intent_type: IntentType,
    /// The category of thing the goal acts on.
    pub target_type: TargetType,
    /// The concrete reference to the target.
    pub target_ref: TargetRef,
    /// The bounding context in which the goal applies.
    pub scope: Scope,
}

// --- Tier 1 sources and validity tokens -----------------------------------

/// A readable reference to the content backing a `Pinnable` source.
///
/// Tier 1 hashes the bytes this reference resolves to when issuing a
/// `ContentHash` token and re-hashes them when evaluating `holds`. It carries a
/// pointer/handle to the content rather than the content itself so large or
/// sensitive payloads are not inlined into tokens or memories.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentRef(pub String);

impl std::fmt::Display for ContentRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ContentRef {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ContentRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A shape/pointer reference to the result of a tool call.
///
/// Recorded on a [`ToolCall`] instead of the full payload: per the design's
/// no-secret-capture guidance, outcomes are referenced by shape or pointer so
/// results that may contain sensitive data are not inlined into nodes or
/// memories. Tier 1 owns the concrete [`EvidenceValue`]; this only points at it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeRef(pub String);

impl std::fmt::Display for OutcomeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for OutcomeRef {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for OutcomeRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A volatility-aware validity token owned and issued by Tier 1.
///
/// The volatility class of the source determines the variant, and each variant
/// captures exactly *how* the cached result's validity is re-checked:
///
/// - [`ValidityToken::ContentHash`] — a pinnable / change-detectable source
///   (files, pinned revisions, immutable blobs); holds iff the source's current
///   content still hashes to the recorded digest.
/// - [`ValidityToken::Ttl`] — a live / volatile source with no cheap change
///   signal; holds iff `now() < issued_at + ttl`.
/// - [`ValidityToken::EventDriven`] — a live source that emits an external
///   change signal; holds iff no event newer than `last_seen` has been observed
///   on `subscription`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ValidityToken {
    /// Pinnable / change-detectable source. Validity == content hash matches.
    ContentHash(Sha256),
    /// Live / volatile source. Validity == the token has not yet expired:
    /// `now() < issued_at + ttl`.
    Ttl {
        /// The instant the token was issued.
        issued_at: Timestamp,
        /// How long after `issued_at` the token remains valid.
        ttl: Duration,
    },
    /// Live source with an external change signal. Validity == no event newer
    /// than `last_seen` has been observed on `subscription`.
    EventDriven {
        /// The event stream this token watches for change signals.
        subscription: EventKey,
        /// The latest event position observed on `subscription` at issuance.
        last_seen: EventSeq,
    },
}

/// Describes a source in enough detail to pick a volatility class and issue the
/// matching [`ValidityToken`].
///
/// The mapping from descriptor to token is fixed: `Pinnable -> ContentHash`,
/// `Volatile -> Ttl`, `Signalled -> EventDriven`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SourceDescriptor {
    /// A pinnable / change-detectable source, yielding a `ContentHash` token.
    Pinnable {
        /// A readable reference to the source content, hashed at issuance.
        content: ContentRef,
    },
    /// A volatile source with no cheap change signal, yielding a `Ttl` token.
    Volatile {
        /// How long an issued `Ttl` token remains valid.
        ttl: Duration,
    },
    /// A source that emits an external change signal, yielding an `EventDriven`
    /// token.
    Signalled {
        /// The event stream the issued token subscribes to.
        subscription: EventKey,
    },
}

// --- ToolCall -------------------------------------------------------------

/// The reproducible record of one tool invocation.
///
/// A `ToolCall` captures a tool invocation against a stable contract so that
/// equal calls key the same Tier 1 cache entry and can be re-validated during
/// Tier 2 replay. Its `tool` and `normalized_args` form the exact-match key,
/// `validity_token` records how the result's freshness is re-checked, and
/// `outcome` points at the result by shape/pointer (Tier 1 owns the concrete
/// [`EvidenceValue`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolCall {
    /// The tool that was invoked.
    pub tool: ToolName,
    /// Arguments normalized to canonical form so equal calls hash equally.
    pub normalized_args: CanonicalJson,
    /// The Tier 1 validity token captured at call time (volatility-aware).
    pub validity_token: ValidityToken,
    /// A shape/pointer reference to the result, not necessarily the full payload.
    pub outcome: OutcomeRef,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_newtypes_are_unique_and_roundtrip() {
        let a = GoalNodeId::new();
        let b = GoalNodeId::new();
        assert_ne!(a, b, "freshly generated ids must be distinct");

        let json = serde_json::to_string(&a).expect("serialize id");
        let back: GoalNodeId = serde_json::from_str(&json).expect("deserialize id");
        assert_eq!(a, back, "id must survive a serde round trip");
    }

    #[test]
    fn string_newtypes_convert_from_str_and_string() {
        let from_str = ToolName::from("read_file");
        let from_string = ToolName::from(String::from("read_file"));
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.to_string(), "read_file");
    }

    #[test]
    fn timestamp_add_duration_saturates() {
        let base = Timestamp(10);
        assert_eq!(base.plus(Duration(5)), Timestamp(15));
        assert_eq!(Timestamp(u64::MAX).plus(Duration(1)), Timestamp(u64::MAX));
    }

    #[test]
    fn intent_signature_roundtrips_all_four_fields() {
        let sig = IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        };
        let json = serde_json::to_string(&sig).expect("serialize signature");
        let back: IntentSignature = serde_json::from_str(&json).expect("deserialize signature");
        assert_eq!(sig, back);
    }

    #[test]
    fn subtree_hash_wraps_sha256_and_displays() {
        let h = SubtreeHash(Sha256::from("abc123"));
        assert_eq!(h.to_string(), "abc123");
        let json = serde_json::to_string(&h).expect("serialize hash");
        let back: SubtreeHash = serde_json::from_str(&json).expect("deserialize hash");
        assert_eq!(h, back);
    }

    #[test]
    fn canonical_json_exposes_bytes() {
        let c = CanonicalJson("{\"a\":1}".to_owned());
        assert_eq!(c.as_bytes(), b"{\"a\":1}");
    }

    #[test]
    fn validity_token_variants_roundtrip() {
        let tokens = vec![
            ValidityToken::ContentHash(Sha256::from("deadbeef")),
            ValidityToken::Ttl {
                issued_at: Timestamp(100),
                ttl: Duration(50),
            },
            ValidityToken::EventDriven {
                subscription: EventKey::from("fs-changes"),
                last_seen: EventSeq(7),
            },
        ];
        for token in tokens {
            let json = serde_json::to_string(&token).expect("serialize token");
            let back: ValidityToken = serde_json::from_str(&json).expect("deserialize token");
            assert_eq!(token, back, "validity token must survive a serde round trip");
        }
    }

    #[test]
    fn source_descriptor_variants_roundtrip() {
        let descriptors = vec![
            SourceDescriptor::Pinnable {
                content: ContentRef::from("file://src/lib.rs"),
            },
            SourceDescriptor::Volatile {
                ttl: Duration(1_000),
            },
            SourceDescriptor::Signalled {
                subscription: EventKey::from("clock-tick"),
            },
        ];
        for descriptor in descriptors {
            let json = serde_json::to_string(&descriptor).expect("serialize descriptor");
            let back: SourceDescriptor =
                serde_json::from_str(&json).expect("deserialize descriptor");
            assert_eq!(descriptor, back);
        }
    }

    #[test]
    fn tool_call_roundtrips_all_fields() {
        let call = ToolCall {
            tool: ToolName::from("read_file"),
            normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
            validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
            outcome: OutcomeRef::from("outcome://1"),
        };
        let json = serde_json::to_string(&call).expect("serialize tool call");
        let back: ToolCall = serde_json::from_str(&json).expect("deserialize tool call");
        assert_eq!(call, back);
    }
}
