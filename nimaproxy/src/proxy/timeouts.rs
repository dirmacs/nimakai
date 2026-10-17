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
