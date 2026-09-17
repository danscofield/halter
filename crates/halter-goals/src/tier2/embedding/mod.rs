//! The Tier 2 embedding source implementation.
//!
//! This module turns the Tier 2 embedding seam from a test-only abstraction
//! into a working OpenAI-backed implementation on both the query path and the
//! write path. It hosts the deterministic input-construction rule shared by
//! both paths, the in-process LRU [`EmbeddingCache`], the concrete
//! `OpenAiEmbeddingSource`, and the write-time `MemoryEmbeddingWriter`.
//!
//! The pieces here are built incrementally by the `openai-embedding-source`
//! tasks. This file registers the submodules, hosts the deterministic
//! `embedding_input_text` construction, and re-exports the public surface.

pub mod cache;
pub mod settings;
pub mod source;
pub mod writer;

/// Async [`halter_providers::EmbeddingClient`] test doubles (scripted fake +
/// attempt-counting spy) shared by the source/writer unit and property tests.
///
/// Gated behind `#[cfg(test)]` and kept `pub(crate)` so sibling
/// `tier2::embedding` test modules can reach the same doubles without each
/// redefining them (Req 6.3, 8.1, 8.2, 10.8).
#[cfg(test)]
pub(crate) mod test_doubles;

pub use cache::{EmbeddingCache, EmbeddingCacheKey};
pub use settings::{
    ResolvedEmbeddingSettings, DEFAULT_EMBEDDING_BASE_URL, MAX_EMBEDDING_MAX_ATTEMPTS,
    MIN_EMBEDDING_MAX_ATTEMPTS,
};
pub use source::OpenAiEmbeddingSource;
pub use writer::{
    insert_memory_with_writer, EmbeddingWriteError, MemoryEmbeddingWriter, WriteEmbedding,
};

use crate::types::IntentSignature;

/// The fixed field separator placed between [`IntentSignature`] fields when
/// building the embedding input text.
///
/// A unit separator (U+001F) is used because it does not appear in ordinary
/// field text, so field boundaries stay unambiguous and no two distinct field
/// tuples can collide (Req 2.1, 2.6).
pub const FIELD_SEPARATOR: char = '\u{001F}';

/// The fixed placeholder for an absent or empty field.
///
/// Using an empty string preserves each field's ordered position so field
/// boundaries stay stable, keeping construction byte-identical for
/// field-wise-equal inputs (Req 2.6).
pub const EMPTY_FIELD_PLACEHOLDER: &str = "";

/// Build the deterministic embedding input text from an [`IntentSignature`].
///
/// The text is derived *solely* from the four [`IntentSignature`] fields in the
/// fixed order `intent_type`, `target_type`, `target_ref`, `scope`, joined by
/// [`FIELD_SEPARATOR`] and encoded as UTF-8 (Req 2.1). No other input
/// influences the result.
///
/// - Field-wise-equal signatures yield byte-identical output (Req 2.2, 2.4).
/// - Signatures differing in any field yield different output, because the
///   separator cannot appear inside a field, so no two distinct field tuples
///   collide (Req 2.5).
/// - An absent or empty field is represented by [`EMPTY_FIELD_PLACEHOLDER`] in
///   its ordered position so boundaries are preserved (Req 2.6).
///
/// Both the query path and the write path call this exact function (the writer
/// passes `memory.intent`), guaranteeing comparable embeddings (Req 2.3, 2.4).
#[must_use]
pub fn embedding_input_text(sig: &IntentSignature) -> String {
    let fields = [
        field_or_placeholder(&sig.intent_type.0),
        field_or_placeholder(&sig.target_type.0),
        field_or_placeholder(&sig.target_ref.0),
        field_or_placeholder(&sig.scope.0),
    ];
    fields.join(&FIELD_SEPARATOR.to_string())
}

/// Return `field` if non-empty, otherwise the fixed empty-field placeholder.
///
/// The four `IntentSignature` fields are always-present `String` newtypes, so
/// an "absent" field is represented by an empty string; either way it maps to
/// [`EMPTY_FIELD_PLACEHOLDER`], preserving the field's ordered position.
fn field_or_placeholder(field: &str) -> &str {
    if field.is_empty() {
        EMPTY_FIELD_PLACEHOLDER
    } else {
        field
    }
}
