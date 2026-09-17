//! Opt-in, default-off similar-goal summaries (Part 2).
//!
//! The [`SummaryProvider`] turns memories retrieved for a goal into compact,
//! natural-language-style [`GoalSummary`] values describing what each similar
//! past goal did and what it concluded. It is disabled by default via
//! [`SummaryConfig`] and, when enabled, reads through the existing structured
//! -first `Retrieval` without ever mutating a persisted record.
//!
//! The pure `summarize` function distills one retrieved [`Memory`] into a
//! [`GoalSummary`], and [`SummaryProvider::summaries_for`] drives it over the
//! structured-first `Retrieval` results, honoring the opt-in config.

use crate::tier2::memory::{AnswerMode, Memory, MemoryKind};
use crate::tier2::retrieval::Retrieval;
use crate::types::IntentSignature;

/// Opt-in configuration for similar-goal summaries. Default: disabled.
///
/// The derived [`Default`] yields `enabled: false` and `max_summaries: None`,
/// i.e. disabled and unbounded (Requirement 13.1).
#[derive(Debug, Clone, Default)]
pub struct SummaryConfig {
    /// Whether the provider runs retrieval-driven summarization. Default: false.
    pub enabled: bool,
    /// Maximum number of GoalSummary values returned. `None` = unbounded.
    pub max_summaries: Option<usize>,
}

/// A compact, natural-language-style summary of one past similar goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalSummary {
    /// What the goal did, distilled from its IntentSignature + Plan.
    pub did: String,
    /// The conclusion/outcome reached, from MemoryKind + outcome info.
    pub concluded: String,
    /// True when the summarized memory is a learned dead-end (Negative).
    pub dead_end: bool,
}

/// Turns retrieved memories into compact GoalSummary values.
pub struct SummaryProvider {
    config: SummaryConfig,
}

impl SummaryProvider {
    /// Create a provider bound to the given [`SummaryConfig`].
    #[must_use]
    pub fn new(config: SummaryConfig) -> Self {
        Self { config }
    }

    /// Produce summaries for `sig` using `retrieval`, honoring the config.
    ///
    /// - When [`SummaryConfig::enabled`] is false, returns an empty `Vec`
    ///   **without** invoking `retrieval` at all (Requirement 13.2).
    /// - When enabled, calls [`Retrieval::retrieve_memories`] (already ordered
    ///   by score, structured-head-first) and maps up to
    ///   [`SummaryConfig::max_summaries`] of them to [`GoalSummary`], preserving
    ///   retrieval order (Requirements 11.1, 11.2, 11.3).
    /// - Never mutates any persisted record (Requirement 14.3): it only reads
    ///   the compact [`Memory`] values retrieval already returned.
    ///
    /// This method is `async` because [`Retrieval::retrieve_memories`] now
    /// awaits the embedding backend when the structured head is thin
    /// (Requirements 1.4, 6.3); the disabled path still returns without
    /// awaiting retrieval at all.
    #[must_use]
    pub async fn summaries_for(
        &self,
        sig: &IntentSignature,
        retrieval: &impl Retrieval,
    ) -> Vec<GoalSummary> {
        // Disabled: do not consult retrieval at all (Requirement 13.2).
        if !self.config.enabled {
            return Vec::new();
        }

        // Enabled: retrieval already returns candidates ordered by score,
        // structured-head-first (Requirements 11.1, 11.2). Map them in order,
        // bounding to `max_summaries` when set (Requirement 11.3).
        let candidates = retrieval.retrieve_memories(sig).await;
        let mut summaries: Vec<GoalSummary> = candidates
            .iter()
            .map(|scored| summarize(&scored.memory))
            .collect();
        if let Some(max) = self.config.max_summaries {
            summaries.truncate(max);
        }
        summaries
    }
}

/// Build one [`GoalSummary`] from a retrieved memory (pure, total function).
///
/// `did` is distilled from the memory's [`IntentSignature`] and [`Plan`] step
/// descriptions (Requirement 12.1); `concluded` is distilled from the memory's
/// [`MemoryKind`], its [`OutcomeShape`] schema, and the *shape* of any
/// [`CachedOutcome`] — never the raw cached answer value (Requirement 12.2). A
/// [`MemoryKind::Negative`] memory is phrased as a learned dead-end and sets
/// `dead_end = true` (Requirement 12.3). No `evidence_contract` value or raw
/// transcript is emitted (Requirement 12.4); the `Memory` model carries only
/// contracts, so this is satisfied by simply never reading those fields.
///
/// [`IntentSignature`]: crate::types::IntentSignature
/// [`Plan`]: crate::tier2::memory::Plan
/// [`OutcomeShape`]: crate::tier2::memory::OutcomeShape
/// [`CachedOutcome`]: crate::tier2::memory::CachedOutcome
#[must_use]
fn summarize(mem: &Memory) -> GoalSummary {
    let intent = &mem.intent;

    // `did`: what the goal was and how it was approached. Drawn only from the
    // intent signature and the plan step descriptions.
    let mut did = format!(
        "{} {} {} (scope: {})",
        intent.intent_type, intent.target_type, intent.target_ref, intent.scope
    );
    if mem.plan.steps.is_empty() {
        did.push_str(" — no recorded steps");
    } else {
        let steps: Vec<&str> = mem
            .plan
            .steps
            .iter()
            .map(|step| step.description.as_str())
            .collect();
        did.push_str(" — steps: ");
        did.push_str(&steps.join("; "));
    }

    // `concluded`: the outcome/conclusion, from kind + outcome shape + the
    // *shape* of any cached outcome (never the raw answer value).
    let dead_end = matches!(mem.kind, MemoryKind::Negative);
    let concluded = if dead_end {
        // Negative memory: phrase as a learned dead-end (Requirement 12.3).
        format!(
            "learned dead-end: this approach did not resolve the goal (outcome shape: {})",
            mem.outcome_shape.schema
        )
    } else {
        let kind = match mem.kind {
            MemoryKind::Fragment => "resolved as a fragment procedure",
            MemoryKind::Composite => "resolved as a composite procedure",
            MemoryKind::Negative => unreachable!("handled by dead_end branch"),
        };
        let mut concluded =
            format!("{kind}; outcome shape: {}", mem.outcome_shape.schema);
        // Describe only the *shape* of the cached answer, never its value.
        match &mem.cached_outcome {
            Some(cached) => {
                let mode = match cached.mode {
                    AnswerMode::SoundPinnable => "sound/pinnable",
                    AnswerMode::BoundedVolatile { .. } => "bounded-volatile",
                };
                concluded.push_str(&format!("; a cached answer is available ({mode})"));
            }
            None => concluded.push_str("; no cached answer"),
        }
        concluded
    };

    GoalSummary {
        did,
        concluded,
        dead_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier2::memory::{
        AnswerMode, Applicability, CachedOutcome, EvidenceContract, EvidenceContractItem,
        MemoryKind, MemoryVersion, OutcomeShape, OutcomeValue, Plan, PlanStep, Provenance,
        Reinforcement,
    };
    use crate::tier2::retrieval::{Retrieval, ScoredMemory};
    use crate::types::{
        CanonicalJson, GoalNodeId, IntentType, MemoryId, OutcomeRef, Scope, Sha256, SubtreeHash,
        TargetRef, TargetType, Timestamp, ToolName, ValidityToken,
    };
    use async_trait::async_trait;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ----- Retrieval spy -----------------------------------------------------

    /// A fake `Retrieval` that returns a preconfigured, fixed-order list of
    /// `ScoredMemory` and records whether `retrieve_memories` was invoked.
    ///
    /// The recorded flag lets the disabled-provider property (Property 10)
    /// assert that retrieval was never consulted.
    struct SpyRetrieval {
        candidates: Vec<ScoredMemory>,
        called: AtomicBool,
    }

    impl SpyRetrieval {
        fn new(candidates: Vec<ScoredMemory>) -> Self {
            Self {
                candidates,
                called: AtomicBool::new(false),
            }
        }

        fn called(&self) -> bool {
            self.called.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Retrieval for SpyRetrieval {
        async fn retrieve_memories(&self, _sig: &IntentSignature) -> Vec<ScoredMemory> {
            self.called.store(true, Ordering::SeqCst);
            self.candidates.clone()
        }
    }

    // ----- Memory builders ---------------------------------------------------

    fn sig() -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    /// A valid `Fragment` memory whose `target_ref` carries `marker` so the
    /// resulting summary's `did` text can be distinguished and ordered.
    ///
    /// Mirrors the `valid_memory` shape used across the store tests: a non-empty
    /// evidence contract and a sound (`ContentHash`-only) cached outcome.
    fn memory_with_marker(id: &str, marker: &str) -> Memory {
        Memory {
            id: MemoryId::from(id),
            kind: MemoryKind::Fragment,
            intent: IntentSignature {
                intent_type: IntentType::from("lookup"),
                target_type: TargetType::from("file"),
                target_ref: TargetRef::from(marker),
                scope: Scope::from("repo"),
            },
            parameter_schema: crate::tier2::memory::ParameterSchema::default(),
            applicability: Applicability::unconstrained(),
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
                    normalized_args: CanonicalJson("{\"path\":\"x\"}".to_owned()),
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
                origins: vec![(GoalNodeId::from("node-1"), SubtreeHash(Sha256::from("h")))],
            },
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        }
    }

    fn scored(mem: Memory, score: f32) -> ScoredMemory {
        ScoredMemory { memory: mem, score }
    }

    // ----- Proptest generators ----------------------------------------------

    /// A marker string safe to embed in a `TargetRef` and distinguishable by
    /// prefix from evidence markers.
    fn marker_strategy() -> impl Strategy<Value = String> {
        "ref-[a-z0-9]{1,8}"
    }

    /// A batch of candidates with distinct, order-revealing `target_ref`
    /// markers, each wrapped as a `ScoredMemory`. The `did` of the i-th summary
    /// will contain the i-th marker, so summary order can be checked against the
    /// candidate order.
    fn arb_candidates() -> impl Strategy<Value = Vec<ScoredMemory>> {
        prop::collection::vec(marker_strategy(), 0..8).prop_map(|markers| {
            markers
                .into_iter()
                .enumerate()
                .map(|(i, marker)| {
                    // Distinguish each marker by index so duplicates from the
                    // generator do not defeat the order check.
                    let unique = format!("{marker}-{i}");
                    scored(memory_with_marker(&format!("mem-{i}"), &unique), 0.0)
                })
                .collect()
        })
    }

    // ----- Task 8.4: Property 9 — summary order and count bound --------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 9: Summaries preserve retrieval
        // order and count bound — when enabled, `GoalSummary` values appear in
        // the same order as retrieval candidates and number at most
        // `max_summaries` when set.
        //
        // Validates: Requirements 11.2, 11.3
        #[test]
        fn prop_summaries_preserve_order_and_bound(
            candidates in arb_candidates(),
            max in prop::option::of(0_usize..10),
        ) {
            let markers: Vec<String> = candidates
                .iter()
                .map(|c| c.memory.intent.target_ref.to_string())
                .collect();

            let retrieval = SpyRetrieval::new(candidates.clone());
            let provider = SummaryProvider::new(SummaryConfig {
                enabled: true,
                max_summaries: max,
            });

            let rt = tokio::runtime::Runtime::new().unwrap();
            let summaries = rt.block_on(provider.summaries_for(&sig(), &retrieval));

            // Count bound: at most `max` when set, else all candidates.
            let expected_len = match max {
                Some(m) => markers.len().min(m),
                None => markers.len(),
            };
            prop_assert_eq!(summaries.len(), expected_len);

            // Order: the i-th summary's `did` must contain the i-th candidate's
            // distinguishing marker, in the same order as retrieval returned.
            for (i, summary) in summaries.iter().enumerate() {
                prop_assert!(
                    summary.did.contains(&markers[i]),
                    "summary {} did {:?} must carry candidate marker {:?}",
                    i,
                    summary.did,
                    markers[i],
                );
            }
        }
    }

    // ----- Task 8.5: Property 10 — disabled provider -------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 10: Disabled provider retrieves
        // nothing and yields no summaries — when disabled, `summaries_for`
        // returns empty without invoking `Retrieval`.
        //
        // Validates: Requirements 13.1, 13.2
        #[test]
        fn prop_disabled_provider_retrieves_nothing(
            candidates in arb_candidates(),
            max in prop::option::of(0_usize..10),
        ) {
            let retrieval = SpyRetrieval::new(candidates);
            let provider = SummaryProvider::new(SummaryConfig {
                enabled: false,
                max_summaries: max,
            });

            let rt = tokio::runtime::Runtime::new().unwrap();
            let summaries = rt.block_on(provider.summaries_for(&sig(), &retrieval));

            prop_assert!(summaries.is_empty(), "a disabled provider yields no summaries");
            prop_assert!(
                !retrieval.called(),
                "a disabled provider must not invoke Retrieval"
            );
        }
    }

    // ----- Task 8.6: Property 12 — negative dead-ends, evidence excluded -----

    /// An arbitrary marker string planted into evidence contract items and the
    /// cached outcome, used to prove no raw evidence value leaks into a summary.
    fn evidence_marker_strategy() -> impl Strategy<Value = String> {
        "EVIDENCE-[A-Z0-9]{4,10}"
    }

    /// A `Negative` memory whose evidence contract items and cached outcome all
    /// carry `evidence_marker` in their raw values. The summary must never echo
    /// that marker (Requirement 12.4) and must flag the memory as a dead-end.
    fn negative_memory_with_evidence(evidence_marker: &str) -> Memory {
        Memory {
            id: MemoryId::from("neg-1"),
            kind: MemoryKind::Negative,
            intent: IntentSignature {
                intent_type: IntentType::from("lookup"),
                target_type: TargetType::from("file"),
                target_ref: TargetRef::from("src/lib.rs"),
                scope: Scope::from("repo"),
            },
            parameter_schema: crate::tier2::memory::ParameterSchema::default(),
            applicability: Applicability::unconstrained(),
            plan: Plan {
                steps: vec![PlanStep {
                    description: "attempt the dead-end approach".to_owned(),
                    tool: Some(ToolName::from("read_file")),
                    intent: None,
                }],
            },
            // Both the normalized args and validity token embed the marker; the
            // summary must not reproduce either.
            evidence_contract: EvidenceContract {
                items: vec![EvidenceContractItem {
                    tool: ToolName::from("read_file"),
                    normalized_args: CanonicalJson(format!("{{\"arg\":\"{evidence_marker}\"}}")),
                    validity_token: ValidityToken::ContentHash(Sha256::from(evidence_marker)),
                }],
            },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            // The cached answer embeds the marker too — its value must never
            // appear, only its shape.
            cached_outcome: Some(CachedOutcome {
                answer: OutcomeValue(CanonicalJson(format!("{{\"answer\":\"{evidence_marker}\"}}"))),
                mode: AnswerMode::SoundPinnable,
                validity_tokens: vec![ValidityToken::ContentHash(Sha256::from(evidence_marker))],
                issued_at: Timestamp(1_000),
            }),
            provenance: Provenance {
                origins: vec![(GoalNodeId::from("node-1"), SubtreeHash(Sha256::from("h")))],
            },
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        // Feature: sqlite-memory-store, Property 12: Negative memories are
        // summarized as dead-ends, excluding evidence values — a `Negative`
        // memory yields `dead_end == true` with dead-end phrasing, and no
        // summary text contains raw evidence values or transcript content.
        //
        // Validates: Requirements 12.1, 12.2, 12.3, 12.4
        #[test]
        fn prop_negative_dead_end_excludes_evidence(
            evidence_marker in evidence_marker_strategy(),
        ) {
            let mem = negative_memory_with_evidence(&evidence_marker);

            // Summarize both directly and via an enabled provider.
            let direct = summarize(&mem);
            let retrieval = SpyRetrieval::new(vec![scored(mem.clone(), 1.0)]);
            let provider = SummaryProvider::new(SummaryConfig {
                enabled: true,
                max_summaries: None,
            });
            let rt = tokio::runtime::Runtime::new().unwrap();
            let via_provider = rt.block_on(provider.summaries_for(&sig(), &retrieval));

            for summary in std::iter::once(&direct).chain(via_provider.iter()) {
                // Negative -> dead-end flagged with dead-end phrasing (12.3).
                prop_assert!(summary.dead_end, "a Negative memory must be a dead-end");
                prop_assert!(
                    summary.concluded.to_lowercase().contains("dead-end"),
                    "the conclusion must phrase a learned dead-end: {:?}",
                    summary.concluded,
                );
                // No raw evidence value leaks into either field (12.4).
                prop_assert!(
                    !summary.did.contains(&evidence_marker),
                    "did must not contain the raw evidence marker: {:?}",
                    summary.did,
                );
                prop_assert!(
                    !summary.concluded.contains(&evidence_marker),
                    "concluded must not contain the raw evidence marker: {:?}",
                    summary.concluded,
                );
            }
        }
    }

    // ----- Task 8.7: unit tests — summary content examples -------------------

    #[test]
    fn negative_memory_produces_dead_end_summary() {
        // Requirement 12.3: a Negative memory is summarized as a learned dead-end.
        let mem = negative_memory_with_evidence("EVIDENCE-XYZ12");
        let summary = summarize(&mem);

        assert!(summary.dead_end, "a Negative memory is a dead-end");
        assert!(
            summary.concluded.to_lowercase().contains("dead-end"),
            "the conclusion phrases a learned dead-end: {:?}",
            summary.concluded
        );
        // The raw evidence value is excluded (Requirement 12.4).
        assert!(!summary.did.contains("EVIDENCE-XYZ12"));
        assert!(!summary.concluded.contains("EVIDENCE-XYZ12"));
    }

    #[tokio::test]
    async fn enabled_provider_bounds_and_orders_candidates() {
        // Requirement 11.3: an enabled provider with max_summaries = 2 over three
        // candidates returns two summaries, in retrieval order.
        let candidates = vec![
            scored(memory_with_marker("mem-0", "ref-first"), 3.0),
            scored(memory_with_marker("mem-1", "ref-second"), 2.0),
            scored(memory_with_marker("mem-2", "ref-third"), 1.0),
        ];
        let retrieval = SpyRetrieval::new(candidates);
        let provider = SummaryProvider::new(SummaryConfig {
            enabled: true,
            max_summaries: Some(2),
        });

        let summaries = provider.summaries_for(&sig(), &retrieval).await;

        assert_eq!(summaries.len(), 2, "bounded to max_summaries");
        assert!(
            summaries[0].did.contains("ref-first"),
            "first summary is the first candidate: {:?}",
            summaries[0].did
        );
        assert!(
            summaries[1].did.contains("ref-second"),
            "second summary is the second candidate: {:?}",
            summaries[1].did
        );
    }

    #[tokio::test]
    async fn disabled_provider_returns_no_summaries() {
        // Requirement 13.1: a disabled provider returns none and never consults
        // retrieval.
        let candidates = vec![scored(memory_with_marker("mem-0", "ref-only"), 1.0)];
        let retrieval = SpyRetrieval::new(candidates);
        let provider = SummaryProvider::new(SummaryConfig::default());

        let summaries = provider.summaries_for(&sig(), &retrieval).await;

        assert!(summaries.is_empty(), "a disabled provider yields no summaries");
        assert!(
            !retrieval.called(),
            "a disabled provider does not invoke retrieval"
        );
    }
}
