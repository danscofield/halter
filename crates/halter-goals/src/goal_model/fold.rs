//! The deterministic goal fold (task 6.2): the goal-tree analogue of
//! [`halter_protocol::fold::apply_event`].
//!
//! Given the projected [`GoalTree`] and a [`GoalEvent`], [`apply_goal_event`]
//! produces the next state, and [`fold_goal_events`] folds a whole sequence in
//! ascending sequence order. The projected tree is the fold of all
//! `GoalEvent`s; the append-only event log is never rewritten (Requirements
//! 2.3, 3.1).
//!
//! # Event semantics
//!
//! - [`GoalEvent::GoalNodeCreated`] inserts a fresh `Open` node (empty children,
//!   empty resolution conditions, empty tool calls, `subtree_hash` `None`) with
//!   the supplied id/parent/hypothesis/intent, and appends the new id to its
//!   parent's `children` exactly once (Requirements 1.2, 1.3).
//! - [`GoalEvent::GoalNodeRevised`] layers a delta onto the node, replacing
//!   `resolution_conditions`, `tool_calls`, and/or `children` when `Some`. A
//!   revision that changes the resolved subtree invalidates prior closure, so
//!   the node and its ancestors have their stored `subtree_hash` cleared
//!   (Requirements 2.3, 2.4). `GoalNodeRevised` is the **only** fold path that
//!   ever writes `tool_calls`: the field is an agent-curated convenience
//!   projection, never auto-populated from `goal_node`-tagged tool events on
//!   the shared log (Requirements 9.1, 9.2).
//! - [`GoalEvent::GoalNodeResolved`] sets the node's resolution. Setting it back
//!   to `Open` re-opens the node and supersedes any previously computed closure
//!   for the node and its ancestors (their `subtree_hash` is cleared); setting a
//!   non-`Open` resolution does **not** itself close the subtree — closure and
//!   `subtree_hash` stamping happen at [`GoalEvent::GoalClosed`] (Requirement
//!   2.4).
//! - [`GoalEvent::GoalClosed`] is the closure boundary and the fold point at
//!   which `subtree_hash` is recomputed over the resolved subtree: the node's
//!   `subtree_hash` is recomputed from folded state via
//!   [`subtree_hash`](super::subtree_hash::subtree_hash) and stored as `Some`.
//!
//! Unknown-node events (a revision/resolution/closure for an id absent from the
//! tree) are no-ops, mirroring how the protocol fold ignores payloads it cannot
//! apply; the [`GoalStore`](super) is responsible for rejecting such calls
//! before they are ever appended.

use crate::types::GoalNodeId;

use super::model::{GoalEvent, GoalNode, GoalTree, Resolution};
use super::subtree_hash::subtree_hash;

/// Clear the stored `subtree_hash` of `id` and every ancestor: a re-open or a
/// structural revision at `id` invalidates the closure of `id` and of every
/// node whose resolved subtree contained it (Requirements 2.4, 4.6).
fn invalidate_closure_upward(tree: &mut GoalTree, id: &GoalNodeId) {
    let mut current = Some(id.clone());
    while let Some(node_id) = current {
        let Some(node) = tree.node_mut(&node_id) else {
            break;
        };
        node.subtree_hash = None;
        current = node.parent.clone();
    }
}

/// Apply one [`GoalEvent`] to `tree`, mutating only the projected goal-tree
/// state. See the module docs for per-variant semantics.
pub fn apply_goal_event(tree: &mut GoalTree, event: &GoalEvent) {
    match event {
        GoalEvent::GoalNodeCreated {
            id,
            parent,
            hypothesis,
            intent,
        } => {
            // Insert the fresh Open node.
            tree.insert(GoalNode {
                id: id.clone(),
                parent: parent.clone(),
                children: Vec::new(),
                hypothesis: hypothesis.clone(),
                resolution_conditions: Vec::new(),
                resolution: Resolution::Open,
                intent: intent.clone(),
                tool_calls: Vec::new(),
                subtree_hash: None,
            });
            // Link into the parent's children exactly once, and — since a new
            // Open child re-opens the parent's subtree — invalidate the
            // parent's (and its ancestors') prior closure.
            if let Some(parent_id) = parent
                && let Some(parent_node) = tree.node_mut(parent_id)
            {
                if !parent_node.children.contains(id) {
                    parent_node.children.push(id.clone());
                }
                invalidate_closure_upward(tree, parent_id);
            }
        }
        GoalEvent::GoalNodeRevised { id, revision } => {
            let Some(node) = tree.node_mut(id) else {
                return;
            };
            if let Some(conditions) = &revision.resolution_conditions {
                node.resolution_conditions = conditions.clone();
            }
            if let Some(tool_calls) = &revision.tool_calls {
                // The sole write path for `tool_calls`. It is an agent-curated
                // convenience projection carried by the `revise` action, never
                // auto-populated from `goal_node`-tagged tool events on the
                // shared log (Requirements 9.1, 9.2).
                node.tool_calls = tool_calls.clone();
            }
            if let Some(children) = &revision.children {
                node.children = children.clone();
            }
            // A revision changes the node's contribution to its resolved
            // subtree, so any previously computed closure for this node and its
            // ancestors is superseded (Requirements 2.4, 2.5). The next
            // GoalClosed recomputes subtree_hash from the revised state.
            invalidate_closure_upward(tree, id);
        }
        GoalEvent::GoalNodeResolved { id, resolution } => {
            let Some(node) = tree.node_mut(id) else {
                return;
            };
            node.resolution = *resolution;
            // Re-opening a node supersedes prior closure for it and its
            // ancestors. A non-Open resolution does not itself close — closure
            // is stamped at GoalClosed — but it also does not validate a stale
            // stored hash, so clearing on any resolution change keeps the
            // stored hash honest until GoalClosed recomputes it.
            invalidate_closure_upward(tree, id);
        }
        GoalEvent::GoalClosed { id, .. } => {
            // The closure boundary: recompute subtree_hash from folded state
            // rather than trusting the value carried on the event, so replay is
            // a pure function of the projected tree (Requirement 3.1).
            let Some(node) = tree.node(id) else {
                return;
            };
            let hash = subtree_hash(node, tree);
            if let Some(node) = tree.node_mut(id) {
                node.subtree_hash = Some(hash);
            }
        }
    }
}

/// Fold a sequence of [`GoalEvent`]s onto `tree` in the supplied (ascending
/// sequence) order, returning the resulting tree. Folding the same sequence is
/// deterministic and reproduces an identical node set, hierarchy, per-node
/// resolution, and per-node `subtree_hash` on every replay (Requirement 3.1).
#[must_use]
pub fn fold_goal_events(mut tree: GoalTree, events: &[GoalEvent]) -> GoalTree {
    for event in events {
        apply_goal_event(&mut tree, event);
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CanonicalJson, IntentSignature, IntentType, OutcomeRef, Scope, TargetRef, TargetType,
        ToolCall, ToolName, ValidityToken,
    };
    use crate::goal_model::model::GoalNodeRevision;

    fn intent(target: &str) -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from(target),
            scope: Scope::from("repo"),
        }
    }

    fn tool_call(path: &str) -> ToolCall {
        ToolCall {
            tool: ToolName::from("read_file"),
            normalized_args: CanonicalJson(format!("{{\"path\":\"{path}\"}}")),
            validity_token: ValidityToken::ContentHash(crate::types::Sha256::from("abc")),
            outcome: OutcomeRef::from("outcome://1"),
        }
    }

    fn created(id: &str, parent: Option<&str>) -> GoalEvent {
        GoalEvent::GoalNodeCreated {
            id: GoalNodeId::from(id),
            parent: parent.map(GoalNodeId::from),
            hypothesis: format!("hypothesis {id}"),
            intent: intent(id),
        }
    }

    fn resolved(id: &str, resolution: Resolution) -> GoalEvent {
        GoalEvent::GoalNodeResolved {
            id: GoalNodeId::from(id),
            resolution,
        }
    }

    fn closed(id: &str) -> GoalEvent {
        GoalEvent::GoalClosed {
            id: GoalNodeId::from(id),
            // The fold recomputes from state; the carried value is ignored.
            subtree_hash: crate::types::SubtreeHash(crate::types::Sha256::from("ignored")),
        }
    }

    #[test]
    fn created_inserts_open_node_and_links_parent_once() {
        let events = vec![created("root", None), created("child", Some("root"))];
        let tree = fold_goal_events(GoalTree::new(), &events);

        let root = tree.node(&GoalNodeId::from("root")).expect("root exists");
        assert!(root.is_open());
        assert_eq!(root.children, vec![GoalNodeId::from("child")]);
        assert_eq!(root.hypothesis, "hypothesis root");
        assert!(root.subtree_hash.is_none());

        let child = tree.node(&GoalNodeId::from("child")).expect("child exists");
        assert_eq!(child.parent, Some(GoalNodeId::from("root")));
        assert!(child.children.is_empty());
        assert!(child.resolution_conditions.is_empty());
        assert!(child.tool_calls.is_empty());

        assert_eq!(tree.root(), Some(&GoalNodeId::from("root")));
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn revision_layers_delta() {
        let events = vec![
            created("n1", None),
            GoalEvent::GoalNodeRevised {
                id: GoalNodeId::from("n1"),
                revision: GoalNodeRevision {
                    resolution_conditions: Some(vec!["cond".to_owned()]),
                    tool_calls: Some(vec![tool_call("n1")]),
                    children: None,
                },
            },
        ];
        let tree = fold_goal_events(GoalTree::new(), &events);
        let node = tree.node(&GoalNodeId::from("n1")).unwrap();
        assert_eq!(node.resolution_conditions, vec!["cond".to_owned()]);
        assert_eq!(node.tool_calls.len(), 1);
    }

    #[test]
    fn goal_closed_stamps_subtree_hash() {
        let events = vec![
            created("n1", None),
            resolved("n1", Resolution::Accepted),
            closed("n1"),
        ];
        let tree = fold_goal_events(GoalTree::new(), &events);
        let node = tree.node(&GoalNodeId::from("n1")).unwrap();
        assert!(node.subtree_hash.is_some(), "GoalClosed must stamp subtree_hash");

        // The stored hash is recomputed from state, not the event's carried
        // value ("ignored").
        let expected = subtree_hash(node, &tree);
        assert_eq!(node.subtree_hash.as_ref(), Some(&expected));
    }

    #[test]
    fn reopen_clears_subtree_hash() {
        // Close a node, then re-open it; the stored subtree_hash must clear.
        let events = vec![
            created("n1", None),
            resolved("n1", Resolution::Accepted),
            closed("n1"),
        ];
        let mut tree = fold_goal_events(GoalTree::new(), &events);
        assert!(tree.node(&GoalNodeId::from("n1")).unwrap().subtree_hash.is_some());

        apply_goal_event(&mut tree, &resolved("n1", Resolution::Open));
        let node = tree.node(&GoalNodeId::from("n1")).unwrap();
        assert!(node.is_open());
        assert!(node.subtree_hash.is_none(), "re-open must supersede closure (2.4)");
    }

    #[test]
    fn reopening_descendant_clears_ancestor_closure() {
        // root -> child, both closed, then re-open child: root's closure is
        // superseded because its resolved subtree changed.
        let events = vec![
            created("root", None),
            created("child", Some("root")),
            resolved("child", Resolution::Accepted),
            resolved("root", Resolution::Accepted),
            closed("child"),
            closed("root"),
        ];
        let mut tree = fold_goal_events(GoalTree::new(), &events);
        assert!(tree.node(&GoalNodeId::from("root")).unwrap().subtree_hash.is_some());
        assert!(tree.node(&GoalNodeId::from("child")).unwrap().subtree_hash.is_some());

        apply_goal_event(&mut tree, &resolved("child", Resolution::Open));
        assert!(
            tree.node(&GoalNodeId::from("child")).unwrap().subtree_hash.is_none(),
            "re-opened child clears its own closure"
        );
        assert!(
            tree.node(&GoalNodeId::from("root")).unwrap().subtree_hash.is_none(),
            "re-opened descendant supersedes ancestor closure (4.6)"
        );
    }

    #[test]
    fn unknown_node_events_are_noops() {
        let mut tree = fold_goal_events(GoalTree::new(), &[created("root", None)]);
        let before = tree.clone();
        apply_goal_event(&mut tree, &resolved("ghost", Resolution::Accepted));
        apply_goal_event(&mut tree, &closed("ghost"));
        apply_goal_event(
            &mut tree,
            &GoalEvent::GoalNodeRevised {
                id: GoalNodeId::from("ghost"),
                revision: GoalNodeRevision {
                    resolution_conditions: Some(vec!["x".to_owned()]),
                    tool_calls: None,
                    children: None,
                },
            },
        );
        assert_eq!(tree, before, "events for absent nodes are no-ops");
    }

    #[test]
    fn fold_is_deterministic_across_replays() {
        let events = vec![
            created("root", None),
            created("a", Some("root")),
            created("b", Some("root")),
            resolved("a", Resolution::Accepted),
            resolved("b", Resolution::Rejected),
            closed("a"),
            closed("b"),
            resolved("root", Resolution::Accepted),
            closed("root"),
        ];
        let t1 = fold_goal_events(GoalTree::new(), &events);
        let t2 = fold_goal_events(GoalTree::new(), &events);
        assert_eq!(t1, t2, "folding the same sequence reproduces the same tree (3.1)");

        // Per-node subtree_hash is reproduced identically.
        for id in ["root", "a", "b"] {
            let h1 = t1.node(&GoalNodeId::from(id)).unwrap().subtree_hash.clone();
            let h2 = t2.node(&GoalNodeId::from(id)).unwrap().subtree_hash.clone();
            assert_eq!(h1, h2);
            assert!(h1.is_some(), "closed node {id} carries a subtree_hash");
        }
    }

    #[test]
    fn tool_calls_are_only_populated_via_revise_never_auto_populated() {
        // Requirements 9.1, 9.2: the `goal_node` tag on the shared log is the
        // sole attribution source of truth; `GoalNode.tool_calls` is an
        // agent-curated convenience projection that is populated ONLY by the
        // `revise` action (a GoalNodeRevised event) and is NEVER auto-populated
        // from tagged tool events. This guard exercises every non-`revise`
        // goal-tree mutation and asserts none of them ever writes `tool_calls`.
        //
        // Create -> resolve -> close leaves `tool_calls` empty, even though the
        // node has been fully closed (the point at which tagged tool events for
        // the node would exist on the shared log). Attribution rode the tag, not
        // this field.
        let events = vec![
            created("root", None),
            created("child", Some("root")),
            resolved("child", Resolution::Accepted),
            resolved("root", Resolution::Accepted),
            closed("child"),
            closed("root"),
        ];
        let tree = fold_goal_events(GoalTree::new(), &events);
        for id in ["root", "child"] {
            let node = tree.node(&GoalNodeId::from(id)).unwrap();
            assert!(
                node.tool_calls.is_empty(),
                "no non-revise event may populate tool_calls for {id} (9.1, 9.2)"
            );
        }

        // Only a GoalNodeRevised carrying `tool_calls` populates the field...
        let mut tree = tree;
        apply_goal_event(
            &mut tree,
            &GoalEvent::GoalNodeRevised {
                id: GoalNodeId::from("child"),
                revision: GoalNodeRevision {
                    resolution_conditions: None,
                    tool_calls: Some(vec![tool_call("child")]),
                    children: None,
                },
            },
        );
        assert_eq!(
            tree.node(&GoalNodeId::from("child")).unwrap().tool_calls.len(),
            1,
            "the revise action is the sole tool_calls write path (9.2)"
        );

        // ...and a revise that omits `tool_calls` (None) leaves the curated
        // value untouched — it is never cleared or auto-refreshed from events.
        apply_goal_event(
            &mut tree,
            &GoalEvent::GoalNodeRevised {
                id: GoalNodeId::from("child"),
                revision: GoalNodeRevision {
                    resolution_conditions: Some(vec!["another condition".to_owned()]),
                    tool_calls: None,
                    children: None,
                },
            },
        );
        assert_eq!(
            tree.node(&GoalNodeId::from("child")).unwrap().tool_calls.len(),
            1,
            "a revise without tool_calls must not disturb the curated projection"
        );
    }

    #[test]
    fn duplicate_created_does_not_double_link_parent() {
        // A defensive check: re-applying a create for the same child id must not
        // append the id to the parent's children twice.
        let mut tree = fold_goal_events(
            GoalTree::new(),
            &[created("root", None), created("child", Some("root"))],
        );
        apply_goal_event(&mut tree, &created("child", Some("root")));
        assert_eq!(
            tree.node(&GoalNodeId::from("root")).unwrap().children,
            vec![GoalNodeId::from("child")]
        );
    }
}
