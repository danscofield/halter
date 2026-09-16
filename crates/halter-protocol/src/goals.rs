//! Goal Model wire types shared across the workspace.
//!
//! These identifier, value, and event types were relocated here from
//! `halter-goals` so the protocol crate can reference them — in particular the
//! [`GoalNodeId`] attribution tag and the richer [`GoalEvent`] carried by
//! `SessionEventPayload::Goal` — without depending on `halter-goals` (which
//! already depends on this crate; the reverse edge would be a cycle).
//!
//! `halter-goals` re-exports every type defined here, so its
//! `goal_model`/`tier1`/`tier2`/`integration` modules and external callers keep
//! their existing `halter_goals::{...}` and `crate::types::{...}` paths.
//!
//! Identifier newtypes wrap `String` (matching the [`crate`] `id_type!`
//! convention), and value newtypes wrap the smallest primitive that captures
//! their meaning. Every type derives `serde::Serialize`/`Deserialize` so it can
//! ride on the event-sourced session store, and `schemars::JsonSchema` so the
//! `SessionEventPayload` schema (which now embeds [`GoalEvent`]) stays derivable.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Generate an opaque, UUID-backed identifier newtype over `String`.
///
/// Mirrors the [`crate`] `id_type!` convention: identifiers are randomly
/// generated, `Display`-able, and convertible from string types.
macro_rules! goal_id_newtype {
    ($name:ident, $doc:expr) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
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
macro_rules! goal_string_newtype {
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
            JsonSchema,
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

goal_id_newtype!(GoalNodeId, "Opaque identifier for a node in the goal tree.");
goal_id_newtype!(MemoryId, "Opaque identifier for a Tier 2 procedural memory.");

// --- Structured value newtypes over `String` ------------------------------

goal_string_newtype!(
    IntentType,
    "The kind of intent a goal expresses (e.g. `lookup`, `mutate`), forming one field of an `IntentSignature`."
);
goal_string_newtype!(
    TargetType,
    "The category of thing a goal acts on (e.g. `file`, `service`), forming one field of an `IntentSignature`."
);
goal_string_newtype!(
    TargetRef,
    "A concrete reference to the target a goal acts on, forming one field of an `IntentSignature`."
);
goal_string_newtype!(
    Scope,
    "The bounding context in which a goal applies (e.g. repository, session), forming one field of an `IntentSignature`."
);
goal_string_newtype!(GoalToolName, "The name of a tool whose calls Tier 1 caches.");

// --- Content / hash value newtypes ----------------------------------------

/// Byte-stable canonical JSON encoding of tool arguments.
///
/// Produced by Tier 1 argument normalization: logically-equal arguments for a
/// tool yield byte-equal `CanonicalJson`, so they key the same cache entry. The
/// wrapped `String` is the canonical UTF-8 byte sequence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceValue(pub CanonicalJson);

// --- Time value newtypes --------------------------------------------------

/// A UTC instant expressed as whole milliseconds since the Unix epoch.
///
/// Used for token issuance times and memory timestamps. Kept as a plain integer
/// so it is byte-stable across processes without a datetime dependency.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
    JsonSchema,
)]
pub struct GoalTimestamp(pub u64);

/// A span of time expressed in whole milliseconds.
///
/// Used for TTL windows on `Ttl` validity tokens and `max_age` bounds on
/// Mode B answer caching.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
    JsonSchema,
)]
pub struct GoalDuration(pub u64);

impl GoalTimestamp {
    /// The instant `duration` after this timestamp (saturating on overflow).
    #[must_use]
    pub fn plus(self, duration: GoalDuration) -> Self {
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
    JsonSchema,
)]
pub struct EventSeq(pub u64);

/// An opaque key identifying a logical event stream that an `EventDriven`
/// validity token subscribes to.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema,
)]
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
/// The Goal Model derives all four fields on create and revise; Tier 2 uses
/// them as the structured filter for retrieval and as the dedup/merge key for
/// induction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
/// Recorded on a [`GoalToolCall`] instead of the full payload: per the design's
/// no-secret-capture guidance, outcomes are referenced by shape or pointer so
/// results that may contain sensitive data are not inlined into nodes or
/// memories. Tier 1 owns the concrete [`EvidenceValue`]; this only points at it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ValidityToken {
    /// Pinnable / change-detectable source. Validity == content hash matches.
    ContentHash(Sha256),
    /// Live / volatile source. Validity == the token has not yet expired:
    /// `now() < issued_at + ttl`.
    Ttl {
        /// The instant the token was issued.
        issued_at: GoalTimestamp,
        /// How long after `issued_at` the token remains valid.
        ttl: GoalDuration,
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub enum SourceDescriptor {
    /// A pinnable / change-detectable source, yielding a `ContentHash` token.
    Pinnable {
        /// A readable reference to the source content, hashed at issuance.
        content: ContentRef,
    },
    /// A volatile source with no cheap change signal, yielding a `Ttl` token.
    Volatile {
        /// How long an issued `Ttl` token remains valid.
        ttl: GoalDuration,
    },
    /// A source that emits an external change signal, yielding an `EventDriven`
    /// token.
    Signalled {
        /// The event stream the issued token subscribes to.
        subscription: EventKey,
    },
}

// --- GoalToolCall ---------------------------------------------------------

/// The reproducible record of one tool invocation.
///
/// A `GoalToolCall` captures a tool invocation against a stable contract so that
/// equal calls key the same Tier 1 cache entry and can be re-validated during
/// Tier 2 replay. Its `tool` and `normalized_args` form the exact-match key,
/// `validity_token` records how the result's freshness is re-checked, and
/// `outcome` points at the result by shape/pointer (Tier 1 owns the concrete
/// [`EvidenceValue`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
pub struct GoalToolCall {
    /// The tool that was invoked.
    pub tool: GoalToolName,
    /// Arguments normalized to canonical form so equal calls hash equally.
    pub normalized_args: CanonicalJson,
    /// The Tier 1 validity token captured at call time (volatility-aware).
    pub validity_token: ValidityToken,
    /// A shape/pointer reference to the result, not necessarily the full payload.
    pub outcome: OutcomeRef,
}

// --- Resolution / GoalNode / GoalNodeRevision / GoalEvent / GoalTree ------

/// The resolution state of a goal node.
///
/// A node is *closed* when its `resolution` is non-[`Open`](Resolution::Open)
/// **and** every node in its subtree is also closed; **goal closure** is the
/// transition from not-closed to closed and is the single trigger for
/// induction. Because nodes are retroactively revisable, a closed node may
/// re-open (a revision sets it or a descendant back to `Open`) and later close
/// again under a new `subtree_hash`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
    JsonSchema,
)]
pub enum Resolution {
    /// Still being worked; not eligible for closure.
    #[default]
    Open,
    /// Hypothesis confirmed by its resolution conditions.
    Accepted,
    /// Hypothesis disconfirmed — a determinate dead-end.
    Rejected,
    /// Conditions evaluated but yielded neither accept nor reject.
    Inconclusive,
}

impl Resolution {
    /// Whether this resolution denotes a node that is still open.
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, Resolution::Open)
    }
}

/// A hypothesis-shaped unit of goal work in the goal tree.
///
/// A `GoalNode` is the projected (folded) view of a node: its identity and
/// place in the hierarchy (`id`, `parent`, `children`), the hypothesis it
/// tests, the conditions under which it resolves, its current [`Resolution`],
/// the structured [`IntentSignature`] used by Tier 2 retrieval/merge, the
/// reproducible [`GoalToolCall`] contracts gathered while working it, and —
/// once its resolved subtree has been closed — its [`SubtreeHash`].
///
/// `children` is an ordered [`Vec`] rather than a set: the design describes a
/// "children set" but `subtree_hash` combines children in a *canonical* child
/// order, so the ordered vector preserves insertion order while the canonical
/// ordering used for hashing is defined by the goal model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalNode {
    /// This node's unique identifier within the session's goal tree.
    pub id: GoalNodeId,
    /// The parent node, or `None` for the top-level goal.
    pub parent: Option<GoalNodeId>,
    /// Child node ids in insertion order (canonical ordering for hashing is
    /// defined by the goal model).
    pub children: Vec<GoalNodeId>,
    /// The hypothesis this node tests. Required (non-empty) at creation.
    pub hypothesis: String,
    /// The conditions under which the hypothesis is considered resolved.
    pub resolution_conditions: Vec<String>,
    /// The node's current resolution state.
    pub resolution: Resolution,
    /// The structured retrieval/merge key derived for this node.
    pub intent: IntentSignature,
    /// The reproducible tool-call contracts the agent chose to record on this
    /// node.
    ///
    /// This field is an **optional, agent-curated convenience projection**, not
    /// the attribution mechanism. Under runtime goal tracking, the source of
    /// truth for which artifacts (messages, tool calls, reasoning, usage) belong
    /// to which node is the `goal_node` tag stamped on the shared event log —
    /// **not** this field. `tool_calls` is populated **only** via the goal
    /// tool's `revise` action (a [`GoalEvent::GoalNodeRevised`] carrying
    /// `tool_calls`); it is **never auto-populated from tagged tool events**.
    /// Auto-populating it would conflate the semantic contract that Tier 2
    /// consumes with the raw incidental transcript. It nonetheless remains part
    /// of a node's semantic contract for `subtree_hash` (see the goal model's
    /// `subtree_hash`), so agent-recorded contracts version the node while
    /// incidental tagged events do not.
    pub tool_calls: Vec<GoalToolCall>,
    /// The stable content hash of this node's resolved subtree, present only
    /// once the subtree has been closed (`None` while open).
    pub subtree_hash: Option<SubtreeHash>,
}

impl GoalNode {
    /// Whether this node's own resolution is still open (ignores descendants).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.resolution.is_open()
    }
}

/// A retroactive-revision delta carried by [`GoalEvent::GoalNodeRevised`].
///
/// Each field is optional: `Some` replaces the corresponding field on the
/// target node when the fold applies the revision, and `None` leaves it
/// unchanged. Revision is expressed as a *new appended event* rather than a
/// rewrite of prior events, preserving append-only event-log semantics while
/// letting the projected tree change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalNodeRevision {
    /// If present, replaces the node's `resolution_conditions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_conditions: Option<Vec<String>>,
    /// If present, replaces the node's `tool_calls`. This is the **only** way
    /// `tool_calls` is populated: it is an agent-curated convenience projection
    /// recorded through the goal tool's `revise` action, never auto-populated
    /// from tagged tool events on the shared log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<GoalToolCall>>,
    /// If present, replaces the node's ordered `children`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<GoalNodeId>>,
}

impl GoalNodeRevision {
    /// Whether this revision carries no changes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.resolution_conditions.is_none()
            && self.tool_calls.is_none()
            && self.children.is_none()
    }
}

/// An append-only goal-tree mutation recorded on the session event log.
///
/// Each variant is appended as a session-event payload and applied by the goal
/// fold, mirroring how transcript events mutate session state today. The log is
/// never rewritten; the projected [`GoalTree`] is the fold of all `GoalEvent`s
/// in sequence order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum GoalEvent {
    /// A node was created (`Open`) under an optional parent.
    GoalNodeCreated {
        /// The id assigned to the new node.
        id: GoalNodeId,
        /// The parent node, or `None` for the top-level goal.
        parent: Option<GoalNodeId>,
        /// The hypothesis the new node tests.
        hypothesis: String,
        /// The structured intent signature derived for the new node.
        intent: IntentSignature,
    },
    /// A node was revised retroactively (fields and/or child set changed).
    GoalNodeRevised {
        /// The node being revised.
        id: GoalNodeId,
        /// The delta to layer on top of the node's prior state.
        revision: GoalNodeRevision,
    },
    /// A node reached a resolution (or was re-opened by a later revision).
    GoalNodeResolved {
        /// The node whose resolution changed.
        id: GoalNodeId,
        /// The node's new resolution state.
        resolution: Resolution,
    },
    /// A node's subtree became fully closed under this `subtree_hash`.
    ///
    /// Records the closure boundary the closure signal fires on; the fold
    /// recomputes `subtree_hash` from folded state.
    GoalClosed {
        /// The node whose subtree closed.
        id: GoalNodeId,
        /// The stable content hash of the resolved subtree at closure.
        subtree_hash: SubtreeHash,
    },
}

/// The projected goal-tree state produced by folding a session's
/// [`GoalEvent`]s.
///
/// State is a map from [`GoalNodeId`] to its projected [`GoalNode`] plus a
/// pointer to the root (the top-level goal). It is *foldable state*: the
/// [`Default`] value is an empty tree with no root, and the goal fold mutates
/// it event by event.
///
/// A [`BTreeMap`] backs the node map so iteration order over nodes is
/// deterministic (ordered by id), which keeps replay reproducible independent
/// of insertion order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GoalTree {
    /// All projected nodes keyed by id.
    nodes: BTreeMap<GoalNodeId, GoalNode>,
    /// The root (top-level goal) node id, if the tree has one.
    root: Option<GoalNodeId>,
}

/// Alias for the projected goal-tree state, matching the design's
/// `GoalTreeState` naming used by the fold.
pub type GoalTreeState = GoalTree;

impl GoalTree {
    /// Create an empty goal tree with no nodes and no root.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a node by id.
    #[must_use]
    pub fn node(&self, id: &GoalNodeId) -> Option<&GoalNode> {
        self.nodes.get(id)
    }

    /// Mutable access to a node by id (used by the fold).
    #[must_use]
    pub fn node_mut(&mut self, id: &GoalNodeId) -> Option<&mut GoalNode> {
        self.nodes.get_mut(id)
    }

    /// The root (top-level goal) node id, if any.
    #[must_use]
    pub fn root(&self) -> Option<&GoalNodeId> {
        self.root.as_ref()
    }

    /// Whether the tree contains no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The number of nodes in the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether a node with the given id exists in the tree.
    #[must_use]
    pub fn contains(&self, id: &GoalNodeId) -> bool {
        self.nodes.contains_key(id)
    }

    /// Iterate over all `(id, node)` pairs in deterministic id order.
    pub fn iter(&self) -> impl Iterator<Item = (&GoalNodeId, &GoalNode)> {
        self.nodes.iter()
    }

    /// The root node ids: an explicit root when set, otherwise every node whose
    /// `parent` is `None`. Returned in deterministic id order.
    #[must_use]
    pub fn roots(&self) -> Vec<&GoalNodeId> {
        if let Some(root) = self.root.as_ref() {
            return vec![root];
        }
        self.nodes
            .iter()
            .filter(|(_, node)| node.parent.is_none())
            .map(|(id, _)| id)
            .collect()
    }

    /// The children of a node in projected (insertion) order, or an empty slice
    /// when the node is absent.
    #[must_use]
    pub fn children(&self, id: &GoalNodeId) -> &[GoalNodeId] {
        self.nodes
            .get(id)
            .map_or(&[][..], |node| node.children.as_slice())
    }

    /// Insert a projected node, returning any node it replaced. Intended for
    /// use by the fold; sets the root when inserting the first parentless node.
    pub fn insert(&mut self, node: GoalNode) -> Option<GoalNode> {
        if node.parent.is_none() && self.root.is_none() {
            self.root = Some(node.id.clone());
        }
        self.nodes.insert(node.id.clone(), node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_intent() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn sample_tool_call() -> GoalToolCall {
        GoalToolCall {
            tool: GoalToolName::from("read_file"),
            normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
            validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
            outcome: OutcomeRef::from("outcome://1"),
        }
    }

    fn sample_node(id: &str, parent: Option<&str>) -> GoalNode {
        GoalNode {
            id: GoalNodeId::from(id),
            parent: parent.map(GoalNodeId::from),
            children: Vec::new(),
            hypothesis: "the file exists".to_owned(),
            resolution_conditions: vec!["file readable".to_owned()],
            resolution: Resolution::Open,
            intent: sample_intent(),
            tool_calls: vec![sample_tool_call()],
            subtree_hash: None,
        }
    }

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
    fn goal_event_variants_roundtrip() {
        let events = vec![
            GoalEvent::GoalNodeCreated {
                id: GoalNodeId::from("n1"),
                parent: None,
                hypothesis: "root goal".to_owned(),
                intent: sample_intent(),
            },
            GoalEvent::GoalNodeRevised {
                id: GoalNodeId::from("n1"),
                revision: GoalNodeRevision {
                    resolution_conditions: Some(vec!["cond".to_owned()]),
                    tool_calls: None,
                    children: None,
                },
            },
            GoalEvent::GoalNodeResolved {
                id: GoalNodeId::from("n1"),
                resolution: Resolution::Rejected,
            },
            GoalEvent::GoalClosed {
                id: GoalNodeId::from("n1"),
                subtree_hash: SubtreeHash(Sha256::from("cafef00d")),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("serialize event");
            let back: GoalEvent = serde_json::from_str(&json).expect("deserialize event");
            assert_eq!(event, back, "goal event must survive a serde round trip");
        }
    }

    #[test]
    fn goal_tree_insert_sets_root_and_exposes_accessors() {
        let mut tree = GoalTree::new();
        let mut root = sample_node("root", None);
        root.children.push(GoalNodeId::from("child"));
        let child = sample_node("child", Some("root"));

        assert!(tree.insert(root).is_none());
        assert!(tree.insert(child).is_none());

        assert_eq!(tree.len(), 2);
        assert!(!tree.is_empty());
        assert_eq!(tree.root(), Some(&GoalNodeId::from("root")));
        assert!(tree.contains(&GoalNodeId::from("child")));
        assert_eq!(tree.children(&GoalNodeId::from("root")), &[GoalNodeId::from("child")]);
        assert_eq!(tree.roots(), vec![&GoalNodeId::from("root")]);
    }
}
