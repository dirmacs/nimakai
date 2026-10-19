# Retrying through nimaproxy

## Symptom

The client reports: `provider stream ended with an error`, `ECONNRESET`, or
`stream closed before a finish_reason` while generating through nimaproxy.

## What it is

A mid-stream truncation: the generation started (the client already holds HTTP
200) and the SSE stream died before `[DONE]` / `finish_reason`. The proxy logs
the same event as `record_kind=completion, stream_ended_cleanly=false,
failure_mode=truncated_stream, error="stream ended without data: [DONE]"`.
Three names, one event. It is NOT a request-side rejection and NOT a first-byte
timeout (those surface as 504 with `upstream sent no bytes within Nms`).

## Correct retry

Re-issue the SAME prompt as a FRESH request (new stream). No payload change.

## Do NOT

Shrink the payload to "fix" it. Terminator loss is not payload-size dependent;
a smaller prompt costs a full regeneration and carries the same exposure.
(Payload reduction is the right fix for 413s and context overflows — not for
cut streams.)

## What the proxy does itself

The proxy applies the same rule on the paths where a retry cannot double-send:

* **Non-streaming (buffered) stall.** The body is assembled in memory and the
  client has seen nothing, so a mid-body stall is retried with the IDENTICAL
  payload on another key (`failed_key_idx` is excluded at admission), within
  the attempt budget. The request FSM models this as `Assembling -> Retryable`
  (`src/proxy/request_state.rs`); the retry loop lives in `chat_completions`
  (`src/proxy/chat.rs`).
* **Streaming stall.** The 200 is already committed, so the proxy does NOT
  retry — a second request would double-send a generation the client never
  asked for. The stall surfaces as the idle guard's typed error
  (`StreamStalled`, `src/proxy/streaming.rs`) on the committed stream, and the
  client retries same-prompt-as-fresh per this document.

Both directions are pinned by `tests/retry_guidance.rs`: the non-streaming
test asserts byte-identical bodies across attempts on different keys, and the
streaming test asserts exactly one upstream request per stalled stream.
Exhaustion falls through to the original 504.

## Context

Single-model funnel: one routed model means an upstream degradation has no
alternate leg. First-attempt stalls ARE retried by the proxy on another key
(budget is 2xwindow + admission wait + slack); post-200 stream cuts are
unretryable by construction and must be retried by the client.
