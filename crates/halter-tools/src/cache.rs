//! Tool-result cache backend abstraction.
//!
//! Capability B interposes a time-window cache of tool *results* inside
//! [`crate::ToolRuntime`]. The concrete storage is abstracted behind the async
//! [`ToolResultStore`] trait so the in-memory implementation is just the first
//! backend and durable/remote backends (Redis, sqlite) can be added later as
//! config-only changes.
//!
//! The value boundary is raw bytes ([`Vec<u8>`]): the runtime owns
//! serialization/deserialization and cache-hit tagging, while the store owns
//! keying storage and expiry (TTL). A backend serves a stored value only while
//! it is within its TTL window and returns [`None`] once it has expired.

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use halter_goals::normalize_args;
use halter_protocol::SessionId;
use serde_json::Value;

/// The composite lookup key for the tool-result cache (Req 14.1, Q3).
///
/// A cache entry is scoped to a `(session, tool, normalized_args)` triple:
///
/// * `session` — the [`SessionId`] the call was made in. Embedding it makes the
///   cache **per-session** (Q3), so a result stored in one session can never be
///   served to another.
/// * `tool` — the tool name, so calls to different tools never collide.
/// * `normalized_args` — the canonical, byte-stable argument form produced by
///   the existing `normalize_args` canonicalization, so that logically-equal
///   calls (e.g. reordered object keys, equivalent number encodings) key to the
///   same entry (Req 14.1).
///
/// The key is rendered to a single flat string by
/// [`to_key_string`](CacheKey::to_key_string), which every backend uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// The session the call belongs to (per-session scope, Q3).
    pub session: SessionId,
    /// The tool name.
    pub tool: String,
    /// The canonical argument bytes produced by `normalize_args`.
    pub normalized_args: String,
}

impl CacheKey {
    /// Compute a [`CacheKey`] for a call, canonicalizing `input` via the shared
    /// `normalize_args` (Req 14.1).
    ///
    /// Returns [`None`] when `normalize_args` reports the arguments
    /// non-canonicalizable (Req 14.2) — for example a non-finite number or a
    /// duplicate object key. In that case the caller must bypass the cache and
    /// invoke the tool directly, reading and writing no entry.
    #[must_use]
    pub fn compute(session: &SessionId, tool: &str, input: &Value) -> Option<Self> {
        let canonical = normalize_args(&tool.into(), input).ok()?;
        Some(Self {
            session: session.clone(),
            tool: tool.to_owned(),
            normalized_args: canonical.0,
        })
    }

    /// Render the key to a single flat, **injective** string.
    ///
    /// Each of the three components is length-prefixed with its byte length and
    /// a `:` delimiter, in the fixed order `session`, `tool`, `normalized_args`:
    ///
    /// ```text
    /// "{len(session)}:{session}{len(tool)}:{tool}{len(args)}:{args}"
    /// ```
    ///
    /// # Injectivity
    ///
    /// The rendering is injective: two distinct `(session, tool,
    /// normalized_args)` triples can never render to the same string. The byte
    /// length that prefixes each component makes the boundary between components
    /// unambiguous, so a delimiter or digit appearing *inside* a component can
    /// never be mistaken for a component boundary. Reading a rendered string
    /// left to right, each component is recovered by parsing its length prefix
    /// up to the `:`, then taking exactly that many bytes — a deterministic,
    /// unambiguous parse. Because the parse is unique, the render is injective:
    /// equal key strings imply equal triples, and distinct triples imply
    /// distinct key strings.
    #[must_use]
    pub fn to_key_string(&self) -> String {
        let session = self.session.0.as_str();
        let tool = self.tool.as_str();
        let args = self.normalized_args.as_str();
        format!(
            "{}:{session}{}:{tool}{}:{args}",
            session.len(),
            tool.len(),
            args.len(),
        )
    }
}

/// Async backend trait for the tool-result cache.
///
/// Implementations map a flat string key to opaque bytes with a per-entry
/// time-to-live. The store owns expiry: [`get`](ToolResultStore::get) returns
/// the stored bytes only while the entry is within its TTL window and [`None`]
/// once it has expired (or was never stored).
///
/// The value boundary is bytes ([`Vec<u8>`]); the runtime is responsible for
/// serializing the tool result into bytes before [`put`](ToolResultStore::put)
/// and deserializing on retrieval, as well as any cache-hit tagging.
#[async_trait]
pub trait ToolResultStore: Send + Sync {
    /// Return the stored bytes for `key` if an entry exists and is still within
    /// its TTL window; otherwise return [`None`].
    async fn get(&self, key: &str) -> Option<Vec<u8>>;

    /// Store `value` under `key` with a time-to-live of `ttl`, replacing any
    /// existing entry for `key`. The store owns the resulting expiry deadline.
    async fn put(&self, key: String, value: Vec<u8>, ttl: Duration);
}

/// Default global entry cap for [`InMemoryToolResultStore`].
///
/// The in-memory backend is a bounded cache: once the number of live entries
/// reaches this cap, [`put`](ToolResultStore::put) evicts the entry with the
/// nearest (soonest) expiry deadline to make room for the incoming entry.
pub const DEFAULT_MEMORY_STORE_CAPACITY: usize = 4096;

/// In-memory, TTL-bounded [`ToolResultStore`] backend.
///
/// Entries are stored in a [`RwLock`]-guarded [`HashMap`] keyed by the flat
/// cache key. Each value is a `(bytes, deadline)` pair where the deadline is an
/// [`Instant`] equal to `stored_at + ttl`.
///
/// * [`get`](ToolResultStore::get) returns the stored bytes only while
///   `Instant::now() <= deadline`; an expired (or missing) entry yields
///   [`None`].
/// * [`put`](ToolResultStore::put) first performs a lazy sweep that drops all
///   expired entries, then—if the map is still at or above the configured
///   capacity—evicts the entry with the nearest deadline before inserting the
///   new entry.
///
/// Lock poisoning is recovered transparently via
/// [`PoisonError::into_inner`], so a panic in an unrelated holder never wedges
/// the cache.
#[derive(Debug)]
pub struct InMemoryToolResultStore {
    entries: RwLock<HashMap<String, (Vec<u8>, Instant)>>,
    capacity: usize,
}

impl InMemoryToolResultStore {
    /// Construct a store with the [`DEFAULT_MEMORY_STORE_CAPACITY`] entry cap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MEMORY_STORE_CAPACITY)
    }

    /// Construct a store with an explicit global entry cap.
    ///
    /// A capacity of `0` is clamped to `1` so at least the most recently
    /// inserted entry can be retained.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Insert `(value, deadline)` directly, applying the same lazy-sweep and
    /// nearest-deadline eviction policy as [`ToolResultStore::put`].
    ///
    /// This is the shared synchronous core used by the async trait method and
    /// keeps the whole operation deterministic for testing (the deadline is
    /// supplied by the caller rather than computed from a live clock).
    fn insert(&self, key: String, value: Vec<u8>, deadline: Instant, now: Instant) {
        let mut guard = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner);

        // (1) Lazy expiry sweep: drop every entry whose deadline has passed.
        guard.retain(|_, (_, entry_deadline)| now <= *entry_deadline);

        // (2) If we are still at/over the cap (and the key is not an in-place
        // replacement), evict the entry with the nearest (soonest) deadline.
        let at_capacity = !guard.contains_key(&key) && guard.len() >= self.capacity;
        if at_capacity
            && let Some(evict_key) = guard
                .iter()
                .min_by_key(|(_, (_, entry_deadline))| *entry_deadline)
                .map(|(k, _)| k.clone())
        {
            guard.remove(&evict_key);
        }

        // (3) Insert the new entry, replacing any existing entry for `key`.
        guard.insert(key, (value, deadline));
    }
}

impl Default for InMemoryToolResultStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolResultStore for InMemoryToolResultStore {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let now = Instant::now();
        let guard = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        guard
            .get(key)
            .filter(|(_, deadline)| now <= *deadline)
            .map(|(bytes, _)| bytes.clone())
    }

    async fn put(&self, key: String, value: Vec<u8>, ttl: Duration) {
        let now = Instant::now();
        let deadline = now + ttl;
        self.insert(key, value, deadline, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hit within the TTL window returns the stored bytes.
    #[tokio::test]
    async fn get_returns_bytes_within_ttl() {
        let store = InMemoryToolResultStore::new();
        store
            .put("k".to_string(), b"hello".to_vec(), Duration::from_secs(60))
            .await;

        assert_eq!(store.get("k").await, Some(b"hello".to_vec()));
    }

    /// A missing key is a miss.
    #[tokio::test]
    async fn get_missing_key_is_none() {
        let store = InMemoryToolResultStore::new();
        assert_eq!(store.get("absent").await, None);
    }

    /// An entry whose deadline is in the past is a miss (deterministic: the
    /// deadline is inserted directly in the past).
    #[test]
    fn get_after_expiry_is_none() {
        let store = InMemoryToolResultStore::new();
        let now = Instant::now();
        let past = now - Duration::from_secs(1);
        store.insert("k".to_string(), b"stale".to_vec(), past, now);

        let guard = store.entries.read().unwrap();
        let live = guard
            .get("k")
            .filter(|(_, deadline)| Instant::now() <= *deadline)
            .map(|(bytes, _)| bytes.clone());
        assert_eq!(live, None);
    }

    /// Reaching the cap evicts the entry with the nearest (soonest) deadline,
    /// preserving entries with later deadlines.
    #[test]
    fn cap_eviction_removes_nearest_deadline() {
        let store = InMemoryToolResultStore::with_capacity(2);
        let now = Instant::now();

        // "soon" expires first, "later" second — both still live.
        let soon = now + Duration::from_secs(10);
        let later = now + Duration::from_secs(100);
        store.insert("soon".to_string(), b"s".to_vec(), soon, now);
        store.insert("later".to_string(), b"l".to_vec(), later, now);

        // Inserting a third entry at capacity must evict "soon" (nearest).
        let newest = now + Duration::from_secs(1000);
        store.insert("newest".to_string(), b"n".to_vec(), newest, now);

        let guard = store.entries.read().unwrap();
        assert_eq!(guard.len(), 2);
        assert!(!guard.contains_key("soon"), "nearest-deadline entry evicted");
        assert!(guard.contains_key("later"));
        assert!(guard.contains_key("newest"));
    }

    /// The lazy sweep in `insert` drops expired entries before applying the
    /// capacity check, so an expired entry never counts against the cap.
    #[test]
    fn put_sweeps_expired_before_cap_check() {
        let store = InMemoryToolResultStore::with_capacity(2);
        let now = Instant::now();

        // One already-expired entry and one live entry.
        store.insert(
            "expired".to_string(),
            b"e".to_vec(),
            now - Duration::from_secs(1),
            now,
        );
        store.insert(
            "live".to_string(),
            b"v".to_vec(),
            now + Duration::from_secs(100),
            now,
        );

        // At capacity by count, but "expired" is swept, so no live entry is
        // evicted and the new entry is added alongside "live".
        store.insert(
            "fresh".to_string(),
            b"f".to_vec(),
            now + Duration::from_secs(100),
            now,
        );

        let guard = store.entries.read().unwrap();
        assert!(!guard.contains_key("expired"));
        assert!(guard.contains_key("live"));
        assert!(guard.contains_key("fresh"));
        assert_eq!(guard.len(), 2);
    }

    // --- CacheKey ---------------------------------------------------------

    use serde_json::json;

    fn session() -> SessionId {
        SessionId::from("sess-1")
    }

    /// `compute` returns `Some` for canonicalizable arguments (Req 14.1).
    #[test]
    fn compute_some_for_canonicalizable_args() {
        let key = CacheKey::compute(&session(), "read_file", &json!({ "path": "a.rs" }));
        assert!(key.is_some());
        let key = key.unwrap();
        assert_eq!(key.session, session());
        assert_eq!(key.tool, "read_file");
        assert_eq!(key.normalized_args, r#"{"path":"a.rs"}"#);
    }

    /// `compute` returns `Some`/`None` exactly when `normalize_args` reports
    /// success/failure, so a non-canonicalizable call bypasses the cache
    /// (Req 14.2). The two documented reject inputs — non-finite numbers and
    /// duplicate object keys — cannot be constructed through the safe
    /// `serde_json` surface (`from_f64` and the parser reject non-finite
    /// numbers, and `Map` dedups keys), so their bypass is covered by
    /// `normalize`'s own tests; here we pin the delegation contract: `compute`
    /// yields `Some` iff `normalize_args` yields `Ok`, across scalar, array, and
    /// object shapes.
    #[test]
    fn compute_matches_normalize_decision() {
        for value in [
            json!(null),
            json!(true),
            json!(42),
            json!("text"),
            json!([1, 2, 3]),
            json!({ "nested": { "z": 1, "a": 2 } }),
        ] {
            assert_eq!(
                CacheKey::compute(&session(), "read_file", &value).is_some(),
                normalize_args(&"read_file".into(), &value).is_ok(),
                "compute/normalize disagree for {value}",
            );
        }
    }

    /// Key-reordered-equivalent args produce equal key strings, because
    /// `normalize_args` canonicalizes object-key order (Req 14.1).
    #[test]
    fn reordered_args_produce_equal_key_strings() {
        let a = CacheKey::compute(&session(), "read_file", &json!({ "a": 1, "b": 2 })).unwrap();
        let b = CacheKey::compute(&session(), "read_file", &json!({ "b": 2, "a": 1 })).unwrap();
        assert_eq!(a.to_key_string(), b.to_key_string());
    }

    /// Distinct sessions produce distinct key strings (per-session scope, Q3).
    #[test]
    fn distinct_sessions_produce_distinct_keys() {
        let args = json!({ "path": "a.rs" });
        let a = CacheKey::compute(&SessionId::from("s1"), "read_file", &args).unwrap();
        let b = CacheKey::compute(&SessionId::from("s2"), "read_file", &args).unwrap();
        assert_ne!(a.to_key_string(), b.to_key_string());
    }

    /// Distinct tools produce distinct key strings.
    #[test]
    fn distinct_tools_produce_distinct_keys() {
        let args = json!({ "path": "a.rs" });
        let a = CacheKey::compute(&session(), "read_file", &args).unwrap();
        let b = CacheKey::compute(&session(), "grep", &args).unwrap();
        assert_ne!(a.to_key_string(), b.to_key_string());
    }

    /// Distinct arguments produce distinct key strings.
    #[test]
    fn distinct_args_produce_distinct_keys() {
        let a = CacheKey::compute(&session(), "read_file", &json!({ "path": "a.rs" })).unwrap();
        let b = CacheKey::compute(&session(), "read_file", &json!({ "path": "b.rs" })).unwrap();
        assert_ne!(a.to_key_string(), b.to_key_string());
    }

    /// Injectivity spot check: a `:` or digits appearing inside a component
    /// cannot be mistaken for a component boundary, so triples that would
    /// collide under naive concatenation render to distinct key strings.
    #[test]
    fn length_prefix_prevents_boundary_collision() {
        // Without length prefixing, ("ab", "c") and ("a", "bc") could collide.
        // Here we exercise it with the session/tool split: session "ab" + tool
        // "c" versus session "a" + tool "bc".
        let args = json!({});
        let left = CacheKey::compute(&SessionId::from("ab"), "c", &args).unwrap();
        let right = CacheKey::compute(&SessionId::from("a"), "bc", &args).unwrap();
        assert_ne!(left.to_key_string(), right.to_key_string());
    }

    /// The rendered string uses the documented length-prefixed layout.
    #[test]
    fn key_string_layout_is_length_prefixed() {
        let key = CacheKey::compute(&SessionId::from("s1"), "read_file", &json!({ "a": 1 }))
            .unwrap();
        // session "s1" (len 2), tool "read_file" (len 9), args {"a":1} (len 7).
        assert_eq!(key.to_key_string(), r#"2:s19:read_file7:{"a":1}"#);
    }

    /// Replacing an existing key updates its value without triggering eviction.
    #[test]
    fn put_replaces_existing_key_without_eviction() {
        let store = InMemoryToolResultStore::with_capacity(1);
        let now = Instant::now();
        let deadline = now + Duration::from_secs(100);

        store.insert("k".to_string(), b"v1".to_vec(), deadline, now);
        store.insert("k".to_string(), b"v2".to_vec(), deadline, now);

        let guard = store.entries.read().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard.get("k").map(|(b, _)| b.clone()), Some(b"v2".to_vec()));
    }
}
