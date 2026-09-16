//! A goal-aware compaction strategy that compresses the transcript along the
//! session's goal boundaries: maximal fully-closed subtrees distill to their
//! outcome while the open frontier and unmapped messages are preserved
//! verbatim. Every non-goal-oriented path falls back to [`ModelSummary`], so
//! the strategy is never worse than the default checkpoint.
// pattern: Imperative Shell

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use halter_goals::{GoalNodeId, GoalStoreError, GoalTree, MemoryId, Resolution, SubtreeHash};
use halter_protocol::{
    CompactedContext, CompactionResult, Message, MessageId, PromptSegment, Usage, UserMessage,
    UserPart,
};
use halter_tools::Tool;
use sha2::{Digest, Sha256};

use crate::compaction_strategy::{CompactionContext, CompactionStrategy, GoalLog, WindowPolicy};
use crate::context::CompactionEffects;
use crate::model_summary::ModelSummary;

/// The mapped-coverage threshold: below this fraction of the window mapping to
/// a node in the folded tree, the transcript is "largely unmapped" and the pass
/// falls back to [`ModelSummary`] (Requirements 6.3, 6.4).
///
/// A fixed fraction in the closed interval `[0, 1]`, applied identically on
/// every pass (Requirement 6.4). Chosen conservative: a goal-oriented rewrite
/// only helps when a clear majority of the window is attributable.
const MAPPED_COVERAGE_THRESHOLD: f64 = 0.5;

/// The thin-tree bound: a tree with only the root (or empty) has no closed
/// subtree worth distilling, so the default checkpoint is at least as good and
/// the pass falls back (Requirement 6.2). The gate is `tree.len() <=
/// THIN_TREE_MAX_LEN`.
const THIN_TREE_MAX_LEN: usize = 1;

/// The resolved owner of one message in the window.
///
/// [`Owner::Node`] is used only when the message's `goal_node` tag names a node
/// present in the folded tree; every other case — a missing tag, or a tag that
/// names a node absent from the folded tree — is [`Owner::Unmapped`]
/// (Requirement 3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// The message is owned by a node present in the folded goal tree.
    Node(GoalNodeId),
    /// The message's tag is `None`, or names a node absent from the folded
    /// tree (Requirement 3.4).
    Unmapped,
}

/// One entry in a [`Partition`]: the resolved owner of the message at the given
/// original window index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PartitionEntry {
    /// The message's original position in the window (Requirements 3.3, 5.5).
    index: usize,
    /// The message's resolved owner (Requirements 3.1, 3.4).
    owner: Owner,
}

/// A total, order-preserving partition of the message window by owning node.
///
/// `entries[i]` describes the original message at window index `i`, in original
/// order, so concatenating the messages the entries reference reproduces the
/// original window with no loss and no duplication (Requirements 3.2, 3.3).
/// `mapped_count / entries.len()` is the mapped-coverage fraction the unmapped
/// gate consults (Requirement 6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Partition {
    /// One entry per original message, in original window order (Requirements
    /// 3.2, 3.3).
    entries: Vec<PartitionEntry>,
    /// The number of entries whose owner resolved to a node in the folded tree.
    mapped_count: usize,
}

impl Partition {
    /// The mapped-coverage fraction in the closed interval `[0, 1]`.
    ///
    /// An empty window yields `1.0` (nothing is unmapped); a thin or empty tree
    /// is already handled upstream, so this only ever gates a non-thin tree
    /// (Requirement 6.3).
    fn mapped_coverage(&self) -> f64 {
        if self.entries.is_empty() {
            1.0
        } else {
            self.mapped_count as f64 / self.entries.len() as f64
        }
    }
}

/// The compact record that replaces a fully-closed subtree's messages.
///
/// Built purely from the folded tree node (never model-written), so the pass
/// sends nothing to a provider (Requirement 2.4). It carries the subtree root's
/// `hypothesis`, its non-`Open` `resolution`, and its `subtree_hash` (required;
/// Requirements 5.2, 9.2), an optional Tier 2 memory reference (Requirement
/// 5.3), and the original index at which it is placed in the replacement window
/// (the subtree's first message; Requirement 5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Distillation {
    /// The subtree root node the distillation summarizes.
    node_id: GoalNodeId,
    /// The subtree root's hypothesis (Requirement 5.2).
    hypothesis: String,
    /// The subtree root's resolution — non-`Open`, guaranteed fully-closed
    /// (Requirement 5.2).
    resolution: Resolution,
    /// The subtree root's stable content hash; required for a distillation
    /// (Requirements 5.2, 9.2).
    subtree_hash: SubtreeHash,
    /// An optional reference to an existing Tier 2 memory for this subtree
    /// (Requirement 5.3).
    memory: Option<MemoryId>,
    /// The original window index at which the distillation is placed — the
    /// subtree's first message (Requirement 5.5).
    anchor_index: usize,
}

/// Render a [`Distillation`] into exactly one user-role [`Message`] carrying the
/// stable, deterministic text layout the design fixes for a distilled subtree.
///
/// The layout is (with the `memory:` line present only when `d.memory` is
/// `Some`):
///
/// ```text
/// [goal ✓ <Resolution>] <hypothesis>
/// resolution: <Resolution>
/// subtree: <subtree_hash>
/// memory: <memory_id>
/// ```
///
/// The message is **user-role** — the same role [`ModelSummary`]'s checkpoint
/// uses (every provider accepts it, and it sits in the strongest attention
/// position; Requirement 7.3). One distillation collapses one-or-more original
/// messages into exactly one message, so the replacement window is never larger
/// than the original (Requirement 7.3).
///
/// The result is a **pure function of the [`Distillation`] fields**: the text
/// is built by direct field interpolation, the [`MessageId`] is derived from a
/// content hash of that text, and `created_at` is pinned to the Unix epoch — no
/// wall-clock timestamp, no random id, and no dependence on iteration order. So
/// two renders of the same distillation are byte-identical, and two passes over
/// the same tree + window produce byte-identical output (Requirements 5.2, 5.3,
/// 8.1). The distillation is assembled locally and never sent to a provider
/// (Requirement 2.4).
fn render_distillation(d: &Distillation) -> Message {
    // `Resolution` has no `Display`; its `Debug` form is the stable, human
    // token the design's layout uses verbatim (e.g. `Accepted`).
    let resolution = format!("{:?}", d.resolution);

    let mut text = format!(
        "[goal \u{2713} {resolution}] {hypothesis}\nresolution: {resolution}\nsubtree: {subtree}",
        hypothesis = d.hypothesis,
        subtree = d.subtree_hash,
    );
    if let Some(memory) = &d.memory {
        text.push_str(&format!("\nmemory: {memory}"));
    }

    // A stable, content-derived id keeps the whole `Message` byte-identical
    // across renders (a fresh random id would not), preserving determinism
    // without leaking a wall-clock timestamp.
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let id = MessageId::from(format!("distillation-{:x}", hasher.finalize()));

    Message::User(UserMessage {
        id,
        created_at: DateTime::<Utc>::UNIX_EPOCH,
        parts: vec![UserPart::Text { text }],
    })
}

/// Map every message id in the session log to the goal node that owned its
/// emitting event.
///
/// Built by replaying the shared session log's
/// `SessionEventPayload::MessageItem { message, goal_node: Some(n) }` payloads
/// — the same read the seam performs — and recording `message.id() -> n`. A
/// message whose event carried `goal_node = None`, or that has no
/// `MessageItem` event, is absent from the map, so a lookup miss means the
/// message is *unmapped* (Requirement 3.4).
///
/// The mapping is a pure read over the committed `goal_node` tags: it never
/// inspects message content, wall-clock time, or collection iteration order
/// (Requirements 2.2, 8.3). The pass mutates neither the goal tree nor the
/// goal event log.
///
/// # Errors
///
/// Propagates any failure replaying the shared session log as a
/// [`GoalStoreError::Log`].
async fn owning_nodes(
    goal_log: &GoalLog<'_>,
) -> Result<HashMap<MessageId, GoalNodeId>, GoalStoreError> {
    goal_log.message_owners().await
}

/// The stable [`MessageId`] carried by any transcript [`Message`] variant.
///
/// Mirrors the private helper the seam uses to build the owning-node map, so
/// [`partition_window`] can correlate each window message with its
/// `goal_node` tag by id (Requirement 2.2).
fn message_id(message: &Message) -> MessageId {
    match message {
        Message::System(m) => m.id.clone(),
        Message::User(m) => m.id.clone(),
        Message::Assistant(m) => m.id.clone(),
        Message::Tool(m) => m.id.clone(),
    }
}

/// Partition the live message window by owning goal node.
///
/// Emits one [`PartitionEntry`] per message in original window order, so the
/// concatenation of the referenced messages reproduces the window with no loss
/// and no duplication (Requirements 3.1, 3.2, 3.3). A message's owner is
/// [`Owner::Node`]`(n)` only when its `goal_node` tag resolves to a node that
/// is present in the folded tree — that is, `owning.get(id)` is `Some(n)` and
/// `tree.contains(&n)`; every other case (missing tag, or a tag naming a node
/// absent from the tree) is [`Owner::Unmapped`] (Requirements 2.2, 3.4).
///
/// `mapped_count` is incremented once per mapped entry, so
/// [`Partition::mapped_coverage`] reflects the fraction of the window that
/// resolves to a node in the folded tree. The result is a pure function of the
/// window, the committed tags, and the folded tree.
fn partition_window(
    window: &[Message],
    owning: &HashMap<MessageId, GoalNodeId>,
    tree: &GoalTree,
) -> Partition {
    let mut entries = Vec::with_capacity(window.len());
    let mut mapped_count = 0;

    for (index, message) in window.iter().enumerate() {
        let id = message_id(message);
        let owner = match owning.get(&id) {
            Some(node) if tree.contains(node) => {
                mapped_count += 1;
                Owner::Node(node.clone())
            }
            _ => Owner::Unmapped,
        };
        entries.push(PartitionEntry { index, owner });
    }

    Partition {
        entries,
        mapped_count,
    }
}

/// Compresses a session's transcript along its goal tree: maximal fully-closed
/// subtrees distill to their outcome while the open frontier and unmapped
/// messages are preserved verbatim (Requirements 4, 5). Falls back to
/// [`ModelSummary`] on every non-goal-oriented path — off mode, a tree-read or
/// replay error, a thin tree, a largely-unmapped transcript, or a
/// totality/size-gate failure — so it is never worse than the default
/// checkpoint (Requirements 1.3, 2.3, 6, 7, 9).
///
/// The pass is a pure, read-only function of the message window and the folded
/// goal tree it reads: it appends no `Goal` payload, mutates neither the goal
/// tree nor the goal event log, and sends nothing to a provider (the
/// distillation is assembled locally).
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

    /// Look up an existing Tier 2 memory for the subtree root's
    /// `(node, hash)` idempotency key (Requirement 5.3).
    ///
    /// A [`Distillation`] *may* reference an already-authored Tier 2 memory by
    /// id so a distilled subtree points at the procedural memory it produced.
    /// The reference is **best-effort and strictly read-only**: it calls
    /// `MemoryStore::has_memory_for((node, hash))` when a memory store is wired
    /// into the session and returns the stored [`MemoryId`], and returns `None`
    /// otherwise. It never authors, ranks, retrieves, or invalidates a memory
    /// (out of scope), and it never mutates goal state.
    ///
    /// # No memory store is wired
    ///
    /// The per-session [`GoalLog`] seam this strategy reads through exposes the
    /// session's [`GoalStore`](halter_goals::GoalStore) and shared session log
    /// only; the runtime wires **no** Tier 2 [`MemoryStore`](halter_goals::MemoryStore)
    /// into a session, and the seam carries no accessor for one. So there is no
    /// memory store reachable on this path today, and the lookup returns `None`
    /// — a distillation carries no memory reference. This is exactly the
    /// design's "returns `None` when no memory store is wired" contract: the
    /// hook is in place and read-only, and wiring a store later (a
    /// seam-and-runtime change out of this feature's scope) is all that is
    /// needed to make it surface a real id. Keeping it here — rather than
    /// dropping the field — means the distillation shape and its determinism
    /// (`memory: None` renders no `memory:` line) are already correct.
    ///
    /// The `goal_log`, `node`, and `hash` parameters are the seam handle and the
    /// exact `(node_id, subtree_hash)` key `has_memory_for` expects; they are
    /// accepted now so the signature is stable once a store is reachable.
    #[expect(
        clippy::unused_self,
        reason = "signature is the design's memory_ref contract; the body returns \
                  None until a Tier 2 MemoryStore is wired through the seam"
    )]
    fn memory_ref(
        &self,
        goal_log: &GoalLog<'_>,
        node: &GoalNodeId,
        hash: &SubtreeHash,
    ) -> Option<MemoryId> {
        // No Tier 2 MemoryStore is reachable through the GoalLog seam today, so
        // the lookup is a no-op that yields no reference (Requirement 5.3).
        // When a store is wired, this becomes
        // `store.has_memory_for(&(node.clone(), hash.clone()))`.
        let _ = (goal_log, node, hash);
        None
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

    /// Compact the session along goal boundaries, falling back to
    /// [`ModelSummary`] on every non-goal-oriented path so the strategy is
    /// never worse than the default checkpoint.
    ///
    /// The gates run in a fixed order, each converging on the *same*
    /// [`ModelSummary`] fallback with the *same* [`CompactionContext`] so the
    /// fallback result is byte-identical to the default:
    ///
    /// 1. **Off-mode gate** — [`goal_log`](CompactionContext::goal_log) is
    ///    `None`; take the byte-identical no-goal path and make no goal-store
    ///    call (Requirements 1.3, 7.2).
    /// 2. **Tree-read gate** — `get_tree()` errors degrade to the fallback for
    ///    this pass without failing it (Requirements 2.3, 9.1).
    /// 3. **Thin gate** — `tree.len() <= THIN_TREE_MAX_LEN` (also a legacy
    ///    empty tree) falls back (Requirements 6.1, 6.2, 9.4).
    /// 4. **Replay gate** — an owning-node replay error degrades to the
    ///    fallback (Requirement 9.1).
    /// 5. **Unmapped gate** — mapped-coverage below
    ///    [`MAPPED_COVERAGE_THRESHOLD`] falls back (Requirements 6.3, 6.4).
    /// 6. **Classify + distill + assemble** — preserve open-frontier and
    ///    unmapped messages verbatim, distill maximal closed subtrees, and
    ///    build an order-preserving replacement window (Requirements 3, 4, 5).
    /// 7. **Totality/size gate** — if nothing was distilled, the window would
    ///    grow, or an open-frontier/unmapped message would be dropped, fall
    ///    back rather than emit a lossy or larger window (Requirements 7.1,
    ///    7.3, 7.4). Otherwise return the goal-oriented
    ///    [`CompactionEffects`].
    ///
    /// A `Manual`-trigger fallback error propagates through
    /// `self.fallback.compact(ctx)` exactly as [`ModelSummary`] propagates it,
    /// satisfying the trigger contract (Requirement 9.3).
    async fn compact(
        &self,
        ctx: CompactionContext<'_>,
    ) -> anyhow::Result<Option<CompactionEffects>> {
        // (1) Off mode: no goal store wired. Behave exactly like ModelSummary
        //     and make no goal-store call (Requirements 1.3, 7.2).
        let Some(goal_log) = ctx.goal_log() else {
            return self.fallback.compact(ctx).await;
        };

        // (2) Read the tree; a read error degrades to the fallback for this
        //     pass rather than failing it (Requirements 2.3, 9.1).
        let tree = match goal_log.get_tree().await {
            Ok(tree) => tree,
            Err(_) => return self.fallback.compact(ctx).await,
        };

        // (3) Thin tree (also a legacy empty tree, Requirement 9.4): the
        //     default checkpoint is at least as good (Requirements 6.1, 6.2).
        if tree.len() <= THIN_TREE_MAX_LEN {
            return self.fallback.compact(ctx).await;
        }

        // (4) Owning-node tags from the shared log (Requirement 2.2). A replay
        //     failure degrades to the fallback (Requirement 9.1).
        let owning = match owning_nodes(&goal_log).await {
            Ok(map) => map,
            Err(_) => return self.fallback.compact(ctx).await,
        };

        let window = &ctx.state().messages;
        let partition = partition_window(window, &owning, &tree);

        // (5) Largely unmapped transcript: defer to the default
        //     (Requirements 6.3, 6.4).
        if partition.mapped_coverage() < MAPPED_COVERAGE_THRESHOLD {
            return self.fallback.compact(ctx).await;
        }

        // (6) Classify + distill maximal closed subtrees; assemble an
        //     order-preserving replacement window that keeps open-frontier and
        //     unmapped messages verbatim (Requirements 3, 4, 5).
        let distillations = self.distill(&goal_log, &tree, &partition);
        let replacement = assemble(window, &partition, &tree, &distillations);

        // (7) Totality/size gate: never worse than the default. Fall back when
        //     nothing was distilled (no gain), the window would grow, or an
        //     open-frontier/unmapped message would be dropped (Requirements
        //     7.1, 7.3, 7.4).
        if distillations.is_empty()
            || !within_size_bound(window, &replacement)
            || !retains_all_open_and_unmapped(window, &replacement, &partition, &tree)
        {
            return self.fallback.compact(ctx).await;
        }

        // Original messages folded into distillations = window minus the
        // messages that survived as themselves (replacement minus the one
        // synthetic message per distillation).
        let retained = replacement.len().saturating_sub(distillations.len());
        let compacted_count = window.len().saturating_sub(retained);
        let summary = summarize(&distillations, &partition);

        Ok(Some(CompactionEffects {
            messages: replacement,
            // The goal path builds no provider-native compaction prefix — it
            // rewrites only the message window.
            compacted_context: CompactedContext::default(),
            result: CompactionResult {
                compacted_count,
                summary,
            },
            // No provider call is made on the goal path.
            usage: Usage::default(),
        }))
    }
}

/// A human-readable one-line summary of a goal-oriented compaction pass for the
/// `ContextCompacted` event and PostCompact hooks.
///
/// Reports how many closed goal subtrees were distilled out of the original
/// window, so the summary is a pure function of the distillations and the
/// partition (no wall-clock, no iteration-order dependence).
fn summarize(distillations: &[Distillation], partition: &Partition) -> String {
    format!(
        "Distilled {} closed goal subtree(s) from a window of {} message(s).",
        distillations.len(),
        partition.entries.len(),
    )
}

/// Fold the whole tree once (post-order) into a per-node *fully-closed* flag.
///
/// A node is **fully-closed** iff its own resolution is non-`Open`, it carries a
/// stamped `subtree_hash`, and every node in its subtree is likewise
/// fully-closed. Every other node is **on the open frontier** — it, or some
/// descendant, is still `Open` or lacks a `subtree_hash` (Requirement 4.2).
///
/// The result is a pure function of the folded tree: it is computed with a
/// single iterative post-order walk from the tree's `roots()` (deterministic id
/// order), so it depends on neither wall-clock time nor collection iteration
/// order (Requirement 4.4, 8.1). A node reachable only through a cycle or a
/// dangling parent pointer is treated as open (never marked closed), keeping the
/// classifier conservative — it never distills work it cannot prove finished.
fn fully_closed_flags(tree: &GoalTree) -> HashMap<GoalNodeId, bool> {
    /// A frame in the manual post-order stack: `expanded` tracks whether the
    /// node's children were already pushed for processing.
    struct Frame {
        id: GoalNodeId,
        expanded: bool,
    }

    let mut closed: HashMap<GoalNodeId, bool> = HashMap::with_capacity(tree.len());
    // Guard against cycles / re-visiting a shared node via multiple parents.
    let mut visited: HashMap<GoalNodeId, ()> = HashMap::with_capacity(tree.len());
    let mut stack: Vec<Frame> = tree
        .roots()
        .into_iter()
        .map(|id| Frame {
            id: id.clone(),
            expanded: false,
        })
        .collect();

    while let Some(frame) = stack.pop() {
        let Some(node) = tree.node(&frame.id) else {
            // Dangling reference: nothing to fold, leave unmarked (=> open).
            continue;
        };

        if frame.expanded {
            // Children have been folded; combine their flags with this node's.
            let own_closed = !node.resolution.is_open() && node.subtree_hash.is_some();
            let children_closed = node
                .children
                .iter()
                .all(|child| closed.get(child).copied().unwrap_or(false));
            closed.insert(frame.id, own_closed && children_closed);
            continue;
        }

        if visited.insert(frame.id.clone(), ()).is_some() {
            // Already folded through another path; don't recompute.
            continue;
        }

        // Re-push this node as expanded, then push its children to fold first.
        stack.push(Frame {
            id: frame.id.clone(),
            expanded: true,
        });
        for child in node.children.iter().rev() {
            stack.push(Frame {
                id: child.clone(),
                expanded: false,
            });
        }
    }

    closed
}

/// Whether `n` is on the open frontier: `n` or any descendant has
/// `resolution == Open` or lacks a `subtree_hash` (Requirement 4.2).
///
/// Pure over the folded tree. A node absent from the tree is vacuously on the
/// open frontier. Equivalent to `!fully_closed(n)`; exposed as a standalone
/// query for the classifier and totality gate.
fn on_open_frontier(tree: &GoalTree, n: &GoalNodeId) -> bool {
    !fully_closed_flags(tree).get(n).copied().unwrap_or(false)
}

/// The maximal fully-closed subtree roots: every fully-closed node whose parent
/// is absent from the tree or is itself on the open frontier — i.e. the highest
/// closed ancestor of each closed subtree (Requirement 5.1).
///
/// Distillation happens per maximal closed subtree, so a closed parent subsumes
/// its closed children into one root and they do not appear here. Pure over the
/// folded tree and memoized via a single post-order walk
/// ([`fully_closed_flags`]); returned in deterministic id order
/// (Requirement 4.4, 8.1).
fn closed_subtree_roots(tree: &GoalTree) -> Vec<GoalNodeId> {
    let closed = fully_closed_flags(tree);
    tree.iter()
        .filter(|(id, _)| closed.get(*id).copied().unwrap_or(false))
        .filter(|(_, node)| match node.parent.as_ref() {
            // A closed node with a closed parent is subsumed by that parent.
            Some(parent) => !closed.get(parent).copied().unwrap_or(false),
            None => true,
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// The set of node ids in the subtree rooted at `root` (the root itself plus
/// all of its transitive descendants in the folded tree).
///
/// Pure over the folded tree and computed with a single iterative pre-order
/// walk from `root` down its `children` edges (deterministic; no wall-clock or
/// collection-iteration dependence). A `visited` guard keeps the walk finite in
/// the presence of a cycle or a node reachable through more than one parent
/// pointer, and a `root` absent from the tree yields an empty set.
fn subtree_members(tree: &GoalTree, root: &GoalNodeId) -> std::collections::HashSet<GoalNodeId> {
    let mut members: std::collections::HashSet<GoalNodeId> = std::collections::HashSet::new();
    if !tree.contains(root) {
        return members;
    }

    let mut stack: Vec<GoalNodeId> = vec![root.clone()];
    while let Some(id) = stack.pop() {
        if !members.insert(id.clone()) {
            // Already recorded (cycle / shared child): don't recurse again.
            continue;
        }
        if let Some(node) = tree.node(&id) {
            for child in &node.children {
                if !members.contains(child) {
                    stack.push(child.clone());
                }
            }
        }
    }

    members
}

/// The original window index of the first message owned by any node in the
/// subtree rooted at `root`, or `None` when the subtree owns no message in the
/// window.
///
/// Scans the partition entries in original window order (Requirements 3.3, 5.5)
/// and returns the index of the earliest entry whose [`Owner::Node`] lies in
/// [`subtree_members`]`(tree, root)`. This is the position at which the
/// subtree's single [`Distillation`] is anchored (Requirement 5.5). Pure over
/// the partition and the folded tree.
fn first_owned_index(partition: &Partition, tree: &GoalTree, root: &GoalNodeId) -> Option<usize> {
    let members = subtree_members(tree, root);
    partition
        .entries
        .iter()
        .filter_map(|entry| match &entry.owner {
            Owner::Node(node) if members.contains(node) => Some(entry.index),
            _ => None,
        })
        .min()
}

/// Build one [`Distillation`] per maximal fully-closed subtree that owns at
/// least one message in the window (Requirement 5.1).
///
/// For each maximal closed root (from [`closed_subtree_roots`]) the distiller
/// reads the root node's `hypothesis`, its non-`Open` `resolution`, and its
/// `subtree_hash` (Requirement 5.2):
///
/// - A root whose `subtree_hash` is **absent** is skipped — the strategy never
///   emits a hash-less distillation and instead leaves the subtree's messages
///   retained verbatim by the assembler (Requirement 9.2).
/// - A root that owns **no** message in the window is skipped — there is
///   nothing to distill (its [`first_owned_index`] is `None`).
///
/// The surviving roots each yield a [`Distillation`] anchored at the subtree's
/// first owned message index (Requirement 5.5). The `memory` field is populated
/// best-effort from [`GoalOrientedCompaction::memory_ref`] — an existing Tier 2
/// memory id for the subtree's `(node_id, subtree_hash)` key when a memory store
/// is wired, `None` otherwise (Requirement 5.3). The distiller never touches
/// open-frontier nodes: only maximal *closed* roots are considered
/// (Requirement 5.4). The result is a pure function of the folded tree, the
/// partition, and the (read-only) memory lookup, returned in the deterministic
/// id order [`closed_subtree_roots`] yields.
impl GoalOrientedCompaction {
    fn distill(
        &self,
        goal_log: &GoalLog<'_>,
        tree: &GoalTree,
        partition: &Partition,
    ) -> Vec<Distillation> {
        let mut distillations = Vec::new();

        for root in closed_subtree_roots(tree) {
            let Some(node) = tree.node(&root) else {
                // A maximal closed root is by construction present in the tree,
                // but stay defensive: a missing node has nothing to distill.
                continue;
            };

            // Missing hash on a would-be distillation: retain the subtree's
            // messages verbatim rather than emit a hash-less distillation
            // (Requirement 9.2). The assembler keeps them because no
            // Distillation claims this subtree.
            let Some(subtree_hash) = node.subtree_hash.clone() else {
                continue;
            };

            // No message in the window is owned by this subtree: nothing to
            // distill, so emit no Distillation for it.
            let Some(anchor_index) = first_owned_index(partition, tree, &root) else {
                continue;
            };

            // Optional Tier 2 reference: an existing memory id for this
            // subtree's (node_id, subtree_hash) key when a store is wired, else
            // None (best-effort, read-only; Requirement 5.3).
            let memory = self.memory_ref(goal_log, &root, &subtree_hash);

            distillations.push(Distillation {
                node_id: root.clone(),
                hypothesis: node.hypothesis.clone(),
                resolution: node.resolution,
                subtree_hash,
                memory,
                anchor_index,
            });
        }

        distillations
    }
}

/// Assemble the order-preserving replacement window.
///
/// Walks the original `window` once in original order (Requirements 3.3, 5.5),
/// consulting the [`Partition`] entry for each message. Every message owned by a
/// node inside a distilled subtree collapses into that subtree's single
/// [`Distillation`], which is emitted exactly once at the subtree's
/// *first-member* position — the earliest window index owned by any member of
/// the subtree — and the subtree's remaining messages are dropped
/// (Requirements 5.4, 5.5). Every other message — an open-frontier message, an
/// unmapped message, or a mapped message whose subtree was not distilled — is
/// emitted verbatim, byte-identical to its original (Requirements 3.4, 4.1,
/// 4.3).
///
/// A distilled subtree is identified by mapping each of its member node ids
/// (via [`subtree_members`]) to the index of its [`Distillation`] in
/// `distillations`. Because [`closed_subtree_roots`] yields *maximal* closed
/// roots, their member sets are disjoint, so a message resolves to at most one
/// distillation. The result is a pure function of the window, the partition, the
/// folded tree, and the distillations.
fn assemble(
    window: &[Message],
    partition: &Partition,
    tree: &GoalTree,
    distillations: &[Distillation],
) -> Vec<Message> {
    // Map every member node of every distilled subtree to that distillation's
    // index. Maximal closed roots have disjoint subtrees, so each node maps to
    // at most one distillation.
    let mut member_to_distillation: HashMap<GoalNodeId, usize> = HashMap::new();
    for (di, distillation) in distillations.iter().enumerate() {
        for member in subtree_members(tree, &distillation.node_id) {
            member_to_distillation.insert(member, di);
        }
    }

    let mut out = Vec::with_capacity(window.len());
    // Distillation indices already emitted, so each fires exactly once at its
    // first-member position.
    let mut emitted: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for entry in &partition.entries {
        // A mapped message whose owning node belongs to a distilled subtree
        // collapses into that subtree's Distillation; everything else is kept.
        let distillation_index = match &entry.owner {
            Owner::Node(node) => member_to_distillation.get(node).copied(),
            Owner::Unmapped => None,
        };

        match distillation_index {
            Some(di) => {
                if emitted.insert(di) {
                    // First member of this subtree seen in window order: emit
                    // the distillation here (Requirement 5.5).
                    out.push(render_distillation(&distillations[di]));
                }
                // Remaining members of the same subtree are dropped.
            }
            None => {
                // Open-frontier, unmapped, or a non-distilled mapped message:
                // keep it verbatim (Requirements 3.4, 4.1, 4.3, 5.4).
                out.push(window[entry.index].clone());
            }
        }
    }

    out
}

/// Whether the `replacement` window retains **every** open-frontier and
/// unmapped message from the original `window`, correlated by [`MessageId`]
/// (Requirements 7.1, 7.4).
///
/// An original message is *retained-required* when it is unmapped
/// ([`Owner::Unmapped`]) or when its owning node is on the open frontier
/// ([`on_open_frontier`] — which includes mapped-but-not-distilled nodes, e.g.
/// a resolved node that never got a `subtree_hash`). Every such message must
/// appear verbatim in the replacement; the check confirms this by building the
/// set of [`MessageId`]s present in the replacement and verifying each
/// retained-required original id is a member.
///
/// The correlation is purely by id (via [`message_id`]): a distillation carries
/// a synthetic `distillation-*` id that never collides with an original
/// transcript id, so distilled messages never mask a required one. The result
/// is a pure function of the two windows, the partition, and the folded tree.
/// When it returns `false`, the caller (the totality gate) falls back to
/// [`ModelSummary`] rather than emit a lossy window (Requirement 7.4).
fn retains_all_open_and_unmapped(
    window: &[Message],
    replacement: &[Message],
    partition: &Partition,
    tree: &GoalTree,
) -> bool {
    let present: std::collections::HashSet<MessageId> =
        replacement.iter().map(message_id).collect();

    partition.entries.iter().all(|entry| {
        let retained_required = match &entry.owner {
            // Unmapped messages are always preserved verbatim (Requirement 4.3).
            Owner::Unmapped => true,
            // Mapped messages must be preserved verbatim whenever their owning
            // node is on the open frontier — this covers open work and any
            // mapped-but-not-distilled node (Requirements 4.1, 7.1).
            Owner::Node(node) => on_open_frontier(tree, node),
        };

        if !retained_required {
            return true;
        }

        present.contains(&message_id(&window[entry.index]))
    })
}

/// Whether the `replacement` window is no larger than the original `window`
/// (Requirement 7.3).
///
/// A single distillation collapses one-or-more original messages into exactly
/// one message, so a non-degenerate rewrite strictly shrinks; this check makes
/// the never-larger guarantee explicit and defensive. The caller (the totality
/// gate) falls back to [`ModelSummary`] when it does not hold.
fn within_size_bound(window: &[Message], replacement: &[Message]) -> bool {
    replacement.len() <= window.len()
}

#[cfg(test)]
mod tests {
    use halter_protocol::goals::{
        GoalNode, GoalNodeId, GoalTree, IntentSignature, Resolution, Sha256, SubtreeHash,
    };

    use super::{THIN_TREE_MAX_LEN, closed_subtree_roots, on_open_frontier};

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

    /// A fully-closed node: non-`Open` resolution plus a stamped `subtree_hash`.
    fn closed_node(id: &str, parent: Option<&str>) -> GoalNode {
        GoalNode {
            resolution: Resolution::Accepted,
            subtree_hash: Some(SubtreeHash(Sha256::from(format!("hash-{id}")))),
            ..node(id, parent)
        }
    }

    /// Build a tree from a slice of nodes, wiring each parent's `children` in
    /// insertion order so the fold's post-order walk has real edges to follow.
    fn tree_of(nodes: Vec<GoalNode>) -> GoalTree {
        let mut tree = GoalTree::new();
        for n in &nodes {
            if let Some(parent) = n.parent.clone()
                && let Some(p) = tree.node_mut(&parent)
            {
                p.children.push(n.id.clone());
            }
            tree.insert(n.clone());
        }
        tree
    }

    fn id(s: &str) -> GoalNodeId {
        GoalNodeId::from(s)
    }

    #[test]
    fn thin_gate_trips_for_empty_or_root_only_trees() {
        // The thin gate in `compact` falls back when `tree.len() <=
        // THIN_TREE_MAX_LEN`: an empty tree (legacy log) and a root-only tree
        // both have no closed subtree worth distilling (Requirements 6.1, 6.2,
        // 9.4).
        let empty = GoalTree::new();
        assert!(
            empty.len() <= THIN_TREE_MAX_LEN,
            "an empty tree trips the thin gate"
        );

        let mut root_only = GoalTree::new();
        root_only.insert(node("root", None));
        assert!(
            root_only.len() <= THIN_TREE_MAX_LEN,
            "a root-only tree trips the thin gate"
        );
    }

    #[test]
    fn thin_gate_clears_once_the_tree_has_a_child() {
        // A tree with more than the root has usable structure, so the thin
        // gate does not trip and the pipeline proceeds past it.
        let mut tree = GoalTree::new();
        tree.insert(node("root", None));
        tree.insert(node("child", Some("root")));
        assert!(
            tree.len() > THIN_TREE_MAX_LEN,
            "a tree with a child clears the thin gate"
        );
    }

    #[test]
    fn open_node_is_on_the_open_frontier() {
        let tree = tree_of(vec![node("root", None)]);
        assert!(
            on_open_frontier(&tree, &id("root")),
            "an Open node is on the frontier"
        );
    }

    #[test]
    fn a_node_missing_from_the_tree_is_on_the_open_frontier() {
        let tree = tree_of(vec![closed_node("root", None)]);
        assert!(
            on_open_frontier(&tree, &id("ghost")),
            "an absent node is vacuously on the frontier"
        );
    }

    #[test]
    fn a_closed_node_missing_its_subtree_hash_is_on_the_open_frontier() {
        // Resolved but never stamped with a subtree_hash => not fully closed.
        let mut resolved = node("root", None);
        resolved.resolution = Resolution::Accepted;
        let tree = tree_of(vec![resolved]);
        assert!(
            on_open_frontier(&tree, &id("root")),
            "a hash-less resolved node is not fully closed"
        );
    }

    #[test]
    fn an_open_descendant_forces_its_ancestors_open() {
        // root (closed shell) -> child (closed shell) -> leaf (OPEN)
        let tree = tree_of(vec![
            closed_node("root", None),
            closed_node("child", Some("root")),
            node("leaf", Some("child")),
        ]);
        assert!(
            on_open_frontier(&tree, &id("leaf")),
            "the open leaf is on the frontier"
        );
        assert!(
            on_open_frontier(&tree, &id("child")),
            "an open descendant forces the child open"
        );
        assert!(
            on_open_frontier(&tree, &id("root")),
            "an open descendant forces the root open"
        );
        assert!(
            closed_subtree_roots(&tree).is_empty(),
            "no subtree is fully closed"
        );
    }

    #[test]
    fn a_fully_closed_subtree_is_not_on_the_open_frontier() {
        let tree = tree_of(vec![
            closed_node("root", None),
            closed_node("child", Some("root")),
        ]);
        assert!(
            !on_open_frontier(&tree, &id("root")),
            "a fully closed subtree root is off the frontier"
        );
        assert!(!on_open_frontier(&tree, &id("child")));
    }

    #[test]
    fn closed_parent_subsumes_closed_children_into_one_maximal_root() {
        let tree = tree_of(vec![
            closed_node("root", None),
            closed_node("a", Some("root")),
            closed_node("b", Some("root")),
        ]);
        assert_eq!(
            closed_subtree_roots(&tree),
            vec![id("root")],
            "the closed parent is the single maximal root; closed children are subsumed"
        );
    }

    #[test]
    fn a_closed_leaf_under_an_open_parent_is_its_own_maximal_root() {
        // root (OPEN) -> closed (fully closed leaf) + open sibling
        let tree = tree_of(vec![
            node("root", None),
            closed_node("closed", Some("root")),
            node("open", Some("root")),
        ]);
        assert!(
            on_open_frontier(&tree, &id("root")),
            "the root is open because a child is open"
        );
        assert_eq!(
            closed_subtree_roots(&tree),
            vec![id("closed")],
            "the closed leaf under an open parent is a maximal closed root"
        );
    }

    #[test]
    fn closed_subtree_roots_are_returned_in_deterministic_id_order() {
        // Two independent closed subtrees under an open root; ids chosen so
        // insertion order differs from sorted id order.
        let tree = tree_of(vec![
            node("root", None),
            closed_node("zeta", Some("root")),
            closed_node("alpha", Some("root")),
        ]);
        assert_eq!(
            closed_subtree_roots(&tree),
            vec![id("alpha"), id("zeta")],
            "roots come back in deterministic id order regardless of insertion order"
        );
    }
}
