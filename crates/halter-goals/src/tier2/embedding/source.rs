//! The concrete OpenAI-backed query-path embedding source (Req 1, 5, 6, 9, 10,
//! 11).
//!
//! [`OpenAiEmbeddingSource`] implements the async `EmbeddingSource` seam by
//! wrapping a [`halter_providers::EmbeddingClient`] transport, an in-process
//! LRU [`EmbeddingCache`], and the runtime-ready [`ResolvedEmbeddingSettings`]
//! (model, dimension, retry/timeout bounds, enable flag, and bearer credential
//! state). It preserves the `Option`-returning contract exactly and degrades to
//! `None` on every failure mode (Req 6).
//!
//! This module defines the struct, its [`OpenAiEmbeddingSource::new`]
//! constructor, and the async `embed` method — the attempt loop, retry
//! classification, cache lookup/insert, and dimension enforcement — behind the
//! `EmbeddingSource` trait `impl`.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use halter_providers::{EmbeddingClient, EmbeddingRequest};
use tokio_util::sync::CancellationToken;

use crate::tier2::embedding::cache::{EmbeddingCache, EmbeddingCacheKey};
use crate::tier2::embedding::embedding_input_text;
use crate::tier2::embedding::settings::ResolvedEmbeddingSettings;
use crate::tier2::memory::Embedding;
use crate::tier2::retrieval::EmbeddingSource;
use crate::types::IntentSignature;

/// Concrete OpenAI-backed [`EmbeddingSource`](crate::tier2::retrieval::EmbeddingSource)
/// for the query path.
///
/// Generic over the transport `C: EmbeddingClient` so production can wrap the
/// real `OpenAiEmbeddingClient` while tests wrap deterministic fakes. All
/// runtime knobs are captured once at construction from
/// [`ResolvedEmbeddingSettings`] (the single source of truth shared with the
/// write-path writer, Req 9.1); the hot `embed` path performs no further
/// resolution.
///
/// The [`EmbeddingCache`] is wrapped in a [`std::sync::Mutex`] for interior
/// mutability because `embed` takes `&self` (mirroring the store's interior
/// mutability). The lock is only ever held for the synchronous cache get/put
/// and never across an `.await` on the client (enforced by the `embed`
/// implementation).
pub struct OpenAiEmbeddingSource<C: EmbeddingClient> {
    /// The single-attempt embeddings transport (from `halter-providers`).
    client: C,
    /// The in-process LRU cache, guarded for `&self` interior mutability.
    cache: Mutex<EmbeddingCache>,
    /// The embeddings model identifier (Req 9.1, 11.1).
    model: String,
    /// Optional dimension override; `None` lets the model default apply, and a
    /// `Some` value is the length every usable vector must have (Req 9.1, 9.3,
    /// 9.4).
    dimension: Option<u32>,
    /// Total attempts per `embed`, clamped to `1..=10`, default 3 (Req 8.2,
    /// 8.3).
    max_attempts: u32,
    /// Per-request timeout, defaulting to 30s (Req 7.1, 7.3).
    timeout: Duration,
    /// Whether the source is enabled; a disabled source returns `None` with no
    /// network call (Req 11.4, 11.5).
    enabled: bool,
    /// Whether resolution produced a usable bearer credential.
    ///
    /// `ResolvedEmbeddingSettings` always carries a `SecretString` bearer (it is
    /// only produced when provider credential resolution succeeded), so the
    /// "no credential" state (Req 5.5) is captured here as the emptiness of that
    /// bearer: an empty bearer means no usable credential, and `embed` must then
    /// return `None` without a network call, leaving the cache untouched.
    has_credential: bool,
}

impl<C: EmbeddingClient> OpenAiEmbeddingSource<C> {
    /// Build a source from a transport `client` and runtime-ready `settings`.
    ///
    /// The settings are already clamped and defaulted by
    /// [`ResolvedEmbeddingSettings::resolve`] (attempts to `1..=10`, timeout to
    /// `>= 1s` else 30s, cache capacity defaulted to 1024). The cache is sized
    /// from [`ResolvedEmbeddingSettings::cache_max_entries`] (Req 10.6).
    ///
    /// The "no credential" state (Req 5.5) is captured up-front: if the resolved
    /// bearer is empty, [`Self::has_credential`] is `false` and the `embed`
    /// method (task 6.2) short-circuits to `None` without any network call.
    #[must_use]
    pub fn new(client: C, settings: ResolvedEmbeddingSettings) -> Self {
        let has_credential = !settings.bearer.expose_secret().is_empty();
        Self {
            client,
            cache: Mutex::new(EmbeddingCache::new(settings.cache_max_entries)),
            model: settings.model,
            dimension: settings.dimension,
            max_attempts: settings.max_attempts,
            timeout: settings.timeout,
            enabled: settings.enabled,
            has_credential,
        }
    }

    /// Build the [`EmbeddingCacheKey`] for `input` under the configured model.
    fn cache_key(&self, input: &str) -> EmbeddingCacheKey {
        EmbeddingCacheKey {
            model: self.model.clone(),
            input: input.to_owned(),
        }
    }

    /// Look up `input` in the cache, holding the lock only for the synchronous
    /// get and never across an `.await` (Req 10.2).
    fn cache_get(&self, input: &str) -> Option<Embedding> {
        let key = self.cache_key(input);
        let mut cache = self
            .cache
            .lock()
            .expect("embedding cache mutex poisoned");
        cache.get(&key)
    }

    /// Best-effort insert of `embedding` under `input`'s key (Req 10.3, 10.4).
    ///
    /// The lock is held only for the synchronous put. Whether the entry is now
    /// present is irrelevant to the caller: the usable embedding is returned
    /// either way (Req 10.4), so the boolean is intentionally discarded.
    fn cache_put(&self, input: &str, embedding: &Embedding) {
        let key = self.cache_key(input);
        let mut cache = self
            .cache
            .lock()
            .expect("embedding cache mutex poisoned");
        let _ = cache.put(key, embedding.clone());
    }

    /// Run the bounded attempt loop against the transport, returning the first
    /// usable vector or `None` once attempts are exhausted or a non-retryable
    /// failure is seen (Req 7.2, 8.1, 8.2, 8.4, 8.5).
    ///
    /// Each attempt is bounded by the configured per-request timeout via
    /// [`tokio::time::timeout`]; an elapsed timeout is treated as a retryable
    /// `Timeout` failure (Req 7.2). A returned vector is validated for
    /// finiteness and (when a dimension is configured) length before being
    /// accepted (Req 1.2, 1.3, 9.3, 9.4); a vector that fails validation is a
    /// non-retryable failure that degrades to `None`.
    async fn attempt_embed(&self, request: &EmbeddingRequest) -> Option<Vec<f32>> {
        for _ in 0..self.max_attempts {
            let cancel = CancellationToken::new();
            let attempt = tokio::time::timeout(
                self.timeout,
                self.client.embed_once(request, cancel),
            )
            .await;

            match attempt {
                // The attempt completed and produced a usable transport
                // response; validate it before accepting (Req 4, 9.3, 9.4).
                Ok(Ok(response)) => {
                    return self.validate_vector(response.vector);
                }
                // The attempt failed with a classified error. Retry only when
                // the classification says the failure is transient and attempts
                // remain; otherwise stop and degrade to `None` (Req 8.4).
                Ok(Err(error)) => {
                    if !error.is_retryable() {
                        return None;
                    }
                }
                // The per-attempt timeout elapsed: treat as a retryable
                // `Timeout` failure (Req 7.2) and try again if attempts remain.
                Err(_elapsed) => {}
            }
        }
        // All permitted attempts failed (Req 8.5).
        None
    }

    /// Validate a returned vector for finiteness and configured dimension.
    ///
    /// Returns `Some(vector)` iff every element is finite (Req 1.2, 1.3) and,
    /// when a dimension is configured, the length equals it (Req 9.3, 9.4);
    /// otherwise `None`. An empty vector fails the dimension check when a
    /// dimension is configured, and is rejected as unusable regardless because
    /// the finiteness check over an empty vector plus the source contract
    /// treats a zero-length vector as no embedding.
    fn validate_vector(&self, vector: Vec<f32>) -> Option<Vec<f32>> {
        if vector.is_empty() {
            return None;
        }
        if !vector.iter().all(|value| value.is_finite()) {
            return None;
        }
        if let Some(dimension) = self.dimension
            && vector.len() != dimension as usize
        {
            return None;
        }
        Some(vector)
    }
}

#[async_trait]
impl<C: EmbeddingClient> EmbeddingSource for OpenAiEmbeddingSource<C> {
    /// Embed `sig`, degrading to `None` on every failure mode (Req 6).
    ///
    /// The algorithm follows the design's numbered steps:
    ///
    /// 1. Disabled source → `None` with no network call (Req 11.5).
    /// 2. Build the deterministic input text; an empty input → `None` with no
    ///    network call (Req 3.4).
    /// 3. Cache lookup by `(model, input)`; on a hit return the cached
    ///    embedding without a backend call. The cache lock is released before
    ///    any `.await` (Req 10.2).
    /// 4. No usable credential → `None` with no network call, cache untouched
    ///    (Req 5.5).
    /// 5. Run the bounded attempt loop with per-attempt timeout and retry
    ///    classification, calling `client.embed_once(..).await` (Req 7, 8).
    /// 6. Validate finiteness and configured dimension (Req 1.2, 1.3, 9.3,
    ///    9.4).
    /// 7. On a usable vector, best-effort cache put and return `Some`
    ///    (Req 10.3, 10.4).
    /// 8. Any failure → `None`, leaving the cache unchanged (Req 6, 10.5).
    ///
    /// The method never panics and never returns an error (Req 6.3).
    async fn embed(&self, sig: &IntentSignature) -> Option<Embedding> {
        // 1. Disabled -> None, no network (Req 11.5).
        if !self.enabled {
            return None;
        }

        // 2. Build input; empty -> None, no network (Req 3.4).
        let input = embedding_input_text(sig);
        if input.is_empty() {
            return None;
        }

        // 3. Cache lookup (lock not held across await) (Req 10.2).
        if let Some(cached) = self.cache_get(&input) {
            return Some(cached);
        }

        // 4. No credential -> None, no network, cache untouched (Req 5.5).
        if !self.has_credential {
            return None;
        }

        // 5-6. Bounded attempt loop + validation (Req 7, 8, 9.3, 9.4).
        let request = EmbeddingRequest {
            model: self.model.clone(),
            input: input.clone(),
            dimensions: self.dimension,
        };
        let vector = self.attempt_embed(&request).await?;

        // 7. Usable -> best-effort cache put, return Some (Req 10.3, 10.4).
        let embedding = Embedding(vector);
        self.cache_put(&input, &embedding);
        Some(embedding)
        // 8. Any failure short-circuited above via `?`/early returns, leaving
        //    the cache unchanged (Req 6, 10.5).
    }
}
