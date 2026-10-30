//! The late hedge: a second upstream call on a different key, armed by a
//! timer, where the first byte wins.
//!
//! The rule (RFC dirmacs/nimakai#63): a timer can START a second call; a
//! timer must never CANCEL a call no other call has beaten. Concretely:
//!
//! 1. The first attempt is sent on its key with its own rail
//!    (`first_byte_timeout_for_model`). A hedge timer starts at that send
//!    (`t0`) and fires after the decided delay of silence.
//! 2. At the delay, the hedge takes a second slot ONCE, non-blocking, via
//!    [`try_acquire_gateway_permits_excluding`] with the first key excluded.
//!    On any error the first call runs alone — the hedge never queues and
//!    never fails the client for lack of a second slot. On a single-key pool
//!    the exclusion degrades to the same key, which is accepted and
//!    documented at [`KeyPool::next_key_with_permit_excluding`].
//! 3. The second call is spawned into a [`JoinSet`] (the racing.rs machinery)
//!    with its OWN rail, measured from its own send. A silent call ends at
//!    its rail while the other keeps waiting.
//! 4. The first call that produces a first byte claims the per-request
//!    winner slot with [`AtomicBool::compare_exchange`] BEFORE the loser's
//!    future is dropped, so the loser's permits release immediately and the
//!    client body is sent exactly once. The loser is cancelled at once,
//!    scores nothing in the rank, and is not an error round.
//! 5. If both calls end with no byte, the client gets ONE 504 when the second
//!    call ends. A hedged request is clamped to 2 attempts: while a hedge is
//!    armed or running, the sequential [`RetryPolicy`] retry is disabled for
//!    that request.
//!
//! Concurrency cap: a hedge may only arm while at least half of the global
//! upstream permits remain available for first calls, so hedged requests
//! cannot fill every slot during a stall. The hedge consults the per-key
//! AIMD window (`current_window`), not just the semaphore: that check lives
//! in `next_key_with_permit_excluding`, which refuses a key whose in-flight
//! count already meets its current window.
//!
//! Worst-case client wait for a hedged request is
//! `delay + headers_rail + chunk_rail`: the delay of silence, then the
//! second call's own first-byte rail (which bounds both its phase timers —
//! headers and first body chunk — from its send).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::proxy::admission::{
    try_acquire_gateway_permits_excluding, try_acquire_gateway_permits_excluding_for_model,
};
use crate::turn_log::{AttemptEnd, LoopKey};
use crate::AppState;
use axum::http::StatusCode;
use bytes::Bytes;
use std::time::Instant;

/// Margin added to the model's p95 first-byte wait when computing the learned
/// hedge delay (milliseconds). Named constant per the RFC owner decision.
pub const HEDGE_MARGIN_MS: u64 = 30_000;

/// Fixed hedge delay of silence (milliseconds) used while the model's
/// success-only ring holds fewer than `dynamic_sample_floor` samples.
pub const HEDGE_FIXED_DELAY_MS: u64 = 170_000;

/// Hard cap on the hedge delay relative to the rail: the delay can never
/// come within 1 s of the rail, so the second call always starts with at
/// least 1 s of its own rail left.
const RAIL_HEADROOM_MS: u64 = 1_000;

/// Compute the delay of silence before a hedge arms, in milliseconds.
///
/// `rail_ms` is the model's first-byte rail. The learned delay is
/// `min(p95_ms + HEDGE_MARGIN_MS, rail_ms - 1000)`, recomputed per request
/// over the model's success-only 100-sample ring when `ring_len >=`
/// `dynamic_sample_floor`; below that floor the delay is the fixed
/// [`HEDGE_FIXED_DELAY_MS`]. Rail-capped and stalled calls never enter the
/// ring (it is success-only), so the learned delay can never reach the rail;
/// the `rail_ms - 1000` cap is defence in depth for a rail smaller than the
/// margin.
pub fn hedge_delay_ms(state: &AppState, model_id: &str, rail_ms: u64) -> u64 {
    match state.model_stats.hedge_delay_sample(model_id) {
        Some((p95_ms, ring_len)) if ring_len >= state.dynamic_sample_floor => {
            let learned = (p95_ms as u64).saturating_add(HEDGE_MARGIN_MS);
            learned.min(rail_ms.saturating_sub(RAIL_HEADROOM_MS))
        }
        _ => HEDGE_FIXED_DELAY_MS.min(rail_ms.saturating_sub(RAIL_HEADROOM_MS)),
    }
}

/// Whether a hedge may arm right now: at least half of the global upstream
/// permits must remain available, so hedged requests cannot fill all slots
/// during a stall — at least half stay reserved for first calls.
pub fn hedge_slot_available(state: &AppState) -> bool {
    state.upstream_permits_available() * 2 >= state.max_upstream_in_flight
}

/// The outcome of one leg of a hedged race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegOutcome {
    /// The leg produced a first byte and claimed the winner slot.
    Won,
    /// The leg ended at its own rail with no byte; the other leg keeps
    /// waiting. Counts as a stall: an error round with the rail as model_ms,
    /// and a rank stall.
    RailExpired,
    /// The leg was cancelled because the other leg won. Scores nothing in
    /// the rank and is not an error round.
    Lost,
    /// The send or the stream failed outright (connect error, I/O error).
    Failed,
}

/// Shared per-request winner slot. The first leg to produce a first byte
/// claims it with `compare_exchange(false, true)`; exactly one leg can win,
/// which is what keeps the client body sent exactly once even when both
/// legs see a byte in the same poll window.
#[derive(Debug, Default)]
pub struct WinnerSlot {
    won: AtomicBool,
}

impl WinnerSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            won: AtomicBool::new(false),
        })
    }

    /// Claim the win. Returns true for exactly one caller.
    pub fn claim(&self) -> bool {
        self.won
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn is_won(&self) -> bool {
        self.won.load(Ordering::Acquire)
    }
}

/// Try to take the second slot for a hedge, once, non-blocking.
///
/// Returns `None` when no second slot is free — the caller then runs the
/// first call alone (no hedge, no queued second call, no client failure).
/// The exclusion of `first_idx` makes the second call land on a different
/// key; on a single-key pool it degrades to the same key, which is accepted
/// and documented at
/// [`crate::key_pool::KeyPool::next_key_with_permit_excluding`]. The per-key
/// AIMD window (`current_window`) is consulted inside the pool pick, not
/// just the semaphore.
#[allow(dead_code)]
pub(crate) fn try_take_hedge_slot(
    state: &AppState,
    first_idx: usize,
) -> Option<(crate::UpstreamPermit, crate::key_pool::KeyLease)> {
    if !hedge_slot_available(state) {
        return None;
    }
    try_acquire_gateway_permits_excluding(state, Some(first_idx)).ok()
}

/// The result of driving one leg to its first byte or its rail.
pub enum LegFirstByte {
    /// Headers and first body chunk arrived inside the leg's rail. Carries
    /// everything needed to forward the winning response: status, headers,
    /// the already-consumed first chunk, and the remaining byte stream.
    Byte {
        status: reqwest::StatusCode,
        headers: reqwest::header::HeaderMap,
        first: bytes::Bytes,
        rest: futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    },
    /// The leg ended at its rail with no byte.
    RailExpired,
    /// The send or stream failed.
    Failed(String),
}

/// Wait for one leg's first byte under its own rail.
///
/// `send` is the already-issued request future; `rail` bounds BOTH phase
/// timers of the leg — the wait for response headers (reqwest's `send()`
/// resolves on headers) and the wait for the first body chunk, which is
/// awaited under the SAME rail measured from the leg's send, so a leg's
/// total pre-first-byte wait never exceeds its rail.
pub(crate) async fn leg_first_byte(
    send: impl Future<Output = Result<reqwest::Response, reqwest::Error>>,
    rail: Duration,
) -> LegFirstByte {
    use futures::StreamExt;
    let resp = match tokio::time::timeout(rail, send).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return LegFirstByte::Failed(e.to_string()),
        Err(_) => return LegFirstByte::RailExpired,
    };
    // reqwest's `bytes_stream()` consumes the response, so capture what the
    // forwarded response needs (status, headers) first, then box the stream.
    let status = resp.status();
    let headers = resp.headers().clone();
    let stream = resp.bytes_stream();
    let mut stream = Box::pin(stream)
        as futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>;
    match tokio::time::timeout(rail, stream.next()).await {
        Ok(Some(Ok(chunk))) => LegFirstByte::Byte {
            status,
            headers,
            first: chunk,
            rest: stream,
        },
        Ok(Some(Err(e))) => LegFirstByte::Failed(e.to_string()),
        Ok(None) | Err(_) => LegFirstByte::RailExpired,
    }
}

/// The resolved outcome of a two-leg race: which leg won, and the loser's
/// outcome. `winner_leg` is `Some` exactly when a leg produced a first byte
/// and claimed the shared slot; `loser` is `Some` only when the other leg
/// was still open and got cancelled by the win (reported `Lost`).
/// `both_rail_expired` is set when both legs ended at their own rails with
/// no byte: the caller then returns ONE 504 and starts no third call.
pub struct HedgeRace {
    pub winner_leg: Option<usize>,
    pub loser: Option<(usize, LegOutcome)>,
    pub both_rail_expired: bool,
}

/// Drive a two-leg first-byte race. `leg1` is spawned immediately; `leg2`
/// is spawned after `arm_delay` (the delay of silence measured from leg 1's
/// send). Each leg runs under its own rail. The first leg to produce a byte
/// claims the shared [`WinnerSlot`] with `compare_exchange`; the loser's
/// future is dropped (aborted) at once, so its permits release immediately.
///
/// This is the unit-testable seam: the legs are caller-supplied futures, so
/// a test can stand in for the upstream (a silent leg = a future that sleeps
/// past the rail; a fast leg = a future that resolves immediately).
pub async fn drive_hedge_race<L1, L2>(leg1: L1, leg2: L2, arm_delay: Duration) -> HedgeRace
where
    L1: Future<Output = LegOutcome> + Send + 'static,
    L2: Future<Output = LegOutcome> + Send + 'static,
{
    use tokio::task::JoinSet;
    let winner = WinnerSlot::new();
    let mut legs: JoinSet<(usize, LegOutcome)> = JoinSet::new();

    // Leg 1: spawned at once.
    {
        let winner = winner.clone();
        legs.spawn(async move {
            let outcome = leg1.await;
            let final_outcome = match outcome {
                LegOutcome::Won => {
                    if winner.claim() {
                        LegOutcome::Won
                    } else {
                        LegOutcome::Lost
                    }
                }
                other => other,
            };
            (0usize, final_outcome)
        });
    }

    // Leg 2: armed by the delay-of-silence timer. The timer checks the
    // winner slot first, so a second call is never started after a byte.
    let winner2 = winner.clone();
    let leg2_handle = tokio::spawn(async move {
        if !arm_delay.is_zero() {
            tokio::time::sleep(arm_delay).await;
        }
        if winner2.is_won() {
            return (1usize, LegOutcome::Lost);
        }
        let outcome = leg2.await;
        let final_outcome = match outcome {
            LegOutcome::Won => {
                if winner2.claim() {
                    LegOutcome::Won
                } else {
                    LegOutcome::Lost
                }
            }
            other => other,
        };
        (1usize, final_outcome)
    });

    // Collect both legs. The first `Won` is the winner; any other `Won`
    // (a both-byte race) is demoted to `Lost`, so exactly one leg wins.
    let mut winner_leg = None;
    let mut loser = None;
    let mut rail_expired = 0usize;
    while let Some(res) = legs.join_next().await {
        if let Ok((leg, outcome)) = res {
            match outcome {
                LegOutcome::Won => {
                    if winner_leg.is_none() {
                        winner_leg = Some(leg);
                    } else {
                        loser = Some((leg, LegOutcome::Lost));
                    }
                }
                LegOutcome::Lost => loser = Some((leg, LegOutcome::Lost)),
                LegOutcome::RailExpired => rail_expired += 1,
                LegOutcome::Failed => {}
            }
        }
    }
    if let Ok((leg, outcome)) = leg2_handle.await {
        match outcome {
            LegOutcome::Won => {
                if winner_leg.is_none() {
                    winner_leg = Some(leg);
                } else {
                    loser = Some((leg, LegOutcome::Lost));
                }
            }
            LegOutcome::Lost => {
                if winner_leg.is_some() && loser.is_none() {
                    loser = Some((leg, LegOutcome::Lost));
                }
            }
            LegOutcome::RailExpired => rail_expired += 1,
            LegOutcome::Failed => {}
        }
    }
    HedgeRace {
        winner_leg,
        loser,
        both_rail_expired: rail_expired >= 2,
    }
}

/// The outcome of racing the first call against a possible hedge leg.
///
/// `FirstOnly` carries the first leg's raw send result so the caller runs
/// the sequential path unchanged (no second slot was free, or the first leg
/// ended before the hedge could matter). `Raced` carries the resolved
/// race: which leg won, and the loser's outcome for logging.
pub enum FirstByteRace {
    FirstOnly(Result<reqwest::Response, reqwest::Error>),
    Raced {
        /// 0 = the first call won, 1 = the hedge leg won.
        winner_leg: usize,
        winner: LegFirstByte,
        loser: Option<(usize, LegOutcome)>,
        both_rail_expired: bool,
        /// The winning leg rebuilt as a `reqwest::Response` when it won with a
        /// byte; `None` when no leg won.
        resolved: Option<Result<reqwest::Response, reqwest::Error>>,
    },
}

/// Race the first call's first byte against a hedge leg armed by the
/// delay-of-silence timer.
///
/// Returns [`FirstByteRace::FirstOnly`] when no second slot can be taken
/// (the first call then runs alone, exactly as the sequential path would),
/// otherwise [`FirstByteRace::Raced`] with the winner's first-byte payload.
///
/// The second slot is taken ONCE, non-blocking, at the delay, via
/// [`try_acquire_gateway_permits_excluding_for_model`] with `first_idx`
/// excluded (single-key pools degrade to the same key — accepted and
/// documented at [`crate::key_pool::KeyPool::next_key_with_permit_excluding`]).
/// The concurrent-hedge cap ([`hedge_slot_available`]) reserves at least
/// half of the global permits for first calls.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn race_first_byte_with_hedge(
    state: Arc<AppState>,
    first_send: impl Future<Output = Result<reqwest::Response, reqwest::Error>> + Send + 'static,
    first_idx: usize,
    upstream_url: &str,
    accept_header: &str,
    body: &Bytes,
    model_id: &str,
    hedge_delay: u64,
    rail: Duration,
) -> FirstByteRace {
    use tokio::sync::oneshot;
    let winner = WinnerSlot::new();
    let (leg2_tx, mut leg2_rx) = oneshot::channel::<(usize, LegOutcome, Option<LegFirstByte>)>();

    // Leg 1: the first call, spawned at once under its own rail.
    let winner1 = winner.clone();
    let leg1_handle = tokio::spawn(async move {
        let out = leg_first_byte(first_send, rail).await;
        let outcome = match &out {
            LegFirstByte::Byte { .. } => {
                if winner1.claim() {
                    LegOutcome::Won
                } else {
                    LegOutcome::Lost
                }
            }
            LegFirstByte::RailExpired => LegOutcome::RailExpired,
            LegFirstByte::Failed(_) => LegOutcome::Failed,
        };
        (0usize, outcome, Some(out))
    });

    // The hedge timer: after `hedge_delay` of silence from the first send,
    // try ONCE, non-blocking, to take a second slot. On any error the first
    // call runs alone.
    let state2 = state.clone();
    let url2 = upstream_url.to_string();
    let accept2 = accept_header.to_string();
    let body2 = body.clone();
    let model2 = model_id.to_string();
    let winner2 = winner.clone();
    let leg2_tx = std::sync::Mutex::new(Some(leg2_tx));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(hedge_delay)).await;
        if winner2.is_won() {
            // A byte already won; never start a second call after that.
            return;
        }
        let slot = if hedge_slot_available(&state2) {
            try_acquire_gateway_permits_excluding_for_model(&state2, Some(first_idx), &model2).ok()
        } else {
            None
        };
        let Some((upstream_permit, key_lease)) = slot else {
            // No free second slot: the first call runs alone. Report nothing.
            return;
        };
        let send2 = state2
            .client
            .post(url2.as_str())
            .header("Authorization", format!("Bearer {}", key_lease.key))
            .header("Content-Type", "application/json")
            .header("Accept", accept2.as_str())
            .body(body2.clone())
            .send();
        let key_idx = key_lease.idx;
        let _permit_guard = upstream_permit;
        let _lease_guard = key_lease;
        let out = leg_first_byte(send2, rail).await;
        let outcome = match &out {
            LegFirstByte::Byte { .. } => {
                if winner2.claim() {
                    LegOutcome::Won
                } else {
                    LegOutcome::Lost
                }
            }
            LegFirstByte::RailExpired => LegOutcome::RailExpired,
            LegFirstByte::Failed(_) => LegOutcome::Failed,
        };
        if let Some(tx) = leg2_tx.lock().unwrap().take() {
            let _ = tx.send((key_idx, outcome, Some(out)));
        }
    });

    // Drive the race: leg 1's join handle plus leg 2's oneshot.
    let leg1_res = leg1_handle.await;
    let (leg1_idx, leg1_outcome, leg1_byte) = match leg1_res {
        Ok(v) => v,
        Err(_) => (0usize, LegOutcome::Failed, None),
    };
    let leg2_res: Option<(usize, LegOutcome, Option<LegFirstByte>)> = leg2_rx.try_recv().ok();

    // Resolve the race. The winner is the leg that claimed the slot with a
    // byte; the loser (if any) is reported Lost. When no leg produced a byte,
    // return FirstOnly so the caller runs the sequential path unchanged.
    let leg2_outcome = leg2_res.as_ref().map(|(_, o, _)| *o);
    let (winner_leg, winner_byte, loser): (
        usize,
        Option<LegFirstByte>,
        Option<(usize, LegOutcome)>,
    ) = match (leg1_outcome, leg2_res) {
        (LegOutcome::Won, None) => (leg1_idx, leg1_byte, None),
        (LegOutcome::Won, Some((_, LegOutcome::Won, Some(_)))) => {
            // Both-byte race: leg 1 claimed first, leg 2 is the loser and
            // is NOT scored.
            (leg1_idx, leg1_byte, Some((1, LegOutcome::Lost)))
        }
        (LegOutcome::Won, Some((_, other, _))) => (leg1_idx, leg1_byte, Some((1, other))),
        (leg1o, Some((_, LegOutcome::Won, Some(byte)))) => (1, Some(byte), Some((leg1_idx, leg1o))),
        (leg1o, Some((_, other, _))) => (
            usize::MAX,
            leg1_byte,
            Some((1, other))
                .filter(|_| leg1o == LegOutcome::Lost)
                .or(None),
        ),
        (_leg1o, None) => (usize::MAX, leg1_byte, None),
    };
    let both_rail_expired = winner_leg == usize::MAX
        && matches!(winner_byte, Some(LegFirstByte::RailExpired) | None)
        && matches!(leg1_outcome, LegOutcome::RailExpired)
        && leg2_outcome == Some(LegOutcome::RailExpired);
    match winner_byte {
        Some(LegFirstByte::Byte {
            status,
            headers,
            first,
            rest,
        }) => {
            // Rebuild a reqwest::Response from the winning leg so the caller's
            // existing Ok(Ok(resp)) path is reused verbatim.
            let mut builder = axum::http::Response::builder().status(status.as_u16());
            for (k, v) in headers.iter() {
                builder = builder.header(k.as_str(), v.as_bytes());
            }
            use futures::StreamExt as _;
            let stream =
                futures::stream::once(async move { Ok::<_, reqwest::Error>(first) }).chain(rest);
            let body = reqwest::Body::wrap_stream(stream);
            let resp = match builder.body(body) {
                Ok(http_resp) => reqwest::Response::from(http_resp),
                Err(_) => return FirstByteRace::FirstOnly(Err(synthetic_timeout_error())),
            };
            FirstByteRace::Raced {
                winner_leg,
                winner: LegFirstByte::Byte {
                    status,
                    headers,
                    first: bytes::Bytes::new(),
                    rest: futures::stream::empty().boxed(),
                },
                loser,
                both_rail_expired,
                resolved: Some(Ok(resp)),
            }
        }
        _ => FirstByteRace::Raced {
            winner_leg,
            winner: winner_byte.unwrap_or(LegFirstByte::RailExpired),
            loser,
            both_rail_expired,
            resolved: None,
        },
    }
}

/// A synthetic reqwest error standing in for "the rail expired with no
/// byte" on the fallback path. The caller only inspects `Err(_)`, never the
/// message, so the exact error kind is not load-bearing. It is produced by
/// building a request with an invalid header value, which fails at
/// request-build time with a genuine `reqwest::Error` and no I/O.
pub(crate) fn synthetic_timeout_error() -> reqwest::Error {
    synthetic_error()
}

/// Build a real `reqwest::Error` without performing I/O (an invalid header
/// value fails at request-build time).
pub(crate) fn synthetic_error() -> reqwest::Error {
    reqwest::Client::new()
        .get("http://127.0.0.1/")
        .header("x-invalid", "\u{0}")
        .build()
        .expect_err("an invalid header value must fail at request-build time")
}

/// Log the cancelled loser of a hedge race as `other_call_won`, with
/// `won_by_request_id` pointing at the winner. The loser is NOT scored in the
/// rank and is NOT an error round.
pub fn log_loser(
    _state: &Arc<AppState>,
    original_model: &str,
    model_id: &str,
    loser: Option<(usize, LegOutcome)>,
    loop_key: Option<LoopKey>,
    t0: Instant,
    rail_ms: u64,
) {
    use crate::proxy::http::log_turn_request;
    use crate::turn_log::TurnRecord;
    let Some((_leg, LegOutcome::Lost)) = loser else {
        return;
    };
    log_turn_request(TurnRecord {
        requested_model: original_model.to_string(),
        responding_model: model_id.to_string(),
        latency_ms: t0.elapsed().as_millis() as u64,
        success: false,
        status_code: StatusCode::GATEWAY_TIMEOUT.as_u16(),
        request_message_count: 0,
        response_message_count: 1,
        has_tool_calls: false,
        tool_call_count: 0,
        request_has_tools: None,
        request_max_tokens: None,
        key_label: None,
        is_racing: false,
        loop_key,
        error: Some("cancelled: other call won the race".to_string()),
        request_tokens: None,
        response_tokens: None,
        response_tool_calls: None,
        response_body_id: None,
        attempt_end: Some(AttemptEnd::OtherCallWon),
        rail_ms: Some(rail_ms),
        won_by_request_id: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winner_slot_claims_exactly_once() {
        let slot = WinnerSlot::new();
        assert!(slot.claim());
        assert!(!slot.claim());
        assert!(slot.is_won());
    }

    #[test]
    fn fixed_delay_below_sample_floor() {
        let state = crate::test_utils::create_test_state();
        // No samples recorded: the fixed 170 s delay applies, capped by the
        // rail headroom when the rail is smaller.
        assert_eq!(
            hedge_delay_ms(&state, "mistralai/mistral-large-4-1t-a52b", 300_000),
            HEDGE_FIXED_DELAY_MS
        );
        assert_eq!(
            hedge_delay_ms(&state, "mistralai/mistral-large-4-1t-a52b", 100_000),
            99_000
        );
    }

    #[test]
    fn learned_delay_is_p95_plus_margin_capped_by_rail() {
        let state = crate::test_utils::MockAppStateBuilder::new()
            .with_keys(2)
            .build();
        let model = "m";
        // 30 success samples at 1000 ms each -> ring_len 30 >= floor (10),
        // p95 = 1000, learned = 1000 + 30000 = 31000.
        for _ in 0..30 {
            state.model_stats.record(model, 1000.0, true);
        }
        assert_eq!(hedge_delay_ms(&state, model, 300_000), 31_000);
        // Rail cap: rail 20000 -> delay capped at 19000.
        assert_eq!(hedge_delay_ms(&state, model, 20_000), 19_000);
    }
}
