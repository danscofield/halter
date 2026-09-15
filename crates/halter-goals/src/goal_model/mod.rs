//! The Goal Model — the hypothesis-shaped `GoalNode` tree, its resolution
//! lifecycle, `subtree_hash` computation, `IntentSignature` derivation,
//! persistence on the event-sourced session store, and the goal-closure signal.
//!
//! This module is populated incrementally by the Goal Model tasks. The data
//! model (the [`GoalNode`] structure, the [`Resolution`] lifecycle, the
//! projected [`GoalTree`] state, and the append-only [`GoalEvent`] payloads)
//! lives in [`model`]. The deterministic fold lives in [`fold`], `subtree_hash`
//! computation in [`subtree_hash`], and `IntentSignature` derivation/attachment
//! in [`intent`]. The goal-closure signal and the async induction-enqueue seam
//! ([`ClosureSignal`], [`InductionQueue`]) live in [`closure`]. The [`GoalStore`]
//! over the event-sourced session log — the
//! trait, the [`ClosureOutcome`] result, the [`GoalStoreError`] error space, and
//! the [`GoalEventLog`] append/replay abstraction that realizes the single
//! gap-free sequence-ordered log — lives in [`store`].

pub mod closure;
pub mod fold;
pub mod intent;
pub mod model;
pub mod store;
pub mod subtree_hash;

pub use closure::{
    ClosureSignal, InductionQueue, NoopInductionQueue, RecordingInductionQueue,
    SharedInductionQueue,
};
pub use fold::{apply_goal_event, fold_goal_events};
pub use intent::{
    attached_intent, derive_intent, IntentDerivationError, IntentField, IntentInput,
};
pub use model::{GoalEvent, GoalNode, GoalNodeRevision, GoalTree, GoalTreeState, Resolution};
pub use store::{
    ClosureOutcome, EventLogGoalStore, GoalEventLog, GoalStore, GoalStoreError,
    InMemoryGoalEventLog, SequencedGoalEvent,
};
pub use subtree_hash::subtree_hash;
