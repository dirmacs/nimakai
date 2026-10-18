use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

/// Per-process random salt for conversation fingerprints.
///
/// The salt is mixed into the hash of the first user message's content so the
/// fingerprint cannot be reversed to prompt text. A loop never outlives the
/// process, so cross-restart stability is not required — only in-process
/// stability, which a per-process salt gives us for free.
static FINGERPRINT_SALT: LazyLock<u64> = LazyLock::new(|| {
    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut hasher);
    hasher.finish()
});

/// Fingerprint a conversation from its first user message.
///
/// Returns `(last_role, fingerprint)`. `last_role` is the role of the final
/// element of `messages`. `fingerprint` is derived from the first message
/// whose role is `user` (its serialized `content`, hashed with a per-process
/// salt) and is formatted as `fp:<16 hex chars>`. If there is no user message
/// the fingerprint is `None`.
///
/// The hash is salted per process so it cannot be reversed to prompt text, and
/// a loop never outlives the process, so cross-restart stability is not
/// required. Malformed JSON (or no `messages` array) does not panic: it yields
/// `(None, None)`, which means "do not track this request".
pub(crate) fn conversation_identity(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let Some(messages) = json.get("messages").and_then(|m| m.as_array()) else {
        return (None, None);
    };
    let last_role = messages
        .last()
        .and_then(|m| m.get("role").and_then(|r| r.as_str()))
        .map(|r| r.to_string());
    let fingerprint = messages
        .iter()
        .find_map(|m| {
            if m.get("role").and_then(|r| r.as_str()) == Some("user") {
                m.get("content")
                    .and_then(|c| c.as_str())
                    .map(|c| c.to_string())
            } else {
                None
            }
        })
        .map(|content| {
            let mut hasher = DefaultHasher::new();
            content.hash(&mut hasher);
            FINGERPRINT_SALT.hash(&mut hasher);
            format!("fp:{:016x}", hasher.finish())
        });
    (last_role, fingerprint)
}
/// Sanitize a client-supplied session id before it is written to a log file.
///
/// Trimmed, control characters stripped, and length bounded at 128 chars so a
/// malicious or malformed header cannot inject log entries or denial-of-service
/// the reader.
pub(crate) fn sanitize_session(session: &str) -> String {
    let trimmed = session.trim();
    let no_control: String = trimmed.chars().filter(|c| !c.is_control()).collect();
    no_control.chars().take(128).collect()
}
use crate::AppState;
use bytes::Bytes;
use serde_json::Value;
pub(crate) fn timeout_for_model(state: &AppState, model_id: &str) -> u64 {
    state.model_stats.get_model_timeout_with_policy(
        model_id,
        state.racing_timeout_ms,
        state.min_dynamic_timeout_ms,
        state.dynamic_sample_floor,
    )
}
pub(crate) fn record_model_timeout(
    state: &AppState,
    model_id: &str,
    key_label: Option<&str>,
    elapsed_ms: u64,
) {
    if let Some(label) = key_label {
        state
            .model_stats
            .record_timeout_with_key(model_id, label, elapsed_ms as f64);
    } else {
        state
            .model_stats
            .record_timeout(model_id, elapsed_ms as f64);
    }
}
pub(crate) fn request_turn_summary(body: &Bytes) -> (usize, bool, usize) {
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return (0, false, 0);
    };
    let Some(messages) = json.get("messages").and_then(|m| m.as_array()) else {
        return (0, false, 0);
    };
    let mut tool_count = 0usize;
    let mut has_tool_role = false;
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) == Some("tool") {
            has_tool_role = true;
            tool_count += 1;
        }
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
            tool_count += tool_calls.len();
        }
    }
    (messages.len(), has_tool_role || tool_count > 0, tool_count)
}
pub(crate) fn prompt_chars(body: &Bytes) -> usize {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("messages").cloned())
        .and_then(|m| m.as_array().cloned())
        .map(|messages| {
            messages
                .iter()
                .map(|msg| msg.get("content").map(|c| c.to_string().len()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The last role reported is the role of the final message, so a turn that
    /// ends with a `user` message reports `"user"` and one that ends with
    /// `assistant` reports `"assistant"`.
    #[test]
    fn conversation_identity_reports_last_role() {
        let body = br#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}]}"#;
        let (last_role, _) = conversation_identity(body);
        assert_eq!(last_role.as_deref(), Some("assistant"));

        let body = br#"{"messages":[{"role":"assistant","content":"hello"},{"role":"user","content":"hi"}]}"#;
        let (last_role, _) = conversation_identity(body);
        assert_eq!(last_role.as_deref(), Some("user"));
    }

    /// The fingerprint is stable for the same first user message and differs
    /// for a different one, so the same conversation loops back to the same
    /// loop id across requests while different conversations do not collide.
    #[test]
    fn conversation_identity_fingerprint_is_stable_and_discriminating() {
        let a = br#"{"messages":[{"role":"user","content":"same prompt"}]}"#;
        let b = br#"{"messages":[{"role":"user","content":"same prompt"}]}"#;
        let c = br#"{"messages":[{"role":"user","content":"different prompt"}]}"#;
        let (_, fa) = conversation_identity(a);
        let (_, fb) = conversation_identity(b);
        let (_, fc) = conversation_identity(c);
        assert_eq!(
            fa, fb,
            "same first user message yields the same fingerprint"
        );
        assert_ne!(
            fa, fc,
            "different first user messages yield different fingerprints"
        );
        assert!(fa.unwrap().starts_with("fp:"));
    }

    /// No user message yields `None` fingerprint, so the request is not
    /// tracked as part of a loop.
    #[test]
    fn conversation_identity_no_user_message_is_none() {
        let body = br#"{"messages":[{"role":"assistant","content":"hello"}]}"#;
        let (_, fingerprint) = conversation_identity(body);
        assert_eq!(fingerprint, None);
    }

    /// Malformed JSON does not panic: it yields `(None, None)`, which means
    /// "do not track this request".
    #[test]
    fn conversation_identity_malformed_json_does_not_panic() {
        let (last_role, fingerprint) = conversation_identity(b"{not json");
        assert_eq!(last_role, None);
        assert_eq!(fingerprint, None);
    }

    /// An empty messages array yields `(None, None)`.
    #[test]
    fn conversation_identity_empty_messages_is_none() {
        let body = br#"{"messages":[]}"#;
        let (last_role, fingerprint) = conversation_identity(body);
        assert_eq!(last_role, None);
        assert_eq!(fingerprint, None);
    }

    /// `sanitize_session` trims whitespace, strips control characters, and
    /// bounds length at 128 chars so a malicious header cannot inject log
    /// entries.
    #[test]
    fn sanitize_session_trims_and_bounds() {
        let long = "a".repeat(200);
        let sanitized = sanitize_session(&long);
        assert_eq!(sanitized.len(), 128);

        // Tab and newline ARE control characters and must be STRIPPED, not
        // merely trimmed. A session id is written verbatim into a JSONL log
        // line, so a value carrying a newline could forge an extra record. An
        // earlier version of this test asserted that tab and newline survived,
        // which was asserting the vulnerability rather than the contract.
        assert_eq!(sanitize_session("  hello\t\nworld  "), "helloworld");

        let with_ctrl_char = "hello\x00world";
        assert!(!sanitize_session(with_ctrl_char).contains('\x00'));
    }
}
