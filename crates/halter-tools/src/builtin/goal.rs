// pattern: Imperative Shell
//
// `GoalTool` exposes the agent-facing surface for advancing the hypothesis-shaped
// goal tree — the `create`/`revise`/`resolve`/`focus`/`tree` actions described in
// design § "Component 3: `GoalTool`". It mirrors the action-based shape of
// `TaskTool` (see `task.rs`): a single `execute` dispatches on an `action` field,
// each action validates its input, drives the persistence layer, and returns a
// JSON result.
//
// # Persistence and state (architecture decision)
//
// `TaskTool` persists on a per-session in-memory `TaskList` reached through
// `ToolSessionStore`. `GoalTool` persists differently: goal mutations ride the
// event-sourced session log through a shared [`GoalStore`] (the `halter-goals`
// interface, methods keyed by `&SessionId`), so a single store instance serves
// every session. Only the **active-goal stack** is per-session bookkeeping, so it
// is what `ToolSessionStore` scopes per session — exactly the shape the `TaskList`
// accessor uses.
//
// The design places the canonical `ActiveGoalStack` in `halter-runtime`
// (session-owned checkpoint state, rehydrated on resume). `halter-tools` must not
// depend on `halter-runtime` (that edge is a cycle: `halter-runtime` depends on
// `halter-tools`). So the tool carries its own minimal [`GoalStack`] — the same
// push / resolve-to-parent / focus semantics over `GoalNodeId`/`GoalTree` from
// `halter-goals` — as the tool-side view of the active node. Task 9.1/9.2 wire the
// runtime's `ActiveGoalStack` and the tool's stack to the same session state and
// rehydrate on resume; this task delivers the tool actions and the stack they
// drive. The tool never reads or writes the flat `TaskList` (Requirement 7.8).

use std::sync::Arc;

use async_trait::async_trait;
use halter_goals::goal_model::intent::{IntentInput, derive_intent};
use halter_goals::tier2::{GoalSummary, MemoryKind};
use halter_goals::{
    AdvisorySummary, GoalNodeId, GoalNodeRevision, GoalStore, GoalStoreError, GoalTree,
    IntentSignature, Resolution, ScoredMemory, ToolCall as GoalToolCall,
};
use halter_protocol::{
    SessionId, ToolCapabilities, ToolConcurrency, ToolName, ToolResult, ToolSpec,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{Tool, ToolContext};

use super::common::{ToolScope, ensure_not_cancelled, optional_string, required_string};

/// The tool-side active-goal pointer: a stack whose bottom (index 0) is the
/// per-session root once created and whose top is the active node.
///
/// This mirrors `halter_runtime::ActiveGoalStack`'s advancement semantics
/// (push-makes-active, resolve-pops-to-parent with the root never popped, and
/// focus-rebuilds-the-root-to-node-path) without depending on `halter-runtime`
/// (that dependency edge would be a cycle). Task 9.1/9.2 reconcile this tool-side
/// stack with the runtime's canonical stack and rehydrate it on resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalStack {
    /// The root→active path. `stack[0]` is the root once initialized; the last
    /// element is the active node.
    stack: Vec<GoalNodeId>,
}

impl GoalStack {
    /// Create an empty stack with no active node.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The active node id — the top of the stack — or `None` before any node
    /// has been pushed.
    #[must_use]
    pub fn active(&self) -> Option<&GoalNodeId> {
        self.stack.last()
    }

    /// Whether the stack has a root (and thus at least one node).
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Push a freshly created node, making it the active node (Requirement 6.1).
    pub fn push(&mut self, node: GoalNodeId) {
        self.stack.push(node);
    }

    /// Pop the resolved node and any still-active descendants back to its parent,
    /// so the active node becomes the resolved node's parent — or the root when
    /// the resolved node was a direct child of the root (Requirement 6.2).
    ///
    /// The root (index 0) is never popped: resolving the root, or a node not on
    /// the stack, leaves the stack unchanged (Requirements 4.4, 6.3).
    pub fn resolve_to_parent(&mut self, resolved: &GoalNodeId) {
        let Some(position) = self.stack.iter().position(|id| id == resolved) else {
            return;
        };
        if position == 0 {
            return;
        }
        self.stack.truncate(position);
    }

    /// Refocus the active node onto an existing node, rebuilding the stack as the
    /// root→node path by walking the node's parent chain in `tree` (Requirement
    /// 6.6). Returns `false` and leaves the stack unchanged when `node` is absent
    /// from `tree` (Requirement 10.3).
    #[must_use]
    pub fn focus(&mut self, node: &GoalNodeId, tree: &GoalTree) -> bool {
        if !tree.contains(node) {
            return false;
        }
        let mut path = Vec::new();
        let mut cursor = Some(node.clone());
        while let Some(id) = cursor {
            let parent = tree.node(&id).and_then(|n| n.parent.clone());
            path.push(id);
            cursor = parent;
        }
        path.reverse();
        self.stack = path;
        true
    }
}

/// Storage abstraction for the goal tool's per-session active-goal stack.
///
/// Mirrors `TaskStore`: the default in-memory implementation is wired in via
/// `ToolSessionStore`, but a persistent/rehydrating backend (task 9.2) can
/// implement this trait to seed the stack from the folded goal tree on resume.
pub trait GoalStackStore: Send + Sync {
    /// Return the active-goal stack for a session, creating it on first access.
    fn goal_stack(&self, session_id: &SessionId) -> Arc<Mutex<GoalStack>>;
}

/// Default backend used by `ToolSessionStore`: a `DashMap` keyed by session id.
#[derive(Default)]
pub struct InMemoryGoalStackStore {
    sessions: dashmap::DashMap<String, Arc<Mutex<GoalStack>>>,
}

impl GoalStackStore for InMemoryGoalStackStore {
    fn goal_stack(&self, session_id: &SessionId) -> Arc<Mutex<GoalStack>> {
        self.sessions
            .entry(session_id.0.clone())
            .or_insert_with(|| Arc::new(Mutex::new(GoalStack::default())))
            .clone()
    }
}

/// The hot-path advisory capability `GoalTool` consults when a goal opens.
///
/// This seam erases the concrete `MemoryStore`/`EmbeddingSource` generics behind
/// one async method so `GoalTool` needs no Tier 2 type parameters. When a
/// `GoalTool` holds `None` (off mode / no Tier 2 wiring) its behavior is
/// byte-identical to today (Requirement 1.3). This method NEVER runs
/// verification — it returns an advisory only (Requirement 5.4). It never errors:
/// a degraded backend yields an empty or structured-head-only advisory
/// (Requirements 5.5, 10.3).
#[async_trait]
pub trait GoalRetrieval: Send + Sync {
    /// Retrieve the advisory for a newly-opened goal's intent.
    async fn retrieve_advisory(&self, intent: &IntentSignature) -> AdvisorySummary;
}

/// The agent-facing goal tool (design § "Component 3: `GoalTool`").
///
/// Holds a shared [`GoalStore`] handle (methods are keyed by `&SessionId`, so one
/// store serves every session) and reaches the per-session active-goal stack
/// through `ToolContext::tool_sessions`. Mirrors `TaskTool`'s action shape.
///
/// The optional [`GoalRetrieval`] seam folds a short advisory of applicable
/// memories and similar-goal summaries into the `create` result when present;
/// `None` preserves today's behavior exactly (Requirement 1.3).
pub struct GoalTool {
    store: Arc<dyn GoalStore>,
    /// `None` ⇒ byte-identical to today: no advisory is ever folded in.
    retrieval: Option<Arc<dyn GoalRetrieval>>,
}

impl std::fmt::Debug for GoalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalTool")
            .field("store", &"<dyn GoalStore>")
            .field(
                "retrieval",
                &self.retrieval.as_ref().map(|_| "<dyn GoalRetrieval>"),
            )
            .finish()
    }
}

impl GoalTool {
    /// Construct a goal tool over a shared [`GoalStore`] with no advisory seam.
    ///
    /// This is today's constructor: with `retrieval == None` the `create` result
    /// is byte-identical to the pre-Tier-2 shape (Requirement 1.3).
    #[must_use]
    pub fn new(store: Arc<dyn GoalStore>) -> Self {
        Self {
            store,
            retrieval: None,
        }
    }

    /// Construct a goal tool wiring the advisory [`GoalRetrieval`] seam (auto
    /// mode). On `create`, the advisory is folded into the JSON result
    /// (Requirement 5.1, 5.3).
    #[must_use]
    pub fn with_retrieval(store: Arc<dyn GoalStore>, retrieval: Arc<dyn GoalRetrieval>) -> Self {
        Self {
            store,
            retrieval: Some(retrieval),
        }
    }
}

#[async_trait]
impl Tool for GoalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::from("goal"),
            description: "Advance the hypothesis-shaped goal tree scoped to the current \
                session. Pass action='create' with a hypothesis (and optional \
                resolution_conditions and parent) to open a subgoal and make it active; \
                action='revise' with an id to update a node's resolution_conditions, \
                tool_calls, or children; action='resolve' with an id and a resolution \
                ('accepted', 'rejected', or 'inconclusive') to close a node and return to \
                its parent; action='focus' with an id to make an existing node active; or \
                action='tree' to read the current goal tree."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "revise", "resolve", "focus", "tree"]
                    },
                    "hypothesis": {
                        "type": "string",
                        "description": "Required when action='create'. The hypothesis the subgoal tests."
                    },
                    "resolution_conditions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional for action='create' and action='revise'."
                    },
                    "parent": {
                        "type": "string",
                        "description": "Optional node id for action='create'; defaults to the active node."
                    },
                    "id": {
                        "type": "string",
                        "description": "Required when action='revise', 'resolve', or 'focus'."
                    },
                    "resolution": {
                        "type": "string",
                        "enum": ["accepted", "rejected", "inconclusive"],
                        "description": "Required when action='resolve'."
                    }
                },
                "required": ["action"]
            }),
            concurrency: ToolConcurrency::Exclusive,
            capabilities: ToolCapabilities {
                mutating: true,
                requires_approval: false,
                cancellable: false,
                long_running: false,
                ..Default::default()
            },
            provider_aliases: Default::default(),
        }
    }

    async fn execute(&self, context: ToolContext, input: Value) -> anyhow::Result<ToolResult> {
        let _scope = ToolScope::new(&context, "goal");
        ensure_not_cancelled(&context.cancel)?;
        let action = required_string(&input, "action")?;
        let stack = context.tool_sessions.goal_session(&context.session_id);
        let response = match action {
            "create" => {
                self.create_action(&context.session_id, &stack, &input)
                    .await?
            }
            "revise" => self.revise_action(&context.session_id, &input).await?,
            "resolve" => {
                self.resolve_action(&context.session_id, &stack, &input)
                    .await?
            }
            "focus" => {
                self.focus_action(&context.session_id, &stack, &input)
                    .await?
            }
            "tree" => self.tree_action(&context.session_id).await?,
            other => anyhow::bail!(
                "invalid tool input: field 'action' must be one of 'create', 'revise', 'resolve', 'focus', 'tree' (got '{other}')"
            ),
        };
        Ok(ToolResult::json(response))
    }
}

impl GoalTool {
    /// `create`: open a subgoal under `parent` (default: the active node), push
    /// it active, and return its id (Requirement 7.1).
    async fn create_action(
        &self,
        session: &SessionId,
        stack: &Arc<Mutex<GoalStack>>,
        input: &Value,
    ) -> anyhow::Result<Value> {
        let hypothesis = required_string(input, "hypothesis")?.trim();
        if hypothesis.is_empty() {
            anyhow::bail!("invalid tool input: field 'hypothesis' must not be empty");
        }
        let resolution_conditions = optional_string_list(input, "resolution_conditions");

        // Parent defaults to the current active node when omitted.
        let parent = match optional_string(input, "parent") {
            Some(id) => Some(GoalNodeId::from(id)),
            None => stack.lock().active().cloned(),
        };

        // The Goal Model owns intent derivation; derive a signature from the
        // hypothesis so the node carries a full four-field IntentSignature.
        let intent = derive_intent(&intent_input_for(hypothesis))?;

        // Retain a copy of the derived intent for the advisory seam only when it
        // is wired (Requirement 5.1); off mode clones nothing.
        let advisory_intent = self.retrieval.as_ref().map(|_| intent.clone());

        // Drive the store BEFORE mutating the active-goal stack: if the store
        // rejects the create (empty hypothesis, missing parent, OCC conflict, or
        // an unavailable backend), no event is appended and the stack must stay
        // unchanged (Requirements 7.6, 10.1). The stack push happens only after a
        // successful create.
        let id = self
            .store
            .create(session, parent, hypothesis.to_owned(), intent)
            .await
            .map_err(map_store_error)?;

        // Record the optional resolution_conditions as the node's first revision
        // so create carries them (create itself only takes hypothesis + intent).
        if let Some(conditions) = resolution_conditions {
            self.store
                .revise(
                    session,
                    &id,
                    GoalNodeRevision {
                        resolution_conditions: Some(conditions),
                        tool_calls: None,
                        children: None,
                    },
                )
                .await
                .map_err(map_store_error)?;
        }

        stack.lock().push(id.clone());

        let active = stack.lock().active().cloned();
        let mut response = json!({
            "id": id,
            "active": active,
        });

        // Fold the advisory in ONLY when the seam is wired (Requirement 5.1,
        // 5.3). Off mode (`retrieval == None`) leaves `response` untouched, so
        // the result is byte-identical to today with no `advisory` key
        // (Requirement 1.3). The advisory is injected additively and NEVER acted
        // upon (Requirement 5.4); an empty advisory renders empty arrays
        // (Requirement 5.5).
        if let (Some(retrieval), Some(intent)) = (&self.retrieval, &advisory_intent) {
            let advisory = retrieval.retrieve_advisory(intent).await;
            response["advisory"] = render_advisory(&advisory);
        }

        Ok(response)
    }

    /// `revise`: append a `GoalNodeRevised` for an existing node (Requirement 7.2).
    async fn revise_action(&self, session: &SessionId, input: &Value) -> anyhow::Result<Value> {
        let id = GoalNodeId::from(required_string(input, "id")?);
        let revision = GoalNodeRevision {
            resolution_conditions: optional_string_list(input, "resolution_conditions"),
            tool_calls: optional_tool_calls(input)?,
            children: optional_string_list(input, "children")
                .map(|ids| ids.into_iter().map(GoalNodeId::from).collect()),
        };
        // `revise` does not touch the active-goal stack, so an unknown id
        // (`NodeNotFound`), OCC conflict, or backend failure surfaces as a tool
        // error with no event appended and the stack unchanged (Requirements 7.7,
        // 10.1, 10.3).
        self.store
            .revise(session, &id, revision)
            .await
            .map_err(map_store_error)?;
        Ok(json!({ "id": id, "revised": true }))
    }

    /// `resolve`: close a node, pop the stack to its parent, and return the
    /// resolution outcome (including `subtree_hash` on closure) (Requirement 7.3).
    async fn resolve_action(
        &self,
        session: &SessionId,
        stack: &Arc<Mutex<GoalStack>>,
        input: &Value,
    ) -> anyhow::Result<Value> {
        let id = GoalNodeId::from(required_string(input, "id")?);
        let resolution = parse_resolution(required_string(input, "resolution")?)?;

        // Drive the close BEFORE popping the stack: an unknown id
        // (`NodeNotFound`), OCC conflict, or backend failure must surface as a
        // tool error with no event appended and the active node unchanged — so
        // the stack pop happens only after a successful close (Requirements 7.7,
        // 10.1, 10.3).
        let outcome = self
            .store
            .close(session, &id, resolution)
            .await
            .map_err(map_store_error)?;

        // Pop the resolved node (and any still-active descendants) back to its
        // parent; the root is never popped.
        stack.lock().resolve_to_parent(&id);

        let active = stack.lock().active().cloned();
        Ok(json!({
            "id": id,
            "closed": outcome.closed,
            "subtree_hash": outcome.subtree_hash,
            "active": active,
        }))
    }

    /// `focus`: make an existing node the active node, rebuilding the root→node
    /// path (Requirement 7.4).
    async fn focus_action(
        &self,
        session: &SessionId,
        stack: &Arc<Mutex<GoalStack>>,
        input: &Value,
    ) -> anyhow::Result<Value> {
        let id = GoalNodeId::from(required_string(input, "id")?);
        // Project the tree first; a backend failure surfaces as a tool error with
        // the active node unchanged (Requirements 10.1, 10.3).
        let tree = self
            .store
            .get_tree(session)
            .await
            .map_err(map_store_error)?;
        // `focus` leaves the stack unchanged when the id is absent from the tree
        // (Requirements 7.7, 10.3), returning `false`.
        let focused = stack.lock().focus(&id, &tree);
        if !focused {
            anyhow::bail!("failed to execute goal tool: goal node not found: {id}");
        }
        let active = stack.lock().active().cloned();
        Ok(json!({ "id": id, "active": active }))
    }

    /// `tree`: return the current projected goal tree without mutating any state
    /// (Requirement 7.5).
    async fn tree_action(&self, session: &SessionId) -> anyhow::Result<Value> {
        let tree = self
            .store
            .get_tree(session)
            .await
            .map_err(map_store_error)?;
        Ok(json!({ "tree": tree }))
    }
}

/// Map a [`GoalStoreError`] onto a clear, agent-facing tool error.
///
/// Every goal-store rejection surfaces as a tool error while the turn continues;
/// the caller drives the store before mutating the active-goal stack, so a
/// mapped error always leaves the stack (and thus the active node) unchanged
/// (Requirements 7.6, 7.7, 10.1, 10.3). OCC conflicts (`Conflict`) and backend
/// unavailability (`Log`) are surfaced verbatim so the agent can decide whether
/// to retry.
fn map_store_error(error: GoalStoreError) -> anyhow::Error {
    match error {
        GoalStoreError::EmptyHypothesis => {
            anyhow::anyhow!("invalid tool input: field 'hypothesis' must not be empty")
        }
        GoalStoreError::NodeNotFound(id) => {
            anyhow::anyhow!("failed to execute goal tool: goal node not found: {id}")
        }
        GoalStoreError::ParentNotFound(id) => {
            anyhow::anyhow!("failed to execute goal tool: parent node not found: {id}")
        }
        GoalStoreError::Conflict { expected, actual } => anyhow::anyhow!(
            "failed to execute goal tool: goal store conflict (expected head {expected}, \
             found {actual}); the active node is unchanged"
        ),
        GoalStoreError::Log(message) => anyhow::anyhow!(
            "failed to execute goal tool: goal store unavailable: {message}; the active \
             node is unchanged"
        ),
        other => anyhow::anyhow!("failed to execute goal tool: {other}"),
    }
}

/// Build the intent-derivation input from a hypothesis.
///
/// The design does not fully specify the harness-specific source of the four
/// `IntentSignature` fields; this task uses the hypothesis text as a coarse,
/// always-resolvable candidate for each field so a node created through the tool
/// always carries a full four-field signature. Richer derivation (context/tool
/// inspection) is a forward-compatible refinement.
fn intent_input_for(hypothesis: &str) -> IntentInput {
    IntentInput::new("goal", "goal", hypothesis, "session")
}

/// Render an [`AdvisorySummary`] into the agent-facing JSON shape folded into the
/// `create` result (Requirement 5.3).
///
/// Produces `{ "applicable_memories": [...], "similar_goals": [...] }`. An empty
/// advisory renders both as empty arrays (Requirement 5.5). Each memory renders
/// its structured intent, plan step descriptions, kind, and re-rank score —
/// **never** the raw cached-answer value (Requirement 5.4). Each similar goal
/// renders its `{ did, concluded, dead_end }` summary.
fn render_advisory(advisory: &AdvisorySummary) -> Value {
    let applicable_memories: Vec<Value> =
        advisory.memories.iter().map(render_scored_memory).collect();
    let similar_goals: Vec<Value> = advisory.summaries.iter().map(render_goal_summary).collect();
    json!({
        "applicable_memories": applicable_memories,
        "similar_goals": similar_goals,
    })
}

/// Render one scored memory as its intent, plan step descriptions, kind, and
/// score. The raw cached answer value is deliberately omitted (Requirement 5.4).
fn render_scored_memory(scored: &ScoredMemory) -> Value {
    let memory = &scored.memory;
    let plan_steps: Vec<&str> = memory
        .plan
        .steps
        .iter()
        .map(|step| step.description.as_str())
        .collect();
    json!({
        "intent": memory.intent,
        "kind": memory_kind_label(memory.kind),
        "plan_steps": plan_steps,
        "score": scored.score,
    })
}

/// Render one similar-goal summary as `{ did, concluded, dead_end }`.
fn render_goal_summary(summary: &GoalSummary) -> Value {
    json!({
        "did": summary.did,
        "concluded": summary.concluded,
        "dead_end": summary.dead_end,
    })
}

/// A stable lowercase label for a [`MemoryKind`].
fn memory_kind_label(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Fragment => "fragment",
        MemoryKind::Composite => "composite",
        MemoryKind::Negative => "negative",
    }
}

/// Parse a non-`Open` resolution string. `resolve` requires a terminal
/// resolution; `open` (or any unknown value) is rejected.
fn parse_resolution(value: &str) -> anyhow::Result<Resolution> {
    match value {
        "accepted" => Ok(Resolution::Accepted),
        "rejected" => Ok(Resolution::Rejected),
        "inconclusive" => Ok(Resolution::Inconclusive),
        other => anyhow::bail!(
            "invalid tool input: field 'resolution' must be one of 'accepted', 'rejected', 'inconclusive' (got '{other}')"
        ),
    }
}

/// Read an optional array-of-strings field, returning `None` when the field is
/// absent or null.
fn optional_string_list(input: &Value, key: &str) -> Option<Vec<String>> {
    input.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect()
    })
}

/// Read the optional `tool_calls` field, deserializing each element into a
/// [`GoalToolCall`]. Returns `None` when the field is absent or null.
fn optional_tool_calls(input: &Value) -> anyhow::Result<Option<Vec<GoalToolCall>>> {
    match input.get("tool_calls") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let calls: Vec<GoalToolCall> = serde_json::from_value(value.clone())
                .map_err(|err| anyhow::anyhow!("invalid tool input: field 'tool_calls' must be an array of tool-call contracts: {err}"))?;
            Ok(Some(calls))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use halter_goals::{
        ClosureOutcome, EventLogGoalStore, GoalNode, InMemoryGoalEventLog, IntentSignature,
    };
    use halter_protocol::ToolResultKind;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DefaultToolPolicy, NoopToolEventSink, PathLockMap, PolicySettings, ToolPolicy,
        ToolSessionStore,
    };

    use super::*;

    fn goal_tool() -> GoalTool {
        let store = Arc::new(EventLogGoalStore::new(InMemoryGoalEventLog::new()));
        GoalTool::new(store)
    }

    /// The kind of failure a [`FailingGoalStore`] injects on its mutating calls,
    /// modeling the two graceful-degradation paths of Requirement 10.1: an
    /// optimistic-concurrency clash and an unavailable backend.
    #[derive(Clone, Copy)]
    enum StoreFailure {
        /// Simulates an OCC clash (`GoalStoreError::Conflict`).
        Conflict,
        /// Simulates backend unavailability (`GoalStoreError::Log`).
        Unavailable,
    }

    /// A [`GoalStore`] whose mutating calls (`create`/`revise`/`close`) always
    /// fail with the configured [`StoreFailure`], while read projections
    /// (`get`/`get_tree`) succeed against an empty tree.
    ///
    /// Used to prove that a store failure surfaces as a tool error, the call
    /// returns `Err` (the turn continues rather than panicking), and the
    /// active-goal stack is left unchanged (Requirement 10.1).
    struct FailingGoalStore {
        failure: StoreFailure,
    }

    impl FailingGoalStore {
        fn error(&self) -> GoalStoreError {
            match self.failure {
                StoreFailure::Conflict => GoalStoreError::Conflict {
                    expected: 1,
                    actual: 2,
                },
                StoreFailure::Unavailable => {
                    GoalStoreError::Log("session store offline".to_owned())
                }
            }
        }
    }

    #[async_trait]
    impl GoalStore for FailingGoalStore {
        async fn create(
            &self,
            _session: &SessionId,
            _parent: Option<GoalNodeId>,
            _hypothesis: String,
            _intent: IntentSignature,
        ) -> Result<GoalNodeId, GoalStoreError> {
            Err(self.error())
        }

        async fn revise(
            &self,
            _session: &SessionId,
            _id: &GoalNodeId,
            _revision: GoalNodeRevision,
        ) -> Result<(), GoalStoreError> {
            Err(self.error())
        }

        async fn close(
            &self,
            _session: &SessionId,
            _id: &GoalNodeId,
            _resolution: Resolution,
        ) -> Result<ClosureOutcome, GoalStoreError> {
            Err(self.error())
        }

        async fn get(
            &self,
            _session: &SessionId,
            _id: &GoalNodeId,
        ) -> Result<Option<GoalNode>, GoalStoreError> {
            Ok(None)
        }

        async fn get_tree(&self, _session: &SessionId) -> Result<GoalTree, GoalStoreError> {
            Ok(GoalTree::default())
        }
    }

    fn failing_goal_tool(failure: StoreFailure) -> GoalTool {
        GoalTool::new(Arc::new(FailingGoalStore { failure }))
    }

    fn tool_context(sessions: Arc<ToolSessionStore>) -> ToolContext {
        ToolContext {
            session_id: halter_protocol::SessionId::new(),
            working_dir: std::env::current_dir().expect("cwd"),
            path_locks: Arc::new(PathLockMap::default()),
            tool_sessions: sessions,
            snapshot: Arc::new(halter_protocol::ResourceSnapshot::empty()),
            cancel: CancellationToken::new(),
            emit: Arc::new(NoopToolEventSink),
            policy: Arc::new(DefaultToolPolicy::new(PolicySettings::default()))
                as Arc<dyn ToolPolicy>,
            shell_timeout_secs: 30,
            subagent_parent: None,
        }
    }

    fn json_value(result: ToolResult) -> Value {
        match result.kind {
            ToolResultKind::Json { value } => value,
            other => panic!("expected json result, got {other:?}"),
        }
    }

    fn node_id(value: &Value) -> String {
        value.as_str().expect("id is a string").to_owned()
    }

    #[tokio::test]
    async fn create_pushes_new_node_active_and_returns_id() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "the bug is in the parser" }),
            )
            .await
            .expect("create"),
        );
        let id = node_id(&created["id"]);
        assert!(!id.is_empty(), "create must return a node id");
        // The new node becomes the active node.
        assert_eq!(node_id(&created["active"]), id);

        // The node is present in the projected tree.
        let tree = json_value(
            tool.execute(context, json!({ "action": "tree" }))
                .await
                .expect("tree"),
        );
        assert!(
            tree["tree"]["nodes"].get(&id).is_some(),
            "created node must be in the tree: {tree:?}"
        );
    }

    #[tokio::test]
    async fn create_defaults_parent_to_active_node() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let root = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root goal" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&root["id"]);

        let child = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "child goal" }),
            )
            .await
            .expect("create child"),
        );
        let child_id = node_id(&child["id"]);

        // The child's parent should default to the previously active node (root).
        let tree = json_value(
            tool.execute(context, json!({ "action": "tree" }))
                .await
                .expect("tree"),
        );
        assert_eq!(tree["tree"]["nodes"][&child_id]["parent"], root_id);
    }

    #[tokio::test]
    async fn create_records_optional_resolution_conditions() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context.clone(),
                json!({
                    "action": "create",
                    "hypothesis": "cache is stale",
                    "resolution_conditions": ["cache miss observed", "ttl elapsed"]
                }),
            )
            .await
            .expect("create"),
        );
        let id = node_id(&created["id"]);

        let tree = json_value(
            tool.execute(context, json!({ "action": "tree" }))
                .await
                .expect("tree"),
        );
        assert_eq!(
            tree["tree"]["nodes"][&id]["resolution_conditions"],
            json!(["cache miss observed", "ttl elapsed"])
        );
    }

    #[tokio::test]
    async fn resolve_closes_node_and_pops_to_parent() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let root = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&root["id"]);

        let child = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "child" }),
            )
            .await
            .expect("create child"),
        );
        let child_id = node_id(&child["id"]);

        let resolved = json_value(
            tool.execute(
                context,
                json!({ "action": "resolve", "id": child_id, "resolution": "accepted" }),
            )
            .await
            .expect("resolve child"),
        );
        assert_eq!(resolved["closed"], true);
        assert!(
            resolved["subtree_hash"].is_string(),
            "closure must return a subtree_hash: {resolved:?}"
        );
        // Popping the resolved child returns the active node to its parent (root).
        assert_eq!(node_id(&resolved["active"]), root_id);
    }

    #[tokio::test]
    async fn focus_sets_active_to_existing_node() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let root = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&root["id"]);

        let child = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "child" }),
            )
            .await
            .expect("create child"),
        );
        let child_id = node_id(&child["id"]);
        // Active is now the child.
        assert_eq!(node_id(&child["active"]), child_id);

        // Refocus onto the root.
        let focused = json_value(
            tool.execute(context, json!({ "action": "focus", "id": root_id }))
                .await
                .expect("focus root"),
        );
        assert_eq!(node_id(&focused["active"]), root_id);
    }

    #[tokio::test]
    async fn revise_updates_resolution_conditions() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "hypothesis" }),
            )
            .await
            .expect("create"),
        );
        let id = node_id(&created["id"]);

        tool.execute(
            context.clone(),
            json!({
                "action": "revise",
                "id": id,
                "resolution_conditions": ["new condition"]
            }),
        )
        .await
        .expect("revise");

        let tree = json_value(
            tool.execute(context, json!({ "action": "tree" }))
                .await
                .expect("tree"),
        );
        assert_eq!(
            tree["tree"]["nodes"][&id]["resolution_conditions"],
            json!(["new condition"])
        );
    }

    #[tokio::test]
    async fn tree_is_read_only() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        tool.execute(
            context.clone(),
            json!({ "action": "create", "hypothesis": "only node" }),
        )
        .await
        .expect("create");

        let before = json_value(
            tool.execute(context.clone(), json!({ "action": "tree" }))
                .await
                .expect("tree before"),
        );
        let after = json_value(
            tool.execute(context, json!({ "action": "tree" }))
                .await
                .expect("tree after"),
        );
        assert_eq!(before, after, "tree must not mutate state");
    }

    #[tokio::test]
    async fn create_rejects_empty_hypothesis() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));
        let error = tool
            .execute(context, json!({ "action": "create", "hypothesis": "   " }))
            .await
            .expect_err("empty hypothesis is rejected");
        assert!(
            error
                .to_string()
                .contains("field 'hypothesis' must not be empty")
        );
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));
        let error = tool
            .execute(context, json!({ "action": "delete", "id": "x" }))
            .await
            .expect_err("unknown action is rejected");
        assert!(
            error
                .to_string()
                .contains("must be one of 'create', 'revise', 'resolve', 'focus', 'tree'")
        );
    }

    #[tokio::test]
    async fn goals_are_isolated_per_session() {
        let sessions = Arc::new(ToolSessionStore::default());
        let tool = goal_tool();
        let context_a = tool_context(sessions.clone());
        let context_b = tool_context(sessions);

        tool.execute(
            context_a,
            json!({ "action": "create", "hypothesis": "session A goal" }),
        )
        .await
        .expect("create A");

        // Session B starts with no active node, so create defaults to a
        // parentless root of its own.
        let created_b = json_value(
            tool.execute(
                context_b,
                json!({ "action": "create", "hypothesis": "session B goal" }),
            )
            .await
            .expect("create B"),
        );
        // B's new node is active and is its own root (no parent), proving the
        // active-goal stack did not leak from session A.
        assert_eq!(node_id(&created_b["active"]), node_id(&created_b["id"]));
    }

    // --- Task 8.2: input validation and error handling --------------------

    /// Read the tool-side active node id for a session (the top of the
    /// active-goal stack), used to assert the stack is unchanged after a
    /// rejected action.
    fn active_node(context: &ToolContext) -> Option<GoalNodeId> {
        context
            .tool_sessions
            .goal_session(&context.session_id)
            .lock()
            .active()
            .cloned()
    }

    #[tokio::test]
    async fn create_empty_hypothesis_leaves_stack_unchanged() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        // Seed an active node so we can prove the failed create does not push.
        let created = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&created["id"]);
        assert_eq!(
            active_node(&context).map(|id| id.0.clone()),
            Some(root_id.clone())
        );

        // A whitespace-only hypothesis is rejected before any store call or push
        // (Requirement 7.6).
        let error = tool
            .execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "   " }),
            )
            .await
            .expect_err("blank hypothesis is rejected");
        assert!(
            error
                .to_string()
                .contains("field 'hypothesis' must not be empty")
        );

        // The active node is unchanged: no new node was pushed.
        assert_eq!(active_node(&context).map(|id| id.0), Some(root_id));
    }

    #[tokio::test]
    async fn revise_unknown_id_is_rejected_and_stack_unchanged() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&created["id"]);

        // Revising an id absent from the folded tree surfaces a not-found tool
        // error, appends no event, and leaves the stack unchanged (Requirement
        // 7.7, 10.3).
        let error = tool
            .execute(
                context.clone(),
                json!({ "action": "revise", "id": "does-not-exist", "resolution_conditions": ["x"] }),
            )
            .await
            .expect_err("unknown id on revise is rejected");
        assert!(
            error.to_string().contains("goal node not found"),
            "expected a not-found tool error, got: {error}"
        );
        assert_eq!(active_node(&context).map(|id| id.0), Some(root_id));
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_rejected_and_stack_unchanged() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&created["id"]);

        // Resolving an unknown id must NOT pop the stack: the store's close()
        // errors with NodeNotFound before any stack mutation (Requirement 7.7,
        // 10.3).
        let error = tool
            .execute(
                context.clone(),
                json!({ "action": "resolve", "id": "does-not-exist", "resolution": "accepted" }),
            )
            .await
            .expect_err("unknown id on resolve is rejected");
        assert!(
            error.to_string().contains("goal node not found"),
            "expected a not-found tool error, got: {error}"
        );
        assert_eq!(active_node(&context).map(|id| id.0), Some(root_id));
    }

    #[tokio::test]
    async fn focus_unknown_id_is_rejected_and_stack_unchanged() {
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "root" }),
            )
            .await
            .expect("create root"),
        );
        let root_id = node_id(&created["id"]);

        // Focusing an id absent from the folded tree is rejected and leaves the
        // active node unchanged (Requirement 7.7, 10.3).
        let error = tool
            .execute(
                context.clone(),
                json!({ "action": "focus", "id": "does-not-exist" }),
            )
            .await
            .expect_err("unknown id on focus is rejected");
        assert!(
            error.to_string().contains("goal node not found"),
            "expected a not-found tool error, got: {error}"
        );
        assert_eq!(active_node(&context).map(|id| id.0), Some(root_id));
    }

    #[tokio::test]
    async fn create_store_conflict_surfaces_tool_error_active_node_unchanged() {
        // A store OCC conflict during create surfaces as a tool error (Err from
        // execute, so the turn continues), appends no event, and leaves the
        // active node unchanged (Requirement 10.1).
        let tool = failing_goal_tool(StoreFailure::Conflict);
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let error = tool
            .execute(
                context.clone(),
                json!({ "action": "create", "hypothesis": "will fail" }),
            )
            .await
            .expect_err("store conflict surfaces as a tool error");
        assert!(
            error.to_string().contains("goal store conflict"),
            "expected an OCC conflict tool error, got: {error}"
        );
        // No node was pushed: the active node is still absent.
        assert_eq!(active_node(&context), None);
    }

    #[tokio::test]
    async fn resolve_store_unavailable_surfaces_tool_error_active_node_unchanged() {
        // Backend unavailability during resolve surfaces as a tool error and
        // must not pop the stack, so the previously active node is unchanged
        // (Requirement 10.1).
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        // Seed a tool-side active node directly so resolve has something to (not)
        // pop; the failing store never mutates it.
        let seeded = GoalNodeId::from("active-node");
        context
            .tool_sessions
            .goal_session(&context.session_id)
            .lock()
            .push(seeded.clone());

        let tool = failing_goal_tool(StoreFailure::Unavailable);
        let error = tool
            .execute(
                context.clone(),
                json!({ "action": "resolve", "id": "active-node", "resolution": "accepted" }),
            )
            .await
            .expect_err("store unavailability surfaces as a tool error");
        assert!(
            error.to_string().contains("goal store unavailable"),
            "expected an unavailability tool error, got: {error}"
        );
        // The active node is unchanged: close() failed before resolve_to_parent.
        assert_eq!(active_node(&context), Some(seeded));
    }

    // --- Task 10.4: the GoalRetrieval advisory seam -----------------------

    use halter_goals::tier2::{
        EvidenceContract, GoalSummary, Memory, MemoryKind, MemoryVersion, OutcomeShape, PlanStep,
        Provenance, Reinforcement,
    };
    use halter_goals::tier2::memory::{Applicability, ParameterSchema, Plan};
    use halter_goals::{
        AdvisorySummary, MemoryId, OutcomeRef, ScoredMemory, Sha256, SubtreeHash,
    };

    /// A fake [`GoalRetrieval`] that returns a fixed [`AdvisorySummary`].
    struct FakeRetrieval {
        advisory: AdvisorySummary,
    }

    #[async_trait]
    impl GoalRetrieval for FakeRetrieval {
        async fn retrieve_advisory(&self, _intent: &IntentSignature) -> AdvisorySummary {
            self.advisory.clone()
        }
    }

    /// Build a minimal `Memory` carrying the given intent, kind, and plan step
    /// descriptions for advisory-rendering assertions.
    fn sample_memory(kind: MemoryKind, steps: &[&str]) -> Memory {
        Memory {
            id: MemoryId::from("mem-advisory"),
            kind,
            intent: IntentSignature {
                intent_type: halter_goals::IntentType::from("lookup"),
                target_type: halter_goals::TargetType::from("file"),
                target_ref: halter_goals::TargetRef::from("src/lib.rs"),
                scope: halter_goals::Scope::from("repo"),
            },
            parameter_schema: ParameterSchema::default(),
            applicability: Applicability::unconstrained(),
            plan: Plan {
                steps: steps
                    .iter()
                    .map(|d| PlanStep {
                        description: (*d).to_owned(),
                        tool: None,
                        intent: None,
                    })
                    .collect(),
            },
            evidence_contract: EvidenceContract::default(),
            outcome_shape: OutcomeShape {
                result_ref: OutcomeRef::from("outcome://1"),
                schema: "s".to_owned(),
            },
            cached_outcome: None,
            provenance: Provenance::default(),
            reinforcement: Reinforcement::default(),
            version: MemoryVersion(SubtreeHash(Sha256::from("h"))),
        }
    }

    fn goal_tool_with_retrieval(advisory: AdvisorySummary) -> GoalTool {
        let store = Arc::new(EventLogGoalStore::new(InMemoryGoalEventLog::new()));
        GoalTool::with_retrieval(store, Arc::new(FakeRetrieval { advisory }))
    }

    #[tokio::test]
    async fn off_mode_create_result_has_no_advisory_key() {
        // `GoalTool::new` (no retrieval seam) must emit today's byte-identical
        // shape: exactly `{ id, active }` with no `advisory` key (Requirement
        // 1.3).
        let tool = goal_tool();
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context,
                json!({ "action": "create", "hypothesis": "off-mode goal" }),
            )
            .await
            .expect("create"),
        );

        assert!(created.get("id").is_some(), "id must be present");
        assert!(created.get("active").is_some(), "active must be present");
        assert!(
            created.get("advisory").is_none(),
            "off mode must not fold in an advisory key: {created:?}"
        );
        let object = created.as_object().expect("create result is an object");
        assert_eq!(
            object.len(),
            2,
            "off-mode create result must be exactly {{ id, active }}: {created:?}"
        );
    }

    #[tokio::test]
    async fn with_retrieval_folds_empty_advisory_arrays() {
        // An empty advisory renders empty arrays for both keys (Requirement 5.5).
        let tool = goal_tool_with_retrieval(AdvisorySummary::empty());
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context,
                json!({ "action": "create", "hypothesis": "advisory goal" }),
            )
            .await
            .expect("create"),
        );

        let advisory = &created["advisory"];
        assert_eq!(
            advisory["applicable_memories"],
            json!([]),
            "empty advisory renders empty applicable_memories: {created:?}"
        );
        assert_eq!(
            advisory["similar_goals"],
            json!([]),
            "empty advisory renders empty similar_goals: {created:?}"
        );
    }

    #[tokio::test]
    async fn with_retrieval_folds_non_empty_advisory() {
        // A non-empty advisory folds the memories + summaries into the two
        // arrays, rendering intent / plan steps / kind / score and the goal
        // summary fields (Requirement 5.3).
        let memory = sample_memory(MemoryKind::Negative, &["inspect parser", "run tests"]);
        let advisory = AdvisorySummary {
            memories: vec![ScoredMemory {
                memory,
                score: 0.75,
            }],
            summaries: vec![GoalSummary {
                did: "looked up the parser".to_owned(),
                concluded: "found the off-by-one".to_owned(),
                dead_end: false,
            }],
        };
        let tool = goal_tool_with_retrieval(advisory);
        let context = tool_context(Arc::new(ToolSessionStore::default()));

        let created = json_value(
            tool.execute(
                context,
                json!({ "action": "create", "hypothesis": "advisory goal" }),
            )
            .await
            .expect("create"),
        );

        let memories = created["advisory"]["applicable_memories"]
            .as_array()
            .expect("applicable_memories is an array");
        assert_eq!(memories.len(), 1, "one memory folded in: {created:?}");
        let mem = &memories[0];
        assert_eq!(mem["kind"], "negative");
        assert_eq!(mem["score"], json!(0.75));
        assert_eq!(mem["plan_steps"], json!(["inspect parser", "run tests"]));
        assert_eq!(mem["intent"]["intent_type"], "lookup");
        // The raw cached-answer value is never rendered (Requirement 5.4).
        assert!(
            mem.get("cached_outcome").is_none() && mem.get("cached_answer").is_none(),
            "advisory must not leak the cached answer: {created:?}"
        );

        let goals = created["advisory"]["similar_goals"]
            .as_array()
            .expect("similar_goals is an array");
        assert_eq!(goals.len(), 1, "one summary folded in: {created:?}");
        assert_eq!(goals[0]["did"], "looked up the parser");
        assert_eq!(goals[0]["concluded"], "found the off-by-one");
        assert_eq!(goals[0]["dead_end"], false);
    }
}
