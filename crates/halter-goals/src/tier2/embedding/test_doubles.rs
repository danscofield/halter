//! Async [`EmbeddingClient`] test doubles for the embedding source and writer.
//!
//! These doubles let the `OpenAiEmbeddingSource` / `MemoryEmbeddingWriter`
//! logic — retry accounting, caching, dimension enforcement, and
//! degradation — be exercised deterministically, without real network I/O
//! (Req 6.3). They implement the `halter-providers` [`EmbeddingClient`] trait
//! whose single `embed_once` performs exactly one attempt and returns a
//! *classified* [`EmbeddingClientError`], so retry is layered by the caller and
//! can be driven from a scripted sequence.
//!
//! Two doubles are provided:
//!
//! - [`ScriptedEmbeddingClient`] — returns a caller-supplied sequence of
//!   `Result<EmbeddingResponse, EmbeddingClientError>`, one per `embed_once`
//!   call, in order. This drives:
//!   - **retry** tests (script N retryable errors then an `Ok`, assert the
//!     source recovers within `max_attempts`) (Req 8.1, 8.2),
//!   - **degradation** tests (script all errors, assert the source degrades to
//!     `None` after the bounded attempts) (Req 6.3, 8.5),
//!   - **caching** tests (script a single `Ok` and assert the backend is called
//!     at most once across two embeds of the same input) (Req 10.8).
//!   It also counts its invocations, so callers can assert the *number* of
//!   attempts is bounded (Req 8.2) or that a cache hit avoided a second call
//!   (Req 10.8).
//! - [`CountingEmbeddingClient`] — an attempt-counting spy that returns a fixed
//!   result for every call and records how many times `embed_once` was invoked
//!   (Req 8.2, 10.8). Use it when a test only needs the invocation count and a
//!   constant outcome (e.g. "the cache served the second embed, so the client
//!   was called exactly once").
//!
//! Both doubles are `Send + Sync` (the trait requires it) via an
//! [`AtomicUsize`] counter and a [`Mutex`]-guarded script, so they can be held
//! across the `.await` inside the source and shared behind `&self`.
//!
//! Placement: this module is `#[cfg(test)] pub(crate)`, so unit and property
//! tests in sibling `tier2::embedding` modules (the source in `mod.rs`, the
//! writer) can all reach the same doubles without each redefining them.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use halter_providers::{EmbeddingClient, EmbeddingClientError, EmbeddingRequest, EmbeddingResponse};
use tokio_util::sync::CancellationToken;

/// A scripted [`EmbeddingClient`] returning a caller-supplied sequence of
/// results, one per `embed_once` call, and counting its invocations.
///
/// Each `embed_once` pops the next scripted `Result` from the front of the
/// queue and returns it, incrementing the attempt counter. When the script is
/// exhausted, `embed_once` returns the configured [`Self::exhausted`] fallback
/// (default [`EmbeddingClientError::Transport`], a retryable error) so a test
/// that under-scripts still sees a well-defined, non-panicking outcome rather
/// than an index-out-of-bounds.
///
/// # Determinism
///
/// The script is consumed strictly in order, so a test that wants "two
/// retryable failures, then success" scripts exactly
/// `[Err(Server(503)), Err(Timeout), Ok(response)]` and asserts the source
/// returns `Some` after three attempts. Because the request argument is
/// ignored, the sequence is a pure function of call order — fully
/// deterministic (Req 6.3).
pub(crate) struct ScriptedEmbeddingClient {
    /// The remaining scripted results, consumed front-to-back.
    script: Mutex<VecDeque<Result<EmbeddingResponse, EmbeddingClientError>>>,
    /// The result returned once the script is exhausted.
    exhausted: Mutex<Result<EmbeddingResponse, EmbeddingClientError>>,
    /// The number of `embed_once` calls made so far (Req 8.2, 10.8).
    calls: AtomicUsize,
}

impl ScriptedEmbeddingClient {
    /// Build a scripted client from `results`, returned one per call in order.
    ///
    /// Once the script is exhausted, further calls return a retryable
    /// [`EmbeddingClientError::Transport`]; use [`Self::with_exhausted`] to
    /// override that fallback.
    pub(crate) fn new(
        results: impl IntoIterator<Item = Result<EmbeddingResponse, EmbeddingClientError>>,
    ) -> Self {
        Self {
            script: Mutex::new(results.into_iter().collect()),
            exhausted: Mutex::new(Err(EmbeddingClientError::Transport)),
            calls: AtomicUsize::new(0),
        }
    }

    /// Build a scripted client whose post-exhaustion calls return `exhausted`.
    pub(crate) fn with_exhausted(
        results: impl IntoIterator<Item = Result<EmbeddingResponse, EmbeddingClientError>>,
        exhausted: Result<EmbeddingResponse, EmbeddingClientError>,
    ) -> Self {
        Self {
            script: Mutex::new(results.into_iter().collect()),
            exhausted: Mutex::new(exhausted),
            calls: AtomicUsize::new(0),
        }
    }

    /// The number of `embed_once` calls made so far.
    ///
    /// Lets tests assert the attempt count is bounded by `max_attempts`
    /// (Req 8.2) or that a cache hit avoided a second backend call (Req 10.8).
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EmbeddingClient for ScriptedEmbeddingClient {
    async fn embed_once(
        &self,
        _request: &EmbeddingRequest,
        _cancel: CancellationToken,
    ) -> Result<EmbeddingResponse, EmbeddingClientError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .script
            .lock()
            .expect("scripted embedding client mutex poisoned")
            .pop_front();
        match next {
            Some(result) => result,
            None => self
                .exhausted
                .lock()
                .expect("scripted embedding client exhausted mutex poisoned")
                .clone(),
        }
    }
}

/// An attempt-counting spy [`EmbeddingClient`] returning a fixed result for
/// every call.
///
/// Unlike [`ScriptedEmbeddingClient`], the outcome does not vary per call —
/// every `embed_once` clones and returns the configured `outcome`. Its purpose
/// is the invocation count: tests assert bounded retry (Req 8.2) or single-call
/// caching (Req 10.8) by checking [`Self::calls`].
pub(crate) struct CountingEmbeddingClient {
    /// The fixed result returned by every `embed_once` call.
    outcome: Result<EmbeddingResponse, EmbeddingClientError>,
    /// The number of `embed_once` calls made so far (Req 8.2, 10.8).
    calls: AtomicUsize,
}

impl CountingEmbeddingClient {
    /// Build a counting spy that always returns `outcome`.
    pub(crate) fn new(outcome: Result<EmbeddingResponse, EmbeddingClientError>) -> Self {
        Self {
            outcome,
            calls: AtomicUsize::new(0),
        }
    }

    /// Build a counting spy that always succeeds with `vector`.
    pub(crate) fn always_ok(vector: Vec<f32>) -> Self {
        Self::new(Ok(EmbeddingResponse { vector }))
    }

    /// Build a counting spy that always fails with `error`.
    pub(crate) fn always_err(error: EmbeddingClientError) -> Self {
        Self::new(Err(error))
    }

    /// The number of `embed_once` calls made so far (Req 8.2, 10.8).
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EmbeddingClient for CountingEmbeddingClient {
    async fn embed_once(
        &self,
        _request: &EmbeddingRequest,
        _cancel: CancellationToken,
    ) -> Result<EmbeddingResponse, EmbeddingClientError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(vector: &[f32]) -> EmbeddingResponse {
        EmbeddingResponse {
            vector: vector.to_vec(),
        }
    }

    #[tokio::test]
    async fn scripted_client_returns_results_in_order_and_counts_calls() {
        let client = ScriptedEmbeddingClient::new([
            Err(EmbeddingClientError::Server(503)),
            Ok(response(&[0.1, 0.2, 0.3])),
        ]);
        let request = EmbeddingRequest {
            model: "text-embedding-3-small".to_owned(),
            input: "input".to_owned(),
            dimensions: None,
        };

        // First call: the scripted retryable error.
        let first = client.embed_once(&request, CancellationToken::new()).await;
        assert!(matches!(first, Err(EmbeddingClientError::Server(503))));

        // Second call: the scripted success.
        let second = client.embed_once(&request, CancellationToken::new()).await;
        assert_eq!(second.unwrap(), response(&[0.1, 0.2, 0.3]));

        assert_eq!(client.calls(), 2, "each embed_once increments the counter");
    }

    #[tokio::test]
    async fn scripted_client_falls_back_to_retryable_error_when_exhausted() {
        let client = ScriptedEmbeddingClient::new(std::iter::empty());
        let request = EmbeddingRequest {
            model: "m".to_owned(),
            input: "i".to_owned(),
            dimensions: None,
        };

        let result = client.embed_once(&request, CancellationToken::new()).await;
        assert!(
            matches!(result, Err(EmbeddingClientError::Transport)),
            "an exhausted script yields the default retryable fallback",
        );
        assert_eq!(client.calls(), 1);
    }

    #[tokio::test]
    async fn scripted_client_honors_custom_exhausted_fallback() {
        let client = ScriptedEmbeddingClient::with_exhausted(
            std::iter::empty(),
            Ok(response(&[1.0])),
        );
        let request = EmbeddingRequest {
            model: "m".to_owned(),
            input: "i".to_owned(),
            dimensions: None,
        };

        let result = client.embed_once(&request, CancellationToken::new()).await;
        assert_eq!(result.unwrap(), response(&[1.0]));
    }

    #[tokio::test]
    async fn counting_client_returns_fixed_outcome_and_counts_every_call() {
        let client = CountingEmbeddingClient::always_ok(vec![0.5, 0.5]);
        let request = EmbeddingRequest {
            model: "m".to_owned(),
            input: "i".to_owned(),
            dimensions: None,
        };

        for _ in 0..3 {
            let result = client.embed_once(&request, CancellationToken::new()).await;
            assert_eq!(result.unwrap(), response(&[0.5, 0.5]));
        }
        assert_eq!(client.calls(), 3, "every call is counted");
    }

    #[tokio::test]
    async fn counting_client_can_always_fail() {
        let client = CountingEmbeddingClient::always_err(EmbeddingClientError::Client(400));
        let request = EmbeddingRequest {
            model: "m".to_owned(),
            input: "i".to_owned(),
            dimensions: None,
        };

        let result = client.embed_once(&request, CancellationToken::new()).await;
        assert!(matches!(result, Err(EmbeddingClientError::Client(400))));
        assert_eq!(client.calls(), 1);
    }
}
