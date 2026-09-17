//! The write-time memory embedding writer (Req 2.3, 6.4, 9).
//!
//! [`MemoryEmbeddingWriter`] is the write-path counterpart of the query-path
//! `OpenAiEmbeddingSource`. It produces the [`Embedding`] stored in
//! `MemoryRecord.embedding` at memory-write time, using the *same* transport
//! (`halter-providers`' [`EmbeddingClient`]), the *same* deterministic input
//! construction (`embedding_input_text`), and — crucially — the *same*
//! configured model and dimension as the source, read from a single
//! [`ResolvedEmbeddingSettings`] so query and write embeddings stay comparable
//! (Req 9.1).
//!
//! This module defines the write-path *types* and the writer struct + its
//! constructor only:
//!
//! - [`EmbeddingWriteError`] — the fallible outcomes the write path surfaces to
//!   the caller so the store can be *rejected* rather than silently corrupted
//!   (Req 9.5, 9.6). Unlike the query path (which degrades every failure to
//!   `None`), a wrong-dimension vector must not be stored, because it would
//!   corrupt ANN cosine ranking.
//! - [`WriteEmbedding`] — the success outcome of producing a write-time
//!   embedding: a usable [`Embedding`] of the configured dimension
//!   ([`WriteEmbedding::Embedded`], Req 9.2), or a signal that the backend was
//!   unavailable and the caller should store an empty embedding
//!   ([`WriteEmbedding::Unavailable`], Req 6.4).
//! - [`MemoryEmbeddingWriter`] — the writer struct and its
//!   [`MemoryEmbeddingWriter::new`] constructor.
//!
//! The async `embed_for_write` method (the attempt/retry/timeout loop and
//! dimension enforcement) is implemented by task 8.2; store integration is
//! task 8.3.

use std::time::Duration;

use halter_providers::{EmbeddingClient, EmbeddingRequest};
use tokio_util::sync::CancellationToken;

use crate::tier2::embedding::embedding_input_text;
use crate::tier2::embedding::settings::ResolvedEmbeddingSettings;
use crate::tier2::memory::{Embedding, Memory};
use crate::tier2::store::{MemoryStore, MemoryStoreError};
use crate::types::{GoalNodeId, IntentSignature, MemoryId, SubtreeHash};

/// A write-time embedding failure that the caller must *not* silently degrade.
///
/// The query path maps every failure to `None`, but the write path has two
/// conditions where storing anything would corrupt ANN cosine ranking, so they
/// are surfaced as errors and the caller rejects the store, leaving any
/// previously stored embedding unchanged (Req 9.5, 9.6).
///
/// Both variants carry only integer dimensions — never any credential, URL, or
/// response body text — mirroring the redaction discipline of the transport
/// error surface (Req 5.6).
///
/// The [`std::error::Error`] / [`std::fmt::Display`] impls are hand-written
/// rather than derived via `thiserror`, because the design names the second
/// variant's field `source` (Req 9.6). `thiserror` unconditionally treats a
/// field literally named `source` as the error's `Error::source()` and requires
/// that field's type to implement [`std::error::Error`], which a `u32` does not
/// ([`thiserror` issue #138](https://github.com/dtolnay/thiserror/issues/138)).
/// Hand-writing the impls preserves the design's exact field name while keeping
/// the same public surface a derive would produce (`thiserror` is deliberately
/// invisible in the public API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingWriteError {
    /// The backend produced a usable vector, but its length does not equal the
    /// configured dimension (Req 9.5).
    ///
    /// `actual` is the produced vector's length; `configured` is the writer's
    /// configured dimension. The caller rejects the store rather than persist a
    /// mismatched-length embedding.
    DimensionMismatch {
        /// The produced vector's length.
        actual: usize,
        /// The writer's configured dimension.
        configured: u32,
    },
    /// The writer's configured dimension does not equal the source's configured
    /// dimension (Req 9.6).
    ///
    /// This is a *configuration* fault, not a per-request fault: the two paths
    /// must share one dimension so stored and query embeddings are comparable.
    /// The caller rejects the store rather than persist an embedding produced
    /// under a divergent dimension.
    DimensionMisconfigured {
        /// The writer's configured dimension.
        writer: u32,
        /// The source's configured dimension.
        source: u32,
    },
}

impl std::fmt::Display for EmbeddingWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DimensionMismatch { actual, configured } => write!(
                f,
                "embedding dimension mismatch: produced vector length {actual} \
                 != configured dimension {configured}"
            ),
            Self::DimensionMisconfigured { writer, source } => write!(
                f,
                "embedding dimension misconfigured: writer dimension {writer} \
                 != source dimension {source}"
            ),
        }
    }
}

impl std::error::Error for EmbeddingWriteError {}

/// The successful outcome of producing a write-time embedding.
///
/// Distinguishes a usable embedding from a graceful "backend unavailable"
/// degradation, so the caller can store an empty embedding and still complete
/// the write in the unavailable case (Req 6.4). A dimension failure is *not*
/// represented here — it is an [`EmbeddingWriteError`], because it must reject
/// the store rather than degrade.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteEmbedding {
    /// A usable vector whose length equals the configured dimension (Req 9.2).
    /// The caller stores this [`Embedding`] on the memory record.
    Embedded(Embedding),
    /// The backend was unavailable. The caller stores an empty [`Embedding`],
    /// the write still succeeds, and the memory remains retrievable via the
    /// structured filter (Req 6.4).
    Unavailable,
}

/// Write-time embedding producer for `MemoryRecord.embedding`.
///
/// Generic over the transport `C: EmbeddingClient` so production wraps the real
/// `OpenAiEmbeddingClient` while tests wrap deterministic fakes. Its runtime
/// knobs are captured once at construction from the *same*
/// [`ResolvedEmbeddingSettings`] the query-path source reads, so the two paths
/// agree byte-for-byte on the model and integer-for-integer on the dimension
/// (Req 9.1). Unlike the source, the writer holds no cache: write-time
/// embeddings are produced once per insert and are not re-queried.
///
/// The async [`MemoryEmbeddingWriter::embed_for_write`] method runs the same
/// attempt/retry/timeout logic as the source; store integration is task 8.3.
pub struct MemoryEmbeddingWriter<C: EmbeddingClient> {
    /// The single-attempt embeddings transport (from `halter-providers`), the
    /// same client type the source uses.
    client: C,
    /// The embeddings model identifier; MUST equal the source's model (Req 9.1).
    model: String,
    /// The configured dimension; MUST equal the source's dimension (Req 9.1,
    /// 9.6). Held as `Option<u32>` mirroring the source: `None` lets the model
    /// default apply, and a `Some` value is the length every stored vector must
    /// have (Req 9.2, 9.5). [`MemoryEmbeddingWriter::embed_for_write`] compares
    /// this against the produced vector length (Req 9.5), and
    /// [`MemoryEmbeddingWriter::check_source_dimension`] compares it against the
    /// source's dimension (Req 9.6).
    dimension: Option<u32>,
    /// Total attempts per write embed, clamped to `1..=10`, default 3 (Req 8.2,
    /// 8.3).
    max_attempts: u32,
    /// Per-request timeout, defaulting to 30s (Req 7.1, 7.3).
    timeout: Duration,
    /// Whether embedding is enabled; a disabled writer produces
    /// [`WriteEmbedding::Unavailable`] with no network call (Req 11.4, 11.5).
    enabled: bool,
    /// Whether resolution produced a usable bearer credential.
    ///
    /// Mirrors the source: `ResolvedEmbeddingSettings` always carries a
    /// `SecretString` bearer, so the "no credential" state (Req 5.5) is captured
    /// here as the emptiness of that bearer. An empty bearer means no usable
    /// credential, and [`MemoryEmbeddingWriter::embed_for_write`] then degrades
    /// to [`WriteEmbedding::Unavailable`] without a network call.
    has_credential: bool,
}

impl<C: EmbeddingClient> MemoryEmbeddingWriter<C> {
    /// Build a writer from a transport `client` and runtime-ready `settings`.
    ///
    /// The `settings` MUST be the *same* [`ResolvedEmbeddingSettings`] value the
    /// query-path `OpenAiEmbeddingSource` is built from, so both paths share one
    /// model and one dimension (Req 9.1). The settings are already clamped and
    /// defaulted by [`ResolvedEmbeddingSettings::resolve`] (attempts to `1..=10`,
    /// timeout to `>= 1s` else 30s).
    ///
    /// The "no credential" state (Req 5.5) is captured up-front: if the resolved
    /// bearer is empty, [`Self::has_credential`] is `false` and
    /// [`Self::embed_for_write`] short-circuits to
    /// [`WriteEmbedding::Unavailable`] without any network call.
    #[must_use]
    pub fn new(client: C, settings: ResolvedEmbeddingSettings) -> Self {
        let has_credential = !settings.bearer.expose_secret().is_empty();
        Self {
            client,
            model: settings.model,
            dimension: settings.dimension,
            max_attempts: settings.max_attempts,
            timeout: settings.timeout,
            enabled: settings.enabled,
            has_credential,
        }
    }

    /// Produce the write-time embedding for `intent`, degrading to
    /// [`WriteEmbedding::Unavailable`] when the backend is unavailable and
    /// surfacing an [`EmbeddingWriteError`] only when a dimension invariant is
    /// violated (Req 2.3, 2.4, 6.4, 9.2, 9.5).
    ///
    /// This mirrors the query-path source's attempt/retry/timeout logic exactly
    /// (`OpenAiEmbeddingSource::embed`), applied to the memory's `intent`:
    ///
    /// 1. Disabled writer -> [`WriteEmbedding::Unavailable`] with no network
    ///    call (Req 11.5 analog / 6.4).
    /// 2. Build `input = embedding_input_text(intent)` using the *same* rule as
    ///    the query path so query and write embeddings are comparable (Req 2.3,
    ///    2.4); an empty input yields no usable embedding ->
    ///    [`WriteEmbedding::Unavailable`] with no network call.
    /// 3. No usable credential -> [`WriteEmbedding::Unavailable`] with no
    ///    network call (Req 5.5 analog).
    /// 4. Run the bounded attempt loop with per-attempt timeout and retry
    ///    classification, calling `client.embed_once(..).await`; all failures /
    ///    exhaustion degrade to [`WriteEmbedding::Unavailable`] (Req 6.4). The
    ///    caller stores an empty embedding and the write still succeeds.
    /// 5. On a usable, finite vector, enforce the configured dimension: when a
    ///    dimension is configured and the produced length differs, reject with
    ///    [`EmbeddingWriteError::DimensionMismatch`] so the caller rejects the
    ///    store rather than corrupt ANN cosine ranking (Req 9.5). When no
    ///    dimension is configured, the returned vector is accepted as-is (the
    ///    model default applies).
    /// 6. Otherwise wrap the vector as [`WriteEmbedding::Embedded`] (Req 9.2).
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingWriteError::DimensionMismatch`] when a produced vector
    /// length does not equal the writer's configured dimension (Req 9.5). The
    /// writer-vs-source dimension invariant
    /// ([`EmbeddingWriteError::DimensionMisconfigured`], Req 9.6) is *not*
    /// checked here — `embed_for_write` only has the writer's own settings — but
    /// by [`Self::check_source_dimension`], which the store-integration path
    /// (task 8.3) calls to compare the two paths' dimensions.
    pub async fn embed_for_write(
        &self,
        intent: &IntentSignature,
    ) -> Result<WriteEmbedding, EmbeddingWriteError> {
        // 1. Disabled -> Unavailable, no network (Req 6.4).
        if !self.enabled {
            return Ok(WriteEmbedding::Unavailable);
        }

        // 2. Build input via the SAME rule as the query path (Req 2.3, 2.4);
        //    empty input -> no usable embedding, no network.
        let input = embedding_input_text(intent);
        if input.is_empty() {
            return Ok(WriteEmbedding::Unavailable);
        }

        // 3. No credential -> Unavailable, no network (Req 5.5 analog).
        if !self.has_credential {
            return Ok(WriteEmbedding::Unavailable);
        }

        // 4. Bounded attempt loop; all failures / exhaustion -> Unavailable
        //    (Req 6.4).
        let request = EmbeddingRequest {
            model: self.model.clone(),
            input,
            dimensions: self.dimension,
        };
        let Some(vector) = self.attempt_embed(&request).await else {
            return Ok(WriteEmbedding::Unavailable);
        };

        // 5. Enforce the configured dimension (Req 9.5). A length mismatch is a
        //    hard failure, not a degradation, because a wrong-dimension vector
        //    would corrupt ANN cosine ranking.
        if let Some(dimension) = self.dimension
            && vector.len() != dimension as usize
        {
            return Err(EmbeddingWriteError::DimensionMismatch {
                actual: vector.len(),
                configured: dimension,
            });
        }

        // 6. Usable -> Embedded (Req 9.2).
        Ok(WriteEmbedding::Embedded(Embedding(vector)))
    }

    /// Check the write-vs-query dimension invariant (Req 9.6).
    ///
    /// The writer and the query-path source MUST request embeddings under a
    /// single configured dimension so stored and query vectors are comparable
    /// (Req 9.1). Because [`Self::embed_for_write`] only has the writer's own
    /// settings, this configuration-time invariant is surfaced separately: the
    /// store-integration path (task 8.3) calls this with the source's resolved
    /// dimension before persisting, and rejects the store on error, leaving any
    /// previously stored embedding unchanged.
    ///
    /// A writer or source with no configured dimension (`None`) lets the model
    /// default apply on both paths; since the default is a byte-for-byte-equal
    /// model on both sides, the dimensions still agree, so `None` on either side
    /// is treated as consistent. The invariant is only violated when both
    /// dimensions are configured and differ.
    ///
    /// # Errors
    ///
    /// Returns [`EmbeddingWriteError::DimensionMisconfigured`] when the writer's
    /// configured dimension and `source_dimension` are both set and unequal
    /// (Req 9.6).
    pub fn check_source_dimension(
        &self,
        source_dimension: Option<u32>,
    ) -> Result<(), EmbeddingWriteError> {
        if let (Some(writer), Some(source)) = (self.dimension, source_dimension)
            && writer != source
        {
            return Err(EmbeddingWriteError::DimensionMisconfigured { writer, source });
        }
        Ok(())
    }

    /// Run the bounded attempt loop against the transport, returning the first
    /// usable, finite vector or `None` once attempts are exhausted or a
    /// non-retryable failure is seen (Req 7.2, 8.1, 8.4, 8.5).
    ///
    /// This is the *same* attempt/retry/timeout shape as the query-path
    /// source's `attempt_embed`: each attempt is bounded by the configured
    /// per-request timeout via [`tokio::time::timeout`]; an elapsed timeout is a
    /// retryable failure (Req 7.2); a classified error is retried only when
    /// [`halter_providers::EmbeddingClientError::is_retryable`] and attempts
    /// remain (Req 8.4). A returned vector is checked for finiteness and
    /// non-emptiness here; the configured-dimension check is applied by
    /// [`Self::embed_for_write`] so a length mismatch can surface a
    /// [`EmbeddingWriteError::DimensionMismatch`] rather than silently degrade.
    async fn attempt_embed(&self, request: &EmbeddingRequest) -> Option<Vec<f32>> {
        for _ in 0..self.max_attempts {
            let cancel = CancellationToken::new();
            let attempt =
                tokio::time::timeout(self.timeout, self.client.embed_once(request, cancel))
                    .await;

            match attempt {
                // The attempt produced a transport response; accept it only if
                // it is a non-empty, finite vector (Req 4). The dimension check
                // is deferred to `embed_for_write` (Req 9.5).
                Ok(Ok(response)) => {
                    let vector = response.vector;
                    if vector.is_empty() || !vector.iter().all(|value| value.is_finite()) {
                        return None;
                    }
                    return Some(vector);
                }
                // A classified failure: retry only when transient and attempts
                // remain; otherwise stop and degrade (Req 8.4).
                Ok(Err(error)) => {
                    if !error.is_retryable() {
                        return None;
                    }
                }
                // The per-attempt timeout elapsed: retryable (Req 7.2).
                Err(_elapsed) => {}
            }
        }
        // All permitted attempts failed (Req 8.5).
        None
    }
}

/// Produce a write-time embedding with `writer` and store `mem` under `key`,
/// keeping [`MemoryStore::insert`] synchronous by awaiting the embedding
/// **up front, outside the store lock**, then handing the resulting
/// [`Embedding`] to [`MemoryStore::insert_with_embedding`].
///
/// This is the documented integration seam that ties a
/// [`MemoryEmbeddingWriter`] to a [`MemoryStore`] write (design "Write path
/// (induction / insert)"). It is a free function rather than a method on
/// [`InductionEngine`](crate::tier2::induction::InductionEngine) so wiring it
/// into an existing insert call site is a localized change: a future task can
/// replace an `store.insert(key, mem)` call with
/// `insert_memory_with_writer(&writer, source_dimension, store, key, mem).await`
/// without changing the store trait or the engine's generics broadly.
///
/// The steps, in order:
///
/// 1. **Configuration check (Req 9.6).** Call
///    [`MemoryEmbeddingWriter::check_source_dimension`] with the query-path
///    source's configured `source_dimension`. On mismatch, reject the store —
///    mapping the error into [`MemoryStoreError::EmbeddingDimensionMisconfigured`]
///    — without producing an embedding or touching the store, leaving any
///    previously stored embedding unchanged.
/// 2. **Embed up front (outside the lock).** `await`
///    [`MemoryEmbeddingWriter::embed_for_write`] on `mem.intent`. The store lock
///    is only taken *after* this await, inside the synchronous
///    [`MemoryStore::insert_with_embedding`].
/// 3. **Map the outcome to a synchronous insert:**
///    - [`WriteEmbedding::Embedded`] → store that [`Embedding`] (Req 9.2);
///    - [`WriteEmbedding::Unavailable`] → store [`Embedding::default`] (empty),
///      the write still succeeds and the memory stays retrievable via the
///      structured filter (Req 6.4);
///    - a per-request length fault ([`EmbeddingWriteError::DimensionMismatch`])
///      → reject the store, mapping into
///      [`MemoryStoreError::EmbeddingDimensionMismatch`], leaving any previously
///      stored embedding unchanged (Req 9.5).
///
/// # Errors
///
/// Returns [`MemoryStoreError::EmbeddingDimensionMisconfigured`] (Req 9.6) or
/// [`MemoryStoreError::EmbeddingDimensionMismatch`] (Req 9.5) when a dimension
/// invariant is violated, or any [`MemoryStoreError`] surfaced by
/// [`MemoryStore::insert_with_embedding`]'s write-time validation. On any error
/// the store is left unchanged.
pub async fn insert_memory_with_writer<C, S>(
    writer: &MemoryEmbeddingWriter<C>,
    source_dimension: Option<u32>,
    store: &S,
    key: (GoalNodeId, SubtreeHash),
    mem: Memory,
) -> Result<MemoryId, MemoryStoreError>
where
    C: EmbeddingClient,
    S: MemoryStore + ?Sized,
{
    // 1. Configuration check up front (Req 9.6): a writer/source dimension
    //    divergence rejects the store before any embedding or write.
    writer.check_source_dimension(source_dimension)?;

    // 2. Produce the embedding async, up front, OUTSIDE the store lock. The
    //    synchronous insert below takes the store lock only after this await.
    let write_embedding = writer.embed_for_write(&mem.intent).await?;

    // 3. Map the outcome onto the synchronous insert (Req 6.4, 9.2). A usable
    //    vector is stored as-is; an unavailable backend stores an empty
    //    embedding and still succeeds.
    let embedding = match write_embedding {
        WriteEmbedding::Embedded(embedding) => embedding,
        WriteEmbedding::Unavailable => Embedding::default(),
    };

    store.insert_with_embedding(key, mem, embedding)
}
