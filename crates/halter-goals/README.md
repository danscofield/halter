# halter-goals

The Goal Model and procedural-memory acceleration layers (Tier 1 + Tier 2) for halter.

This crate owns:

- **Goal Model** — the hypothesis-shaped `GoalNode` tree, resolution lifecycle,
  `subtree_hash`, `IntentSignature` derivation, persistence on the event-sourced
  session store, and the goal-closure signal that triggers induction.
- **Tier 1** — the deterministic exact-match result cache, argument
  normalization, and the volatility-aware Validity Token Service.
- **Tier 2** — procedural-memory induction, store/retrieval, replay, and
  two-mode answer caching.

It consumes `halter-protocol`, `halter-session`, and `halter-hooks`.
