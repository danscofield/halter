//! A goal-aware compaction strategy that compresses the transcript along the
//! session's goal boundaries. **Skeleton only** — the closed-subtree
//! compression body is deferred to a later design pass; today the strategy is
//! a safe, buildable shell that never does worse than [`ModelSummary`].
// pattern: Imperative Shell

use std::sync::Arc;

use async_trait::async_trait;
use halter_goals::GoalTree;
use halter_protocol::{PromptSegment, SessionState};
use halter_tools::Tool;

use crate::compaction_strategy::{CompactionContext, CompactionStrategy, WindowPolicy};
use crate::context::CompactionEffects;
use crate::model_summary::ModelSummary;

/// Compresses a session's transcript along its goal tree: closed subtrees
/// distill to their outcome while the open frontier is preserved verbatim
/// (Requirements 12.1, 12.2). Falls back to [`ModelSummary`] whenever the goal
/// tree is thin or the transcript is largely unmapped, so it is never worse
/// than the default checkpoint (Requirements 12.3, 12.4).
///
/// # Deferred
///
/// This is a **skeleton**. The design's closed-subtree compression is deferred
/// to a later pass; see the `TODO(deferred)` in [`compact`](Self::compact). Until
/// then the strategy delegates every decision to its [`ModelSummary`]
/// `fallback`, which keeps the open frontier intact (nothing is dropped) and
/// preserves the byte-identical no-goal path in `off` mode.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalOrientedCompaction {
    /// The default checkpoint strategy every unspecified path delegates to.
    fallback: ModelSummary,
}

impl GoalOrientedCompaction {
    /// A goal-oriented strategy backed by the default [`ModelSummary`] fallback.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fallback: ModelSummary,
        }
    }
}

#[async_trait]
impl CompactionStrategy for GoalOrientedCompaction {
    /// Delegates to [`ModelSummary`]: the skeleton runs under the same
    /// runtime-owned trigger policy as the default.
    fn window_policy(&self) -> WindowPolicy {
        self.fallback.window_policy()
    }

    /// Delegates to [`ModelSummary`]: no goal-specific tools are added by the
    /// skeleton.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.fallback.tools()
    }

    /// Delegates to [`ModelSummary`]: no goal-specific prompt segments are
    /// added by the skeleton.
    fn prompt_segments(&self) -> Vec<PromptSegment> {
        self.fallback.prompt_segments()
    }

    /// Compact the session along goal boundaries.
    ///
    /// The skeleton reads the goal tree through the per-session
    /// [`goal_log`](CompactionContext::goal_log) seam and, in every case,
    /// delegates to the [`ModelSummary`] fallback:
    ///
    /// - `off` mode — [`goal_log`](CompactionContext::goal_log) is `None`, so
    ///   the byte-identical no-goal path is taken (Requirements 12.3, 12.4).
    /// - thin/unmapped tree — the conservative heuristic
    ///   ([`thin_or_unmapped`]) sends the pass to the fallback, so the strategy
    ///   is never worse than the default (Requirements 12.3, 12.4).
    /// - usable tree — the closed-subtree compression is **deferred**; until it
    ///   lands the pass falls back so the open frontier is never dropped
    ///   (Requirement 12.2).
    async fn compact(
        &self,
        ctx: CompactionContext<'_>,
    ) -> anyhow::Result<Option<CompactionEffects>> {
        // Off mode: no goal store wired. Behave exactly like ModelSummary.
        let Some(goal_log) = ctx.goal_log() else {
            return self.fallback.compact(ctx).await;
        };

        let tree = goal_log.get_tree().await?;
        if thin_or_unmapped(&tree, ctx.state()) {
            // Thin or largely unmapped: the default checkpoint is at least as
            // good, so defer to it (Property 12 / Requirements 12.3, 12.4).
            return self.fallback.compact(ctx).await;
        }

        // TODO(deferred): partition ctx.state().messages by owning goal_node;
        // compress CLOSED-subtree events to distilled form; preserve
        // OPEN-frontier events verbatim.
        //
        // The compression body is unspecified until a later design pass. Until
        // then, fall back to ModelSummary so the skeleton is correct and never
        // drops open-frontier events (Property 11 / Requirement 12.2).
        self.fallback.compact(ctx).await
    }
}

/// Whether the goal tree is too thin or the transcript too unmapped for
/// goal-oriented compression to beat the default checkpoint.
///
/// Conservative, minimal heuristic: the tree is thin when it is empty or holds
/// only the root node (`len() <= 1`) — there are no closed subtrees to distill,
/// so the default checkpoint is at least as good. The real criteria (mapped
/// fraction of the transcript, closed-subtree sizes) are **deferred** to a
/// later design pass.
fn thin_or_unmapped(tree: &GoalTree, _state: &SessionState) -> bool {
    tree.len() <= 1
}

#[cfg(test)]
mod tests {
    use halter_protocol::SessionState;
    use halter_protocol::goals::{GoalNode, GoalTree, IntentSignature, Resolution};

    use super::thin_or_unmapped;

    fn intent() -> IntentSignature {
        IntentSignature {
            intent_type: "lookup".into(),
            target_type: "file".into(),
            target_ref: "src/lib.rs".into(),
            scope: "repo".into(),
        }
    }

    fn node(id: &str, parent: Option<&str>) -> GoalNode {
        GoalNode {
            id: id.into(),
            parent: parent.map(Into::into),
            children: Vec::new(),
            hypothesis: "hypothesis".to_owned(),
            resolution_conditions: Vec::new(),
            resolution: Resolution::Open,
            intent: intent(),
            tool_calls: Vec::new(),
            subtree_hash: None,
        }
    }

    #[test]
    fn thin_or_unmapped_is_true_for_empty_or_root_only_trees() {
        let state = SessionState::default();

        let empty = GoalTree::new();
        assert!(thin_or_unmapped(&empty, &state), "empty tree is thin");

        let mut root_only = GoalTree::new();
        root_only.insert(node("root", None));
        assert!(
            thin_or_unmapped(&root_only, &state),
            "root-only tree is thin"
        );
    }

    #[test]
    fn thin_or_unmapped_is_false_once_the_tree_has_a_child() {
        let state = SessionState::default();
        let mut tree = GoalTree::new();
        tree.insert(node("root", None));
        tree.insert(node("child", Some("root")));
        assert!(
            !thin_or_unmapped(&tree, &state),
            "a tree with a child is usable"
        );
    }
}
