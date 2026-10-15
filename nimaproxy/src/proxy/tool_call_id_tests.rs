use crate::proxy::messages::{sanitize_tool_calls, fix_message_ordering, validate_mistral_tool_call_ids};
use serde_json::json;

#[test]
fn test_sanitize_strips_tool_call_id_from_assistant() {
    // Assistant message with tool_call_id should have it stripped
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "content": "Let me call a tool",
                "tool_call_id": "call_123"
            }
        ]
    });

    sanitize_tool_calls(&mut json);

    // tool_call_id should be removed
    assert!(!json["messages"][0]
        .as_object()
        .unwrap()
        .contains_key("tool_call_id"));
}

#[test]
fn test_sanitize_keeps_tool_call_id_in_tool_messages() {
    // Tool messages can keep tool_call_id (it's valid in tool responses)
    let mut json = json!({
        "messages": [
            {
                "role": "tool",
                "content": "Tool result",
                "tool_call_id": "call_123"
            }
        ]
    });

    sanitize_tool_calls(&mut json);

    // tool_call_id should remain in tool messages
    assert!(json["messages"][0]
        .as_object()
        .unwrap()
        .contains_key("tool_call_id"));
}

#[test]
fn test_sanitize_removes_tool_call_id_even_with_tool_calls() {
    // Assistant message with both tool_call_id and tool_calls
    let mut json = json!({
        "messages": [
            {
                "role": "assistant",
                "content": null,
                "tool_call_id": "call_123",
                "tool_calls": [
                    {
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{}"
                        }
                    }
                ]
            }
        ]
    });

    sanitize_tool_calls(&mut json);

    // tool_call_id should be removed, tool_calls should remain
    assert!(!json["messages"][0]
        .as_object()
        .unwrap()
        .contains_key("tool_call_id"));
    assert!(json["messages"][0]
        .as_object()
        .unwrap()
        .contains_key("tool_calls"));
}

// ============ fix_message_ordering tests ============

#[test]
fn test_fix_message_ordering_no_change_needed() {
    // No tool messages, should be unchanged
    let mut json = json!({
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hello"}
        ]
    });
    crate::proxy::messages::fix_message_ordering(&mut json);
    assert_eq!(json["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn test_fix_message_ordering_inserts_assistant() {
    // tool followed by user - should insert empty assistant
    let mut json = json!({
        "messages": [
            {"role": "tool", "tool_call_id": "1", "content": "result"},
            {"role": "user", "content": "next"}
        ]
    });
    crate::proxy::messages::fix_message_ordering(&mut json);
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], serde_json::Value::String(String::new()));
    assert_eq!(msgs[2]["role"], "user");
}

#[test]
fn test_fix_message_ordering_developer_role() {
    // tool followed by developer (<turn-aborted>) then user - should insert empty assistant
    // This is the exact OMP pattern: tool[N]->developer[N+1]->user[N+2]
    let mut json = json!({
        "messages": [
            {"role": "assistant", "content": null, "tool_calls": [{"id": "abc123XYZ", "type": "function", "function": {"name": "read"}}]},
            {"role": "tool", "tool_call_id": "abc123XYZ", "content": "result"},
            {"role": "developer", "content": "<turn-aborted>\nThe previous turn was aborted."},
            {"role": "user", "content": "continue"}
        ]
    });
    crate::proxy::messages::fix_message_ordering(&mut json);
    let msgs = json["messages"].as_array().unwrap();
    // Should insert assistant between tool[1] and developer[2]
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[0]["role"], "assistant");
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[2]["role"], "assistant"); // inserted
    assert_eq!(msgs[2]["content"], serde_json::Value::String(String::new()));
    assert_eq!(msgs[3]["role"], "developer");
    assert_eq!(msgs[4]["role"], "user");
}

#[test]
fn test_fix_message_ordering_multiple_tools_before_user() {
    // Multiple tool messages followed by user
    let mut json = json!({
        "messages": [
            {"role": "tool", "tool_call_id": "1", "content": "r1"},
            {"role": "tool", "tool_call_id": "2", "content": "r2"},
            {"role": "user", "content": "next"}
        ]
    });
    crate::proxy::messages::fix_message_ordering(&mut json);
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[3]["role"], "user");
}

#[test]
fn test_fix_message_ordering_assistant_exists_no_change() {
    // tool followed by assistant followed by user - no change needed
    let mut json = json!({
        "messages": [
            {"role": "tool", "tool_call_id": "1", "content": "result"},
            {"role": "assistant", "content": "summary"},
            {"role": "user", "content": "next"}
        ]
    });
    crate::proxy::messages::fix_message_ordering(&mut json);
    assert_eq!(json["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn test_validate_mistral_tool_call_ids_valid() {
    let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "abc123XYZ", "type": "function", "function": {"name": "test"}}]}]});
    let result = crate::proxy::messages::validate_mistral_tool_call_ids(
        &json,
        "mistralai/mistral-medium-3.5-128b",
    );
    assert!(result.is_ok());
}

#[test]
fn test_validate_mistral_tool_call_ids_invalid_length() {
    let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "test"}}]}]});
    let result =
        crate::proxy::messages::validate_mistral_tool_call_ids(&json, "mistralai/mistral-7b-instruct");
    assert!(result.is_ok()); // validate is warn-only, not hard reject;
}

#[test]
fn test_validate_mistral_tool_call_ids_invalid_chars() {
    let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "call_123", "type": "function", "function": {"name": "test"}}]}]});
    let result =
        crate::proxy::messages::validate_mistral_tool_call_ids(&json, "mistralai/mistral-7b-instruct");
    assert!(result.is_ok()); // validate is warn-only, not hard reject;
}

#[test]
fn test_validate_mistral_tool_call_ids_non_mistral() {
    let json = serde_json::json!({"messages": [{"role": "assistant", "tool_calls": [{"id": "call_123", "type": "function", "function": {"name": "test"}}]}]});
    let result = crate::proxy::messages::validate_mistral_tool_call_ids(&json, "openai/gpt-4");
    assert!(result.is_ok());
}
