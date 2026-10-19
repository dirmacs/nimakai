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
                m.get("content").and_then(content_text)
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

/// The text of a message's `content`, which OpenAI-compatible clients send
/// either as a plain string or as an array of typed parts.
///
/// Handling only the string form was a real defect, not a theoretical one. The
/// array form is what a harness sends for every message, so those conversations
/// got no fingerprint and were therefore not tracked at all. Measured on the
/// live proxy: 363 of 391 records in a 90-minute window carried no loop
/// identity while a real 420-message agent loop ran through it — the exact
/// workload this feature exists to measure.
///
/// A part with no `text` (an image, for instance) contributes nothing but must
/// not prevent a fingerprint, so it is skipped rather than failing the match.
/// Content with no text at all returns `None` so the search moves on to the
/// next user message, which is what keeps the id stable: it is always taken
/// from the earliest user message that carries text.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                    text.push_str(s);
                }
            }
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
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

/// Request-shape signals for the turn record: whether the request carried
/// tools, and the effective `max_tokens` the upstream will apply.
///
/// Returns `(request_has_tools, request_max_tokens)`. `request_has_tools` is
/// `None` when the body has no `tools` key (or it is not an array) and
/// `Some(false)` for an explicitly empty array, so "client said no tools"
/// stays distinguishable from "shape unknown". `request_max_tokens` is the
/// integer `max_tokens` value when one fits in an `i32` — negatives
/// preserved, since the upstream decides their meaning — and `None` for a
/// missing, non-integer, or overflowing value.
///
/// Callers pass the POST-`resolve_model` body: `resolve_model` injects
/// per-model `max_tokens` defaults via `apply_model_params`, so this reads
/// the effective value, not the raw client value. Fail-soft like
/// [`request_turn_summary`]: malformed JSON yields `(None, None)`.
pub(crate) fn request_shape_summary(body: &Bytes) -> (Option<bool>, Option<i32>) {
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let has_tools = json
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| !a.is_empty());
    let max_tokens = json
        .get("max_tokens")
        .and_then(|m| m.as_i64())
        .and_then(|v| i32::try_from(v).ok());
    (has_tools, max_tokens)
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

    /// A user message whose `content` is an array of typed parts — the form a
    /// harness actually sends — must still produce a fingerprint. Handling only
    /// the string form left every such conversation untracked, which was
    /// measured on the live proxy: 363 of 391 records in a 90-minute window had
    /// no loop identity while a real 420-message agent loop ran through it.
    #[test]
    fn conversation_identity_handles_array_content() {
        let body = br#"{"messages":[{"role":"system","content":"sys"},{"role":"user","content":[{"type":"text","text":"hello parts"},{"type":"image_url","image_url":{"url":"x"}}]},{"role":"tool","content":"r"}]}"#;
        let (last_role, fingerprint) = conversation_identity(body);
        assert_eq!(last_role.as_deref(), Some("tool"));
        assert!(
            fingerprint.as_deref().is_some_and(|f| f.starts_with("fp:")),
            "array content must fingerprint, or the whole conversation is untracked"
        );
    }

    /// The id must come from the SAME message however the conversation grows,
    /// or a single loop would split as soon as history changed.
    #[test]
    fn conversation_identity_is_stable_as_history_grows() {
        let one = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"origin"}]}]}"#;
        let two = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"origin"}]},{"role":"assistant","content":"a"},{"role":"tool","content":"b"}]}"#;
        assert_eq!(
            conversation_identity(one).1,
            conversation_identity(two).1,
            "the id must be taken from the earliest user message, not the last"
        );
    }

    /// Content carrying no text at all (an image-only message) must not stop
    /// the search: the next textful user message supplies the id.
    #[test]
    fn conversation_identity_skips_textless_content() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]},{"role":"user","content":"real text"},{"role":"tool","content":"r"}]}"#;
        let (_, fingerprint) = conversation_identity(body);
        assert!(
            fingerprint.is_some(),
            "a later textful user message must be used"
        );
    }

    /// A body carrying both `tools` and `max_tokens` reports both, so a
    /// tool-using turn with an explicit cap is fully described.
    #[test]
    fn request_shape_summary_reports_tools_and_cap() {
        let body =
            Bytes::from_static(br#"{"model":"m","tools":[{"type":"function"}],"max_tokens":4096}"#);
        assert_eq!(request_shape_summary(&body), (Some(true), Some(4096)));
    }

    /// Missing keys yield `(None, None)` — "shape unknown", not "no tools".
    #[test]
    fn request_shape_summary_missing_keys_are_none() {
        let body = Bytes::from_static(br#"{"model":"m","messages":[]}"#);
        assert_eq!(request_shape_summary(&body), (None, None));
    }

    /// An explicitly empty `tools` array is `Some(false)`: the client said
    /// no tools, which differs from a body that never mentioned them.
    #[test]
    fn request_shape_summary_empty_tools_is_some_false() {
        let body = Bytes::from_static(br#"{"model":"m","tools":[]}"#);
        let (has_tools, max_tokens) = request_shape_summary(&body);
        assert_eq!(has_tools, Some(false));
        assert_eq!(max_tokens, None);
    }

    /// Negative caps are preserved (the upstream decides their meaning) while
    /// values outside `i32` range degrade to `None` rather than wrapping.
    #[test]
    fn request_shape_summary_negative_and_overflow_caps() {
        let body = Bytes::from_static(br#"{"model":"m","max_tokens":-1}"#);
        assert_eq!(request_shape_summary(&body).1, Some(-1));
        let body = Bytes::from_static(br#"{"model":"m","max_tokens":9999999999}"#);
        assert_eq!(request_shape_summary(&body).1, None);
    }
}
