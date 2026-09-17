//! OpenAI embeddings transport for the goals/memory Tier 2 embedding seam.
// The wire DTOs and domain types below are the type foundation for the
// embeddings codec (subtask 2.2) and async client (subtask 2.4). Some fields
// (e.g. the wire `index`, and the codec's `to_wire`) are exercised only by the
// codec round-trip property test (subtask 2.5) and the `halter-goals` source
// (task 6), so dead-code analysis is silenced module-wide rather than leaking
// `#[allow]` onto each individual item.
#![allow(dead_code)]
// pattern: Imperative Shell
//
// This module owns both the embeddings request/response representation
// (functional core) and the async transport (imperative shell). The wire DTOs
// (`OpenAiEmbeddingRequestBody`, `OpenAiEmbeddingResponseBody`,
// `OpenAiEmbeddingObject`) mirror the OpenAI `/v1/embeddings` JSON shape and
// stay `pub(crate)` because they are transport-internal. The domain types
// (`EmbeddingRequest`, `EmbeddingResponse`) are the crate-public surface the
// `halter-goals` embedding source builds against, decoupled from the wire
// encoding so response parsing and dimension enforcement stay in the pure
// core. `OpenAiEmbeddingClient` is the imperative shell: it drives the shared
// `JsonHttpClient` transport (per-request timeout, cancellation, `Retry-After`
// parsing, sensitive-header redaction, HTTP classification), holds the bearer
// as a `SecretString`, and maps the transport's typed `ProviderError` onto the
// classified `EmbeddingClientError` surface.

use async_trait::async_trait;
use halter_protocol::ProviderErrorKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::http_client::{JsonHttpClient, JsonRequest, join_url, provider_error_from_anyhow};
use crate::resilience::ResiliencePolicy;
use crate::secret::SecretString;

/// The fixed OpenAI embeddings path segment appended to the configured base
/// URL to resolve the request endpoint (Req 3.3).
const EMBEDDINGS_PATH: &str = "/v1/embeddings";

/// The transport label used in shared HTTP-client diagnostics for embeddings
/// requests. Matches the `openai` provider label convention.
const PROVIDER_LABEL: &str = "openai-embeddings";

/// Wire body for a single OpenAI `/v1/embeddings` request.
///
/// The `dimensions` field is omitted from the serialized body when no
/// dimension override is configured, so the model's default dimension applies
/// (Req 3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OpenAiEmbeddingRequestBody {
    /// The configured OpenAI embeddings model identifier (Req 3.1).
    pub model: String,
    /// The deterministic input text to embed (Req 3.1).
    pub input: String,
    /// Optional dimension override; omitted when `None` (Req 3.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
}

/// Wire body for a successful OpenAI `/v1/embeddings` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OpenAiEmbeddingResponseBody {
    /// The embedding objects returned by the API, in response order.
    pub data: Vec<OpenAiEmbeddingObject>,
    /// The model that produced the embeddings.
    pub model: String,
}

/// A single embedding object within an OpenAI `/v1/embeddings` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OpenAiEmbeddingObject {
    /// The embedding vector, in the element order returned by the API (Req 4.1).
    pub embedding: Vec<f32>,
    /// The zero-based index of this object within the response batch.
    pub index: u32,
}

/// A single embedding request: model + input text (+ optional dimension).
///
/// This is the crate-public domain request the `halter-goals` embedding source
/// builds and hands to the transport, decoupled from the wire DTO above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingRequest {
    /// The configured OpenAI embeddings model identifier (Req 3.1).
    pub model: String,
    /// The deterministic input text to embed (Req 3.1).
    pub input: String,
    /// Optional dimension override; when `None` the model default applies
    /// (Req 3.2).
    pub dimensions: Option<u32>,
}

/// A parsed, usable embedding response: the selected embedding object's vector.
///
/// The `vector` preserves the element order returned by the API (Req 4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingResponse {
    /// The embedding vector, element order preserved from the API response.
    pub vector: Vec<f32>,
}

/// Parse a wire response body into a usable [`EmbeddingResponse`], applying the
/// full set of `None` rules.
///
/// The first embedding object's vector is selected and its element order is
/// preserved (Req 4.1). The response degrades to `None` when:
///
/// - the `data` array is empty, i.e. zero embedding objects (Req 4.3);
/// - the selected object's vector is zero-length (Req 4.4);
/// - any element of the selected vector is NaN or infinite (Req 1.3);
/// - the selected vector's length does not equal `dimension` (Req 9.4).
///
/// Bodies that cannot be deserialized into at least one embedding object with a
/// numeric vector (Req 4.2) never reach this function — they fail at the serde
/// boundary and are handled by the caller as a `None`.
///
/// `dimension` is the configured [`Embedding_Dimension`] the request was made
/// with; enforcing it here keeps the query-time and write-time paths comparable
/// (Req 9.4). Passing the actual returned length disables the dimension check
/// for callers that enforce it elsewhere.
#[must_use]
pub(crate) fn parse_embedding(
    body: &OpenAiEmbeddingResponseBody,
    dimension: u32,
) -> Option<EmbeddingResponse> {
    // Req 4.3: a response with zero embedding objects yields None.
    let first = body.data.first()?;
    let vector = &first.embedding;

    // Req 4.4: a zero-length selected vector yields None.
    if vector.is_empty() {
        return None;
    }

    // Req 1.3: any NaN or infinite element yields None.
    if vector.iter().any(|element| !element.is_finite()) {
        return None;
    }

    // Req 9.4: a length that does not match the configured dimension yields
    // None rather than an embedding of a mismatched length.
    if vector.len() != dimension as usize {
        return None;
    }

    Some(EmbeddingResponse {
        vector: vector.clone(),
    })
}

/// Serialize a usable embedding vector back into a wire response body carrying a
/// single embedding object.
///
/// This is the serialize half of the round-trip codec (Req 4.5): for any
/// success payload that parses into a non-empty vector, `parse → to_wire →
/// parse` yields an element-wise-equal vector. `model` labels the produced body
/// with the model that generated the embedding.
#[must_use]
pub(crate) fn to_wire(vector: &[f32], model: &str) -> OpenAiEmbeddingResponseBody {
    OpenAiEmbeddingResponseBody {
        data: vec![OpenAiEmbeddingObject {
            embedding: vector.to_vec(),
            index: 0,
        }],
        model: model.to_owned(),
    }
}

/// Classified failure surface for a single embeddings attempt.
///
/// Each variant maps a transport-level condition onto the retry classification
/// the requirements demand (see the error-surface mapping in the design). The
/// variants deliberately carry **no** credential, URL, or response-body text:
/// only a coarse HTTP status code (for `Server`/`Client`) and an optional
/// server-supplied retry hint are retained, so the resolved bearer credential
/// can never leak through a log or error value (Req 5.6). The redacted messages
/// keep the same discipline as the sensitive-header redaction elsewhere in the
/// crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddingClientError {
    /// A connection or transport fault reaching the backend. Retryable
    /// (Req 6.1, 8.1).
    #[error("embedding request failed: transport error")]
    Transport,
    /// The attempt exceeded its per-request timeout. Retryable (Req 7.2, 8.1).
    #[error("embedding request failed: timed out")]
    Timeout,
    /// The backend returned HTTP 429. Retryable; `retry_after` carries any
    /// server-supplied delay hint, or `None` when none was provided
    /// (Req 8.1).
    #[error("embedding request failed: rate limited")]
    RateLimited {
        /// Optional server-supplied retry-after delay (e.g. parsed from a
        /// `Retry-After` header). `None` when the backend supplied no hint.
        retry_after: Option<std::time::Duration>,
    },
    /// The backend returned an HTTP status in the 500–599 range. Retryable
    /// (Req 8.1). Carries only the numeric status, never any body text.
    #[error("embedding request failed: server error {0}")]
    Server(u16),
    /// The backend returned an HTTP status in the 400–499 range other than
    /// 429. Not retryable (Req 6.2, 8.4). Carries only the numeric status,
    /// never any body text.
    #[error("embedding request failed: client error {0}")]
    Client(u16),
    /// The response was received but is unusable: empty `data`, a zero-length
    /// vector, a non-finite element, or an otherwise unparseable body. Not
    /// retryable (Req 4.2–4.4).
    #[error("embedding request failed: unusable response")]
    Unusable,
}

impl EmbeddingClientError {
    /// Whether the classified failure is safe to retry.
    ///
    /// Retryable iff the condition is transient: a transport fault, a request
    /// timeout, an HTTP 429 rate limit, or an HTTP 5xx server error
    /// (Req 8.1). Non-retryable conditions are 4xx client errors other than
    /// 429 and unusable responses (Req 6.2, 8.4, 4.2–4.4).
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport | Self::Timeout | Self::RateLimited { .. } | Self::Server(_)
        )
    }
}

/// Async OpenAI embeddings transport.
///
/// A single call performs **exactly one** embedding attempt and returns the
/// parsed response or a classified [`EmbeddingClientError`]. Retry is layered
/// by the caller (the `halter-goals` embedding source, task 6.2) so it can be
/// exercised deterministically against a fake client, and so this trait stays a
/// thin, single-attempt transport seam. Implemented for production by
/// [`OpenAiEmbeddingClient`] and by test fakes in `halter-goals`.
#[async_trait]
pub trait EmbeddingClient: Send + Sync {
    /// Perform one embedding attempt.
    ///
    /// Returns the parsed [`EmbeddingResponse`] on success, or a classified
    /// [`EmbeddingClientError`] whose [`EmbeddingClientError::is_retryable`]
    /// tells the caller whether another attempt is worthwhile. Never retries
    /// internally.
    async fn embed_once(
        &self,
        request: &EmbeddingRequest,
        cancel: CancellationToken,
    ) -> Result<EmbeddingResponse, EmbeddingClientError>;
}

/// Production OpenAI embeddings client.
///
/// Drives the shared [`JsonHttpClient`] transport, which already implements the
/// per-request timeout, cancellation, `Retry-After` parsing, sensitive-header
/// redaction, and HTTP status classification the requirements demand (Req 7,
/// 8). The bearer credential is held as a [`SecretString`] so it can never leak
/// through a `Debug`/`Display` render (Req 5.6), and is exposed only at the
/// point the `Authorization` header is built. The endpoint is resolved once at
/// construction by appending [`EMBEDDINGS_PATH`] to the configured base URL,
/// defaulting to `https://api.openai.com` (Req 3.3).
#[derive(Clone)]
pub struct OpenAiEmbeddingClient {
    http: JsonHttpClient,
    endpoint: String,
    bearer: SecretString,
}

impl std::fmt::Debug for OpenAiEmbeddingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omit `bearer` (a SecretString redacts itself, but the
        // endpoint alone is the useful diagnostic) — no credential surface.
        f.debug_struct("OpenAiEmbeddingClient")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl OpenAiEmbeddingClient {
    /// Default OpenAI base URL used when no override is configured (Req 3.3).
    pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

    /// Build a client from a resolved bearer credential, an optional base-URL
    /// override, and a [`ResiliencePolicy`] whose `timeouts.request` bounds
    /// each attempt (Req 7).
    ///
    /// The endpoint is resolved eagerly by appending the fixed embeddings path
    /// segment to `base_url` (or the default when `base_url` is `None`),
    /// trimming any duplicate slash (Req 3.3).
    ///
    /// # Errors
    /// Returns an error if the underlying HTTP client cannot be constructed.
    pub fn new(
        bearer: SecretString,
        base_url: Option<&str>,
        policy: ResiliencePolicy,
    ) -> anyhow::Result<Self> {
        let http = JsonHttpClient::try_new_with_timeouts(policy.timeouts)?;
        let base = base_url
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(Self::DEFAULT_BASE_URL);
        let endpoint = join_url(base, EMBEDDINGS_PATH);
        Ok(Self {
            http,
            endpoint,
            bearer,
        })
    }

    /// Serialize the domain request into the transport-internal wire body.
    fn wire_body(request: &EmbeddingRequest) -> OpenAiEmbeddingRequestBody {
        OpenAiEmbeddingRequestBody {
            model: request.model.clone(),
            input: request.input.clone(),
            dimensions: request.dimensions,
        }
    }
}

#[async_trait]
impl EmbeddingClient for OpenAiEmbeddingClient {
    async fn embed_once(
        &self,
        request: &EmbeddingRequest,
        cancel: CancellationToken,
    ) -> Result<EmbeddingResponse, EmbeddingClientError> {
        // Serialize the wire body; a serialization failure here is a fatal,
        // non-retryable defect in request construction rather than a transport
        // condition, so it maps to `Unusable`.
        let body = serde_json::to_value(Self::wire_body(request))
            .map_err(|_| EmbeddingClientError::Unusable)?;

        let http_request = JsonRequest {
            provider_label: PROVIDER_LABEL,
            url: self.endpoint.clone(),
            // The bearer is exposed only here, into a header the shared client
            // marks sensitive so reqwest/http redact it (Req 5.2, 5.3, 5.6).
            headers: vec![(
                "Authorization".to_owned(),
                format!("Bearer {}", self.bearer.expose_secret()),
            )],
            body,
        };

        // `post_json` bounds the whole request with `timeouts.request`, races
        // cancellation, parses `Retry-After`, and classifies non-2xx statuses.
        let value = match self.http.post_json(http_request, cancel).await {
            Ok(value) => value,
            Err(error) => return Err(classify_transport_error(error)),
        };

        // The dimension the API returns is authoritative for the transport
        // layer; the source enforces the configured dimension when mapping to
        // the domain `Embedding`. Pass the returned length so `parse_embedding`
        // still rejects empty/non-finite bodies here (Req 4.2-4.4, 1.3).
        parse_response_value(&value, request.dimensions)
    }
}

/// Map the shared transport's typed [`ProviderError`] onto the classified
/// embeddings error surface (see the design's error-mapping table).
///
/// `JsonHttpClient` collapses HTTP status into `RateLimited` (429),
/// `Transient` (5xx, transport faults, and timeouts), `Fatal` (4xx and
/// undecodable success bodies), and `Cancelled`. The numeric status is not
/// recovered, so the retryable `Transient` bucket maps to [`Timeout`] when the
/// message identifies a timeout and [`Transport`] otherwise, and the
/// non-retryable `Fatal`/`Cancelled` buckets map to [`Client`], preserving the
/// requirement-critical retryable-vs-not classification (Req 6.1, 6.2, 7.2,
/// 8.1, 8.4).
///
/// [`Timeout`]: EmbeddingClientError::Timeout
/// [`Transport`]: EmbeddingClientError::Transport
/// [`Client`]: EmbeddingClientError::Client
fn classify_transport_error(error: anyhow::Error) -> EmbeddingClientError {
    let provider_error = provider_error_from_anyhow(error);
    match provider_error.kind {
        ProviderErrorKind::RateLimited => EmbeddingClientError::RateLimited {
            retry_after: provider_error.backoff_hint,
        },
        ProviderErrorKind::Transient => {
            if is_timeout_message(&provider_error.message) {
                EmbeddingClientError::Timeout
            } else {
                EmbeddingClientError::Transport
            }
        }
        // A `Fatal` transport error is a decisive rejection (4xx / auth /
        // malformed request) or an undecodable body — non-retryable. Cancelled
        // requests must likewise stop rather than retry. Any future
        // `ProviderErrorKind` variant defaults to the same non-retryable
        // client-class failure (the numeric status is not recoverable from the
        // shared transport). `ProviderErrorKind` is `#[non_exhaustive]`, so the
        // wildcard arm is required.
        ProviderErrorKind::Fatal | ProviderErrorKind::Cancelled | _ => {
            EmbeddingClientError::Client(reqwest::StatusCode::BAD_REQUEST.as_u16())
        }
    }
}

/// Whether a transient transport message describes a request timeout, so it can
/// be classified as [`EmbeddingClientError::Timeout`] rather than the generic
/// [`EmbeddingClientError::Transport`]. Both are retryable; the distinction is
/// diagnostic (Req 7.2, 8.1).
fn is_timeout_message(message: &str) -> bool {
    message.contains("timed out")
}

/// Parse a decoded success-response JSON value into the domain response.
///
/// A body that cannot be deserialized into at least one embedding object with a
/// numeric vector is `Unusable` (Req 4.2); an otherwise-parseable body that
/// fails the `None` rules (empty data, zero-length vector, non-finite element,
/// or length mismatch) is likewise `Unusable` (Req 4.3, 4.4, 1.3, 9.4).
fn parse_response_value(
    value: &Value,
    dimension: Option<u32>,
) -> Result<EmbeddingResponse, EmbeddingClientError> {
    let body: OpenAiEmbeddingResponseBody =
        serde_json::from_value(value.clone()).map_err(|_| EmbeddingClientError::Unusable)?;
    // When no dimension override is configured the model's default dimension
    // applies, so the transport enforces only finiteness/non-emptiness by
    // checking against the returned length; the source enforces the configured
    // dimension when it maps the response to a domain `Embedding` (Req 9.4).
    let expected = dimension.unwrap_or_else(|| {
        body.data
            .first()
            .map_or(0, |object| object.embedding.len() as u32)
    });
    parse_embedding(&body, expected).ok_or(EmbeddingClientError::Unusable)
}

#[cfg(test)]
mod tests {
    use super::{OpenAiEmbeddingObject, OpenAiEmbeddingResponseBody, parse_embedding, to_wire};

    fn body(objects: Vec<Vec<f32>>) -> OpenAiEmbeddingResponseBody {
        OpenAiEmbeddingResponseBody {
            data: objects
                .into_iter()
                .enumerate()
                .map(|(index, embedding)| OpenAiEmbeddingObject {
                    embedding,
                    index: index as u32,
                })
                .collect(),
            model: "text-embedding-3-small".to_owned(),
        }
    }

    #[test]
    fn parses_first_object_preserving_order() {
        let response = body(vec![vec![1.0, 2.0, 3.0], vec![9.0, 9.0, 9.0]]);
        let parsed = parse_embedding(&response, 3).expect("first object should parse");
        assert_eq!(parsed.vector, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn empty_data_yields_none() {
        // Req 4.3: zero embedding objects -> None.
        let response = body(vec![]);
        assert!(parse_embedding(&response, 3).is_none());
    }

    #[test]
    fn zero_length_vector_yields_none() {
        // Req 4.4: selected vector length zero -> None.
        let response = body(vec![vec![]]);
        assert!(parse_embedding(&response, 0).is_none());
    }

    #[test]
    fn nan_element_yields_none() {
        // Req 1.3: any NaN element -> None.
        let response = body(vec![vec![1.0, f32::NAN, 3.0]]);
        assert!(parse_embedding(&response, 3).is_none());
    }

    #[test]
    fn infinite_element_yields_none() {
        // Req 1.3: any infinite element -> None.
        let response = body(vec![vec![1.0, f32::INFINITY, 3.0]]);
        assert!(parse_embedding(&response, 3).is_none());

        let response = body(vec![vec![f32::NEG_INFINITY, 2.0]]);
        assert!(parse_embedding(&response, 2).is_none());
    }

    #[test]
    fn dimension_mismatch_yields_none() {
        // Req 9.4: length != configured dimension -> None.
        let response = body(vec![vec![1.0, 2.0, 3.0]]);
        assert!(parse_embedding(&response, 4).is_none());
        assert!(parse_embedding(&response, 2).is_none());
    }

    #[test]
    fn to_wire_produces_single_data_object() {
        let wire = to_wire(&[1.0, 2.0, 3.0], "text-embedding-3-small");
        assert_eq!(wire.data.len(), 1);
        assert_eq!(wire.data[0].embedding, vec![1.0, 2.0, 3.0]);
        assert_eq!(wire.data[0].index, 0);
        assert_eq!(wire.model, "text-embedding-3-small");
    }

    #[test]
    fn parse_to_wire_parse_round_trips() {
        // Req 4.5: parse -> to_wire -> parse yields an element-wise-equal vector.
        let response = body(vec![vec![0.5, -1.25, 3.0, 42.0]]);
        let first = parse_embedding(&response, 4).expect("initial parse should succeed");

        let wire = to_wire(&first.vector, "text-embedding-3-small");
        let round_tripped = parse_embedding(&wire, 4).expect("round-trip parse should succeed");

        assert_eq!(first.vector, round_tripped.vector);
    }
}
