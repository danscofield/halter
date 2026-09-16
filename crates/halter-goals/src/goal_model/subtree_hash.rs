//! Deterministic, order-insensitive `subtree_hash` computation over a resolved
//! goal subtree (task 6.4).
//!
//! `subtree_hash(n)` is the versioning and idempotency key for induction. It is
//! computed over node `n` together with its **resolved subtree**: the
//! canonicalized tuple of `n`'s intent-relevant fields (`hypothesis`,
//! `resolution_conditions`, `resolution`, `intent`, and the normalized
//! `tool_calls` contracts) combined with the `subtree_hash` of each **resolved**
//! child, in **canonical child order** (Requirement 5.1).
//!
//! # Determinism and order-insensitivity
//!
//! Two structurally-equal resolved subtrees hash equally regardless of child
//! insertion order or incidental encoding of the intent-relevant fields
//! (Requirement 5.2). This is achieved two ways:
//!
//! - Children's hashes are **recomputed bottom-up** (this function never trusts
//!   the stored `subtree_hash` `Option`, so a re-fold that has not yet stamped a
//!   child still hashes identically), then **sorted** into canonical order, so
//!   reordering a parent's children yields the same parent hash.
//! - The hash input is a **length-prefixed** byte string: every field and every
//!   child hash is emitted as its byte length (as a fixed-width big-endian
//!   `u64`) followed by its bytes. Length-prefixing makes field boundaries
//!   unambiguous, so distinct field contents can never alias by concatenation
//!   (e.g. `("ab", "c")` and `("a", "bc")` hash differently).
//!
//! Because the encoding is a pure function of the resolved subtree's structure
//! and content, recomputation always yields the same value (Requirement 5.3), a
//! structurally-unchanged revision yields the same value (Requirement 5.4), and
//! any change to an intent-relevant field or to any resolved child's
//! `subtree_hash` yields a different value (Requirement 5.5).
//!
//! # Precondition
//!
//! The node's subtree is expected to be fully resolved (every node non-`Open`)
//! when `subtree_hash` is called — this is the closure boundary the goal fold
//! computes it at. Only **resolved** children contribute to the hash; an `Open`
//! child (or a child absent from the tree) is skipped, matching the "resolved
//! subtree" definition.

use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::types::{Sha256, SubtreeHash};

use super::model::{GoalNode, GoalTree};

/// Domain-separation tags emitted before each field so that, even with
/// length-prefixing, two different fields with coincidentally equal bytes
/// occupy distinct positions in the canonical encoding.
const TAG_HYPOTHESIS: u8 = 0x01;
const TAG_RESOLUTION_CONDITION: u8 = 0x02;
const TAG_RESOLUTION: u8 = 0x03;
const TAG_INTENT: u8 = 0x04;
const TAG_TOOL_CALL: u8 = 0x05;
const TAG_CHILD_HASH: u8 = 0x06;

/// Append `bytes` to `buf` as a length-prefixed field: an 8-byte big-endian
/// length followed by the raw bytes. Unambiguous field boundaries mean distinct
/// field contents cannot alias by concatenation.
fn push_field(buf: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
    buf.push(tag);
    buf.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// Canonically encode a single tool call into `buf`. Each of the four fields is
/// itself length-prefixed so a tool call's boundaries never blur into its
/// neighbours.
fn push_tool_call(buf: &mut Vec<u8>, call: &crate::types::ToolCall) {
    // A tool call's canonical bytes are its four fields, each length-prefixed,
    // concatenated into a sub-buffer that is then emitted as one tagged field.
    let mut inner = Vec::new();
    push_field(&mut inner, 0, call.tool.0.as_bytes());
    push_field(&mut inner, 0, call.normalized_args.as_bytes());
    // The validity token and outcome are part of the reproducible contract.
    let token_json =
        serde_json::to_vec(&call.validity_token).expect("validity token serializes to JSON");
    push_field(&mut inner, 0, &token_json);
    push_field(&mut inner, 0, call.outcome.0.as_bytes());
    push_field(buf, TAG_TOOL_CALL, &inner);
}

/// Compute the canonical child order: the recomputed `subtree_hash` of every
/// **resolved** child, sorted ascending. Sorting makes the parent hash
/// insensitive to child insertion order (Requirement 5.2). Unresolved or absent
/// children contribute nothing, matching the resolved-subtree definition.
fn resolved_child_hashes(node: &GoalNode, tree: &GoalTree) -> Vec<SubtreeHash> {
    let mut hashes: Vec<SubtreeHash> = node
        .children
        .iter()
        .filter_map(|child_id| tree.node(child_id))
        .filter(|child| !child.is_open())
        .map(|child| subtree_hash(child, tree))
        .collect();
    hashes.sort();
    hashes
}

/// Build the canonical, length-prefixed byte encoding of a resolved node and
/// its resolved subtree.
fn canonical_bytes(node: &GoalNode, tree: &GoalTree) -> Vec<u8> {
    let mut buf = Vec::new();

    // hypothesis
    push_field(&mut buf, TAG_HYPOTHESIS, node.hypothesis.as_bytes());

    // resolution_conditions — order-significant list; each entry length-prefixed
    // and preceded by a count so two lists that differ only by splitting an
    // entry cannot alias.
    buf.push(TAG_RESOLUTION_CONDITION);
    buf.extend_from_slice(&(node.resolution_conditions.len() as u64).to_be_bytes());
    for condition in &node.resolution_conditions {
        push_field(&mut buf, TAG_RESOLUTION_CONDITION, condition.as_bytes());
    }

    // resolution — a small, closed enum; its serde encoding is stable.
    let resolution_json =
        serde_json::to_vec(&node.resolution).expect("resolution serializes to JSON");
    push_field(&mut buf, TAG_RESOLUTION, &resolution_json);

    // intent — the four-field IntentSignature; serde encoding is stable and the
    // struct field order is fixed.
    let intent_json = serde_json::to_vec(&node.intent).expect("intent serializes to JSON");
    push_field(&mut buf, TAG_INTENT, &intent_json);

    // tool_calls — order-significant list of reproducible contracts.
    buf.push(TAG_TOOL_CALL);
    buf.extend_from_slice(&(node.tool_calls.len() as u64).to_be_bytes());
    for call in &node.tool_calls {
        push_tool_call(&mut buf, call);
    }

    // resolved children, in canonical (sorted-by-hash) order.
    let child_hashes = resolved_child_hashes(node, tree);
    buf.push(TAG_CHILD_HASH);
    buf.extend_from_slice(&(child_hashes.len() as u64).to_be_bytes());
    for child_hash in &child_hashes {
        push_field(&mut buf, TAG_CHILD_HASH, child_hash.0.0.as_bytes());
    }

    buf
}

/// Compute the deterministic, order-insensitive [`SubtreeHash`] of `node`'s
/// resolved subtree within `tree`.
///
/// The hash is derived only from the node's intent-relevant fields
/// (`hypothesis`, `resolution_conditions`, `resolution`, `intent`, canonically
/// normalized `tool_calls`) combined with each resolved child's recomputed
/// `subtree_hash` in canonical child order (Requirement 5.1). Structurally-equal
/// resolved subtrees hash equally regardless of child order or incidental
/// encoding (5.2); recomputation is stable (5.3); a structurally-unchanged
/// revision preserves the hash (5.4); any intent-relevant or child change
/// alters it (5.5).
///
/// Children's hashes are recomputed recursively rather than read from the stored
/// `subtree_hash` field, so the result is well-defined even before the fold has
/// stamped descendants.
#[must_use]
pub fn subtree_hash(node: &GoalNode, tree: &GoalTree) -> SubtreeHash {
    let bytes = canonical_bytes(node, tree);
    let mut hasher = Sha256Hasher::new();
    hasher.update(&bytes);
    SubtreeHash(Sha256(format!("{:x}", hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_model::model::Resolution;
    use crate::types::{
        CanonicalJson, GoalNodeId, IntentSignature, IntentType, OutcomeRef, Scope, TargetRef,
        TargetType, ToolCall, ToolName, ValidityToken,
    };

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
            validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
            outcome: OutcomeRef::from("outcome://1"),
        }
    }

    fn node(id: &str, parent: Option<&str>, resolution: Resolution) -> GoalNode {
        GoalNode {
            id: GoalNodeId::from(id),
            parent: parent.map(GoalNodeId::from),
            children: Vec::new(),
            hypothesis: format!("hypothesis for {id}"),
            resolution_conditions: vec![format!("cond-{id}")],
            resolution,
            intent: intent(id),
            tool_calls: vec![tool_call(id)],
            subtree_hash: None,
        }
    }

    fn tree_with(nodes: Vec<GoalNode>) -> GoalTree {
        let mut tree = GoalTree::new();
        for n in nodes {
            tree.insert(n);
        }
        tree
    }

    #[test]
    fn deterministic_across_recomputation() {
        let n = node("root", None, Resolution::Accepted);
        let tree = tree_with(vec![n.clone()]);
        let root = tree.node(&GoalNodeId::from("root")).unwrap();
        let h1 = subtree_hash(root, &tree);
        let h2 = subtree_hash(root, &tree);
        assert_eq!(h1, h2, "recomputation must be identical (5.3)");
    }

    #[test]
    fn insensitive_to_child_insertion_order() {
        // Parent with two resolved children, in order [a, b].
        let mut parent_ab = node("root", None, Resolution::Accepted);
        parent_ab.children = vec![GoalNodeId::from("a"), GoalNodeId::from("b")];
        let child_a = node("a", Some("root"), Resolution::Rejected);
        let child_b = node("b", Some("root"), Resolution::Accepted);
        let tree_ab = tree_with(vec![parent_ab, child_a.clone(), child_b.clone()]);
        let h_ab = subtree_hash(tree_ab.node(&GoalNodeId::from("root")).unwrap(), &tree_ab);

        // Same structure, children listed [b, a].
        let mut parent_ba = node("root", None, Resolution::Accepted);
        parent_ba.children = vec![GoalNodeId::from("b"), GoalNodeId::from("a")];
        let tree_ba = tree_with(vec![parent_ba, child_a, child_b]);
        let h_ba = subtree_hash(tree_ba.node(&GoalNodeId::from("root")).unwrap(), &tree_ba);

        assert_eq!(h_ab, h_ba, "reordering children must not change the hash (5.2)");
    }

    #[test]
    fn unchanged_subtree_same_hash_ignores_stored_option() {
        // Whether or not the stored subtree_hash Option is populated, the
        // recomputed value is the same (5.4).
        let mut n1 = node("root", None, Resolution::Accepted);
        let tree1 = tree_with(vec![n1.clone()]);
        let expected = subtree_hash(tree1.node(&GoalNodeId::from("root")).unwrap(), &tree1);

        n1.subtree_hash = Some(SubtreeHash(Sha256::from("stale-value")));
        let tree2 = tree_with(vec![n1]);
        let recomputed = subtree_hash(tree2.node(&GoalNodeId::from("root")).unwrap(), &tree2);

        assert_eq!(expected, recomputed, "stored Option must not affect the hash (5.4)");
    }

    #[test]
    fn intent_relevant_change_changes_hash() {
        let base = node("root", None, Resolution::Accepted);
        let tree_base = tree_with(vec![base.clone()]);
        let h_base = subtree_hash(tree_base.node(&GoalNodeId::from("root")).unwrap(), &tree_base);

        // Change hypothesis.
        let mut changed = base.clone();
        changed.hypothesis = "a different hypothesis".to_owned();
        let tree_changed = tree_with(vec![changed]);
        let h_changed =
            subtree_hash(tree_changed.node(&GoalNodeId::from("root")).unwrap(), &tree_changed);
        assert_ne!(h_base, h_changed, "hypothesis change must change the hash (5.5)");

        // Change resolution.
        let resolved_differently = node("root", None, Resolution::Rejected);
        let tree_res = tree_with(vec![resolved_differently]);
        let h_res = subtree_hash(tree_res.node(&GoalNodeId::from("root")).unwrap(), &tree_res);
        assert_ne!(h_base, h_res, "resolution change must change the hash (5.5)");
    }

    #[test]
    fn child_hash_change_changes_parent_hash() {
        let mut parent = node("root", None, Resolution::Accepted);
        parent.children = vec![GoalNodeId::from("a")];
        let child = node("a", Some("root"), Resolution::Accepted);
        let tree = tree_with(vec![parent.clone(), child]);
        let h = subtree_hash(tree.node(&GoalNodeId::from("root")).unwrap(), &tree);

        // Mutate the child's intent-relevant content.
        let mut changed_child = node("a", Some("root"), Resolution::Accepted);
        changed_child.hypothesis = "child hypothesis changed".to_owned();
        let tree2 = tree_with(vec![parent, changed_child]);
        let h2 = subtree_hash(tree2.node(&GoalNodeId::from("root")).unwrap(), &tree2);

        assert_ne!(h, h2, "a child subtree_hash change must change the parent hash (5.5)");
    }

    #[test]
    fn open_children_are_excluded() {
        // A parent whose only child is Open hashes the same as a childless
        // parent, because only resolved children contribute.
        let mut parent_with_open = node("root", None, Resolution::Accepted);
        parent_with_open.children = vec![GoalNodeId::from("open-child")];
        let open_child = node("open-child", Some("root"), Resolution::Open);
        let tree = tree_with(vec![parent_with_open, open_child]);
        let h_with_open =
            subtree_hash(tree.node(&GoalNodeId::from("root")).unwrap(), &tree);

        let childless = node("root", None, Resolution::Accepted);
        let tree_childless = tree_with(vec![childless]);
        let h_childless =
            subtree_hash(tree_childless.node(&GoalNodeId::from("root")).unwrap(), &tree_childless);

        assert_eq!(h_with_open, h_childless, "Open children must not contribute to the hash");
    }

    #[test]
    fn subtree_hash_is_keyed_only_on_the_semantic_contract() {
        // Requirements 9.3, 9.4: subtree_hash is derived ONLY from the semantic
        // contract (hypothesis, resolution_conditions, resolution, intent,
        // normalized tool_calls, resolved children in canonical order) and NOT
        // from the raw tagged transcript. Because the tagged transcript lives on
        // the shared session log — never inside GoalNode — the guard here fixes
        // the entire semantic contract and asserts the hash is unchanged.
        //
        // Two nodes with byte-for-byte identical contracts must hash equally.
        // Any number of incidental turn events could have been tagged to either
        // node on the shared log; none of that content is an input to the hash,
        // so the hashes cannot differ.
        let contract = node("root", None, Resolution::Accepted);

        let a = contract.clone();
        let tree_a = tree_with(vec![a]);
        let h_a = subtree_hash(tree_a.node(&GoalNodeId::from("root")).unwrap(), &tree_a);

        // An independently constructed node with the identical semantic
        // contract — standing in for the "same goal work, different incidental
        // tagged transcript" case — hashes identically.
        let b = contract.clone();
        let tree_b = tree_with(vec![b]);
        let h_b = subtree_hash(tree_b.node(&GoalNodeId::from("root")).unwrap(), &tree_b);

        assert_eq!(
            h_a, h_b,
            "subtree_hash must depend only on the semantic contract, so tagging \
             incidental transcript to a node never perturbs it (9.3, 9.4)"
        );

        // Sanity anchor: the ONLY inputs that can change the hash are the
        // semantic-contract fields. Perturbing a contract field changes it
        // (guarding against a degenerate constant hash that would make the
        // invariance above vacuous).
        let mut curated = contract.clone();
        curated.tool_calls = vec![tool_call("root"), tool_call("root/extra")];
        let tree_c = tree_with(vec![curated]);
        let h_c = subtree_hash(tree_c.node(&GoalNodeId::from("root")).unwrap(), &tree_c);
        assert_ne!(
            h_a, h_c,
            "the curated tool_calls contract is a hash input; only it (not the \
             tagged transcript) can move the hash"
        );
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        // Two nodes whose concatenated field bytes would coincide without
        // length-prefixing must hash differently.
        let mut a = node("root", None, Resolution::Accepted);
        a.hypothesis = "ab".to_owned();
        a.resolution_conditions = vec!["c".to_owned()];
        let tree_a = tree_with(vec![a]);
        let h_a = subtree_hash(tree_a.node(&GoalNodeId::from("root")).unwrap(), &tree_a);

        let mut b = node("root", None, Resolution::Accepted);
        b.hypothesis = "a".to_owned();
        b.resolution_conditions = vec!["bc".to_owned()];
        let tree_b = tree_with(vec![b]);
        let h_b = subtree_hash(tree_b.node(&GoalNodeId::from("root")).unwrap(), &tree_b);

        assert_ne!(h_a, h_b, "field boundaries must not alias across fields");
    }
}
