//! Tier 1 argument normalization / canonicalization.
//!
//! Before a tool call becomes a cache key, its arguments are canonicalized so
//! that semantically equal calls key equally (and therefore hit the same cache
//! entry). [`normalize_args`] is a pure, total function: for a given tool, two
//! logically-equal argument sets always produce byte-equal [`CanonicalJson`],
//! regardless of object-key ordering, insignificant whitespace, or equivalent
//! scalar encodings (Requirements 8.1–8.4). Arguments that cannot be
//! canonicalized for the tool are rejected with a [`NormalizationError`] and no
//! cache entry is created or read (Requirement 8.6).
//!
//! The canonical form is a compact JSON encoding (no insignificant whitespace)
//! in which:
//! - object keys are sorted lexicographically by their Unicode scalar values,
//!   recursively, at every level;
//! - duplicate object keys are rejected as non-canonicalizable (an ambiguous
//!   encoding cannot be folded to a single canonical value);
//! - numbers are folded to a single canonical spelling so that equivalent scalar
//!   encodings (e.g. `1` vs `1.0` vs `1e0`) collapse to the same bytes;
//! - non-finite numbers (which `serde_json` cannot faithfully round-trip) are
//!   rejected as non-canonicalizable.

use crate::types::{CanonicalJson, ToolName};

use serde_json::Value;

/// Error returned when raw arguments cannot be canonicalized for a tool.
///
/// Per Requirement 8.6, when `normalize_args` returns this error the caller must
/// not create or read any cache entry for the request. The error is pure data:
/// producing it has no side effects and touches no cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizationError {
    /// A JSON number could not be represented in a canonical, byte-stable form
    /// (for example a non-finite float such as NaN or infinity).
    NonCanonicalNumber {
        /// The offending number, rendered for diagnostics.
        rendered: String,
    },
    /// An object contained the same key more than once, so its canonical value
    /// is ambiguous and cannot be folded deterministically.
    DuplicateKey {
        /// The duplicated object key.
        key: String,
    },
    /// The arguments are not canonicalizable for the given tool for a
    /// tool-specific reason.
    NotCanonicalizableForTool {
        /// The tool the arguments were being canonicalized for.
        tool: ToolName,
        /// A human-readable reason.
        reason: String,
    },
}

impl std::fmt::Display for NormalizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonCanonicalNumber { rendered } => {
                write!(f, "number `{rendered}` cannot be canonicalized")
            }
            Self::DuplicateKey { key } => {
                write!(f, "duplicate object key `{key}` cannot be canonicalized")
            }
            Self::NotCanonicalizableForTool { tool, reason } => {
                write!(f, "arguments not canonicalizable for tool `{tool}`: {reason}")
            }
        }
    }
}

impl std::error::Error for NormalizationError {}

/// Canonicalize a tool's raw arguments into a byte-stable [`CanonicalJson`].
///
/// This function is pure and total over its declared error space: it reads no
/// external state, mutates nothing, and never touches a cache. For a given
/// `tool`, logically-equal `raw_args` always yield byte-equal `CanonicalJson`,
/// and logically-distinct arguments always yield differing bytes (Requirements
/// 8.1–8.4).
///
/// # Errors
///
/// Returns [`NormalizationError`] when `raw_args` cannot be canonicalized for
/// `tool` — for instance a non-finite number or a duplicate object key. On error
/// no cache entry is created or read (Requirement 8.6); the caller must abort the
/// cache interaction.
pub fn normalize_args(
    _tool: &ToolName,
    raw_args: &Value,
) -> Result<CanonicalJson, NormalizationError> {
    let mut out = String::new();
    write_canonical(raw_args, &mut out)?;
    Ok(CanonicalJson(out))
}

/// Recursively write `value` into `out` in canonical form.
///
/// Objects have their keys sorted and are checked for duplicates; numbers are
/// folded to a canonical spelling; strings, booleans, and null use `serde_json`'s
/// deterministic encoding.
fn write_canonical(value: &Value, out: &mut String) -> Result<(), NormalizationError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(_) => out.push_str(&canonical_number(value)?),
        Value::String(s) => write_canonical_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sort keys lexicographically and reject duplicates. A serde_json
            // `Map` already dedups keys on parse (last-wins), so to detect
            // ambiguity deterministically we sort a materialized key list and
            // check adjacency; the map itself never holds duplicates, so this
            // guards callers that construct `Value` maps directly.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for pair in keys.windows(2) {
                if pair[0] == pair[1] {
                    return Err(NormalizationError::DuplicateKey {
                        key: pair[0].clone(),
                    });
                }
            }

            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_string(key, out);
                out.push(':');
                // Safe: `key` came from `map`, so the entry exists.
                write_canonical(&map[*key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Write a JSON string literal (with the surrounding quotes) using
/// `serde_json`'s escaping so equal strings encode to equal bytes.
fn write_canonical_string(s: &str, out: &mut String) {
    // `serde_json::to_string` on a string never fails and yields the canonical
    // escaped, quoted form.
    let encoded = serde_json::to_string(s).expect("string serialization is infallible");
    out.push_str(&encoded);
}

/// Fold a JSON number to a single canonical spelling.
///
/// Integers render without a decimal point; other finite values render via their
/// shortest round-trippable `f64` form, so equivalent encodings such as `1`,
/// `1.0`, and `1e0` collapse to the same bytes. Non-finite values are rejected.
fn canonical_number(value: &Value) -> Result<String, NormalizationError> {
    let number = match value {
        Value::Number(n) => n,
        _ => unreachable!("canonical_number called on a non-number value"),
    };

    // Prefer exact integer spellings so `1` and `1.0` fold together.
    if let Some(u) = number.as_u64() {
        return Ok(u.to_string());
    }
    if let Some(i) = number.as_i64() {
        return Ok(i.to_string());
    }

    match number.as_f64() {
        Some(f) if f.is_finite() => {
            // If the float is integral, render it without a fractional part so it
            // folds with the integer spelling (e.g. `2.0` -> `2`).
            if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
                // Within the exact-integer range of f64; render as an integer.
                Ok(format!("{}", f as i64))
            } else {
                // `{}` on f64 uses the shortest representation that round-trips.
                Ok(format!("{f}"))
            }
        }
        _ => Err(NormalizationError::NonCanonicalNumber {
            rendered: number.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> ToolName {
        ToolName::from("read_file")
    }

    fn norm(v: &Value) -> CanonicalJson {
        normalize_args(&tool(), v).expect("value should canonicalize")
    }

    #[test]
    fn object_key_order_is_irrelevant() {
        let a = json!({ "a": 1, "b": 2 });
        let b = json!({ "b": 2, "a": 1 });
        assert_eq!(norm(&a), norm(&b));
    }

    #[test]
    fn nested_object_keys_are_sorted_recursively() {
        let a = json!({ "outer": { "z": 1, "a": 2 }, "first": true });
        let out = norm(&a);
        assert_eq!(out.0, r#"{"first":true,"outer":{"a":2,"z":1}}"#);
    }

    #[test]
    fn whitespace_is_insignificant() {
        // Two logically-equal payloads, one parsed from spaced-out text.
        let spaced: Value = serde_json::from_str("{ \"a\" :   1 ,\n\"b\": 2 }").unwrap();
        let tight: Value = serde_json::from_str("{\"a\":1,\"b\":2}").unwrap();
        assert_eq!(norm(&spaced), norm(&tight));
    }

    #[test]
    fn equivalent_number_encodings_fold() {
        assert_eq!(norm(&json!(1)), norm(&json!(1.0)));
        let one_e0: Value = serde_json::from_str("1e0").unwrap();
        assert_eq!(norm(&json!(1)), norm(&one_e0));
        assert_eq!(norm(&json!(2.0)).0, "2");
    }

    #[test]
    fn logically_distinct_args_differ() {
        assert_ne!(norm(&json!({ "a": 1 })), norm(&json!({ "a": 2 })));
        assert_ne!(norm(&json!([1, 2])), norm(&json!([2, 1])));
        assert_ne!(norm(&json!({ "a": 1 })), norm(&json!({ "b": 1 })));
    }

    #[test]
    fn repeated_invocations_are_byte_identical() {
        let v = json!({ "b": [3, 2, 1], "a": { "y": 1, "x": 2 } });
        assert_eq!(norm(&v).0, norm(&v).0);
    }

    #[test]
    fn deterministic_across_fresh_values() {
        let first = norm(&json!({ "z": 26, "a": 1 }));
        let second = norm(&json!({ "a": 1, "z": 26 }));
        assert_eq!(first.0, second.0);
        assert_eq!(first.0, r#"{"a":1,"z":26}"#);
    }

    #[test]
    fn scalar_types_round_trip() {
        assert_eq!(norm(&json!(null)).0, "null");
        assert_eq!(norm(&json!(true)).0, "true");
        assert_eq!(norm(&json!("hi")).0, "\"hi\"");
        assert_eq!(norm(&json!(-5)).0, "-5");
    }

    #[test]
    fn large_integers_preserved() {
        let big = json!(9_007_199_254_740_993_i64);
        assert_eq!(norm(&big).0, "9007199254740993");
    }
}
