//! Runtime-ready resolution of the embedding configuration surface.
//!
//! [`ResolvedEmbeddingSettings`] is the single source of truth shared by the
//! query-path `OpenAiEmbeddingSource` and the write-path `MemoryEmbeddingWriter`
//! (Req 9.1). It bridges `halter-config`'s [`EmbeddingConfig`] and resolved
//! OpenAI provider auth ([`ResolvedProviderAuth`]) into a form the source and
//! writer consume, holding the bearer credential as a redacting
//! [`SecretString`] (Req 5.6).
//!
//! Placement: this type lives in `halter-goals` (not `halter-config`) because
//! it carries a [`SecretString`], which lives in `halter-providers`. Keeping it
//! here avoids introducing a `halter-config -> halter-providers` dependency,
//! while `halter-goals` already depends on both crates and is where the source
//! and writer that consume these settings live.

use std::time::Duration;

use halter_config::{
    DEFAULT_EMBEDDING_CACHE_MAX_ENTRIES, DEFAULT_EMBEDDING_MAX_ATTEMPTS,
    DEFAULT_EMBEDDING_TIMEOUT_SECS, EmbeddingConfig, ResolvedProviderAuth,
};
use halter_providers::SecretString;

/// The default embedding base URL, used when no override is configured
/// (Req 3.3). This mirrors the OpenAI provider's default base URL.
pub const DEFAULT_EMBEDDING_BASE_URL: &str = "https://api.openai.com";

/// Lowest permitted runtime value for `max_attempts` (Req 8.2, 8.3).
pub const MIN_EMBEDDING_MAX_ATTEMPTS: u32 = 1;
/// Highest permitted runtime value for `max_attempts` (Req 8.2, 8.3).
pub const MAX_EMBEDDING_MAX_ATTEMPTS: u32 = 10;

/// Runtime-ready embedding settings shared by the query and write paths.
///
/// Produced by [`ResolvedEmbeddingSettings::resolve`], which applies runtime
/// defaults and clamps to `EmbeddingConfig` values that pass schema validation
/// but still fall outside the runtime ranges the requirements demand:
///
/// - `max_attempts` is clamped to `1..=10`, defaulting to 3 (Req 8.2, 8.3).
/// - `timeout` falls back to 30s when unset or less than one second (Req 7.1,
///   7.3).
/// - `base_url` defaults to `https://api.openai.com` (Req 3.3).
///
/// `model` and `dimension` are read from a single configuration source so the
/// source and writer agree byte-for-byte on the model and integer-for-integer
/// on the dimension (Req 9.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEmbeddingSettings {
    /// Whether the embedding source is enabled (Req 11.4, 11.5).
    pub enabled: bool,
    /// The embeddings model identifier (Req 9.1, 11.1).
    pub model: String,
    /// Optional dimension override; `None` lets the model default apply
    /// (Req 3.2, 9.1).
    pub dimension: Option<u32>,
    /// The resolved base URL, defaulting to `https://api.openai.com` (Req 3.3).
    pub base_url: String,
    /// Per-request timeout, defaulting to 30s (Req 7.1, 7.3).
    pub timeout: Duration,
    /// Total attempts per `embed`, clamped to `1..=10`, default 3 (Req 8.2,
    /// 8.3).
    pub max_attempts: u32,
    /// Maximum embedding-cache entry count, defaulting to 1024 (Req 10.6). The
    /// source sizes its in-process LRU cache from this value.
    pub cache_max_entries: usize,
    /// The resolved bearer credential, held in a redacting wrapper (Req 5.2,
    /// 5.3, 5.6).
    pub bearer: SecretString,
}

impl ResolvedEmbeddingSettings {
    /// Resolve runtime settings from an [`EmbeddingConfig`] and the resolved
    /// OpenAI provider auth.
    ///
    /// The `auth` argument is produced by
    /// `halter_config::resolve_provider_runtime_config` for
    /// `ConfiguredProvider::OpenAi`, which already applies the credential
    /// precedence rules (configured api_key/oauth win over the environment,
    /// api_key and oauth are mutually exclusive, `OPENAI_API_KEY` is the env
    /// fallback) that Req 5.1–5.4 require. This function only *selects the
    /// bearer* from that resolved auth (Req 5.2, 5.3) and wraps it in a
    /// [`SecretString`] (Req 5.6):
    ///
    /// - [`ResolvedProviderAuth::ApiKey`] -> the API key is the bearer.
    /// - [`ResolvedProviderAuth::OpenAiOAuth`] -> the OAuth `access_token` is
    ///   the bearer.
    #[must_use]
    pub fn resolve(config: &EmbeddingConfig, auth: &ResolvedProviderAuth) -> Self {
        Self {
            enabled: config.enabled,
            model: config.model.clone(),
            dimension: config.dimension,
            base_url: resolve_base_url(config.base_url.as_deref()),
            timeout: resolve_timeout(config.timeout_secs),
            max_attempts: resolve_max_attempts(config.max_attempts),
            cache_max_entries: resolve_cache_max_entries(config.cache_max_entries),
            bearer: select_bearer(auth),
        }
    }

    /// A disabled, credential-less settings value.
    ///
    /// Used to build the write-path [`MemoryEmbeddingWriter`] behind
    /// `InductionEngine::new`'s backward-compatible disabled path: `enabled` is
    /// `false` and the bearer is empty, so the writer short-circuits every
    /// `embed_for_write` to `WriteEmbedding::Unavailable` with no network call
    /// and induced memories degrade to an empty embedding exactly as before the
    /// write path was wired in (Req 6.4, 10.4). All other knobs take their
    /// runtime defaults; none influence a disabled writer.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            dimension: None,
            base_url: DEFAULT_EMBEDDING_BASE_URL.to_owned(),
            timeout: resolve_timeout(None),
            max_attempts: resolve_max_attempts(None),
            cache_max_entries: resolve_cache_max_entries(None),
            bearer: SecretString::from(""),
        }
    }
}

/// Resolve the base URL, defaulting to [`DEFAULT_EMBEDDING_BASE_URL`] when no
/// override is configured or the override trims to empty (Req 3.3).
fn resolve_base_url(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_EMBEDDING_BASE_URL)
        .to_owned()
}

/// Resolve the per-request timeout (Req 7.1, 7.3).
///
/// Uses the configured value when it is one second or greater; otherwise falls
/// back to the 30-second default. A configured `0` (which schema validation
/// rejects, but which the runtime still guards against) also falls back.
fn resolve_timeout(configured_secs: Option<u64>) -> Duration {
    let secs = match configured_secs {
        Some(secs) if secs >= 1 => secs,
        _ => DEFAULT_EMBEDDING_TIMEOUT_SECS,
    };
    Duration::from_secs(secs)
}

/// Resolve `max_attempts`, clamping to `1..=10` and defaulting to 3 (Req 8.2,
/// 8.3).
///
/// A configured value inside the range is used as-is; a value outside the
/// range falls back to the default of 3 total attempts (Req 8.3).
fn resolve_max_attempts(configured: Option<u32>) -> u32 {
    match configured {
        Some(attempts)
            if (MIN_EMBEDDING_MAX_ATTEMPTS..=MAX_EMBEDDING_MAX_ATTEMPTS).contains(&attempts) =>
        {
            attempts
        }
        _ => DEFAULT_EMBEDDING_MAX_ATTEMPTS,
    }
}

/// Resolve the embedding-cache capacity, defaulting to
/// [`DEFAULT_EMBEDDING_CACHE_MAX_ENTRIES`] (1024) when unset (Req 10.6).
///
/// A configured value is used as-is; schema validation already rejects a
/// zero-valued `cache_max_entries`, so any configured value that reaches the
/// runtime is `>= 1`. The `u64` config value is narrowed to `usize` (the cache
/// capacity type), saturating on platforms where it would overflow.
fn resolve_cache_max_entries(configured: Option<u64>) -> usize {
    match configured {
        Some(entries) => usize::try_from(entries).unwrap_or(usize::MAX),
        None => DEFAULT_EMBEDDING_CACHE_MAX_ENTRIES,
    }
}

/// Select the bearer credential from the resolved provider auth and wrap it in
/// a redacting [`SecretString`] (Req 5.2, 5.3, 5.6).
fn select_bearer(auth: &ResolvedProviderAuth) -> SecretString {
    match auth {
        ResolvedProviderAuth::ApiKey(api_key) => SecretString::from(api_key.as_str()),
        ResolvedProviderAuth::OpenAiOAuth(oauth) => {
            SecretString::from(oauth.access_token.as_str())
        }
    }
}
