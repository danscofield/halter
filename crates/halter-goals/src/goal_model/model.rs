//! The Goal Model data model: [`GoalNode`], the [`Resolution`] lifecycle, the
//! projected [`GoalTree`]/[`GoalTreeState`], the retroactive-revision delta
//! [`GoalNodeRevision`], and the append-only [`GoalEvent`] payloads.
//!
//! These types were relocated into `halter-protocol` (module
//! [`halter_protocol::goals`]) so the protocol crate can carry the `GoalEvent`
//! payload on `SessionEventPayload::Goal` without a circular dependency on
//! `halter-goals`. This module re-exports them under their historical
//! `halter-goals` names, so the fold (`fold`), `subtree_hash` computation
//! (`subtree_hash`), the store (`store`), and external callers keep their
//! existing paths and vocabulary unchanged.
//!
//! Every re-exported type derives `serde::Serialize`/`Deserialize` so
//! [`GoalEvent`]s can ride on the event-sourced session store and the projected
//! [`GoalNode`]s can be persisted and replayed.

pub use halter_protocol::goals::{
    GoalEvent, GoalNode, GoalNodeRevision, GoalTree, GoalTreeState, Resolution,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CanonicalJson, GoalNodeId, IntentSignature, IntentType, OutcomeRef, Scope, Sha256,
        SubtreeHash, TargetRef, TargetType, ToolCall, ToolName, ValidityToken,
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
