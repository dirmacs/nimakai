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
    if let Some(max_tokens) = params.max_tokens {
        json["max_tokens"] = Value::from(max_tokens);
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
