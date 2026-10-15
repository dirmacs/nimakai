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
pub(crate) fn try_acquire_gateway_permits(
    state: &AppState,
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
    match state.pool.next_key_with_permit() {
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
pub(crate) async fn acquire_gateway_permits(state: &AppState) -> Result<(UpstreamPermit, KeyLease), Response> {
    let start = Instant::now();
    let wait = Duration::from_millis(state.admission_wait_ms);
    let sleep_step = Duration::from_millis(25);
    loop {
        match try_acquire_gateway_permits(state) {
            Ok(permits) => return Ok(permits),
            Err(GatewayAcquireError::NoKeys) if state.pool.len() == 0 => {
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
