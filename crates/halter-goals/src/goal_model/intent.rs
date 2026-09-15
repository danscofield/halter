//! `IntentSignature` derivation and attachment (task 7.1).
//!
//! Every goal node carries a structured [`IntentSignature`] — the four-field
//! retrieval/merge key (`intent_type`, `target_type`, `target_ref`, `scope`)
//! that Tier 2 uses to decide when two pieces of goal work target "the same
//! thing" before any embedding similarity is considered. The Goal Model **owns**
//! the derivation of that signature: it is derived and attached on create and on
//! revise (Requirement 7.1), and a node's attached signature is readable — with
//! all four fields — for Tier 2 retrieval/merge (Requirement 7.3).
//!
//! # Derivation contract
//!
//! The harness-specific source of the four candidate values (hypothesis text,
//! surrounding context, tool selection, …) is not fully specified by the design,
//! so this module models the derivation *input* as an explicit [`IntentInput`]:
//! a bundle of optional candidate values for the four signature fields. The
//! caller (the `GoalStore` in task 8, or a harness adapter that inspects the
//! hypothesis/context) populates whichever candidates it could resolve and hands
//! the bundle to [`derive_intent`].
//!
//! [`derive_intent`] is **pure and total over its error space**: it either
//! returns a fully-populated [`IntentSignature`] (all four fields resolved,
//! Requirement 7.1) or an [`IntentDerivationError`] that enumerates *which*
//! field or fields could not be resolved (Requirement 7.2). A field is
//! considered unresolved when its candidate is absent (`None`) or blank
//! (empty/whitespace-only), since a signature field carrying no meaningful value
//! could not serve as a retrieval key. Because the function performs no I/O and
//! mutates nothing, a caller that receives an error can simply decline to append
//! its create/revise event, leaving the node's prior state unchanged
//! (Requirement 7.2); the actual event append lives in the `GoalStore`.
//!
//! # Reading an attached signature
//!
//! [`attached_intent`] exposes a node's attached [`IntentSignature`] by id for
//! Tier 2 (Requirement 7.3). A node's signature is also directly readable as the
//! public `GoalNode::intent` field; the helper is the by-id convenience Tier 2
//! uses when it holds a tree and a node id.

use crate::types::{GoalNodeId, IntentSignature, IntentType, Scope, TargetRef, TargetType};

use super::model::GoalTree;

/// One of the four fields of an [`IntentSignature`].
///
/// Used by [`IntentDerivationError`] to name precisely which field(s) could not
/// be resolved during derivation (Requirement 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntentField {
    /// The `intent_type` field (the kind of intent the goal expresses).
    IntentType,
    /// The `target_type` field (the category of thing the goal acts on).
    TargetType,
    /// The `target_ref` field (the concrete reference to the target).
    TargetRef,
    /// The `scope` field (the bounding context in which the goal applies).
    Scope,
}

impl IntentField {
    /// The field's canonical name, matching the [`IntentSignature`] field.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::IntentType => "intent_type",
            Self::TargetType => "target_type",
            Self::TargetRef => "target_ref",
            Self::Scope => "scope",
        }
    }
}

impl std::fmt::Display for IntentField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when an [`IntentSignature`] cannot be fully derived.
///
/// Carries the non-empty, de-duplicated list of [`IntentField`]s that could not
/// be resolved, in canonical field order (`intent_type`, `target_type`,
/// `target_ref`, `scope`). Per Requirement 7.2, a caller that receives this
/// error must reject the create/revise operation and leave the node's prior
/// state unchanged; producing the error has no side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentDerivationError {
    /// The field(s) that could not be resolved, in canonical field order.
    pub unresolved: Vec<IntentField>,
}

impl IntentDerivationError {
    /// Whether the given field is among the unresolved fields.
    #[must_use]
    pub fn is_unresolved(&self, field: IntentField) -> bool {
        self.unresolved.contains(&field)
    }
}

impl std::fmt::Display for IntentDerivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.unresolved.iter().map(|field| field.name()).collect();
        write!(
            f,
            "cannot derive IntentSignature: unresolved field(s): {}",
            names.join(", ")
        )
    }
}

impl std::error::Error for IntentDerivationError {}

/// The raw candidate values available when deriving an [`IntentSignature`] at
/// create or revise time.
///
/// Each field is an optional candidate string: the caller (a harness adapter or
/// the `GoalStore`) populates whichever values it could extract from the
/// hypothesis, context, and tool selection. [`derive_intent`] resolves each
/// candidate, treating an absent (`None`) or blank (empty/whitespace-only)
/// candidate as *unresolved* for that field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntentInput {
    /// Candidate for `intent_type` (the kind of intent the goal expresses).
    pub intent_type: Option<String>,
    /// Candidate for `target_type` (the category of thing the goal acts on).
    pub target_type: Option<String>,
    /// Candidate for `target_ref` (the concrete reference to the target).
    pub target_ref: Option<String>,
    /// Candidate for `scope` (the bounding context in which the goal applies).
    pub scope: Option<String>,
}

impl IntentInput {
    /// Construct an input from four candidate strings, one per field, in
    /// signature order.
    #[must_use]
    pub fn new(
        intent_type: impl Into<String>,
        target_type: impl Into<String>,
        target_ref: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            intent_type: Some(intent_type.into()),
            target_type: Some(target_type.into()),
            target_ref: Some(target_ref.into()),
            scope: Some(scope.into()),
        }
    }
}

/// Resolve one candidate: `Some(trimmed)` when the candidate is present and not
/// blank, `None` when it is absent or whitespace-only.
fn resolve(candidate: Option<&String>) -> Option<&str> {
    candidate
        .map(|value| value.trim())
        .filter(|trimmed| !trimmed.is_empty())
}

/// Derive a fully-populated [`IntentSignature`] from the raw candidate values in
/// `input`.
///
/// Populates all four fields (`intent_type`, `target_type`, `target_ref`,
/// `scope`) when every candidate resolves to a non-blank value (Requirement
/// 7.1). If any candidate is absent or blank, returns an
/// [`IntentDerivationError`] enumerating exactly which field(s) could not be
/// resolved, in canonical field order (Requirement 7.2).
///
/// The function is pure and total: it performs no I/O and mutates nothing, so a
/// caller that receives an error can decline to append its create/revise event
/// and thereby leave the node's prior state unchanged.
///
/// # Errors
///
/// Returns [`IntentDerivationError`] with a non-empty `unresolved` list when one
/// or more of the four fields cannot be resolved from `input`.
pub fn derive_intent(input: &IntentInput) -> Result<IntentSignature, IntentDerivationError> {
    let intent_type = resolve(input.intent_type.as_ref());
    let target_type = resolve(input.target_type.as_ref());
    let target_ref = resolve(input.target_ref.as_ref());
    let scope = resolve(input.scope.as_ref());

    let mut unresolved = Vec::new();
    if intent_type.is_none() {
        unresolved.push(IntentField::IntentType);
    }
    if target_type.is_none() {
        unresolved.push(IntentField::TargetType);
    }
    if target_ref.is_none() {
        unresolved.push(IntentField::TargetRef);
    }
    if scope.is_none() {
        unresolved.push(IntentField::Scope);
    }

    if !unresolved.is_empty() {
        return Err(IntentDerivationError { unresolved });
    }

    Ok(IntentSignature {
        intent_type: IntentType::from(intent_type.expect("checked resolved above")),
        target_type: TargetType::from(target_type.expect("checked resolved above")),
        target_ref: TargetRef::from(target_ref.expect("checked resolved above")),
        scope: Scope::from(scope.expect("checked resolved above")),
    })
}

/// Expose the [`IntentSignature`] attached to the node identified by `id` in
/// `tree`, for Tier 2 retrieval/merge (Requirement 7.3).
///
/// Returns `None` when no node with that id exists. When a node exists, its
/// signature is returned with all four fields readable. A node's signature is
/// also directly readable via the public `GoalNode::intent` field; this helper
/// is the by-id convenience for a caller holding a tree and a node id.
#[must_use]
pub fn attached_intent<'tree>(
    tree: &'tree GoalTree,
    id: &GoalNodeId,
) -> Option<&'tree IntentSignature> {
    tree.node(id).map(|node| &node.intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal_model::model::{GoalNode, Resolution};

    fn full_input() -> IntentInput {
        IntentInput::new("locate", "file", "src/lib.rs", "repo")
    }

    fn node_with_intent(id: &str, intent: IntentSignature) -> GoalNode {
        GoalNode {
            id: GoalNodeId::from(id),
            parent: None,
            children: Vec::new(),
            hypothesis: "the file exists".to_owned(),
            resolution_conditions: Vec::new(),
            resolution: Resolution::Open,
            intent,
            tool_calls: Vec::new(),
            subtree_hash: None,
        }
    }

    #[test]
    fn derives_all_four_fields_on_success() {
        let sig = derive_intent(&full_input()).expect("all candidates present");
        assert_eq!(sig.intent_type, IntentType::from("locate"));
        assert_eq!(sig.target_type, TargetType::from("file"));
        assert_eq!(sig.target_ref, TargetRef::from("src/lib.rs"));
        assert_eq!(sig.scope, Scope::from("repo"));
    }

    #[test]
    fn trims_surrounding_whitespace_on_resolved_fields() {
        let input = IntentInput::new("  locate  ", "\tfile", "src/lib.rs\n", " repo ");
        let sig = derive_intent(&input).expect("blank-trimmed candidates still resolve");
        assert_eq!(sig.intent_type, IntentType::from("locate"));
        assert_eq!(sig.target_type, TargetType::from("file"));
        assert_eq!(sig.target_ref, TargetRef::from("src/lib.rs"));
        assert_eq!(sig.scope, Scope::from("repo"));
    }

    #[test]
    fn rejects_missing_intent_type_and_names_it() {
        let mut input = full_input();
        input.intent_type = None;
        let err = derive_intent(&input).expect_err("missing field must reject");
        assert_eq!(err.unresolved, vec![IntentField::IntentType]);
        assert!(err.is_unresolved(IntentField::IntentType));
        assert!(!err.is_unresolved(IntentField::Scope));
    }

    #[test]
    fn rejects_blank_target_type_and_names_it() {
        let mut input = full_input();
        input.target_type = Some("   ".to_owned());
        let err = derive_intent(&input).expect_err("blank field must reject");
        assert_eq!(err.unresolved, vec![IntentField::TargetType]);
    }

    #[test]
    fn rejects_missing_target_ref_and_names_it() {
        let mut input = full_input();
        input.target_ref = None;
        let err = derive_intent(&input).expect_err("missing field must reject");
        assert_eq!(err.unresolved, vec![IntentField::TargetRef]);
    }

    #[test]
    fn rejects_missing_scope_and_names_it() {
        let mut input = full_input();
        input.scope = None;
        let err = derive_intent(&input).expect_err("missing field must reject");
        assert_eq!(err.unresolved, vec![IntentField::Scope]);
    }

    #[test]
    fn enumerates_multiple_unresolved_fields_in_canonical_order() {
        // All candidates absent: every field should be listed, in signature order.
        let err = derive_intent(&IntentInput::default()).expect_err("empty input must reject");
        assert_eq!(
            err.unresolved,
            vec![
                IntentField::IntentType,
                IntentField::TargetType,
                IntentField::TargetRef,
                IntentField::Scope,
            ]
        );
        // The Display message names each unresolved field.
        let message = err.to_string();
        for field in ["intent_type", "target_type", "target_ref", "scope"] {
            assert!(message.contains(field), "message should mention {field}: {message}");
        }
    }

    #[test]
    fn attached_intent_returns_all_four_fields_for_existing_node() {
        let sig = derive_intent(&full_input()).expect("derive");
        let mut tree = GoalTree::new();
        tree.insert(node_with_intent("n1", sig.clone()));

        let read = attached_intent(&tree, &GoalNodeId::from("n1")).expect("node exists");
        assert_eq!(read, &sig);
        // All four fields are readable.
        assert_eq!(read.intent_type, IntentType::from("locate"));
        assert_eq!(read.target_type, TargetType::from("file"));
        assert_eq!(read.target_ref, TargetRef::from("src/lib.rs"));
        assert_eq!(read.scope, Scope::from("repo"));
    }

    #[test]
    fn attached_intent_is_none_for_absent_node() {
        let tree = GoalTree::new();
        assert!(attached_intent(&tree, &GoalNodeId::from("missing")).is_none());
    }
}
