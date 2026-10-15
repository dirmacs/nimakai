use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use crate::proxy::chat_helpers::{apply_model_params, count_repetitions};
use bytes::Bytes;
use serde_json::Value;
use crate::AppState;
pub(crate) fn extract_response_metrics(text: &str) -> (u32, u32, bool) {
    let mut output_tokens = 0u32;
    let repetition_count = count_repetitions(text);
    let mut has_tool_call = false;
    if let Ok(json) = serde_json::from_str::<Value>(text) {
        if let Some(usage) = json.get("usage").and_then(|u| u.get("completion_tokens")) {
            if let Some(tokens) = usage.as_u64() {
                output_tokens = tokens as u32;
            }
        }
        if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if choice
                    .get("message")
                    .and_then(|m| m.get("tool_calls"))
                    .is_some()
                {
                    has_tool_call = true;
                }
            }
        }
    }
    if output_tokens == 0 {
        output_tokens = (text.len() as u32) / 4;
    }
    (output_tokens, repetition_count, has_tool_call)
}
/// Validate a model name for chat completion requests.
/// Returns Ok(()) for valid models (including "auto" and empty).
/// Returns Err with message for invalid models not in the available list.
pub fn validate_model_exists(model: &str, state: &AppState) -> Result<(), String> {
    let model = normalize_requested_model(model);
    // "auto" and empty are always valid - they'll be resolved via router
    if model.is_empty() || model == "auto" {
        return Ok(());
    }
    let configured_models = state.configured_models();
    if !configured_models.is_empty() {
        if configured_models.iter().any(|m| m == model) {
            return Ok(());
        }
        return Err(format!("model '{}' not found in configured models", model));
    }
    // No routing configured - accept any model (passthrough to NVIDIA)
    // This preserves backward compatibility: when no models are configured,
    // passthrough mode allows any model through
    Ok(())
}
pub(crate) fn normalize_requested_model(model: &str) -> &str {
    if model == "nimaproxy/auto" {
        "auto"
    } else {
        model
    }
}
/// Sanitize tool_calls and tools to remove entries with empty names.
/// NVIDIA NIM (via Azure OpenAI validation) rejects empty function names with:
/// "Must be a-z, A-Z, 0-9, or contain underscores and dashes, with a maximum length of 64"
pub(crate) fn sanitize_tool_calls(json: &mut Value) {
    // Sanitize tool_calls in messages
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            // Strip tool_call_id from assistant messages - most models don't accept it
            // Pydantic error: "Extra inputs are not permitted" for tool_call_id field
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                if role == "assistant" {
                    if let Some(obj) = msg.as_object_mut() {
                        obj.remove("tool_call_id");
                        obj.remove("reasoning"); // Strip reasoning field - not accepted by most models
                                                 // NVIDIA NIM requires: EITHER content OR tool_calls, not both
                                                 // When tool_calls is present, set content to null
                        if obj.get("tool_calls").is_some() {
                            obj.insert("content".to_string(), serde_json::Value::Null);
                        }
                    }
                }
            }
            if let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|tc| tc.as_array_mut()) {
                let original_len = tool_calls.len();
                // Filter out tool_calls with empty names
                tool_calls.retain(|tc| {
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        !name.is_empty()
                    } else {
                        // Keep if no name field (shouldn't happen but be safe)
                        true
                    }
                });
                // If all tool_calls were removed (and there were some originally), remove the tool_calls field entirely
                if original_len > 0 && tool_calls.is_empty() {
                    if let Some(obj) = msg.as_object_mut() {
                        obj.remove("tool_calls");
                    }
                }
            }
        }
    }
    // Sanitize tools array (tool definitions) — fix schema fields that break Jinja templates
    // NVIDIA models crash with 500 "tool_use:98" when tool.function.description is null/undefined
    // or when tool.function.parameters is missing/null (template does `description + " "` → boom)
    if let Some(tools) = json.get_mut("tools").and_then(|t| t.as_array_mut()) {
        // First pass: fix null/missing description and parameters before filtering
        // NVIDIA Jinja templates do string concat on description → null/undefined causes 500 "tool_use:98"
        for tool in tools.iter_mut() {
            if let Some(func) = tool.get_mut("function").and_then(|f| f.as_object_mut()) {
                match func.get("description") {
                    None | Some(Value::Null) => {
                        func.insert("description".to_string(), Value::String(String::new()));
                    }
                    _ => {}
                }
                match func.get("parameters") {
                    None | Some(Value::Null) => {
                        func.insert(
                            "parameters".to_string(),
                            serde_json::json!({"type": "object", "properties": {}}),
                        );
                    }
                    _ => {}
                }
            }
        }
        // Second pass: filter out tools with empty function names
        tools.retain(|tool| {
            if let Some(name) = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                !name.is_empty()
            } else {
                true
            }
        });
        // If all tools were removed, remove the tools field entirely
        if tools.is_empty() {
            if let Some(obj) = json.as_object_mut() {
                obj.remove("tools");
            }
        }
    }
}
pub(crate) fn normalize_assistant_messages(json: &mut Value) {
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let Some(obj) = msg.as_object_mut() else {
                continue;
            };
            obj.remove("tool_call_id");
            obj.remove("reasoning");
            let has_tool_calls = obj
                .get("tool_calls")
                .and_then(|tc| tc.as_array())
                .is_some_and(|tc| !tc.is_empty());
            if has_tool_calls {
                obj.insert("content".to_string(), Value::Null);
            } else {
                obj.remove("tool_calls");
                let missing_or_null = obj
                    .get("content")
                    .map(|content| content.is_null())
                    .unwrap_or(true);
                if missing_or_null {
                    obj.insert("content".to_string(), Value::String(String::new()));
                }
            }
        }
    }
}
/// Validate tool call IDs for Mistral models.
/// Mistral requires tool call IDs to be exactly 9 alphanumeric characters.
/// Also validates that the number of tool calls matches the number of tool responses
/// (only when tool messages are present in the request).
/// Validate tool call IDs for Mistral models.
/// Mistral requires tool call IDs to be exactly 9 alphanumeric characters.
/// Also validates that the number of tool calls matches the number of tool responses
/// (only when tool messages are present in the request).
pub(super) fn validate_mistral_tool_call_ids(
    json: &Value,
    model_id: &str,
) -> Result<(), (StatusCode, String)> {
    if !is_mistral_model(model_id) {
        return Ok(());
    }
    let mut tool_call_ids = std::collections::HashSet::new();
    let mut tool_response_ids = std::collections::HashSet::new();
    let mut has_tool_messages = false;
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                if role == "assistant" {
                    if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                        for tc in tool_calls {
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                if !id.chars().all(|c| c.is_alphanumeric()) {
                                    eprintln!("WARNING: Tool call id '{}' may be invalid for Mistral models.", id);
                                    continue;
                                }
                                tool_call_ids.insert(id.to_string());
                            }
                        }
                    }
                } else if role == "tool" {
                    has_tool_messages = true;
                    if let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) {
                        tool_response_ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    if has_tool_messages {
        for id in &tool_call_ids {
            if !tool_response_ids.contains(id) {
                eprintln!("WARNING: Tool call id '{}' has no matching response.", id);
                continue;
            }
        }
        for id in &tool_response_ids {
            if !tool_call_ids.contains(id) {
                eprintln!("WARNING: Tool response id '{}' has no matching call.", id);
                continue;
            }
        }
    }
    Ok(())
}
/// Fix message ordering for OpenAI API compatibility.
/// After tool messages, the API requires an assistant message before the next user message.
/// This function inserts empty assistant messages where needed.
pub(crate) fn fix_message_ordering(json: &mut Value) {
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let mut i = 0;
        while i < messages.len() {
            let current_role = messages[i]
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            if current_role == "tool" {
                // Check if next message exists and is "user" or "developer" (developer→user transform may not have run yet)
                if i + 1 < messages.len() {
                    let next_role = messages[i + 1]
                        .get("role")
                        .and_then(|r| r.as_str())
                        .unwrap_or("");
                    if next_role == "user" || next_role == "developer" {
                        // Insert an assistant message after the tool message
                        // Must have ONLY content (no tool_calls field)
                        // NVIDIA NIM rejects messages with both content AND tool_calls
                        let empty_assistant = serde_json::json!({
                            "role": "assistant",
                            "content": "",
                        });
                        messages.insert(i + 1, empty_assistant);
                        i += 2; // Skip the inserted message
                        continue;
                    }
                }
            }
            i += 1;
        }
    }
}
/// - "developer" → "user" (NVIDIA NIM doesn't support developer role)
/// - "tool" → "assistant" (NVIDIA NIM doesn't support tool role)
///
/// For tool messages, we also need to:
/// - Keep tool_call_id (required for matching tool results to calls)
/// - Keep content as the tool output
pub(crate) fn transform_message_roles(json: &mut Value, model_id: &str, state: &AppState) {
    let transform_developer = state.model_compat.should_transform_developer_role(model_id);
    let transform_tool = state.model_compat.should_transform_tool_messages(model_id);
    if !transform_developer && !transform_tool {
        return;
    }
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages {
            let role = msg
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            if transform_developer && role == "developer" {
                if let Some(v) = msg.get_mut("role") {
                    *v = Value::String("user".to_string());
                }
            } else if transform_tool && role == "tool" {
                // Transform tool role to assistant
                if let Some(v) = msg.get_mut("role") {
                    *v = Value::String("assistant".to_string());
                }
                // Tool messages have tool_call_id which assistant messages also support
                // when they're responding to a tool call, so we keep it
            }
        }
    }
}
/// Check if the conversation has tool messages or tool calls (indicating a tool call flow).
/// This requires special handling for Mistral models on NVIDIA NIM.
pub(crate) fn has_tool_messages(json: &Value) -> bool {
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        let has_tool_role = messages
            .iter()
            .any(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("tool"));
        let has_tool_calls = messages.iter().any(|msg| msg.get("tool_calls").is_some());
        let has_tool = has_tool_role || has_tool_calls;
        return has_tool;
    }
    false
}
/// Check if a model is a Mistral model (requires special tool calling handling).
pub(crate) fn is_mistral_model(model_id: &str) -> bool {
    model_id.contains("mistral") || model_id.contains("devstral")
}
/// Check if model is MiniMax (requires JSON tool calling format hint)
pub(crate) fn is_minimax_model(model_id: &str) -> bool {
    model_id.starts_with("minimaxai/")
}
/// Inject system message for MiniMax models to use JSON tool calling format
pub(crate) fn inject_minimax_system_message(json: &mut Value, model_id: &str) {
    if !is_minimax_model(model_id) {
        return;
    }
    let minmax_instruction = r#"When using tools, output JSON in this exact format:
{"tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "function_name", "arguments": {"arg": "value"}}}]}
Do NOT use XML tags like <minimax:tool_call> or <invoke>."#;
    if let Some(messages) = json.get_mut("messages").and_then(|m| m.as_array_mut()) {
        if let Some(first) = messages.get_mut(0) {
            if first.get("role").and_then(|r| r.as_str()) == Some("system") {
                if let Some(content) = first.get_mut("content") {
                    if let Some(s) = content.as_str() {
                        *content = Value::String(format!("{}\n\n{}", s, minmax_instruction));
                    }
                }
            } else {
                let system_msg =
                    serde_json::json!({"role": "system", "content": minmax_instruction});
                messages.insert(0, system_msg);
            }
        } else {
            let system_msg = serde_json::json!({"role": "system", "content": minmax_instruction});
            messages.insert(0, system_msg);
        }
    }
}
/// Check if the last message in the conversation is from the assistant.
pub(crate) fn is_last_message_from_assistant(json: &Value) -> bool {
    if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
        if let Some(last) = messages.last() {
            if let Some(role) = last.get("role").and_then(|r| r.as_str()) {
                return role == "assistant";
            }
        }
    }
    false
}
/// Inject parameters for tool calling and conversation continuation.
/// When the last message is from the assistant, we must set:
/// - add_generation_prompt=false (tells API we're continuing, not starting new)
/// - continue_final_message=true (tells API to continue from assistant's partial response)
/// This applies to ALL models on NVIDIA NIM, not just Mistral.
pub(crate) fn inject_mistral_tool_params(json: &mut Value, model_id: &str) {
    let is_mistral = is_mistral_model(model_id);
    let has_tools = has_tool_messages(json);
    let last_from_assistant = is_last_message_from_assistant(json);
    // Only inject Mistral-specific parameters for Mistral models
    // These params are rejected by NVIDIA for non-Mistral models
    if is_mistral {
        if has_tools {
            json["add_generation_prompt"] = Value::Bool(false);
        }
        if last_from_assistant {
            json["continue_final_message"] = Value::Bool(true);
        }
    }
}
/// Resolve the model field, optionally rewriting the body for "auto" routing.
/// Returns (model_id_string, possibly_rewritten_body).
pub fn resolve_model(body: Bytes, state: &AppState) -> (String, Bytes) {
    let mut json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return ("unknown".to_string(), body),
    };
    let requested = normalize_requested_model(json["model"].as_str().unwrap_or("")).to_string();
    if requested.is_empty() || requested == "auto" {
        if let Some(router) = &state.router {
            if let Some(picked) = router.pick(&state.model_stats) {
                json["model"] = Value::String(picked.clone());
            }
        } else if requested == "auto" {
            json["model"] = Value::String("auto".to_string());
        }
    }
    // Use the actual model ID from JSON after potential rewrite (for "auto" routing)
    let model_id = json["model"].as_str().unwrap_or("unknown").to_string();
    // Inject Mistral-specific parameters BEFORE message transformations
    // so has_tool_messages() can detect tool messages in the original JSON
    inject_mistral_tool_params(&mut json, &model_id);
    // Inject MiniMax system message for JSON tool calling
    inject_minimax_system_message(&mut json, &model_id);
    // Sanitize tool_calls to remove entries with empty names (Azure OpenAI rejects these)
    sanitize_tool_calls(&mut json);
    // Transform roles first (developer→user) so fix_message_ordering sees the
    // final role assignments when inserting assistant messages between tool→user gaps.
    transform_message_roles(&mut json, &model_id, state);
    fix_message_ordering(&mut json);
    normalize_assistant_messages(&mut json);
    if let Some(params) = state.model_params.get(&model_id) {
        apply_model_params(&mut json, params);
    }
    (model_id, Bytes::from(json.to_string()))
}
pub(crate) fn mistral_validation_error(model_id: &str, body: &Bytes) -> Option<Response> {
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        if let Err((status, msg)) = validate_mistral_tool_call_ids(&json, model_id) {
            return Some((status, msg).into_response());
        }
    }
    None
}
