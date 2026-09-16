//! Shared identifier and value types used across the Goal Model, Tier 1, and
//! Tier 2 subsystems.
//!
//! These types were relocated into `halter-protocol` (module
//! [`halter_protocol::goals`]) so the protocol crate can carry the
//! `GoalNodeId` attribution tag and the `GoalEvent` payload on
//! `SessionEventPayload` without a circular dependency on `halter-goals`.
//! This module re-exports them under their historical `halter-goals` names so
//! the `goal_model`, `tier1`, `tier2`, and `integration` subsystems — and
//! external callers using `halter_goals::{...}` — keep their existing paths and
//! vocabulary unchanged.

// Re-exported with historical names. In `halter-protocol` a few of these carry
// a `Goal`/`Goal*` prefix to avoid colliding with the protocol crate's own
// `Timestamp`/`Duration`/`ToolName`/`ToolCall`; here they keep the names the
// Goal Model has always used.
pub use halter_protocol::goals::{
    CanonicalJson, ContentRef, EventKey, EventSeq, EvidenceValue, GoalNodeId, IntentSignature,
    IntentType, MemoryId, OutcomeRef, Scope, Sha256, SourceDescriptor, SubtreeHash, TargetRef,
    TargetType, ValidityToken,
};
pub use halter_protocol::goals::{
    GoalDuration as Duration, GoalTimestamp as Timestamp, GoalToolCall as ToolCall,
    GoalToolName as ToolName,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_newtypes_are_unique_and_roundtrip() {
        let a = GoalNodeId::new();
        let b = GoalNodeId::new();
        assert_ne!(a, b, "freshly generated ids must be distinct");

        let json = serde_json::to_string(&a).expect("serialize id");
        let back: GoalNodeId = serde_json::from_str(&json).expect("deserialize id");
        assert_eq!(a, back, "id must survive a serde round trip");
    }

    #[test]
    fn string_newtypes_convert_from_str_and_string() {
        let from_str = ToolName::from("read_file");
        let from_string = ToolName::from(String::from("read_file"));
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.to_string(), "read_file");
    }

    #[test]
    fn timestamp_add_duration_saturates() {
        let base = Timestamp(10);
        assert_eq!(base.plus(Duration(5)), Timestamp(15));
        assert_eq!(Timestamp(u64::MAX).plus(Duration(1)), Timestamp(u64::MAX));
    }

    #[test]
    fn intent_signature_roundtrips_all_four_fields() {
        let sig = IntentSignature {
            intent_type: IntentType::from("lookup"),
            target_type: TargetType::from("file"),
            target_ref: TargetRef::from("src/lib.rs"),
            scope: Scope::from("repo"),
        };
        let json = serde_json::to_string(&sig).expect("serialize signature");
        let back: IntentSignature = serde_json::from_str(&json).expect("deserialize signature");
        assert_eq!(sig, back);
    }

    #[test]
    fn subtree_hash_wraps_sha256_and_displays() {
        let h = SubtreeHash(Sha256::from("abc123"));
        assert_eq!(h.to_string(), "abc123");
        let json = serde_json::to_string(&h).expect("serialize hash");
        let back: SubtreeHash = serde_json::from_str(&json).expect("deserialize hash");
        assert_eq!(h, back);
    }

    #[test]
    fn canonical_json_exposes_bytes() {
        let c = CanonicalJson("{\"a\":1}".to_owned());
        assert_eq!(c.as_bytes(), b"{\"a\":1}");
    }

    #[test]
    fn validity_token_variants_roundtrip() {
        let tokens = vec![
            ValidityToken::ContentHash(Sha256::from("deadbeef")),
            ValidityToken::Ttl {
                issued_at: Timestamp(100),
                ttl: Duration(50),
            },
            ValidityToken::EventDriven {
                subscription: EventKey::from("fs-changes"),
                last_seen: EventSeq(7),
            },
        ];
        for token in tokens {
            let json = serde_json::to_string(&token).expect("serialize token");
            let back: ValidityToken = serde_json::from_str(&json).expect("deserialize token");
            assert_eq!(token, back, "validity token must survive a serde round trip");
        }
    }

    #[test]
    fn source_descriptor_variants_roundtrip() {
        let descriptors = vec![
            SourceDescriptor::Pinnable {
                content: ContentRef::from("file://src/lib.rs"),
            },
            SourceDescriptor::Volatile {
                ttl: Duration(1_000),
            },
            SourceDescriptor::Signalled {
                subscription: EventKey::from("clock-tick"),
            },
        ];
        for descriptor in descriptors {
            let json = serde_json::to_string(&descriptor).expect("serialize descriptor");
            let back: SourceDescriptor =
                serde_json::from_str(&json).expect("deserialize descriptor");
            assert_eq!(descriptor, back);
        }
    }

    #[test]
    fn tool_call_roundtrips_all_fields() {
        let call = ToolCall {
            tool: ToolName::from("read_file"),
            normalized_args: CanonicalJson("{\"path\":\"src/lib.rs\"}".to_owned()),
            validity_token: ValidityToken::ContentHash(Sha256::from("abc123")),
            outcome: OutcomeRef::from("outcome://1"),
        };
        let json = serde_json::to_string(&call).expect("serialize tool call");
        let back: ToolCall = serde_json::from_str(&json).expect("deserialize tool call");
        assert_eq!(call, back);
    }
}
