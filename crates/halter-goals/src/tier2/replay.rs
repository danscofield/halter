//! Tier 2 — the Replay Engine (`decide_replay`).
//!
//! This module is where Tier 2's single non-negotiable soundness guarantee
//! lives: **a cached answer is served `verified = true` only when every one of
//! its validity tokens is confirmed to hold at serve time** (Requirements 20.x).
//! Everything else in the engine is arranged so that guarantee is obvious and
//! testable.
//!
//! [`decide_replay`] follows a *fixed* decision order (Requirement 19):
//!
//! 1. **Answer-cache check first.** If the memory carries a [`CachedOutcome`]:
//!    - **Mode A** ([`AnswerMode::SoundPinnable`]) — serve verified iff *all*
//!      tokens hold; any stale token, or Tier 1 being unavailable, falls through
//!      to replay. A Mode A outcome is *never* served believed-unverified: it is
//!      served verified or not at all (Requirements 20.1–20.4).
//!    - **Mode B** ([`AnswerMode::BoundedVolatile`]) — serve *believed-unverified*
//!      (`verified = false`) iff the intent type has opted in **and** the answer's
//!      age (`now - issued_at`) is within `max_age`; otherwise fall through. Mode B
//!      is opt-in per intent type and never active by default (Requirements
//!      21.1–21.5).
//!    - If there is no `cached_outcome`, the check is skipped entirely
//!      (Requirement 19.3).
//! 2. **Procedure-replay fallback.** Re-validate *every* evidence-contract item
//!    through the validator, reporting `evidence_fully_fresh` only when all items
//!    are [`Freshness::Fresh`]. If Tier 1 is unavailable, all evidence is treated
//!    stale and `evidence_fully_fresh` is `false` (Requirements 23.1–23.4).
//!
//! ## The `EvidenceValidator` bridge
//!
//! The design's pseudocode calls `t1.revalidate(e.tool, e.normalized_args,
//! e.validity_token)` and `allTokensHold(co.validity_tokens)`. But
//! [`Tier1Cache::revalidate`](crate::tier1::cache::Tier1Cache::revalidate)'s real
//! signature also needs a [`SourceDescriptor`](crate::types::SourceDescriptor)
//! and a [`SourceProvider`](crate::tier1::tokens::SourceProvider) to re-observe
//! the source — and it is generic over the provider, which makes `Tier1Cache`
//! *not* object-safe. Meanwhile an [`EvidenceContractItem`] deliberately carries
//! only `tool + normalized_args + validity_token` and *no* `SourceDescriptor`
//! (Tier 2 stores contracts, not sources).
//!
//! To reconcile this without leaking Tier 1's generics into the engine, the
//! engine depends on a small abstraction, [`EvidenceValidator`], that answers the
//! only two questions `decide_replay` actually asks:
//!
//! - [`EvidenceValidator::revalidate_item`] — is *this* evidence item's token
//!   still fresh? (bridges to `Tier1Cache::revalidate`); and
//! - [`EvidenceValidator::tokens_hold`] — do *these* answer tokens all still
//!   hold, and if not, is that because they are stale or because Tier 1 is
//!   unavailable? ([`TokensHold`] tri-state).
//!
//! The concrete, Tier 1-backed validator is responsible for resolving each
//! token/item back to its `SourceDescriptor` and driving `Tier1Cache::revalidate`
//! / [`holds`](crate::tier1::tokens::holds) on the hot path; wiring that adapter
//! is task 16.2. This module ships the trait plus an in-memory
//! [`MapEvidenceValidator`] used by the engine's own tests, mirroring the
//! pragmatic-abstraction pattern used by earlier Tier 2 tasks. The tri-state
//! return lets the engine express Requirement 23.4 ("Tier 1 unavailable ⇒ treat
//! all evidence as stale, serve no verified answer") cleanly and locally.

use crate::tier1::cache::Freshness;
use crate::tier2::memory::{
    AnswerMode, CachedOutcome, EvidenceContractItem, Memory, OutcomeValue, Plan,
};
use crate::types::{IntentSignature, IntentType, Timestamp, ValidityToken};

/// The engine's decision for a retrieved memory.
///
/// Either serve the memory's cached answer (with a `verified` flag whose meaning
/// is the crux of the soundness guarantee), or fall through to replaying the
/// procedure with freshly re-validated evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDecision {
    /// Serve the memory's cached answer.
    ///
    /// `verified = true` is returned **only** when every one of the answer's
    /// validity tokens was confirmed to hold at serve time (Mode A). `verified =
    /// false` marks a believed-unverified Mode B answer (opt-in, within
    /// `max_age`).
    ServeAnswer {
        /// The cached answer to serve.
        answer: OutcomeValue,
        /// Whether the answer is soundly verified. `true` iff all Mode A tokens
        /// held at serve time; `false` iff served believed-unverified (Mode B).
        verified: bool,
    },
    /// Fall through to replaying the procedure, carrying each evidence item with
    /// its re-validated freshness.
    ReplayProcedure {
        /// The reusable plan to replay.
        plan: Plan,
        /// Each evidence-contract item paired with its re-validated freshness.
        evidence: Vec<(EvidenceContractItem, Freshness)>,
        /// `true` iff every evidence item re-validated as [`Freshness::Fresh`].
        evidence_fully_fresh: bool,
    },
}

/// Whether a set of answer validity tokens all currently hold — a tri-state that
/// distinguishes "stale" from "Tier 1 unavailable".
///
/// Only [`TokensHold::AllHold`] permits a verified Mode A serve; both
/// [`TokensHold::SomeStale`] and [`TokensHold::Unavailable`] force a fall-through
/// to replay (Requirements 20.2, 23.4). The variants are kept distinct so the
/// engine — and its tests — can assert *why* a Mode A answer was not served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokensHold {
    /// Every token was confirmed to hold at serve time.
    AllHold,
    /// At least one token did not hold (but Tier 1 was reachable).
    SomeStale,
    /// Tier 1 was unavailable, so no token could be confirmed; treated fail-safe
    /// as not-holding (Requirement 23.4).
    Unavailable,
}

/// Bridges the engine to Tier 1 re-validation without leaking Tier 1's generics
/// (or non-object-safe signature) into [`decide_replay`].
///
/// The engine only ever needs two answers, captured here. Implementations resolve
/// each item/token back to its source and drive
/// [`Tier1Cache::revalidate`](crate::tier1::cache::Tier1Cache::revalidate) /
/// [`holds`](crate::tier1::tokens::holds); an unreachable source is reported
/// fail-safe (`Stale` / [`TokensHold::Unavailable`]).
pub trait EvidenceValidator {
    /// Re-validate a single evidence-contract item's token through Tier 1.
    ///
    /// Returns [`Freshness::Fresh`] iff the item's token still holds; otherwise
    /// (stale token or unreachable source) [`Freshness::Stale`] (Requirements
    /// 23.1, 23.3).
    fn revalidate_item(&self, item: &EvidenceContractItem) -> Freshness;

    /// Report whether *all* of a cached answer's `tokens` currently hold.
    ///
    /// Returns [`TokensHold::AllHold`] iff every token holds, [`TokensHold::SomeStale`]
    /// if at least one is stale while Tier 1 is reachable, or
    /// [`TokensHold::Unavailable`] if Tier 1 cannot be reached at all
    /// (Requirements 20.1–20.4, 23.4).
    fn tokens_hold(&self, tokens: &[ValidityToken]) -> TokensHold;
}

/// Whether a given intent type has opted into Mode B (bounded-volatile) serving.
///
/// Mode B is **opt-in per intent type and never the default** (Requirement 21.5):
/// the provided default, [`DenyAllModeB`], opts *nothing* in, so Mode B stays off
/// unless a policy is explicitly configured to allow an intent type (see
/// [`AllowListModeB`]).
pub trait ModeBPolicy {
    /// Whether `intent_type` has opted into Mode B serving.
    fn opted_in(&self, intent_type: &IntentType) -> bool;
}

/// The safe default Mode B policy: **no** intent type is opted in.
///
/// With this policy `decide_replay` never serves a Mode B answer — a
/// `BoundedVolatile` outcome always falls through to procedure replay
/// (Requirement 21.5).
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllModeB;

impl ModeBPolicy for DenyAllModeB {
    fn opted_in(&self, _intent_type: &IntentType) -> bool {
        false
    }
}

/// A configurable Mode B policy that opts in an explicit allow-list of intent
/// types.
///
/// Only intent types present in the list are opted in; every other intent type
/// (and, in particular, the empty allow-list) is denied, preserving the
/// never-default guarantee (Requirement 21.5).
#[derive(Debug, Clone, Default)]
pub struct AllowListModeB {
    allowed: Vec<IntentType>,
}

impl AllowListModeB {
    /// An allow-list opting in exactly the given intent types.
    #[must_use]
    pub fn new(allowed: Vec<IntentType>) -> Self {
        Self { allowed }
    }

    /// Opt an additional `intent_type` into Mode B.
    #[must_use]
    pub fn allow(mut self, intent_type: IntentType) -> Self {
        self.allowed.push(intent_type);
        self
    }
}

impl ModeBPolicy for AllowListModeB {
    fn opted_in(&self, intent_type: &IntentType) -> bool {
        self.allowed.contains(intent_type)
    }
}

/// The age of a cached answer at serve time: `now - issued_at`, saturating at
/// zero if `issued_at` is somehow in the future (clock skew), so age is never
/// negative and a future-stamped answer is treated as maximally fresh.
fn answer_age(now: Timestamp, issued_at: Timestamp) -> crate::types::Duration {
    crate::types::Duration(now.0.saturating_sub(issued_at.0))
}

/// Decide, in fixed order, whether to serve `mem`'s cached answer or replay its
/// procedure (Requirements 19–23).
///
/// The order is non-negotiable: the answer-cache check runs first, and the
/// procedure-replay fallback runs only when the cache yields nothing servable.
///
/// # The single guarantee
///
/// `ReplayDecision::ServeAnswer { verified: true }` is returned **only** on the
/// Mode A path, and only when [`EvidenceValidator::tokens_hold`] reports
/// [`TokensHold::AllHold`]. There is no other code path that sets `verified =
/// true`; Mode B always sets `verified = false`, and every non-serving path
/// returns [`ReplayDecision::ReplayProcedure`]. This makes the invariant local
/// and auditable.
///
/// # Arguments
///
/// - `mem` — the retrieved memory (its applicability already holds for `sig`).
/// - `validator` — bridges to Tier 1 re-validation (see [`EvidenceValidator`]).
/// - `mode_b_policy` — decides Mode B opt-in per intent type (see [`ModeBPolicy`]).
/// - `sig` — the querying intent signature (its `intent_type` drives Mode B opt-in).
/// - `now` — the serve-time instant, used to compute a Mode B answer's age.
pub fn decide_replay(
    mem: &Memory,
    validator: &impl EvidenceValidator,
    mode_b_policy: &impl ModeBPolicy,
    sig: &IntentSignature,
    now: Timestamp,
) -> ReplayDecision {
    // ---- Step 1: answer-cache check FIRST (Requirement 19.1) ----
    // Skipped entirely when there is no cached_outcome (Requirement 19.3).
    // When there is a cached_outcome and it yields a servable answer, return it.
    // Otherwise (no cached_outcome per 19.3, or no servable answer: stale Mode A /
    // not-opted-in or too-old Mode B per 19.2, 20.2, 21.3, 21.4) fall through to
    // the procedure-replay fallback.
    if let Some(co) = &mem.cached_outcome
        && let Some(decision) = try_serve_answer(co, validator, mode_b_policy, sig, now)
    {
        return decision;
    }

    // ---- Step 2: procedure-replay fallback (Requirements 19.3, 23.x) ----
    replay_procedure(mem, validator)
}

/// The answer-cache check. Returns `Some(decision)` iff a cached answer can be
/// served (Mode A verified, or Mode B believed-unverified); `None` means fall
/// through to procedure replay.
fn try_serve_answer(
    co: &CachedOutcome,
    validator: &impl EvidenceValidator,
    mode_b_policy: &impl ModeBPolicy,
    sig: &IntentSignature,
    now: Timestamp,
) -> Option<ReplayDecision> {
    match co.mode {
        // ---- Mode A: sound, pinnable (Requirements 20.1–20.4) ----
        AnswerMode::SoundPinnable => {
            // Serve verified ONLY when every token is confirmed to hold. Any
            // stale token OR Tier 1 unavailability falls through — a Mode A
            // outcome is never served believed-unverified (20.2, 20.3, 23.4).
            match validator.tokens_hold(&co.validity_tokens) {
                TokensHold::AllHold => Some(ReplayDecision::ServeAnswer {
                    answer: co.answer.clone(),
                    // The ONE place verified = true is ever produced.
                    verified: true,
                }),
                TokensHold::SomeStale | TokensHold::Unavailable => None,
            }
        }
        // ---- Mode B: bounded-volatile (Requirements 21.1–21.5) ----
        AnswerMode::BoundedVolatile { max_age } => {
            // Opt-in per intent type, never default (21.1, 21.5) AND within
            // max_age (21.2); otherwise fall through (21.3, 21.4).
            let opted_in = mode_b_policy.opted_in(&sig.intent_type);
            let within_age = answer_age(now, co.issued_at) <= max_age;
            if opted_in && within_age {
                Some(ReplayDecision::ServeAnswer {
                    answer: co.answer.clone(),
                    // Mode B is ALWAYS believed-unverified (21.3).
                    verified: false,
                })
            } else {
                None
            }
        }
    }
}

/// Re-validate every evidence-contract item through the validator and build the
/// procedure-replay decision (Requirements 23.1–23.4).
///
/// `evidence_fully_fresh` is `true` iff every item re-validated as
/// [`Freshness::Fresh`]; a single [`Freshness::Stale`] (including the
/// Tier 1-unavailable case, which the validator surfaces as `Stale` per item)
/// makes it `false`.
fn replay_procedure(mem: &Memory, validator: &impl EvidenceValidator) -> ReplayDecision {
    let mut evidence = Vec::with_capacity(mem.evidence_contract.items.len());
    let mut all_fresh = true;
    for item in &mem.evidence_contract.items {
        let status = validator.revalidate_item(item);
        if status == Freshness::Stale {
            // INVARIANT: once any item is stale, the replay is not fully fresh.
            all_fresh = false;
        }
        evidence.push((item.clone(), status));
    }
    ReplayDecision::ReplayProcedure {
        plan: mem.plan.clone(),
        evidence,
        evidence_fully_fresh: all_fresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier2::memory::{
        Applicability, EvidenceContract, MemoryKind, MemoryVersion, OutcomeShape, ParameterSchema,
        PlanStep, Provenance, Reinforcement,
    };
    use crate::types::{
        CanonicalJson, Duration, MemoryId, OutcomeRef, Scope, Sha256, SubtreeHash, TargetRef,
        TargetType, ToolName,
    };
    use std::collections::HashMap;

    // --- test validator ---------------------------------------------------

    /// An in-memory [`EvidenceValidator`] driven by explicit per-token freshness,
    /// with an "unavailable" switch modelling Tier 1 being down.
    ///
    /// Tokens are keyed by a stable string form so tests can flip individual
    /// tokens fresh/stale. When `unavailable` is set, `tokens_hold` reports
    /// [`TokensHold::Unavailable`] and `revalidate_item` reports every item
    /// [`Freshness::Stale`] (Requirement 23.4).
    struct MapEvidenceValidator {
        fresh_tokens: HashMap<String, bool>,
        unavailable: bool,
    }

    impl MapEvidenceValidator {
        fn new() -> Self {
            Self {
                fresh_tokens: HashMap::new(),
                unavailable: false,
            }
        }

        fn with_token(mut self, token: &ValidityToken, fresh: bool) -> Self {
            self.fresh_tokens.insert(token_key(token), fresh);
            self
        }

        fn unavailable() -> Self {
            Self {
                fresh_tokens: HashMap::new(),
                unavailable: true,
            }
        }
    }

    fn token_key(token: &ValidityToken) -> String {
        format!("{token:?}")
    }

    impl EvidenceValidator for MapEvidenceValidator {
        fn revalidate_item(&self, item: &EvidenceContractItem) -> Freshness {
            if self.unavailable {
                // Tier 1 down => treat every item as stale (Requirement 23.4).
                return Freshness::Stale;
            }
            match self.fresh_tokens.get(&token_key(&item.validity_token)) {
                Some(true) => Freshness::Fresh,
                _ => Freshness::Stale,
            }
        }

        fn tokens_hold(&self, tokens: &[ValidityToken]) -> TokensHold {
            if self.unavailable {
                return TokensHold::Unavailable;
            }
            if tokens
                .iter()
                .all(|t| self.fresh_tokens.get(&token_key(t)).copied().unwrap_or(false))
            {
                TokensHold::AllHold
            } else {
                TokensHold::SomeStale
            }
        }
    }

    // --- fixtures ---------------------------------------------------------

    fn sig(intent: &str) -> IntentSignature {
        IntentSignature {
            intent_type: IntentType::from(intent),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        }
    }

    fn content_token(h: &str) -> ValidityToken {
        ValidityToken::ContentHash(Sha256::from(h))
    }

    fn ttl_token(issued_at: u64, ttl: u64) -> ValidityToken {
        ValidityToken::Ttl {
            issued_at: Timestamp(issued_at),
            ttl: Duration(ttl),
        }
    }

    fn contract_item(tool: &str, args: &str, token: ValidityToken) -> EvidenceContractItem {
        EvidenceContractItem {
            tool: ToolName::from(tool),
            normalized_args: CanonicalJson(args.to_owned()),
            validity_token: token,
        }
    }

    /// Build a memory with the given cached outcome and evidence contract items.
    fn memory_with(
        cached_outcome: Option<CachedOutcome>,
        items: Vec<EvidenceContractItem>,
    ) -> Memory {
        Memory {
            id: MemoryId::from("mem-replay"),
            kind: MemoryKind::Fragment,
            intent: sig("lookup"),
            parameter_schema: ParameterSchema::default(),
            applicability: Applicability::unconstrained(),
            plan: Plan {
                steps: vec![PlanStep {
                    description: "read the file".to_owned(),
                    tool: Some(ToolName::from("read_file")),
                    intent: None,
                }],
            },
            evidence_contract: EvidenceContract { items },
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "file-contents".to_owned(),
            },
            cached_outcome,
            provenance: Provenance::default(),
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        }
    }

    fn sound_outcome(tokens: Vec<ValidityToken>) -> CachedOutcome {
        CachedOutcome {
            answer: OutcomeValue(CanonicalJson("\"answer\"".to_owned())),
            mode: AnswerMode::SoundPinnable,
            validity_tokens: tokens,
            issued_at: Timestamp(0),
        }
    }

    fn volatile_outcome(
        tokens: Vec<ValidityToken>,
        max_age: u64,
        issued_at: u64,
    ) -> CachedOutcome {
        CachedOutcome {
            answer: OutcomeValue(CanonicalJson("\"volatile-answer\"".to_owned())),
            mode: AnswerMode::BoundedVolatile {
                max_age: Duration(max_age),
            },
            validity_tokens: tokens,
            issued_at: Timestamp(issued_at),
        }
    }

    // --- Requirement 19: fixed decision order -----------------------------

    #[test]
    fn no_cached_outcome_skips_answer_check_and_replays() {
        // Requirements 19.2, 19.3: no cached_outcome => go straight to replay.
        let token = content_token("e1");
        let mem = memory_with(None, vec![contract_item("read_file", "{}", token.clone())]);
        let validator = MapEvidenceValidator::new().with_token(&token, true);

        let decision = decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        match decision {
            ReplayDecision::ReplayProcedure {
                evidence,
                evidence_fully_fresh,
                ..
            } => {
                assert_eq!(evidence.len(), 1);
                assert!(evidence_fully_fresh);
            }
            other => panic!("expected ReplayProcedure, got {other:?}"),
        }
    }

    #[test]
    fn answer_served_before_replay_is_reached() {
        // Requirement 19.1: with BOTH a servable Mode A answer AND evidence, the
        // answer is served and replay is never reached. Prove the latter by making
        // every evidence item stale: if replay ran, we'd see it, but we don't.
        let answer_token = content_token("a1");
        let evidence_token = content_token("e-stale");
        let mem = memory_with(
            Some(sound_outcome(vec![answer_token.clone()])),
            vec![contract_item("read_file", "{}", evidence_token.clone())],
        );
        let validator = MapEvidenceValidator::new()
            .with_token(&answer_token, true)
            .with_token(&evidence_token, false);

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        assert_eq!(
            decision,
            ReplayDecision::ServeAnswer {
                answer: OutcomeValue(CanonicalJson("\"answer\"".to_owned())),
                verified: true,
            },
            "servable Mode A answer must be served before replay is considered"
        );
    }

    // --- Requirement 20: Mode A verified serving --------------------------

    #[test]
    fn mode_a_all_tokens_hold_serves_verified() {
        // Requirements 20.1, 20.3.
        let t1 = content_token("a1");
        let t2 = content_token("a2");
        let mem = memory_with(Some(sound_outcome(vec![t1.clone(), t2.clone()])), vec![]);
        let validator = MapEvidenceValidator::new()
            .with_token(&t1, true)
            .with_token(&t2, true);

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        assert_eq!(
            decision,
            ReplayDecision::ServeAnswer {
                answer: OutcomeValue(CanonicalJson("\"answer\"".to_owned())),
                verified: true,
            }
        );
    }

    #[test]
    fn mode_a_one_token_stale_falls_through_not_verified() {
        // Requirements 20.2, 20.4: any stale token => never verified, fall through.
        let t1 = content_token("a1");
        let t2 = content_token("a2");
        let mem = memory_with(
            Some(sound_outcome(vec![t1.clone(), t2.clone()])),
            vec![contract_item("read_file", "{}", content_token("e1"))],
        );
        let validator = MapEvidenceValidator::new()
            .with_token(&t1, true)
            .with_token(&t2, false); // one stale

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        assert!(
            matches!(decision, ReplayDecision::ReplayProcedure { .. }),
            "a stale Mode A token must fall through to replay, never serve verified"
        );
    }

    #[test]
    fn mode_a_tier1_unavailable_falls_through_not_verified() {
        // Requirements 20.x, 23.3/23.4: Tier 1 down => not verified, fall through.
        let t1 = content_token("a1");
        let mem = memory_with(
            Some(sound_outcome(vec![t1])),
            vec![contract_item("read_file", "{}", content_token("e1"))],
        );
        let validator = MapEvidenceValidator::unavailable();

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        match decision {
            ReplayDecision::ReplayProcedure {
                evidence_fully_fresh,
                evidence,
                ..
            } => {
                assert!(!evidence_fully_fresh, "unavailable Tier 1 => not fully fresh");
                assert!(
                    evidence.iter().all(|(_, f)| *f == Freshness::Stale),
                    "unavailable Tier 1 treats all evidence as stale"
                );
            }
            other => panic!("expected fall-through to replay, got {other:?}"),
        }
    }

    // --- Requirement 21: Mode B bounded-volatile serving ------------------

    #[test]
    fn mode_b_opted_in_within_max_age_serves_believed_unverified() {
        // Requirements 21.1, 21.2, 21.3.
        let token = ttl_token(0, 1_000);
        let mem = memory_with(Some(volatile_outcome(vec![token], 100, 10)), vec![]);
        let validator = MapEvidenceValidator::new();
        let policy = AllowListModeB::new(vec![IntentType::from("lookup")]);

        // age = now(50) - issued_at(10) = 40 <= max_age(100)
        let decision = decide_replay(&mem, &validator, &policy, &sig("lookup"), Timestamp(50));
        assert_eq!(
            decision,
            ReplayDecision::ServeAnswer {
                answer: OutcomeValue(CanonicalJson("\"volatile-answer\"".to_owned())),
                verified: false,
            },
            "opted-in Mode B within max_age serves believed-unverified"
        );
    }

    #[test]
    fn mode_b_not_opted_in_falls_through() {
        // Requirements 21.1, 21.3, 21.5: default deny-all never activates Mode B.
        let token = ttl_token(0, 1_000);
        let mem = memory_with(
            Some(volatile_outcome(vec![token], 100, 10)),
            vec![contract_item("read_file", "{}", content_token("e1"))],
        );
        let validator = MapEvidenceValidator::new().with_token(&content_token("e1"), true);

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(50));
        assert!(
            matches!(decision, ReplayDecision::ReplayProcedure { .. }),
            "Mode B must not activate without opt-in"
        );
    }

    #[test]
    fn mode_b_opted_in_but_too_old_falls_through() {
        // Requirements 21.2, 21.4: age > max_age => fall through.
        let token = ttl_token(0, 1_000);
        let mem = memory_with(
            Some(volatile_outcome(vec![token], 100, 10)),
            vec![contract_item("read_file", "{}", content_token("e1"))],
        );
        let validator = MapEvidenceValidator::new().with_token(&content_token("e1"), true);
        let policy = AllowListModeB::new(vec![IntentType::from("lookup")]);

        // age = now(200) - issued_at(10) = 190 > max_age(100)
        let decision = decide_replay(&mem, &validator, &policy, &sig("lookup"), Timestamp(200));
        assert!(
            matches!(decision, ReplayDecision::ReplayProcedure { .. }),
            "an over-age Mode B answer must fall through to replay"
        );
    }

    #[test]
    fn mode_b_boundary_age_equal_max_age_serves() {
        // Requirement 21.1: age <= max_age is inclusive at the boundary.
        let token = ttl_token(0, 1_000);
        let mem = memory_with(Some(volatile_outcome(vec![token], 100, 10)), vec![]);
        let validator = MapEvidenceValidator::new();
        let policy = AllowListModeB::new(vec![IntentType::from("lookup")]);

        // age = now(110) - issued_at(10) = 100 == max_age(100)
        let decision = decide_replay(&mem, &validator, &policy, &sig("lookup"), Timestamp(110));
        assert_eq!(
            decision,
            ReplayDecision::ServeAnswer {
                answer: OutcomeValue(CanonicalJson("\"volatile-answer\"".to_owned())),
                verified: false,
            }
        );
    }

    // --- Requirement 23: evidence re-validation on replay -----------------

    #[test]
    fn replay_evidence_fully_fresh_when_all_fresh() {
        // Requirements 23.1, 23.2.
        let e1 = content_token("e1");
        let e2 = content_token("e2");
        let mem = memory_with(
            None,
            vec![
                contract_item("read_file", "{\"p\":1}", e1.clone()),
                contract_item("read_file", "{\"p\":2}", e2.clone()),
            ],
        );
        let validator = MapEvidenceValidator::new()
            .with_token(&e1, true)
            .with_token(&e2, true);

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        match decision {
            ReplayDecision::ReplayProcedure {
                evidence,
                evidence_fully_fresh,
                ..
            } => {
                assert!(evidence_fully_fresh);
                assert!(evidence.iter().all(|(_, f)| *f == Freshness::Fresh));
            }
            other => panic!("expected ReplayProcedure, got {other:?}"),
        }
    }

    #[test]
    fn replay_not_fully_fresh_when_any_stale() {
        // Requirement 23.3: one stale item => not fully fresh, but still returns
        // the plan and per-item statuses.
        let e1 = content_token("e1");
        let e2 = content_token("e2");
        let mem = memory_with(
            None,
            vec![
                contract_item("read_file", "{\"p\":1}", e1.clone()),
                contract_item("read_file", "{\"p\":2}", e2.clone()),
            ],
        );
        let validator = MapEvidenceValidator::new()
            .with_token(&e1, true)
            .with_token(&e2, false); // stale

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        match decision {
            ReplayDecision::ReplayProcedure {
                evidence,
                evidence_fully_fresh,
                ..
            } => {
                assert!(!evidence_fully_fresh);
                assert_eq!(evidence[0].1, Freshness::Fresh);
                assert_eq!(evidence[1].1, Freshness::Stale);
            }
            other => panic!("expected ReplayProcedure, got {other:?}"),
        }
    }

    #[test]
    fn replay_tier1_unavailable_treats_all_evidence_stale() {
        // Requirement 23.4: Tier 1 down => every item stale, not fully fresh.
        let e1 = content_token("e1");
        let e2 = content_token("e2");
        let mem = memory_with(
            None,
            vec![
                contract_item("read_file", "{\"p\":1}", e1),
                contract_item("read_file", "{\"p\":2}", e2),
            ],
        );
        let validator = MapEvidenceValidator::unavailable();

        let decision =
            decide_replay(&mem, &validator, &DenyAllModeB, &sig("lookup"), Timestamp(0));
        match decision {
            ReplayDecision::ReplayProcedure {
                evidence,
                evidence_fully_fresh,
                ..
            } => {
                assert!(!evidence_fully_fresh);
                assert!(evidence.iter().all(|(_, f)| *f == Freshness::Stale));
            }
            other => panic!("expected ReplayProcedure, got {other:?}"),
        }
    }

    // --- policy sanity ----------------------------------------------------

    #[test]
    fn deny_all_policy_opts_nothing_in() {
        // Requirement 21.5.
        let policy = DenyAllModeB;
        assert!(!policy.opted_in(&IntentType::from("lookup")));
        assert!(!policy.opted_in(&IntentType::from("anything")));
    }

    #[test]
    fn allow_list_policy_opts_in_only_listed_types() {
        let policy = AllowListModeB::default().allow(IntentType::from("lookup"));
        assert!(policy.opted_in(&IntentType::from("lookup")));
        assert!(!policy.opted_in(&IntentType::from("mutate")));
    }
}
