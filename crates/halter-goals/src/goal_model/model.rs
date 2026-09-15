//! The Goal Model data model: [`GoalNode`], the [`Resolution`] lifecycle, the
//! projected [`GoalTree`]/[`GoalTreeState`], the retroactive-revision delta
//! [`GoalNodeRevision`], and the append-only [`GoalEvent`] payloads.
//!
//! This task only *defines* these types (with docs and `serde` derives). The
//! deterministic fold that applies [`GoalEvent`]s into a [`GoalTree`] is task
//! 6.2, and `subtree_hash` computation is task 6.4; both are layered on top of
//! these definitions.
//!
//! Every type derives `serde::Serialize`/`Deserialize` so [`GoalEvent`]s can
//! ride on the event-sourced session store and the projected [`GoalNode`]s can
//! be persisted and replayed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{GoalNodeId, IntentSignature, SubtreeHash, ToolCall};

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
/// reproducible [`ToolCall`] contracts gathered while working it, and — once
/// its resolved subtree has been closed — its [`SubtreeHash`].
///
/// `children` is an ordered [`Vec`] rather than a set: the design describes a
/// "children set" but `subtree_hash` combines children in a *canonical* child
/// order, so the ordered vector preserves insertion order while a later task
/// (6.4) defines the canonical ordering used for hashing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalNode {
    /// This node's unique identifier within the session's goal tree.
    pub id: GoalNodeId,
    /// The parent node, or `None` for the top-level goal.
    pub parent: Option<GoalNodeId>,
    /// Child node ids in insertion order (canonical ordering for hashing is
    /// defined by a later task).
    pub children: Vec<GoalNodeId>,
    /// The hypothesis this node tests. Required (non-empty) at creation.
    pub hypothesis: String,
    /// The conditions under which the hypothesis is considered resolved.
    pub resolution_conditions: Vec<String>,
    /// The node's current resolution state.
    pub resolution: Resolution,
    /// The structured retrieval/merge key derived for this node.
    pub intent: IntentSignature,
    /// The reproducible tool-call contracts gathered while working this node.
    pub tool_calls: Vec<ToolCall>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalNodeRevision {
    /// If present, replaces the node's `resolution_conditions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_conditions: Option<Vec<String>>,
    /// If present, replaces the node's `tool_calls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
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
/// fold (task 6.2), mirroring how transcript events mutate session state today.
/// The log is never rewritten; the projected [`GoalTree`] is the fold of all
/// `GoalEvent`s in sequence order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
/// [`Default`] value is an empty tree with no root, and the fold (task 6.2)
/// mutates it event by event.
///
/// A [`BTreeMap`] backs the node map so iteration order over nodes is
/// deterministic (ordered by id), which keeps replay reproducible independent
/// of insertion order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    use crate::types::{
        CanonicalJson, IntentType, OutcomeRef, Scope, Sha256, TargetRef, TargetType, ToolName,
        ValidityToken,
    };

    fn sample_intent() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn sample_tool_call() -> ToolCall {
        ToolCall {
            tool: ToolName::from("read_file"),
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
    fn resolution_roundtrips_and_reports_openness() {
        for res in [
            Resolution::Open,
            Resolution::Accepted,
            Resolution::Rejected,
            Resolution::Inconclusive,
        ] {
            let json = serde_json::to_string(&res).expect("serialize resolution");
            let back: Resolution = serde_json::from_str(&json).expect("deserialize resolution");
            assert_eq!(res, back);
        }
        assert!(Resolution::Open.is_open());
        assert!(!Resolution::Accepted.is_open());
        assert_eq!(Resolution::default(), Resolution::Open);
    }

    #[test]
    fn goal_node_roundtrips_all_fields() {
        let mut node = sample_node("n1", None);
        node.children.push(GoalNodeId::from("n2"));
        node.resolution = Resolution::Accepted;
        node.subtree_hash = Some(SubtreeHash(Sha256::from("deadbeef")));

        let json = serde_json::to_string(&node).expect("serialize node");
        let back: GoalNode = serde_json::from_str(&json).expect("deserialize node");
        assert_eq!(node, back);
        assert!(!back.is_open());
    }

    #[test]
    fn goal_node_revision_roundtrips_and_detects_empty() {
        let empty = GoalNodeRevision::default();
        assert!(empty.is_empty());
        let json = serde_json::to_string(&empty).expect("serialize empty revision");
        // Optional fields are skipped when None.
        assert_eq!(json, "{}");

        let revision = GoalNodeRevision {
            resolution_conditions: Some(vec!["revised".to_owned()]),
            tool_calls: Some(vec![sample_tool_call()]),
            children: Some(vec![GoalNodeId::from("child")]),
        };
        assert!(!revision.is_empty());
        let json = serde_json::to_string(&revision).expect("serialize revision");
        let back: GoalNodeRevision = serde_json::from_str(&json).expect("deserialize revision");
        assert_eq!(revision, back);
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
    fn goal_tree_default_is_empty() {
        let tree = GoalTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert!(tree.root().is_none());
        assert!(tree.roots().is_empty());
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
        assert_eq!(tree.node(&GoalNodeId::from("root")).unwrap().hypothesis, "the file exists");
        assert_eq!(tree.children(&GoalNodeId::from("root")), &[GoalNodeId::from("child")]);
        assert_eq!(tree.children(&GoalNodeId::from("missing")), &[] as &[GoalNodeId]);
        assert_eq!(tree.roots(), vec![&GoalNodeId::from("root")]);
    }

    #[test]
    fn goal_tree_roots_falls_back_to_parentless_nodes() {
        let mut tree = GoalTree::new();
        // Insert a node with a parent first so no explicit root gets set.
        tree.insert(sample_node("child", Some("ghost")));
        assert!(tree.root().is_none());
        // A later parentless node is discoverable via roots().
        tree.insert(sample_node("orphan", None));
        assert_eq!(tree.roots(), vec![&GoalNodeId::from("orphan")]);
    }

    #[test]
    fn goal_tree_roundtrips() {
        let mut tree = GoalTree::new();
        tree.insert(sample_node("root", None));
        tree.insert(sample_node("child", Some("root")));
        let json = serde_json::to_string(&tree).expect("serialize tree");
        let back: GoalTree = serde_json::from_str(&json).expect("deserialize tree");
        assert_eq!(tree, back);
    }
}
