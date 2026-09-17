//! The in-process LRU embedding cache (Requirement 10).
//!
//! [`EmbeddingCache`] memoizes previously computed embeddings so that repeated
//! `embed` calls for identical inputs are served without a redundant backend
//! call. It is keyed by the pair (`Embedding_Model`, `Embedding_Input_Text`)
//! ([`EmbeddingCacheKey`]) so two lookups whose model and input text are each
//! byte-identical resolve to the same entry (Req 10.1).
//!
//! The cache is bounded to a configured maximum entry count (Req 10.6) and,
//! when full, evicts the least-recently-*accessed* entry before inserting a new
//! distinct key (Req 10.7). Access order is updated on read so eviction tracks
//! reads as well as writes.
//!
//! A capacity of zero is handled gracefully: the cache holds no backing store,
//! every `put` retains nothing, and every `get` misses. This keeps the type
//! total even for a degenerate (though not operator-reachable, since config
//! validation requires `>= 1`) capacity.

use std::num::NonZeroUsize;

use lru::LruCache;

use crate::tier2::memory::Embedding;

/// The cache key: the pair (`Embedding_Model`, `Embedding_Input_Text`).
///
/// Two keys are equal iff both their `model` and `input` strings are
/// byte-identical, so field-wise-equal lookups resolve to the same entry
/// (Req 10.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmbeddingCacheKey {
    /// The configured embeddings model identifier the entry was produced under.
    pub model: String,
    /// The deterministic `Embedding_Input_Text` the entry was produced from.
    pub input: String,
}

/// An in-process LRU cache from [`EmbeddingCacheKey`] to [`Embedding`].
///
/// Backed by [`lru::LruCache`]. Recency is updated on every successful `get`
/// (Req 10.2, 10.7) and on every `put`. When at capacity, inserting a new
/// distinct key evicts the least-recently-accessed entry first (Req 10.7).
///
/// A capacity of `0` produces a cache that never stores anything.
pub struct EmbeddingCache {
    /// The backing LRU store, or `None` for a zero-capacity cache.
    inner: Option<LruCache<EmbeddingCacheKey, Embedding>>,
}

impl EmbeddingCache {
    /// Create a cache holding at most `capacity` entries (Req 10.6).
    ///
    /// A `capacity` of `0` yields a cache that stores nothing: every `put`
    /// retains no entry and every `get` misses.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let inner = NonZeroUsize::new(capacity).map(LruCache::new);
        Self { inner }
    }

    /// Look up `key`, returning a clone of the stored [`Embedding`] on a hit.
    ///
    /// A hit updates the entry's recency so it becomes the most-recently-
    /// accessed (Req 10.2, 10.7). A miss (including for a zero-capacity cache)
    /// returns `None` and leaves the cache unchanged.
    pub fn get(&mut self, key: &EmbeddingCacheKey) -> Option<Embedding> {
        self.inner.as_mut().and_then(|cache| cache.get(key).cloned())
    }

    /// Insert `value` under `key`, evicting the least-recently-accessed entry
    /// first if the cache is at capacity (Req 10.7).
    ///
    /// Returns whether the entry is now present in the cache (Req 10.4). For a
    /// zero-capacity cache this is always `false`; otherwise it is always
    /// `true`, since a non-zero-capacity `LruCache` retains the just-inserted
    /// key (the freshly inserted entry is the most-recently-used and so is
    /// never the one evicted).
    pub fn put(&mut self, key: EmbeddingCacheKey, value: Embedding) -> bool {
        match self.inner.as_mut() {
            Some(cache) => {
                cache.put(key.clone(), value);
                cache.contains(&key)
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str, input: &str) -> EmbeddingCacheKey {
        EmbeddingCacheKey {
            model: model.to_owned(),
            input: input.to_owned(),
        }
    }

    fn emb(values: &[f32]) -> Embedding {
        Embedding(values.to_vec())
    }

    #[test]
    fn get_returns_stored_value_and_reports_presence_on_put() {
        let mut cache = EmbeddingCache::new(4);
        let k = key("text-embedding-3-small", "a");
        assert!(cache.get(&k).is_none(), "empty cache misses");

        let present = cache.put(k.clone(), emb(&[0.1, 0.2]));
        assert!(present, "non-zero-capacity put retains the entry");
        assert_eq!(cache.get(&k), Some(emb(&[0.1, 0.2])));
    }

    #[test]
    fn key_equality_is_by_model_and_input() {
        let mut cache = EmbeddingCache::new(4);
        cache.put(key("model-a", "input"), emb(&[1.0]));

        assert_eq!(cache.get(&key("model-a", "input")), Some(emb(&[1.0])));
        assert!(
            cache.get(&key("model-b", "input")).is_none(),
            "different model is a different key",
        );
        assert!(
            cache.get(&key("model-a", "other")).is_none(),
            "different input is a different key",
        );
    }

    #[test]
    fn zero_capacity_retains_nothing_and_reports_absence() {
        let mut cache = EmbeddingCache::new(0);
        let k = key("m", "i");
        let present = cache.put(k.clone(), emb(&[0.5]));
        assert!(!present, "zero-capacity put reports absence");
        assert!(cache.get(&k).is_none(), "zero-capacity cache stores nothing");
    }

    #[test]
    fn eviction_removes_least_recently_accessed_entry() {
        let mut cache = EmbeddingCache::new(2);
        cache.put(key("m", "a"), emb(&[1.0]));
        cache.put(key("m", "b"), emb(&[2.0]));

        // Access "a" so "b" becomes the least-recently-accessed entry.
        assert_eq!(cache.get(&key("m", "a")), Some(emb(&[1.0])));

        // Inserting a third distinct key must evict "b", not "a".
        cache.put(key("m", "c"), emb(&[3.0]));

        assert_eq!(cache.get(&key("m", "a")), Some(emb(&[1.0])), "a retained");
        assert_eq!(cache.get(&key("m", "c")), Some(emb(&[3.0])), "c inserted");
        assert!(
            cache.get(&key("m", "b")).is_none(),
            "least-recently-accessed b evicted",
        );
    }
}
