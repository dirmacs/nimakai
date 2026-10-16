//! HTTP proxy layer: request lifecycle, key admission, model racing, fallback.
//!
//! Split by concern from the former 4312-line `proxy.rs`:
//!
//! | module | concern |
//! |---|---|
//! | [`admission`] | gateway permits and key leases; the gate every attempt passes |
//! | [`chat`] | `/v1/chat/completions` and per-attempt request handling |
//! | [`racing`] | fan-out racing: candidate selection, deadlines, join/cancel |
//! | [`fallback`] | sequential single-model fallback over keys |
//! | [`messages`] | request body transforms and model resolution |
//! | [`upstream_errors`] | classification of upstream failures |
//! | [`timeouts`] | per-model timeout policy and turn summaries |
//! | [`http`] | the remaining axum handlers and turn recording |
//!
//! The request-level FSM these implement is specified in `docs/FORMAL-DESIGN.md`.

pub mod admission;
pub mod chat;
pub mod chat_helpers;
pub mod fallback;
pub mod http;
pub mod messages;
pub mod racing;
pub mod request_state;
pub mod retry;
pub mod sse_tap;
pub mod streaming;
pub mod timeouts;
pub mod upstream_errors;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tool_call_id_tests;
