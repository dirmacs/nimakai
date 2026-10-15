use serde_json::Value;
pub(crate) fn is_hard_model_error(error: &str) -> bool {
    error.contains("HTTP 400")
        && (error.contains("Invalid assistant message")
            || error.contains("invalid assistant")
            || error.contains("tool_calls=None")
            || error.contains("BadRequestError"))
}
pub(crate) fn is_transient_race_error(error: &str) -> bool {
    if error.contains("timeout")
        || error.contains("request error")
        || error.contains("body error")
        || error.contains("HTTP 401")
        || error.contains("HTTP 403")
        || error.contains("HTTP 500")
        || error.contains("HTTP 502")
        || error.contains("HTTP 503")
        || error.contains("HTTP 504")
    {
        return true;
    }
    // In-stream errors (an HTTP 2xx whose body carries an embedded upstream error, e.g.
    // NVIDIA NIM's `"code":5033` overloaded-model SSE frame) use a provider-defined code
    // rather than a real HTTP status. Treat any embedded 5xx-family in-stream code as
    // transient so solo fallback can recover, mirroring the real-HTTP-5xx legs above.
    if error.contains("(in-stream)") {
        if let Some(code_str) = error
            .strip_prefix("HTTP ")
            .and_then(|rest| rest.split_whitespace().next())
        {
            return !code_str.is_empty()
                && code_str.starts_with('5')
                && code_str.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}
/// Detect an upstream error embedded in the body of an ostensibly-successful (2xx)
/// response. Some providers (e.g. NVIDIA NIM) return HTTP 200 and then emit an error
/// payload inside the body instead of a real error status: a `text/event-stream` `data:`
/// frame carrying `{"message": "...", "code": 5033, ...}` instead of content deltas, or
/// (for non-streaming legs) an `application/json` body shaped like `{"error": {...}}` /
/// `{"error": "..."}`, or a flat `{"code"|"status": <n>, "message"|"detail": "..."}` body.
/// `race_models` scores a leg by HTTP status alone, so without this check that leg "wins"
/// the race and forwards a fake success to the client.
///
/// Returns `Some((code, message))` when the body carries an embedded error — `code` is the
/// provider-embedded numeric code when present, else `500`. Returns `None` for a genuine
/// successful body, an unrecognized content type, or malformed/empty input. Tolerant of
/// `[DONE]`, blank lines, CRLF line endings, and partial/invalid UTF-8 — never panics.
pub(crate) fn sse_body_error(body: &[u8], content_type: Option<&str>) -> Option<(u16, String)> {
    let content_type = content_type.unwrap_or("").to_ascii_lowercase();
    if content_type.contains("event-stream") {
        let text = String::from_utf8_lossy(body);
        for line in text.split('\n') {
            let line = line.trim_end_matches('\r').trim();
            let payload = match line.strip_prefix("data:") {
                Some(p) => p.trim(),
                None => continue,
            };
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            if let Some(found) = json_embedded_error(&value) {
                return Some(found);
            }
        }
        None
    } else if content_type.contains("json") {
        let value = serde_json::from_slice::<Value>(body).ok()?;
        json_embedded_error(&value)
    } else {
        None
    }
}
/// Extract `(code, message)` from a JSON value shaped like an upstream error: either an
/// `{"error": {...}}` / `{"error": "..."}` wrapper, or a flat
/// `{"code"|"status": <n >= 400>, "message"|"detail": "..."}` body. Returns `None` for a
/// normal content/choices payload.
pub(crate) fn json_embedded_error(value: &Value) -> Option<(u16, String)> {
    let obj = value.as_object()?;
    match obj.get("error") {
        Some(Value::String(s)) if !s.is_empty() => return Some((500, s.clone())),
        Some(Value::Object(err_obj)) => {
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| err_obj.get("detail").and_then(Value::as_str))
                .unwrap_or("upstream error")
                .to_string();
            let code = err_obj
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .unwrap_or(500);
            return Some((code, message));
        }
        _ => {}
    }
    let code = obj
        .get("code")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("status").and_then(Value::as_u64))?;
    if code < 400 {
        return None;
    }
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| obj.get("detail").and_then(Value::as_str))?;
    Some((u16::try_from(code).unwrap_or(500), message.to_string()))
}
