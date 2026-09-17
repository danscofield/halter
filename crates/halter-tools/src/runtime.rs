// pattern: Imperative Shell

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use halter_protocol::{
    CloseSubagentRequest, CloseSubagentResponse, ResourceSnapshot, SendSubagentInputRequest,
    SessionBlueprint, SessionId, SessionState, SpawnSubagentRequest, SubagentStatus,
    ToolConcurrency, ToolResult, ToolSpec, WaitSubagentRequest, WaitSubagentResponse,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{CacheKey, PathLockMap, ToolPolicy, ToolResultStore, ToolSessionStore};

/// Default tool-result cache TTL, mirroring
/// `halter_config::DEFAULT_TOOL_CACHE_TTL_SECS`.
///
/// This is only the field's default value on a freshly constructed runtime; the
/// effective TTL is always the one supplied to [`ToolRuntime::with_cache`].
const DEFAULT_TOOL_CACHE_TTL_SECS: u64 = 60;

/// Determine whether a tool's calls are eligible for the tool-result cache.
///
/// A tool is cacheable iff it is explicitly opted in (`capabilities.cacheable`)
/// AND it is side-effect-free: non-mutating and declared read-only or
/// parallel-safe. Mutating or `Exclusive` tools are never cacheable, and the
/// opt-in defaults to `false`, so caching never suppresses a tool with side
/// effects (Req 15.1, 15.2, 15.3, 15.4, Q1).
pub(crate) fn is_cacheable(spec: &ToolSpec) -> bool {
    spec.capabilities.cacheable
        && !spec.capabilities.mutating
        && matches!(
            spec.concurrency,
            ToolConcurrency::ReadOnly | ToolConcurrency::ParallelSafe
        )
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Runtime event emitted while a tool executes.
pub enum ToolRuntimeEvent {
    Started { tool_name: String },
    Completed { tool_name: String },
    ToolOutput { tool_name: String, chunk: String },
}

/// Sink for tool runtime events.
pub trait ToolEventSink: Send + Sync {
    /// Emit one tool event.
    fn emit(&self, event: ToolRuntimeEvent);
}

#[derive(Debug, Default, PartialEq, Eq)]
/// Tool event sink that drops all events.
pub struct NoopToolEventSink;

impl ToolEventSink for NoopToolEventSink {
    fn emit(&self, _event: ToolRuntimeEvent) {}
}

#[derive(Debug, Clone)]
/// Parent-session material available to subagent tools.
pub struct SubagentParentContext {
    pub blueprint: SessionBlueprint,
    pub state: SessionState,
    pub snapshot: Arc<ResourceSnapshot>,
    pub model: halter_protocol::ModelId,
    pub subagent_model: halter_protocol::ModelId,
}

#[async_trait]
/// Control plane used by the built-in subagent tools.
pub trait SubagentControl: Send + Sync {
    /// Spawn a subagent from a parent session context.
    async fn spawn(
        &self,
        parent: &SubagentParentContext,
        request: SpawnSubagentRequest,
    ) -> anyhow::Result<SubagentStatus>;
    /// Send additional input to a subagent after its current turn is terminal.
    async fn send_input(&self, request: SendSubagentInputRequest)
    -> anyhow::Result<SubagentStatus>;
    /// Wait for subagent progress or completion.
    async fn wait(&self, request: WaitSubagentRequest) -> anyhow::Result<WaitSubagentResponse>;
    /// Close a subagent and return its previous status.
    async fn close(&self, request: CloseSubagentRequest) -> anyhow::Result<CloseSubagentResponse>;
}

#[derive(Debug, Default)]
/// Subagent control implementation used when subagents are unavailable.
pub struct NoopSubagentControl;

#[async_trait]
impl SubagentControl for NoopSubagentControl {
    async fn spawn(
        &self,
        _parent: &SubagentParentContext,
        _request: SpawnSubagentRequest,
    ) -> anyhow::Result<SubagentStatus> {
        anyhow::bail!("failed to execute subagent tool: subagent control is unavailable")
    }

    async fn send_input(
        &self,
        _request: SendSubagentInputRequest,
    ) -> anyhow::Result<SubagentStatus> {
        anyhow::bail!("failed to execute subagent tool: subagent control is unavailable")
    }

    async fn wait(&self, _request: WaitSubagentRequest) -> anyhow::Result<WaitSubagentResponse> {
        anyhow::bail!("failed to execute subagent tool: subagent control is unavailable")
    }

    async fn close(&self, _request: CloseSubagentRequest) -> anyhow::Result<CloseSubagentResponse> {
        anyhow::bail!("failed to execute subagent tool: subagent control is unavailable")
    }
}

#[derive(Clone)]
/// Per-execution context passed to every tool.
pub struct ToolContext {
    pub session_id: SessionId,
    pub working_dir: PathBuf,
    pub path_locks: Arc<PathLockMap>,
    pub tool_sessions: Arc<ToolSessionStore>,
    pub snapshot: Arc<ResourceSnapshot>,
    pub cancel: CancellationToken,
    pub emit: Arc<dyn ToolEventSink>,
    pub policy: Arc<dyn ToolPolicy>,
    pub shell_timeout_secs: u64,
    pub subagent_parent: Option<Arc<SubagentParentContext>>,
}

#[async_trait]
/// Trait implemented by built-in and custom tools.
pub trait Tool: Send + Sync {
    /// Provider-visible tool specification.
    fn spec(&self) -> ToolSpec;
    /// Execute the tool with validated runtime context and raw JSON input.
    async fn execute(&self, context: ToolContext, input: Value) -> anyhow::Result<ToolResult>;
}

/// Registry and dispatcher for tools.
pub struct ToolRuntime {
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    /// Whether the tool-result cache is active. Defaults to `false`; only
    /// [`with_cache`](ToolRuntime::with_cache) turns it on (Req 13.2, 13.4).
    cache_enabled: bool,
    /// The time-to-live applied to cached tool results. The default is a valid
    /// placeholder; the effective TTL is set via
    /// [`with_cache`](ToolRuntime::with_cache).
    cache_ttl: Duration,
    /// The backing store for cached tool results, or `None` when caching is
    /// disabled (the default, and the state after `clone_filtered`).
    store: Option<Arc<dyn ToolResultStore>>,
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            cache_enabled: false,
            cache_ttl: Duration::from_secs(DEFAULT_TOOL_CACHE_TTL_SECS),
            store: None,
        }
    }
}

impl ToolRuntime {
    /// Create an empty tool runtime with caching disabled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable the tool-result cache, installing `store` and applying `ttl` to
    /// cached entries (Req 13.2, 13.4).
    pub fn with_cache(&mut self, store: Arc<dyn ToolResultStore>, ttl: Duration) {
        self.cache_enabled = true;
        self.store = Some(store);
        self.cache_ttl = ttl;
    }

    /// Register or replace a tool by its canonical spec name.
    pub fn register(&self, tool: Arc<dyn Tool>) {
        let spec = tool.spec();
        debug!(tool_name = %spec.name, "registering tool");
        self.tools
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(spec.name.0, tool);
    }

    /// Returns tool specs sorted alphabetically by canonical name.
    /// A stable order keeps the tools section of the prompt byte-identical
    /// across requests, which is the boundary that prefix caches key on.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|tool| tool.spec())
            .collect();
        specs.sort_by(|a, b| a.name.0.cmp(&b.name.0));
        specs
    }

    /// Look up the declared [`halter_protocol::ToolConcurrency`] for a registered tool by name.
    ///
    /// Returns `None` when the tool is not registered, letting the caller pick
    /// a conservative default (e.g. `Exclusive`) rather than guessing.
    #[must_use]
    pub fn concurrency_for(&self, name: &str) -> Option<halter_protocol::ToolConcurrency> {
        self.tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .map(|tool| tool.spec().concurrency)
    }

    /// Clone a runtime containing only allowed tools.
    ///
    /// An empty allowlist means all registered tools are retained.
    #[must_use]
    pub fn clone_filtered(&self, allowed: &[String]) -> Self {
        let allow_all = allowed.is_empty();
        let allowed = allowed.iter().collect::<std::collections::HashSet<_>>();
        let tools = self
            .tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(name, _)| allow_all || allowed.contains(name))
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
        // A filtered clone never inherits the cache: it starts fresh with
        // caching disabled and no store (Req 13.2, 13.4).
        Self {
            tools: RwLock::new(tools),
            cache_enabled: false,
            cache_ttl: Duration::from_secs(DEFAULT_TOOL_CACHE_TTL_SECS),
            store: None,
        }
    }

    /// Execute a registered tool by name.
    ///
    /// When the tool-result cache is enabled and the tool is cacheable
    /// (Req 15), an identical repeated call is short-circuited: within the TTL
    /// window the stored result is returned tagged as a cache hit without
    /// invoking the tool (Req 14.3/16.2/17.1); on a miss, expiry, or
    /// non-canonicalizable arguments the tool is invoked and — on success — its
    /// untagged result is stored under the resolved TTL (Req 14.4/14.6/16.3),
    /// while an error propagates and stores nothing (Req 14.7/17.2). When the
    /// cache is disabled/absent or the tool is not cacheable the dispatch is
    /// byte-identical to the uncached path (Req 13.4/15.3/17.3).
    pub async fn execute(
        &self,
        name: &str,
        context: ToolContext,
        input: Value,
    ) -> anyhow::Result<ToolResult> {
        // Look up the tool and release the read lock before any await so we
        // never hold the lock across an await point.
        let tool = self
            .tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| {
                warn!(tool_name = name, "attempted to execute unknown tool");
                anyhow::anyhow!("failed to execute tool: unknown tool '{}'", name)
            })?;

        debug!(session_id = %context.session_id, tool_name = name, "dispatching tool execution");

        // Short-circuit to a byte-identical dispatch when the cache is disabled
        // or absent, or the tool is not cacheable (Req 13.4/15.3/17.3).
        let spec = tool.spec();
        let Some(store) = self.store.as_ref().filter(|_| self.cache_enabled) else {
            return tool.execute(context, input).await;
        };
        if !is_cacheable(&spec) {
            return tool.execute(context, input).await;
        }

        // Compute the per-session cache key. Non-canonicalizable arguments
        // bypass the cache entirely: invoke directly with no get/put (Req 14.2).
        // `input` is cloned because `tool.execute` consumes it below.
        let Some(key) = CacheKey::compute(&context.session_id, name, &input) else {
            return tool.execute(context, input).await;
        };
        let key_string = key.to_key_string();

        // On a hit within the TTL window, deserialize and return tagged without
        // invoking the tool (Req 14.3/16.2/17.1). Corrupt or legacy bytes that
        // fail to deserialize fall through to a miss (Req 14.5) — we do not
        // return, we invoke the tool.
        if let Some(bytes) = store.get(&key_string).await
            && let Ok(result) = serde_json::from_slice::<ToolResult>(&bytes)
        {
            debug!(tool_name = name, "serving tool result from cache");
            return Ok(result.with_cache_hit(true));
        }

        // Miss / expiry / undecodable: invoke the tool.
        let result = tool.execute(context, input).await?;

        // On success, serialize the untagged result and store it under the
        // resolved TTL, then return it untagged (Req 14.4/14.6/16.3). On error
        // the `?` above already propagated and stored nothing (Req 14.7/17.2).
        match serde_json::to_vec(&result) {
            Ok(bytes) => store.put(key_string, bytes, self.cache_ttl).await,
            Err(error) => {
                // A result that cannot be serialized is simply not cached; the
                // fresh result is still returned untagged.
                warn!(tool_name = name, %error, "failed to serialize tool result for cache");
            }
        }
        Ok(result)
    }
}
