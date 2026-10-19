use std::time::{Duration, Instant};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tokio::time::sleep;

use crate::key_pool::{KeyAcquireError, KeyLease};
use crate::AppState;
use crate::UpstreamPermit;
pub(crate) enum GatewayAcquireError {
    Overloaded,
    NoKeys,
}
pub(crate) fn gateway_overloaded_response(state: &AppState) -> Response {
    state.gateway_metrics.record_overload();
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "gateway overloaded; retry later",
    )
        .into_response()
}
pub(crate) fn no_key_response(state: &AppState) -> Response {
    state.gateway_metrics.record_no_key();
    (StatusCode::TOO_MANY_REQUESTS, "all API keys rate-limited").into_response()
}
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn try_acquire_gateway_permits(
    state: &AppState,
) -> Result<(UpstreamPermit, KeyLease), GatewayAcquireError> {
    try_acquire_gateway_permits_excluding(state, None)
}
/// Acquire permits while skipping the key at `exclude`.
///
/// The retry loop passes the key that just stalled so the next attempt lands
/// on a *different* key; see `KeyPool::next_key_with_permit_excluding` for
/// why. `None` keeps the plain round-robin pick for first attempts and for
/// callers that have no failed key to avoid.
pub(crate) fn try_acquire_gateway_permits_excluding(
    state: &AppState,
    exclude: Option<usize>,
) -> Result<(UpstreamPermit, KeyLease), GatewayAcquireError> {
    // Acquire the KEY LEASE FIRST, then the global permit.
    //
    // `try_acquire_upstream` increments `upstream_attempts` on success
    // (lib.rs:413), and that increment is never undone when the permit is
    // dropped. Acquiring the global permit first meant every failed key
    // acquisition burned a phantom "attempt": the caller loops every 25 ms
    // for up to `admission_wait_ms`, so a request queued behind saturated
    // keys accumulated ~admission_wait_ms/25 attempts (200 at the 5000 ms
    // setting) that represented no upstream call at all. That inflated
    // `upstream_attempts` by ~74x over real upstream calls on this host and
    // made the ratio useless for capacity reasoning.
    //
    // Ordering the key lease first means a failed acquisition costs nothing:
    // we only count an attempt once we actually hold a key and a permit.
    match state.pool.next_key_with_permit_excluding(exclude) {
        Ok(key_lease) => {
            let upstream_permit = state
                .try_acquire_upstream()
                .ok_or(GatewayAcquireError::Overloaded)?;
            Ok((upstream_permit, key_lease))
        }
        Err(KeyAcquireError::AllBusy) => Err(GatewayAcquireError::Overloaded),
        Err(KeyAcquireError::NoKeys | KeyAcquireError::AllCoolingDown) => {
            Err(GatewayAcquireError::NoKeys)
        }
    }
}
/// The `Response` error variant is large, but boxing it would ripple across
/// 4+ call sites on the hot retry path (chat.rs, fallback.rs, http.rs) with no
/// correctness benefit. Every caller pattern-matches on the `Response`
/// directly, so keeping the variant unboxed preserves the call-site shape and
/// avoids churn on a hot path.
///
/// Boxing is not merely undesirable here, it does not work: axum 0.8 (via
/// axum-core 0.5) implements `IntoResponse` for `Box<str>` and `Box<[u8]>`
/// but NOT for `Box<Response>`, so a boxed variant would fail to compile at
/// each call site that returns it. The call signature below is already the
/// axum handler shape, which is why the `Err` arm returns a `Response` that
/// the surrounding handlers forward verbatim.
#[allow(clippy::result_large_err)]
pub(crate) async fn acquire_gateway_permits(
    state: &AppState,
) -> Result<(UpstreamPermit, KeyLease), Response> {
    acquire_gateway_permits_excluding(state, None).await
}
/// Acquire permits while skipping the key at `exclude`.
///
/// The retry loop passes the key that just stalled so the next attempt lands
/// on a *different* key. `None` keeps the plain pick for first attempts.
#[allow(clippy::result_large_err)]
pub(crate) async fn acquire_gateway_permits_excluding(
    state: &AppState,
    exclude: Option<usize>,
) -> Result<(UpstreamPermit, KeyLease), Response> {
    let start = Instant::now();
    let wait = Duration::from_millis(state.admission_wait_ms);
    let sleep_step = Duration::from_millis(25);
    loop {
        match try_acquire_gateway_permits_excluding(state, exclude) {
            Ok(permits) => return Ok(permits),
            Err(GatewayAcquireError::NoKeys) if state.pool.is_empty() => {
                return Err(no_key_response(state));
            }
            Err(err) => {
                if start.elapsed() >= wait {
                    return Err(match err {
                        GatewayAcquireError::NoKeys => no_key_response(state),
                        GatewayAcquireError::Overloaded => gateway_overloaded_response(state),
                    });
                }
                sleep(sleep_step).await;
            }
        }
    }
}
