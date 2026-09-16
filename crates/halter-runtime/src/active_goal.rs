//! The active-goal pointer: session-owned runtime bookkeeping tracking the
//! current [`GoalNodeId`] as a stack whose bottom (index 0) is the always-present
//! per-session root and whose top is the active node.
//!
//! # Placement
//!
//! [`ActiveGoalStack`] is **runtime bookkeeping**, not a fold-covered
//! `SessionState` domain field. Like `pending_tool_calls` and `fired_hook_ids`,
//! it is carried on the session checkpoint (it derives `serde`) so it survives a
//! commit and can be rehydrated on resume (a later task folds the session's goal
//! events into a `GoalTree` and reconstructs the root→active path). It never
//! participates in `halter_protocol::fold::covered_state_matches`, so tagging or
//! advancing the active node never perturbs the log/checkpoint conformance
//! invariant.
//!
//! # Semantics (design § "Component 1: Active-goal pointer")
//!
//! - **Structure — a stack, not a single pointer.** The bottom is the
//!   always-present root; the top is the active node. A stack models nested
//!   subgoal work directly: [`push`](ActiveGoalStack::push) opens a subgoal, and
//!   a resolve that closes a node pops back to its parent
//!   ([`resolve_to_parent`](ActiveGoalStack::resolve_to_parent)).
//! - **Root is lazy and total.** [`ensure_root`](ActiveGoalStack::ensure_root)
//!   creates the root `GoalNode` via [`GoalStore`] on the first tagged event or
//!   first goal-tool call, whichever comes first. After that
//!   [`active`](ActiveGoalStack::active) is always `Some`.
//! - **Root is never popped.** Resolving the root (or any pop request that would
//!   empty the stack) leaves the root in place; a resolved root is a valid
//!   terminal state whose events still attribute to the root (Requirements 4.4,
//!   6.3).
//! - **Turn boundaries — the stack persists across turns.** The active node does
//!   not reset at turn start (Requirement 6.4); it is checkpoint state.
//! - **Focus rebuilds the path.** [`focus`](ActiveGoalStack::focus) moves the
//!   active node to an existing node and rebuilds the stack as the root→node path
//!   (Requirement 6.6).

use halter_goals::{GoalNodeId, GoalStore, GoalStoreError, GoalTree, IntentSignature};
use halter_protocol::{SessionEventPayload, SessionId};
use serde::{Deserialize, Serialize};

/// The hypothesis recorded on the lazily created per-session root goal.
///
/// The root is the always-present total-attribution fallback; its hypothesis is
/// a fixed, non-empty placeholder (the [`GoalStore`] rejects a blank hypothesis).
const ROOT_HYPOTHESIS: &str = "session root";

/// Session-owned runtime bookkeeping tracking the active goal node as a stack.
///
/// The bottom (index 0) is the always-present per-session root once initialized;
/// the top is the active node — the node any event pushed right now is
/// attributed to. See the module docs for the full semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveGoalStack {
    /// The root→active path. `stack[0]` is the root once initialized; the last
    /// element is the active node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stack: Vec<GoalNodeId>,
    /// The lazily created per-session root. `None` until the first attribution or
    /// first goal-tool use, at which point the root `GoalNode` is created and
    /// pushed as `stack[0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<GoalNodeId>,
}

/// The failure returned by [`ActiveGoalStack::focus`] when the target node
/// cannot anchor a root→node path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusError {
    /// The target node is not present in the folded goal tree, so no
    /// root→node path can be reconstructed. The active node is left unchanged.
    NodeNotFound(GoalNodeId),
}

impl std::fmt::Display for FocusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FocusError::NodeNotFound(id) => {
                write!(f, "cannot focus goal node not found in tree: {id}")
            }
        }
    }
}

impl std::error::Error for FocusError {}

impl ActiveGoalStack {
    /// Create an empty stack with no root and no active node.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The active node id — the node any event pushed right now is attributed
    /// to. `None` only before the root exists (before the first tagged event or
    /// first goal-tool call). Once [`ensure_root`](Self::ensure_root) succeeds,
    /// this is always `Some` for the remainder of the session (Requirement 4.2).
    #[must_use]
    pub fn active(&self) -> Option<&GoalNodeId> {
        self.stack.last()
    }

    /// The root node id, if the root has been created.
    #[must_use]
    pub fn root(&self) -> Option<&GoalNodeId> {
        self.root.as_ref()
    }

    /// Whether the root has been created (and thus the stack is non-empty).
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.root.is_some()
    }

    /// Ensure the root exists, creating it via [`GoalStore`] if needed, and
    /// return it (Requirement 4.1).
    ///
    /// On the first call this creates the per-session root `GoalNode` (a
    /// top-level node with no parent) and pushes it as `stack[0]`; subsequent
    /// calls are cheap and return the existing root without touching the store.
    ///
    /// # Errors
    ///
    /// Propagates [`GoalStoreError`] from [`GoalStore::create`]. On error the
    /// stack is left unchanged (no root is recorded), so the caller can degrade
    /// attribution gracefully to `goal_node = None` for that event.
    pub async fn ensure_root(
        &mut self,
        store: &dyn GoalStore,
        session: &SessionId,
    ) -> Result<GoalNodeId, GoalStoreError> {
        if let Some(root) = &self.root {
            return Ok(root.clone());
        }

        let root = store
            .create(
                session,
                None,
                ROOT_HYPOTHESIS.to_owned(),
                root_intent(),
            )
            .await?;

        self.root = Some(root.clone());
        self.stack = vec![root.clone()];
        Ok(root)
    }

    /// Push a freshly created subgoal, making it the active node (Requirement
    /// 6.1). The caller is responsible for having created the node via
    /// [`GoalStore`] first; this only updates the active-goal bookkeeping.
    pub fn push(&mut self, node: GoalNodeId) {
        self.stack.push(node);
    }

    /// Pop the resolved node and any of its still-active descendants back to its
    /// parent, so the active node becomes the resolved node's parent — or the
    /// root when the resolved node was a direct child of the root (Requirement
    /// 6.2).
    ///
    /// The stack is precisely the root→active path, so every element above
    /// `resolved` on the stack is one of its still-active descendants; truncating
    /// at `resolved` removes it together with those descendants.
    ///
    /// The root is never popped: resolving the root (or a node not on the stack)
    /// leaves the stack unchanged, and the root stays active (Requirements 4.4,
    /// 6.3).
    pub fn resolve_to_parent(&mut self, resolved: &GoalNodeId) {
        // Never pop the root (index 0); a resolved root stays active.
        let Some(position) = self.stack.iter().position(|id| id == resolved) else {
            // Not on the current path — nothing to pop.
            return;
        };
        if position == 0 {
            // The resolved node is the root; leave it in place.
            return;
        }
        // Truncate to drop `resolved` and every still-active descendant above it,
        // leaving its parent (at `position - 1`) as the active node.
        self.stack.truncate(position);
    }

    /// Rehydrate the stack from a session's folded goal tree on resume so
    /// attribution resumes at a real, previously-persisted node rather than a
    /// freshly-created duplicate root (Requirement 6.5).
    ///
    /// The stack is derivable from the committed log: folding the session's
    /// `Goal` events yields a [`GoalTree`] whose root is the session's persisted
    /// root goal. This reconstructs the stack as the root→active path.
    ///
    /// # Reconstruction of the active node
    ///
    /// The active pointer — the open frontier node the agent last focused — is
    /// not itself a persisted field (there is no lightweight active-pointer
    /// checkpoint on `SessionState`, and the runtime/tool stacks are transient
    /// per-handle bookkeeping). The design permits the defensible reconstruction
    /// this method implements: **set the active node to the tree's root**. This
    /// keeps attribution total and coherent — a resumed session resumes at the
    /// same persisted root every prior turn attributed to when the agent never
    /// focused a subgoal (the common case), and never orphans events onto a new
    /// duplicate root. When a lightweight persisted active-pointer is added in a
    /// later pass, [`focus`](Self::focus) can be layered on top of this
    /// reconstruction to resume at a deeper node.
    ///
    /// Returns `true` when the tree had a root and the stack was initialized to
    /// it, or `false` when the tree is empty (a session that never created a
    /// goal), leaving the stack empty so the lazy `ensure_root` path still
    /// creates the root on first use exactly as for a fresh session.
    pub fn rehydrate(&mut self, tree: &GoalTree) -> bool {
        let Some(root) = tree.root().cloned() else {
            // Empty tree (no goal events yet): leave the stack empty so the lazy
            // `ensure_root` path creates the root on first use.
            return false;
        };
        self.root = Some(root.clone());
        self.stack = vec![root];
        true
    }

    /// Refocus the active node onto an existing node, rebuilding the stack as the
    /// root→node path (Requirement 6.6).
    ///
    /// This is the one place the agent may set the active node to a node not on
    /// the current stack path (e.g. re-entering a sibling); the stack is rebuilt
    /// from the folded `tree` by walking `node`'s parent chain to the root.
    ///
    /// # Errors
    ///
    /// Returns [`FocusError::NodeNotFound`] when `node` is absent from `tree`,
    /// leaving the active node unchanged (Requirement 10.3).
    pub fn focus(&mut self, node: &GoalNodeId, tree: &GoalTree) -> Result<(), FocusError> {
        if !tree.contains(node) {
            return Err(FocusError::NodeNotFound(node.clone()));
        }

        // Walk the parent chain from `node` up to a parentless (root) node,
        // collecting the path, then reverse it into root→node order.
        let mut path = Vec::new();
        let mut cursor = Some(node.clone());
        while let Some(id) = cursor {
            let parent = tree.node(&id).and_then(|n| n.parent.clone());
            path.push(id);
            cursor = parent;
        }
        path.reverse();

        self.stack = path;
        Ok(())
    }
}

/// Whether the runtime stamps the active goal node onto turn-attributable
/// events at the chokepoint.
///
/// This mirrors `halter_config::GoalTrackingMode` but lives in `halter-runtime`
/// so the chokepoint need not depend on `halter-config` for the flag. Task 9.1
/// (`HalterBuilder`/`RuntimeServices` wiring) is responsible for translating the
/// resolved config value into this runtime flag; task 6.1 only defines the seam
/// and the stamping mechanism it gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GoalAttributionMode {
    /// Goal tracking is off: the chokepoint performs no stack read and no
    /// goal-store call, and payloads are emitted with `goal_node = None` exactly
    /// as today. The default (Requirement 1.4, 2.4).
    #[default]
    Off,
    /// Goal tracking is on: the chokepoint stamps `goal_node = active()` onto
    /// every turn-attributable payload (Requirement 1.5, 3.1).
    Auto,
}

impl GoalAttributionMode {
    /// Whether stamping is active for this mode.
    #[must_use]
    pub fn is_auto(self) -> bool {
        matches!(self, GoalAttributionMode::Auto)
    }
}

/// Stamp `goal_node = active` onto a turn-attributable payload.
///
/// This is the single, pure mechanism the chokepoint uses to attribute an event
/// to the active goal node (design § "Component 2: Attribution at the
/// chokepoint"). It is called only when goal tracking is `auto`; in `off` mode
/// the caller skips it entirely, so the payload keeps `goal_node = None` exactly
/// as today (Requirement 2.1, 2.4).
///
/// # Behavior
///
/// - **Turn-attributable payloads** (`MessageItem`, `ToolExecutionStarted`,
///   `ToolExecutionCompleted`, `ToolOutput`, `TurnCompleted`, `Warning`,
///   `DeltaItem`, `ContextProjectionUpdated`) have their `goal_node` set to
///   `active.cloned()` (Requirements 3.1, 3.2).
/// - **Non-attributable lifecycle/aggregate markers** (`SessionStarted`,
///   `SessionResumed`, `TurnStarted`, `Lagged`, `SessionShutdownComplete`,
///   `Goal`, `HookStarted`/`HookCompleted`, `ApprovalRequested`,
///   `ProviderMetadata`, `ContextRestored`/`ContextCompacted`/
///   `ContextWindowRolledOver`, `TurnFailed`) are never stamped (Requirement
///   3.3); the match arm below leaves them untouched.
///
/// Passing `active = None` (e.g. before the root exists) leaves every payload's
/// `goal_node = None`, which is the graceful-degradation shape task 6.2 relies
/// on.
pub fn stamp_goal_node(payload: &mut SessionEventPayload, active: Option<&GoalNodeId>) {
    let node = active.cloned();
    match payload {
        SessionEventPayload::MessageItem { goal_node, .. }
        | SessionEventPayload::ToolExecutionStarted { goal_node, .. }
        | SessionEventPayload::ToolExecutionCompleted { goal_node, .. }
        | SessionEventPayload::ToolOutput { goal_node, .. }
        | SessionEventPayload::TurnCompleted { goal_node, .. }
        | SessionEventPayload::Warning { goal_node, .. }
        | SessionEventPayload::DeltaItem { goal_node, .. }
        | SessionEventPayload::ContextProjectionUpdated { goal_node, .. } => {
            *goal_node = node;
        }
        // Non-attributable lifecycle/aggregate markers are never stamped.
        SessionEventPayload::SessionStarted
        | SessionEventPayload::SessionResumed
        | SessionEventPayload::TurnStarted { .. }
        | SessionEventPayload::ContextRestored { .. }
        | SessionEventPayload::ProviderMetadata { .. }
        | SessionEventPayload::HookStarted { .. }
        | SessionEventPayload::HookCompleted { .. }
        | SessionEventPayload::ApprovalRequested { .. }
        | SessionEventPayload::ContextCompacted { .. }
        | SessionEventPayload::ContextWindowRolledOver { .. }
        | SessionEventPayload::TurnFailed { .. }
        | SessionEventPayload::Lagged { .. }
        | SessionEventPayload::SessionShutdownComplete
        | SessionEventPayload::Goal { .. } => {}
    }
}

/// The [`IntentSignature`] recorded on the lazily created per-session root goal.
///
/// The root is a structural fallback rather than a derived hypothesis, so its
/// intent is a fixed, generic signature; Tier 2 retrieval keys on richer child
/// nodes, not the root.
fn root_intent() -> IntentSignature {
    IntentSignature {
        intent_type: "session".into(),
        target_type: "session".into(),
        target_ref: "root".into(),
        scope: "session".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halter_goals::goal_model::GoalEvent;
    use halter_goals::goal_model::fold::fold_goal_events;

    fn node(id: &str) -> GoalNodeId {
        GoalNodeId::from(id)
    }

    /// Build a `GoalTree` from a parent→child chain of created nodes, in order.
    fn tree_from_chain(ids: &[(&str, Option<&str>)]) -> GoalTree {
        let events: Vec<GoalEvent> = ids
            .iter()
            .map(|(id, parent)| GoalEvent::GoalNodeCreated {
                id: node(id),
                parent: parent.map(node),
                hypothesis: format!("h-{id}"),
                intent: root_intent(),
            })
            .collect();
        fold_goal_events(GoalTree::new(), &events)
    }

    #[test]
    fn empty_stack_has_no_active_node() {
        let stack = ActiveGoalStack::new();
        assert_eq!(stack.active(), None);
        assert!(!stack.is_initialized());
        assert_eq!(stack.root(), None);
    }

    #[test]
    fn push_makes_the_pushed_node_active() {
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));
        stack.push(node("child"));
        assert_eq!(stack.active(), Some(&node("child")));
    }

    #[test]
    fn resolve_to_parent_pops_back_to_parent() {
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));
        stack.push(node("a"));
        stack.push(node("b"));

        stack.resolve_to_parent(&node("b"));
        assert_eq!(stack.active(), Some(&node("a")));
    }

    #[test]
    fn resolve_to_parent_pops_still_active_descendants() {
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));
        stack.push(node("a"));
        stack.push(node("b"));
        stack.push(node("c"));

        // Resolving `a` pops `a` and its still-active descendants `b`, `c`,
        // returning to `root`.
        stack.resolve_to_parent(&node("a"));
        assert_eq!(stack.active(), Some(&node("root")));
    }

    #[test]
    fn resolve_direct_child_of_root_returns_to_root() {
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));
        stack.push(node("a"));

        stack.resolve_to_parent(&node("a"));
        assert_eq!(stack.active(), Some(&node("root")));
    }

    #[test]
    fn resolving_the_root_leaves_root_active() {
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));

        stack.resolve_to_parent(&node("root"));
        assert_eq!(stack.active(), Some(&node("root")));
        assert_eq!(stack.stack.len(), 1);
    }

    #[test]
    fn resolving_a_node_not_on_the_stack_is_a_noop() {
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));
        stack.push(node("a"));

        stack.resolve_to_parent(&node("missing"));
        assert_eq!(stack.active(), Some(&node("a")));
    }

    #[test]
    fn focus_rebuilds_root_to_node_path() {
        let tree = tree_from_chain(&[
            ("root", None),
            ("a", Some("root")),
            ("b", Some("a")),
        ]);
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));

        stack.focus(&node("b"), &tree).expect("focus known node");
        assert_eq!(stack.active(), Some(&node("b")));
        assert_eq!(
            stack.stack,
            vec![node("root"), node("a"), node("b")],
            "stack must be the root→node path"
        );
    }

    #[test]
    fn rehydrate_sets_stack_to_the_folded_tree_root() {
        // Task 9.2 / Requirement 6.5: on resume, folding the session's goal
        // events yields a tree whose root anchors the reconstructed stack.
        let tree = tree_from_chain(&[
            ("root", None),
            ("a", Some("root")),
            ("b", Some("a")),
        ]);
        let mut stack = ActiveGoalStack::new();

        assert!(stack.rehydrate(&tree), "rehydrate finds the persisted root");
        assert!(stack.is_initialized());
        assert_eq!(stack.root(), Some(&node("root")));
        // Root-fallback reconstruction: attribution resumes at the persisted
        // root (the previously-active node pointer is not separately persisted).
        assert_eq!(stack.active(), Some(&node("root")));
    }

    #[test]
    fn rehydrate_empty_tree_leaves_stack_uninitialized() {
        // A session that never created a goal folds to an empty tree; rehydrate
        // leaves the stack empty so the lazy `ensure_root` path still creates the
        // root on first use exactly as for a fresh session.
        let tree = GoalTree::new();
        let mut stack = ActiveGoalStack::new();

        assert!(!stack.rehydrate(&tree));
        assert!(!stack.is_initialized());
        assert_eq!(stack.active(), None);
    }

    #[test]
    fn focus_unknown_node_errors_and_leaves_active_unchanged() {
        let tree = tree_from_chain(&[("root", None)]);
        let mut stack = ActiveGoalStack::new();
        stack.push(node("root"));

        let err = stack.focus(&node("ghost"), &tree).expect_err("unknown node");
        assert_eq!(err, FocusError::NodeNotFound(node("ghost")));
        assert_eq!(stack.active(), Some(&node("root")));
    }

    // --- Chokepoint stamping (task 6.1) -----------------------------------

    use halter_protocol::{
        Message, ToolCallId, ToolName, TurnId, Usage, UserMessage,
    };

    /// Read the `goal_node` tag off a turn-attributable payload for assertions.
    fn goal_node_of(payload: &SessionEventPayload) -> Option<&GoalNodeId> {
        match payload {
            SessionEventPayload::MessageItem { goal_node, .. }
            | SessionEventPayload::ToolExecutionStarted { goal_node, .. }
            | SessionEventPayload::ToolExecutionCompleted { goal_node, .. }
            | SessionEventPayload::ToolOutput { goal_node, .. }
            | SessionEventPayload::TurnCompleted { goal_node, .. }
            | SessionEventPayload::Warning { goal_node, .. }
            | SessionEventPayload::DeltaItem { goal_node, .. }
            | SessionEventPayload::ContextProjectionUpdated { goal_node, .. } => goal_node.as_ref(),
            _ => None,
        }
    }

    /// A representative subset of turn-attributable variants, each starting with
    /// `goal_node = None`. These are the easy-to-construct ones; together they
    /// exercise the stamping match arm for the shared field.
    fn turn_attributable_payloads() -> Vec<SessionEventPayload> {
        vec![
            SessionEventPayload::MessageItem {
                message: Message::User(UserMessage::text("hi")),
                goal_node: None,
            },
            SessionEventPayload::ToolOutput {
                call_id: ToolCallId::from("call-1"),
                tool_name: ToolName::from("tool"),
                chunk: "chunk".into(),
                goal_node: None,
            },
            SessionEventPayload::TurnCompleted {
                turn_id: TurnId::from("turn-1"),
                usage: Usage::default(),
                goal_node: None,
            },
            SessionEventPayload::Warning {
                message: "warn".to_owned(),
                goal_node: None,
            },
            SessionEventPayload::ContextProjectionUpdated {
                request_tokens: 0,
                goal_node: None,
            },
        ]
    }

    /// Representative non-attributable lifecycle/aggregate markers.
    fn non_attributable_payloads() -> Vec<SessionEventPayload> {
        vec![
            SessionEventPayload::SessionStarted,
            SessionEventPayload::SessionResumed,
            SessionEventPayload::TurnStarted {
                turn_id: TurnId::from("turn-1"),
            },
            SessionEventPayload::Lagged { dropped_events: 1 },
            SessionEventPayload::SessionShutdownComplete,
        ]
    }

    #[test]
    fn stamp_sets_active_on_turn_attributable_payloads() {
        let active = node("goal-42");
        for mut payload in turn_attributable_payloads() {
            stamp_goal_node(&mut payload, Some(&active));
            assert_eq!(
                goal_node_of(&payload),
                Some(&active),
                "turn-attributable payload must carry the active node: {payload:?}"
            );
        }
    }

    #[test]
    fn stamp_never_tags_non_attributable_markers() {
        let active = node("goal-42");
        for payload in non_attributable_payloads() {
            let before = payload.clone();
            let mut after = payload;
            stamp_goal_node(&mut after, Some(&active));
            assert_eq!(
                after, before,
                "non-attributable marker must be left untouched"
            );
        }
    }

    #[test]
    fn stamp_with_no_active_node_is_a_noop() {
        // Before the root exists (or in off mode via `active = None`) stamping
        // leaves `goal_node = None` on every payload — today's shape.
        for payload in turn_attributable_payloads() {
            let before = payload.clone();
            let mut after = payload;
            stamp_goal_node(&mut after, None);
            assert_eq!(after, before, "None active must leave goal_node = None");
            assert_eq!(goal_node_of(&after), None);
        }
    }

    #[test]
    fn goal_payload_is_never_stamped() {
        use halter_goals::goal_model::GoalEvent;
        let active = node("goal-42");
        let mut payload = SessionEventPayload::Goal {
            event: GoalEvent::GoalNodeCreated {
                id: node("child"),
                parent: Some(node("root")),
                hypothesis: "h".to_owned(),
                intent: root_intent(),
            },
        };
        let before = payload.clone();
        stamp_goal_node(&mut payload, Some(&active));
        assert_eq!(payload, before, "Goal payload must not be stamped");
    }

    #[test]
    fn off_mode_is_not_auto() {
        assert!(!GoalAttributionMode::Off.is_auto());
        assert!(GoalAttributionMode::Auto.is_auto());
        assert_eq!(GoalAttributionMode::default(), GoalAttributionMode::Off);
    }
}
