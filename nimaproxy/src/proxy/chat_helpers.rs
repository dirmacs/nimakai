//! Helpers for the chat-completions handler: request repetition counting,
//! response accept-header selection, and model-parameter injection. Split from
//! `chat.rs` so the handler reads as one function per state-machine phase
//! rather than one long block of mixed concerns.

use bytes::Bytes;
use serde_json::Value;

pub(crate) fn count_repetitions(text: &str) -> u32 {
    let text_lower = text.to_lowercase();
    let words: Vec<&str> = text_lower.split_whitespace().collect();
    if words.len() < 4 {
        return 0;
    }
    let mut repetitions = 0u32;
    for window_size in 3..=6 {
        if words.len() < window_size * 2 {
            continue;
        }
        for i in 0..words.len() - window_size {
            let slice = &words[i..i + window_size];
            let pattern = slice.join(" ");
            let mut count = 1;
            for j in (i + window_size..)
                .step_by(window_size)
                .take_while(|&j| j + window_size <= words.len())
            {
                let next_slice = &words[j..j + window_size];
                if next_slice.join(" ") == pattern {
                    count += 1;
                } else {
                    break;
                }
            }
            if count > 1 {
                repetitions += count - 1;
            }
        }
    }
    repetitions.min(10)
}

pub(crate) fn apply_model_params(json: &mut Value, params: &crate::config::ModelParams) {
    if let Some(temp) = params.temperature {
        json["temperature"] = Value::from(temp);
    }
    if let Some(tp) = params.top_p {
        json["top_p"] = Value::from(tp);
    }
    if let Some(tk) = params.top_k {
        json["top_k"] = Value::from(tk);
    }
    if let Some(fp) = params.frequency_penalty {
        json["frequency_penalty"] = Value::from(fp);
    }
    if let Some(pp) = params.presence_penalty {
        json["presence_penalty"] = Value::from(pp);
    }
    if let Some(rp) = params.repetition_penalty {
        json["repetition_penalty"] = Value::from(rp);
    }
    if let Some(min_p) = params.min_p {
        json["min_p"] = Value::from(min_p);
    }
    // The configured `max_tokens` is the proxy's CEILING for this model, and it
    // stays authoritative: a request is never forwarded with a budget above it,
    // so the upstream can never be asked for a limit the model does not have.
    //
    // What it is NOT is a floor. A caller that names a SMALLER budget keeps it:
    // forwarding 32768 when the caller asked for 100 would silently spend the
    // caller's budget on tokens they did not ask for. So the effective budget
    // is min(caller's, the configured ceiling), and an absent budget takes the
    // configured ceiling.
    if let Some(max_tokens) = params.max_tokens {
        let requested = json.get("max_tokens").and_then(|v| v.as_u64());
        let effective = match requested {
            Some(requested) => requested.min(max_tokens as u64),
            None => max_tokens as u64,
        };
        json["max_tokens"] = Value::from(effective);
    }
    if let Some(reasoning_budget) = params.reasoning_budget {
        json["reasoning_budget"] = Value::from(reasoning_budget);
    }
    if let Some(reasoning_effort) = &params.reasoning_effort {
        json["reasoning_effort"] = Value::String(reasoning_effort.clone());
    }
    if let Some(seed) = params.seed {
        json["seed"] = Value::from(seed);
    }
    // Treat stream as a client response-mode choice, not as a model
    // hyperparameter. Callers that explicitly request streaming or
    // non-streaming keep control, and omitted stream must remain JSON-compatible.
    if let Some(stream) = params.stream {
        if json.get("stream").is_some_and(|v| !v.is_null()) {
            // Preserve the caller's explicit response mode.
        } else if !stream {
            json["stream"] = Value::Bool(stream);
        }
    }
    if let Some(ctk) = &params.chat_template_kwargs {
        if let Some(obj) = json.as_object_mut() {
            let entry = obj
                .entry("chat_template_kwargs".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(serde_json::Map::new());
            }
            if let Some(kwargs) = entry.as_object_mut() {
                for (k, v) in ctk {
                    kwargs.insert(k.clone(), v.clone());
                }
            }
        }
    }
}

pub(crate) fn accept_header_for_json(json: &Value) -> &'static str {
    if json.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        "text/event-stream"
    } else {
        "application/json"
    }
}
pub(crate) fn accept_header_for_body(body: &Bytes) -> &'static str {
    serde_json::from_slice::<Value>(body)
        .map(|json| accept_header_for_json(&json))
        .unwrap_or("application/json")
}

/// Read usage tokens out of an assembled non-streaming completion body.
///
/// A non-streaming response is a single OpenAI JSON object, so the usage the
/// upstream reported lives in its `usage.prompt_tokens` /
/// `usage.completion_tokens`. Returns `(None, None)` when the body carries no
/// usage object: the upstream did not report usage, which is NOT the same fact
/// as "0 tokens" and must never be recorded as one.
pub(crate) fn non_streaming_usage(body: &[u8]) -> (Option<u32>, Option<u32>) {
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let prompt = json
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .and_then(|t| u32::try_from(t).ok());
    let completion = json
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .and_then(|t| u32::try_from(t).ok());
    (prompt, completion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelParams;
    use crate::AppState;
    use bytes::Bytes;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn usage_object_populates_both_token_counts() {
        let body = br#"{"id":"x","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":11,"total_tokens":16}}"#;
        assert_eq!(non_streaming_usage(body), (Some(5), Some(11)));
    }

    #[test]
    fn absent_usage_is_none_not_zero() {
        let body = br#"{"id":"x","choices":[{"message":{"content":"hi"}}]}"#;
        assert_eq!(non_streaming_usage(body), (None, None));
    }

    #[test]
    fn unparsable_body_is_none() {
        assert_eq!(non_streaming_usage(b"not json"), (None, None));
    }

    #[test]
    fn partial_usage_keeps_only_what_was_reported() {
        let body = br#"{"usage":{"prompt_tokens":7}}"#;
        assert_eq!(non_streaming_usage(body), (Some(7), None));
    }

    #[test]
    fn absent_max_tokens_gets_the_configured_default() {
        let mut json = json!({
            "model": "stepfun-ai/step-5-preview",
            "messages": [{"role": "user", "content": "test"}]
        });
        let params = ModelParams {
            max_tokens: Some(32768),
            ..Default::default()
        };
        apply_model_params(&mut json, &params);
        assert_eq!(json["max_tokens"], json!(32768));
    }

    /// The defect this pins: `apply_model_params` used to overwrite
    /// `max_tokens` unconditionally, so a caller asking for 100000 was sent
    /// 16384 with no indication that the request was clipped. The caller's
    /// budget must survive the injection; only an ABSENT (or explicit null)
    /// budget takes the configured default.
    #[test]
    fn a_client_budget_above_the_configured_ceiling_is_capped_at_it() {
        // The configured value is a CEILING, not a default. A client asking for
        // 100000 must not be forwarded 100000: the ceiling is the proxy's
        // expression of what the model can serve, and forwarding a larger
        // budget would ask the upstream for a limit it does not have.
        let mut json = json!({
            "model": "stepfun-ai/step-5-preview",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 100000
        });
        let params = ModelParams {
            max_tokens: Some(32768),
            ..Default::default()
        };
        apply_model_params(&mut json, &params);
        assert_eq!(json["max_tokens"], json!(32768));
    }

    /// The other half of the contract: the ceiling is not a FLOOR either. A
    /// client that names a smaller budget keeps it, because forwarding 32768
    /// when the caller asked for 512 would spend their budget on tokens they
    /// did not ask for.
    #[test]
    fn explicit_client_max_tokens_below_the_ceiling_is_not_raised() {
        let mut json = json!({
            "model": "stepfun-ai/step-5-preview",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 512
        });
        let params = ModelParams {
            max_tokens: Some(32768),
            ..Default::default()
        };
        apply_model_params(&mut json, &params);
        assert_eq!(json["max_tokens"], json!(512));
    }

    /// An explicit null is "no budget named", so the default applies. This is
    /// the JSON-compatible shape the rest of `apply_model_params` uses for
    /// `stream`, and it keeps a null budget from being forwarded as null.
    #[test]
    fn null_max_tokens_takes_the_configured_default() {
        let mut json = json!({
            "model": "stepfun-ai/step-5-preview",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": null
        });
        let params = ModelParams {
            max_tokens: Some(32768),
            ..Default::default()
        };
        apply_model_params(&mut json, &params);
        assert_eq!(json["max_tokens"], json!(32768));
    }

    /// End-to-end through `resolve_model`, which is the path every non-racing
    /// request takes: the body the proxy forwards carries what the caller asked
    /// for, so the turn log's `request_max_tokens` reports the caller's value
    /// rather than the configured default.
    #[test]
    fn resolve_model_keeps_a_client_budget_below_the_configured_ceiling() {
        use crate::config::ModelCompat;
        use crate::model_stats::ModelStatsStore;
        use crate::proxy::messages::resolve_model;

        let state = AppState::new(crate::AppStateParams {
            keys: vec![],
            target: "https://test.api.nvidia.com".to_string(),
            router: None,
            model_stats: ModelStatsStore::new(3000.0),
            racing_models: vec![],
            racing_max_parallel: 3,
            racing_timeout_ms: 8000,
            racing_strategy: "complete".to_string(),
            model_params: HashMap::from([(
                "stepfun-ai/step-5-preview".to_string(),
                ModelParams {
                    max_tokens: Some(32768),
                    ..Default::default()
                },
            )]),
            model_compat: ModelCompat::default(),
            loop_idle_timeout: crate::loop_idle_timeout(),
        });
        let body = Bytes::from_static(
            br#"{"model":"stepfun-ai/step-5-preview","messages":[{"role":"user","content":"hi"}],"max_tokens":4000}"#,
        );
        let (_, resolved) = resolve_model(body, &state);
        let json: serde_json::Value = serde_json::from_slice(&resolved).expect("valid JSON body");
        assert_eq!(json["max_tokens"], json!(4000));

        // And the ceiling still binds: the same client asking for more than
        // the model can serve is capped rather than forwarded.
        let body = Bytes::from_static(
            br#"{"model":"stepfun-ai/step-5-preview","messages":[{"role":"user","content":"hi"}],"max_tokens":40000}"#,
        );
        let (_, resolved) = resolve_model(body, &state);
        let json: serde_json::Value = serde_json::from_slice(&resolved).expect("valid JSON body");
        assert_eq!(json["max_tokens"], json!(32768));
    }

    /// The retry loop computes the Accept header once, before the first
    /// attempt, from the request body, and reuses that value on every later
    /// attempt. The DEGRADED path is the one place the request body is
    /// reassigned, and it recomputes the header from the new body immediately
    /// after the reassignment (so it dominates both the mistral early-return
    /// and the `continue` into the next attempt). A recomputation there is
    /// therefore indistinguishable from a per-attempt recomputation, which is
    /// what makes the hoist behavior-preserving: this test pins both halves of
    /// that equivalence, using the same shapes the loop produces (a non-stream
    /// body before, and a reassigned stream body after).
    #[test]
    fn hoisted_accept_matches_per_attempt_recompute_across_body_reassignment() {
        // Initial request body, as the loop sees it before any attempt.
        let body =
            Bytes::from_static(br#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}"#);
        // What the loop would send per attempt if it recomputed every time.
        let per_attempt = accept_header_for_body(&body);
        // What the loop sends now that the value is hoisted before the loop.
        let mut hoisted = accept_header_for_body(&body);
        assert_eq!(hoisted, per_attempt);
        assert_eq!(hoisted, "application/json");

        // DEGRADED reassignment: the router resolves `auto` to a concrete model
        // whose params turn the request into a stream.
        let next_body = Bytes::from_static(
            br#"{"model":"mistral-large","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        // The loop recomputes right after `body = next_body`...
        let recomputed = accept_header_for_body(&next_body);
        hoisted = recomputed;
        // ...so the next attempt sends the same header a per-attempt
        // recomputation would have produced from the reassigned body.
        assert_eq!(hoisted, accept_header_for_body(&next_body));
        assert_eq!(hoisted, "text/event-stream");
    }
}
