//! The Tier 2 procedural-memory model.
//!
//! This module defines the durable, replayable [`Memory`] object and its
//! supporting types. A `Memory` is the distillation of *how* a class of goal
//! was solved: a reusable [`Plan`], the [`EvidenceContract`] that must be
//! re-validated (via Tier 1) before the plan is trusted, an optional
//! goal-level [`CachedOutcome`], and the bookkeeping ([`Provenance`],
//! [`Reinforcement`], [`MemoryVersion`]) that drives dedup, reinforcement, and
//! idempotency.
//!
//! Two invariants are baked into the shapes here and enforced at write time by
//! the `MemoryStore` (task 11.2):
//!
//! - **Contracts, not values.** An [`EvidenceContract`] stores only
//!   `tool + normalized_args + validity_token` per item (see
//!   [`EvidenceContractItem`]). The concrete [`crate::EvidenceValue`] lives in
//!   the Tier 1 cache; Tier 2 never inlines it (Requirement 17.4).
//! - **No secret capture.** [`OutcomeShape`] references the successful result
//!   by shape/pointer rather than inlining a payload that may contain sensitive
//!   data.
//!
//! This task only *defines* the model; validation, induction, retrieval, and
//! replay are implemented by tasks 11.2 and 12–14.

use serde::{Deserialize, Serialize};

use crate::types::{
    CanonicalJson, Duration, GoalNodeId, IntentSignature, IntentType, MemoryId, OutcomeRef,
    Scope, SubtreeHash, TargetRef, TargetType, Timestamp, ToolName, ValidityToken,
};

/// How a memory is classified for search-pruning on recurrence.
///
/// Every memory carries exactly one kind (Requirement 17.1). Positive kinds
/// (`Fragment`, `Composite`) record procedures that *did* resolve an intent;
/// `Negative` records a known dead-end so recurring goals skip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MemoryKind {
    /// A single coherent sub-procedure: one intent, one plan.
    Fragment,
    /// A composition of fragments and/or sub-goals into a larger procedure.
    Composite,
    /// A learned dead-end: "this approach does not resolve this intent." A
    /// `Negative` memory still carries an [`EvidenceContract`], since a dead-end
    /// may only be a dead-end while its evidence holds (Requirement 17.2).
    Negative,
}

/// One parameter a memory generalizes over.
///
/// A `ParameterDef` names a slot the memory abstracts (e.g. the concrete file
/// path it was authored against) and records a coarse kind hint so replay can
/// bind arguments. The `kind` is a free-form descriptor (e.g. `"path"`,
/// `"string"`) rather than a closed enum, keeping the schema open as the design
/// leaves it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ParameterDef {
    /// The name of the parameter slot.
    pub name: String,
    /// A coarse kind/type hint for the parameter (open-ended descriptor).
    pub kind: String,
}

/// The parameters a memory generalizes over, so it applies beyond one target.
///
/// A memory authored while resolving a goal about `src/lib.rs` may generalize
/// to any file path; the `parameter_schema` records those abstracted slots so
/// retrieval and replay can rebind them. An empty schema means the memory is
/// fully concrete.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct ParameterSchema {
    /// The parameter slots this memory abstracts over.
    pub parameters: Vec<ParameterDef>,
}

/// A guard describing when a memory legitimately applies.
///
/// Retrieval (task 13) excludes any candidate whose guard is not satisfied by
/// the querying [`IntentSignature`]. The guard is expressed as optional
/// field-equality constraints over the four signature fields: a `None`
/// constraint matches anything, a `Some(v)` constraint requires the querying
/// signature's field to equal `v`. This keeps the guard byte-stable and cheap
/// to evaluate while still discriminating across intent/target/scope.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Applicability {
    /// If set, the querying signature's `intent_type` must equal this.
    pub required_intent_type: Option<IntentType>,
    /// If set, the querying signature's `target_type` must equal this.
    pub required_target_type: Option<TargetType>,
    /// If set, the querying signature's `target_ref` must equal this.
    pub required_target_ref: Option<TargetRef>,
    /// If set, the querying signature's `scope` must equal this.
    pub required_scope: Option<Scope>,
}

impl Applicability {
    /// A guard that applies to every signature (no constraints).
    #[must_use]
    pub fn unconstrained() -> Self {
        Self::default()
    }

    /// Whether this guard is satisfied by `sig`.
    ///
    /// Each present constraint must equal the corresponding field of `sig`;
    /// absent constraints impose no restriction. Retrieval uses this to exclude
    /// non-applicable candidates.
    #[must_use]
    pub fn applies_to(&self, sig: &IntentSignature) -> bool {
        fn matches<T: PartialEq>(req: &Option<T>, actual: &T) -> bool {
            req.as_ref().is_none_or(|v| v == actual)
        }
        matches(&self.required_intent_type, &sig.intent_type)
            && matches(&self.required_target_type, &sig.target_type)
            && matches(&self.required_target_ref, &sig.target_ref)
            && matches(&self.required_scope, &sig.scope)
    }
}

/// One ordered step of a reusable [`Plan`].
///
/// A step describes a unit of the replayable procedure: a human-readable
/// `description` plus optional references to the tool and/or intent it enacts.
/// References are shape/pointer-level, not inlined payloads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlanStep {
    /// A human-readable description of what this step does.
    pub description: String,
    /// The tool this step invokes, if any.
    pub tool: Option<ToolName>,
    /// The intent signature this step enacts (e.g. a sub-goal), if any.
    pub intent: Option<IntentSignature>,
}

/// The reusable "how": the ordered steps / sub-goals to replay.
///
/// Steps replay in order. An empty plan is valid for a `Negative` memory whose
/// only content is the dead-end it records.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Plan {
    /// The ordered steps to replay.
    pub steps: Vec<PlanStep>,
}

/// One item of an [`EvidenceContract`] — a contract, never a value.
///
/// An item records *what* to re-validate before trusting a replay: the `tool`
/// and `normalized_args` that identify the Tier 1 cache entry, and the
/// `validity_token` recording how freshness is re-checked. Per Requirement
/// 17.4 it deliberately carries **no** [`crate::EvidenceValue`]; the concrete
/// value lives in the Tier 1 `CacheEntry` and is fetched/re-validated at replay
/// time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EvidenceContractItem {
    /// The tool whose cached result this item depends on.
    pub tool: ToolName,
    /// The canonical arguments identifying the Tier 1 cache entry.
    pub normalized_args: CanonicalJson,
    /// How the depended-on result's freshness is re-checked (owned by Tier 1).
    pub validity_token: ValidityToken,
}

/// The evidence a replay depends on — contract items only, no values.
///
/// Every item is re-validated through Tier 1 before the plan is trusted. A
/// `Negative` memory must carry a non-empty contract (Requirement 17.2), since
/// the dead-end only holds while its evidence does; that non-emptiness is
/// enforced at write time in task 11.2.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct EvidenceContract {
    /// The contract items to re-validate; each holds no concrete value.
    pub items: Vec<EvidenceContractItem>,
}

impl EvidenceContract {
    /// Whether the contract carries no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The shape of what a successful application of a memory yields.
///
/// Per the no-secret-capture guidance, the outcome is referenced by shape and
/// pointer rather than inlining a (possibly sensitive) payload: `result_ref`
/// points at the outcome and `schema` describes its shape. The concrete answer,
/// when cached, lives in [`CachedOutcome`] instead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeShape {
    /// A shape/pointer reference to the successful result, not its payload.
    pub result_ref: OutcomeRef,
    /// A description of the result's shape (e.g. a schema name or descriptor).
    pub schema: String,
}

/// The concrete goal-level answer that a memory may cache.
///
/// Unlike [`OutcomeShape`], which only points at the outcome by shape, this is
/// the actual cached answer served (subject to the [`AnswerMode`] rules). It is
/// modeled as canonical JSON so it is byte-stable across processes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OutcomeValue(pub CanonicalJson);

/// How a [`CachedOutcome`] may be served.
///
/// - [`AnswerMode::SoundPinnable`] — Mode A: every dependency is pinnable /
///   content-hashed, so the answer is served *verified* iff every validity
///   token still holds. This is the default, safe path.
/// - [`AnswerMode::BoundedVolatile`] — Mode B: some dependency is volatile.
///   Serving is opt-in per intent type, bounded by `max_age`, and the result is
///   always marked believed-unverified. Mode B never runs by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AnswerMode {
    /// Mode A — sound: all dependencies are pinnable/content-hashed.
    SoundPinnable,
    /// Mode B — bounded-volatile: opt-in, bounded by `max_age`, always marked
    /// believed-unverified.
    BoundedVolatile {
        /// The maximum age at which the cached answer may still be served.
        max_age: Duration,
    },
}

/// An optional goal-level answer cache carried by a [`Memory`].
///
/// Serving the `answer` is governed by `mode`. `validity_tokens` records every
/// token the answer depends on (drawn from its evidence); it must be non-empty
/// whenever a `CachedOutcome` is present (Requirement 17.5, enforced at write
/// time in task 11.2). A `SoundPinnable` outcome must contain only pinnable
/// (`ContentHash`) tokens.
///
/// `issued_at` records when the concrete `answer` was captured. Mode A
/// (`SoundPinnable`) serving ignores it — freshness is decided purely by whether
/// the pinnable tokens still hold — but Mode B (`BoundedVolatile`) serving needs
/// it: the answer's age at serve time is `now - issued_at`, and Mode B serves
/// only while that age is within the outcome's `max_age` (see
/// [`crate::tier2::replay`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CachedOutcome {
    /// The concrete goal-level answer to serve.
    pub answer: OutcomeValue,
    /// How this answer may be served (Mode A vs Mode B).
    pub mode: AnswerMode,
    /// Every validity token the answer depends on; must be non-empty.
    pub validity_tokens: Vec<ValidityToken>,
    /// When the cached `answer` was captured. Used to compute the answer's age
    /// (`now - issued_at`) for the Mode B `max_age` bound; unused for Mode A.
    pub issued_at: Timestamp,
}

/// Where a memory came from: its originating resolved subtree(s).
///
/// A memory retains provenance so a poisoned or incorrect memory can be traced
/// and invalidated. Reinforcement merges provenance, so multiple origins are
/// supported: each origin is a `(node_id, subtree_hash)` pair identifying a
/// resolved subtree that induced or confirmed this memory.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Provenance {
    /// The `(node_id, subtree_hash)` origins that induced/reinforced this memory.
    pub origins: Vec<(GoalNodeId, SubtreeHash)>,
}

/// Reinforcement bookkeeping for dedup and confidence decay.
///
/// Counts accumulate as a memory is retrieved (`hits`), confirmed by a
/// successful replay (`confirms`), or contradicted by a failed one
/// (`contradicts`). `last_updated` supports confidence decay over time.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Reinforcement {
    /// How many times this memory has been retrieved as a candidate.
    pub hits: u64,
    /// How many times a replay of this memory succeeded.
    pub confirms: u64,
    /// How many times a replay of this memory was contradicted.
    pub contradicts: u64,
    /// When the counts were last updated (for decay).
    pub last_updated: Timestamp,
}

/// The version key for a memory.
///
/// The design uses the originating `subtree_hash` as the version-key part; a
/// changed `subtree_hash` may supersede or coexist with a prior memory
/// (versioning policy is out of scope here). `MemoryVersion` wraps that hash so
/// the versioning dimension is explicit on the memory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MemoryVersion(pub SubtreeHash);

/// The durable, replayable distillation of how a class of goal was solved.
///
/// A `Memory` is the primary Tier 2 artifact. It pairs a structured retrieval
/// key ([`IntentSignature`]) and an [`Applicability`] guard with the reusable
/// [`Plan`], the [`EvidenceContract`] to re-validate before trusting the plan,
/// the [`OutcomeShape`] it produces, and an optional [`CachedOutcome`]. The
/// bookkeeping fields drive dedup/reinforcement and idempotency/versioning.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Memory {
    /// The memory's opaque identifier.
    pub id: MemoryId,
    /// Fragment | Composite | Negative.
    pub kind: MemoryKind,
    /// The structured retrieval key.
    pub intent: IntentSignature,
    /// Parameters the memory generalizes over (so it applies beyond one target).
    pub parameter_schema: ParameterSchema,
    /// Guard describing when this memory legitimately applies.
    pub applicability: Applicability,
    /// The reusable "how": ordered steps / sub-goals to replay.
    pub plan: Plan,
    /// What evidence must be re-validated (via Tier 1) before trusting a replay.
    pub evidence_contract: EvidenceContract,
    /// The shape of what a successful application yields.
    pub outcome_shape: OutcomeShape,
    /// Optional goal-level answer cache (see [`CachedOutcome`]).
    pub cached_outcome: Option<CachedOutcome>,
    /// `node_id + subtree_hash` origin(s).
    pub provenance: Provenance,
    /// Hit/confirm/contradict counts and decay bookkeeping.
    pub reinforcement: Reinforcement,
    /// The version key (subtree-hash-based).
    pub version: MemoryVersion,
}

/// An embedding vector used for the ANN tail-recall index.
///
/// Retrieval (task 13) uses embedding recall only when the structured head is
/// thin. The vector is modeled as a newtype over `Vec<f32>` so it can be
/// indexed and serialized without pulling in an embedding library here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Embedding(pub Vec<f32>);

/// The persistent store record wrapping a [`Memory`].
///
/// The structured fields (`intent_type`, `target_type`, `target_ref`, `scope`,
/// `kind`) are indexed for the primary structured filter, `embedding` feeds the
/// ANN index for tail recall, and `body` carries the full serialized memory.
/// `node_id` and `subtree_hash` form the idempotency/version key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// The memory's opaque identifier (mirrors `body.id`).
    pub id: MemoryId,
    /// Indexed: the intent type of `body.intent`.
    pub intent_type: IntentType,
    /// Indexed: the target type of `body.intent`.
    pub target_type: TargetType,
    /// Indexed: the target ref of `body.intent`.
    pub target_ref: TargetRef,
    /// Indexed: the scope of `body.intent`.
    pub scope: Scope,
    /// Indexed: the memory kind.
    pub kind: MemoryKind,
    /// ANN index for tail recall.
    pub embedding: Embedding,
    /// The full serialized memory.
    pub body: Memory,
    /// Idempotency key part: the originating node id.
    pub node_id: GoalNodeId,
    /// Idempotency + version key part: the originating subtree hash.
    pub subtree_hash: SubtreeHash,
    /// When the record was first created.
    pub created_at: Timestamp,
    /// When the record was last updated (e.g. by reinforcement).
    pub updated_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Sha256;

    fn sample_signature() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn sample_memory() -> Memory {
        Memory {
            id: MemoryId::from("mem-1"),
            kind: MemoryKind::Fragment,
            intent: sample_signature(),
            parameter_schema: ParameterSchema {
                parameters: vec![ParameterDef {
                    name: "path".to_owned(),
                    kind: "path".to_owned(),
                }],
            },
            applicability: Applicability {
                required_intent_type: Some(IntentType::from("lookup")),
                ..Applicability::default()
            },
            plan: Plan {
                steps: vec![PlanStep {
                    description: "read the file".to_owned(),
                    tool: Some(ToolName::from("read_file")),
                    intent: None,
                }],
            },
            evidence_contract: EvidenceContract {
                items: vec![EvidenceContractItem {
                    tool: ToolName::from("read_file"),
                    normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
                    validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
                }],
            },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            cached_outcome: Some(CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{\"lines\":42}".to_owned())),
                mode: AnswerMode::SoundPinnable,
                validity_tokens: vec![ValidityToken::ContentHash(Sha256::from("abc123"))],
                issued_at: Timestamp(1_000),
            }),
            provenance: Provenance {
                origins: vec![(
                    GoalNodeId::from("node-1"),
                    SubtreeHash(Sha256::from("hash-1")),
                )],
            },
            reinforcement: Reinforcement {
                hits: 3,
                confirms: 2,
                contradicts: 0,
                last_updated: Timestamp(1_000),
            },
            version: MemoryVersion(SubtreeHash(Sha256::from("hash-1"))),
        }
    }

    #[test]
    fn memory_roundtrips_through_serde() {
        let mem = sample_memory();
        let json = serde_json::to_string(&mem).expect("serialize memory");
        let back: Memory = serde_json::from_str(&json).expect("deserialize memory");
        assert_eq!(mem, back, "memory must survive a serde round trip");
    }

    #[test]
    fn memory_kind_variants_roundtrip() {
        for kind in [MemoryKind::Fragment, MemoryKind::Composite, MemoryKind::Negative] {
            let json = serde_json::to_string(&kind).expect("serialize kind");
            let back: MemoryKind = serde_json::from_str(&json).expect("deserialize kind");
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn cached_outcome_and_answer_mode_roundtrip() {
        let outcomes = vec![
            CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{\"a\":1}".to_owned())),
                mode: AnswerMode::SoundPinnable,
                validity_tokens: vec![ValidityToken::ContentHash(Sha256::from("h"))],
                issued_at: Timestamp(0),
            },
            CachedOutcome {
                answer: OutcomeValue(CanonicalJson("{\"b\":2}".to_owned())),
                mode: AnswerMode::BoundedVolatile {
                    max_age: Duration(5_000),
                },
                validity_tokens: vec![ValidityToken::Ttl {
                    issued_at: Timestamp(1),
                    ttl: Duration(10),
                }],
                issued_at: Timestamp(1),
            },
        ];
        for outcome in outcomes {
            let json = serde_json::to_string(&outcome).expect("serialize outcome");
            let back: CachedOutcome =
                serde_json::from_str(&json).expect("deserialize outcome");
            assert_eq!(outcome, back);
        }
    }

    #[test]
    fn evidence_contract_carries_only_contract_items() {
        let contract = EvidenceContract {
            items: vec![EvidenceContractItem {
                tool: ToolName::from("read_file"),
                normalized_args: CanonicalJson("{\"path\":\"a\"}".to_owned()),
                validity_token: ValidityToken::Ttl {
                    issued_at: Timestamp(0),
                    ttl: Duration(1),
                },
            }],
        };
        assert!(!contract.is_empty());
        let json = serde_json::to_string(&contract).expect("serialize contract");
        // No concrete evidence value should be present in the serialized form.
        assert!(!json.contains("EvidenceValue"));
        assert!(!json.contains("evidence_value"));
        let back: EvidenceContract =
            serde_json::from_str(&json).expect("deserialize contract");
        assert_eq!(contract, back);
    }

    #[test]
    fn memory_record_roundtrips_through_serde() {
        let mem = sample_memory();
        let record = MemoryRecord {
            id: mem.id.clone(),
            intent_type: mem.intent.intent_type.clone(),
            target_type: mem.intent.target_type.clone(),
            target_ref: mem.intent.target_ref.clone(),
            scope: mem.intent.scope.clone(),
            kind: mem.kind,
            embedding: Embedding(vec![0.1, 0.2, 0.3]),
            body: mem,
            node_id: GoalNodeId::from("node-1"),
            subtree_hash: SubtreeHash(Sha256::from("hash-1")),
            created_at: Timestamp(1),
            updated_at: Timestamp(2),
        };
        let json = serde_json::to_string(&record).expect("serialize record");
        let back: MemoryRecord = serde_json::from_str(&json).expect("deserialize record");
        assert_eq!(record, back, "record must survive a serde round trip");
    }

    #[test]
    fn applicability_matches_only_when_constraints_hold() {
        let sig = sample_signature();

        assert!(Applicability::unconstrained().applies_to(&sig));

        let matching = Applicability {
            required_intent_type: Some(IntentType::from("lookup")),
            required_scope: Some(Scope::from("repo")),
            ..Applicability::default()
        };
        assert!(matching.applies_to(&sig));

        let mismatching = Applicability {
            required_intent_type: Some(IntentType::from("mutate")),
            ..Applicability::default()
        };
        assert!(!mismatching.applies_to(&sig));
    }
}
