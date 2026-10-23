use super::*;
use std::time::Duration;

fn make_key_entry(key: &str, label: &str) -> KeyEntry {
    KeyEntry {
        key: key.to_string(),
        label: Some(label.to_string()),
        disabled: false,
    }
}

/// Build a pool that runs the pressure-adaptive recovery rule. Only the
/// adaptive tests use this; every other test in this file keeps the default
/// (fixed) rule, which is what keeps the fixed-path pins meaningful.
fn adaptive_pool(n: usize, max_in_flight_per_key: usize) -> KeyPool {
    let keys = (0..n)
        .map(|i| make_key_entry(&format!("key{}", i), &format!("k{}", i)))
        .collect();
    KeyPool::with_max_in_flight_and_aimd(
        keys,
        max_in_flight_per_key,
        super::aimd::AimdMode::PressureAdaptive,
    )
}

#[test]
fn test_next_key_round_robin() {
    let keys = vec![
        make_key_entry("key1", "k-a"),
        make_key_entry("key2", "k-beta"),
        make_key_entry("key3", "test"),
    ];
    let pool = KeyPool::new(keys);

    // First key should be key1
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "key1");
    assert_eq!(idx, 0);

    // Second key should be key2
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "key2");
    assert_eq!(idx, 1);

    // Third key should be key3
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "key3");
    assert_eq!(idx, 2);

    // Wrap back to key1
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "key1");
    assert_eq!(idx, 0);
}

#[test]
fn test_single_key() {
    let keys = vec![make_key_entry("only-key", "single")];
    let pool = KeyPool::new(keys);

    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "only-key");
    assert_eq!(idx, 0);

    // Should keep returning the same key
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "only-key");
    assert_eq!(idx, 0);
}

#[test]
fn test_empty_pool() {
    let pool = KeyPool::new(vec![]);
    assert_eq!(pool.next_key(), None);
    assert_eq!(pool.len(), 0);
}

#[test]
fn test_mark_rate_limited() {
    let keys = vec![make_key_entry("key1", "a"), make_key_entry("key2", "b")];
    let pool = KeyPool::new(keys);

    // Mark key1 as rate-limited for 1 second
    pool.mark_rate_limited(0, 1);

    // key1 should now be skipped
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "key2");
    assert_eq!(idx, 1);
}

#[test]
fn test_mark_auth_failed_sets_cooldown_and_counts() {
    let keys = vec![make_key_entry("key1", "a"), make_key_entry("key2", "b")];
    let pool = KeyPool::new(keys);

    pool.mark_auth_failed(0, Duration::from_secs(900));

    // key1 should now be skipped (in cooldown), same mechanism as mark_rate_limited
    let (k, idx) = pool.next_key().unwrap();
    assert_eq!(k, "key2");
    assert_eq!(idx, 1);

    let statuses = pool.status();
    assert!(!statuses[0].active);
    assert!(statuses[0].cooldown_secs_remaining > 0);
    assert!(statuses[0].cooldown_secs_remaining <= 900);
    assert_eq!(statuses[0].auth_failures, 1);
    assert_eq!(statuses[1].auth_failures, 0);
}

#[test]
fn test_mark_auth_failed_zero_cooldown_disables_quarantine() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::new(keys);

    pool.mark_auth_failed(0, Duration::from_secs(0));

    // Cooldown must NOT be set when the configured cooldown is zero — the key stays
    // active and immediately reusable — but the failure is still counted.
    let statuses = pool.status();
    assert!(statuses[0].active);
    assert_eq!(statuses[0].cooldown_secs_remaining, 0);
    assert_eq!(statuses[0].auth_failures, 1);
    assert!(pool.next_key().is_some());
}

#[test]
fn test_mark_auth_failed_is_cumulative() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::new(keys);

    pool.mark_auth_failed(0, Duration::from_secs(0));
    pool.mark_auth_failed(0, Duration::from_secs(0));
    pool.mark_auth_failed(0, Duration::from_secs(0));

    assert_eq!(pool.status()[0].auth_failures, 3);
}

#[test]
fn test_status_default_auth_failures_zero() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::new(keys);

    assert_eq!(pool.status()[0].auth_failures, 0);
}

#[test]
fn test_next_key_with_permit_enforces_per_key_limit() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 1);

    let lease = pool.next_key_with_permit().unwrap();
    assert_eq!(lease.idx, 0);
    assert_eq!(pool.available_permits(), 0);
    assert!(matches!(
        pool.next_key_with_permit(),
        Err(KeyAcquireError::AllBusy)
    ));

    drop(lease);
    assert_eq!(pool.available_permits(), 1);
    assert!(pool.next_key_with_permit().is_ok());
}

#[test]
fn test_next_key_with_permit_distinguishes_cooldown() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 1);
    pool.mark_rate_limited(0, 30);

    assert!(matches!(
        pool.next_key_with_permit(),
        Err(KeyAcquireError::AllCoolingDown)
    ));
}

#[test]
fn test_rate_limit_shrinks_dynamic_window() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 4);

    assert_eq!(pool.status()[0].max_in_flight, 4);
    pool.record_rate_limited(0);
    assert_eq!(pool.status()[0].max_in_flight, 2);
    pool.record_rate_limited(0);
    assert_eq!(pool.status()[0].max_in_flight, 1);
}

/// Was `test_successes_reopen_dynamic_window_slowly`: asserted that recovering
/// from one shrink cost 8 successes per slot, which is the asymmetry this
/// change removed. Same scenario, updated to the new contract.
#[test]
fn test_successes_reopen_dynamic_window_proportionally() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 3);
    pool.record_rate_limited(0);
    assert_eq!(pool.status()[0].max_in_flight, 1);

    // deficit 2 below the ceiling of 3: ceil(8/2) = 4 successes for +1.
    for _ in 0..4 {
        pool.record_success(0);
    }
    assert_eq!(pool.status()[0].max_in_flight, 2);

    // deficit 1: one success.
    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 3);

    // At the ceiling, further successes do not overshoot.
    for _ in 0..10 {
        pool.record_success(0);
    }
    assert_eq!(pool.status()[0].max_in_flight, 3);
}

#[test]
fn test_available_permits_respects_dynamic_window() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 3);
    pool.record_rate_limited(0);
    assert_eq!(pool.available_permits(), 1);

    let lease = pool.next_key_with_permit().unwrap();
    assert_eq!(pool.available_permits(), 0);
    assert!(matches!(
        pool.next_key_with_permit(),
        Err(KeyAcquireError::AllBusy)
    ));
    drop(lease);
    assert_eq!(pool.available_permits(), 1);
}

#[test]
fn test_all_keys_cooldown_returns_none() {
    let keys = vec![make_key_entry("key1", "a"), make_key_entry("key2", "b")];
    let pool = KeyPool::new(keys);

    // Mark both keys as rate-limited
    pool.mark_rate_limited(0, 1);
    pool.mark_rate_limited(1, 1);

    // Should return None when all keys are cooling down
    assert_eq!(pool.next_key(), None);
}

#[test]
fn test_status_active() {
    let keys = vec![make_key_entry("nvapi-ABCD1234", "test")];
    let pool = KeyPool::new(keys);

    let statuses = pool.status();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].label, "test");
    assert_eq!(statuses[0].key_hint, "...1234");
    assert!(statuses[0].active);
    assert_eq!(statuses[0].cooldown_secs_remaining, 0);
}

#[test]
fn test_status_cooldown() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::new(keys);

    // Mark key with 10 second cooldown
    pool.mark_rate_limited(0, 10);

    let statuses = pool.status();
    assert!(!statuses[0].active);
    assert!(statuses[0].cooldown_secs_remaining > 0);
    assert!(statuses[0].cooldown_secs_remaining <= 10);
}

#[test]
fn test_key_hint_short_key() {
    let keys = vec![KeyEntry {
        key: "abc".to_string(),
        label: None,
        disabled: false,
    }];
    let pool = KeyPool::new(keys);

    let statuses = pool.status();
    assert_eq!(statuses[0].key_hint, "****");
}

#[test]
fn test_default_label() {
    let keys = vec![KeyEntry {
        key: "key1".to_string(),
        label: None,
        disabled: false,
    }];
    let pool = KeyPool::new(keys);

    let statuses = pool.status();
    assert_eq!(statuses[0].label, "key-0");
}

#[test]
fn test_next_key_all_keys_rate_limited_returns_none() {
    // Test that next_key returns None when all keys are rate-limited
    let keys = vec![
        make_key_entry("key1", "a"),
        make_key_entry("key2", "b"),
        make_key_entry("key3", "c"),
    ];
    let pool = KeyPool::new(keys);

    // Mark all keys as rate-limited for 60 seconds
    pool.mark_rate_limited(0, 60);
    pool.mark_rate_limited(1, 60);
    pool.mark_rate_limited(2, 60);

    // Should return None when all keys are cooling down
    assert_eq!(pool.next_key(), None);
}

#[test]
fn test_cooldown_expiration() {
    // Test that a key becomes available after cooldown expires
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::new(keys);

    // Mark key as rate-limited for 1 second
    pool.mark_rate_limited(0, 1);

    // Should return None while cooling down
    assert_eq!(pool.next_key(), None);

    // Wait for cooldown to expire
    std::thread::sleep(Duration::from_secs(1) + Duration::from_millis(100));

    // Should now return the key
    let (k, idx) = pool
        .next_key()
        .expect("key should be available after cooldown");
    assert_eq!(k, "key1");
    assert_eq!(idx, 0);
}

#[test]
fn test_next_key_single_key_repeated() {
    // Test next_key with single key returns it consistently
    let keys = vec![make_key_entry("single-key", "only")];
    let pool = KeyPool::new(keys);

    // First call
    let (k, idx) = pool.next_key().expect("should return key");
    assert_eq!(k, "single-key");
    assert_eq!(idx, 0);

    // Second call should also return the same key
    let (k, idx) = pool.next_key().expect("should return key again");
    assert_eq!(k, "single-key");
    assert_eq!(idx, 0);

    // Third call still the same
    let (k, idx) = pool.next_key().expect("should return key third time");
    assert_eq!(k, "single-key");
    assert_eq!(idx, 0);
}

#[test]
fn test_status_mixed_active_inactive() {
    // Test status returns correct state for mixed active/inactive keys
    let keys = vec![
        make_key_entry("key1-active", "active"),
        make_key_entry("key2-cooldown", "cooling"),
        make_key_entry("key3-active", "also-active"),
    ];
    let pool = KeyPool::new(keys);

    // Mark only key2 as rate-limited
    pool.mark_rate_limited(1, 30);

    let statuses = pool.status();
    assert_eq!(statuses.len(), 3);

    // Key 1 should be active
    assert_eq!(statuses[0].label, "active");
    assert!(statuses[0].active);
    assert_eq!(statuses[0].cooldown_secs_remaining, 0);

    // Key 2 should be inactive with cooldown
    assert_eq!(statuses[1].label, "cooling");
    assert!(!statuses[1].active);
    assert!(statuses[1].cooldown_secs_remaining > 0);
    assert!(statuses[1].cooldown_secs_remaining <= 30);

    // Key 3 should be active
    assert_eq!(statuses[2].label, "also-active");
    assert!(statuses[2].active);
    assert_eq!(statuses[2].cooldown_secs_remaining, 0);
}

#[test]
fn test_get_key_label_with_label() {
    // Test get_key_label returns explicit label
    let keys = vec![make_key_entry("key1", "labeled-key")];
    let pool = KeyPool::new(keys);

    assert_eq!(pool.get_key_label(0), Some("labeled-key".to_string()));
}

#[test]
fn test_get_key_label_out_of_bounds() {
    // Test get_key_label returns None for out of bounds index
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::new(keys);

    assert_eq!(pool.get_key_label(1), None);
}

#[test]
fn test_get_key_label_default_format() {
    // Test get_key_label uses default format when no label
    let keys = vec![KeyEntry {
        key: "key1".to_string(),
        label: None,
        disabled: false,
    }];
    let pool = KeyPool::new(keys);

    assert_eq!(pool.get_key_label(0), Some("key-0".to_string()));
}

/// Regression for asymmetric AIMD recovery.
///
/// A single 429 halved a key's window, but recovering one slot needed 8
/// successes, so a key stuck one step below its ceiling was throttled for
/// a long stretch. Measured on the live host: two keys sat at
/// max_in_flight=1 against a configured 2 while two others ran at 2,
/// carrying the traffic (successful requests split 32/22 vs 5/1).
#[test]
fn test_aimd_recovery_is_proportional_to_deficit() {
    let mut keys = Vec::new();
    for i in 0..4 {
        keys.push(make_key_entry(&format!("key{}", i), &format!("k{}", i)));
    }
    // max_in_flight_per_key = 4 gives room to observe both one-step and
    // multi-step recovery.
    let pool = KeyPool::with_max_in_flight(keys, 4);

    // Shrink every key once: 4 -> 2 (one multiplicative step).
    for i in 0..4 {
        pool.record_rate_limited(i);
    }
    assert_eq!(pool.status()[0].max_in_flight, 2);

    // Two steps below the ceiling of 4: ceil(8 / 2) = 4 successes on the
    // SAME key (the counter is per-key, not shared).
    for _ in 0..4 {
        pool.record_success(0);
    }
    assert_eq!(pool.status()[0].max_in_flight, 3);

    // Now one step below: recovery costs ONE success (was 8).
    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 4);
}

/// The old behavior: 8 successes to recover a single slot, forever. This
/// documents the asymmetry the fix removed.
#[test]
fn test_aimd_still_throttles_after_a_rate_limit() {
    let keys = vec![make_key_entry("key0", "a"), make_key_entry("key1", "b")];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    pool.record_rate_limited(0);
    assert_eq!(pool.status()[0].max_in_flight, 1);

    // A rate-limited key must NOT leap straight back to the ceiling: the
    // backoff half of AIMD still works.
    pool.record_success(0);
    assert_eq!(
        pool.status()[0].max_in_flight,
        2,
        "one-step deficit recovers in one success"
    );
}

/// Regression test for the fleet-wide lockout measured on 2026-10-15.
///
/// A timeout retry used to call `mark_rate_limited`, which benches the key and
/// halves its window. But all keys are NVIDIA keys on one shared account and
/// the upstream stalls across all of them roughly evenly (measured 27%–57% per
/// key), so "this key timed out" is not evidence "this key is throttled".
/// Punishing it concentrated the same load onto the remaining keys, which then
/// stalled and were benched too: admitted traffic fell to 12.9% of requests
/// and the fleet saw 503/429 for everything else.
///
/// The contract now: an upstream stall is observed and reported, but it never
/// changes the key's window or cooldown. Only a real 429 does that.
#[test]
fn test_upstream_stall_does_not_throttle_the_key() {
    let keys = vec![
        make_key_entry("key0", "a"),
        make_key_entry("key1", "b"),
        make_key_entry("key2", "c"),
        make_key_entry("key3", "d"),
    ];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    // Every key stalls once. Under the old behavior this benched all four and
    // halved every window, leaving the gateway with no usable key.
    for idx in 0..4 {
        pool.note_upstream_stall(idx);
    }

    for idx in 0..4 {
        assert_eq!(
            pool.status()[idx].max_in_flight,
            2,
            "key {idx}: a stall must not shrink the window"
        );
        assert_eq!(
            pool.status()[idx].cooldown_secs_remaining,
            0,
            "key {idx}: a stall must not bench the key"
        );
        assert_eq!(
            pool.status()[idx].stalls,
            1,
            "key {idx}: the stall must still be recorded for observability"
        );
    }

    // The gateway must still be able to serve: every key remains leasable.
    for _ in 0..4 {
        assert!(
            pool.next_key_with_permit().is_ok(),
            "a gateway whose keys merely stalled must still admit traffic"
        );
    }
}

/// The complement: a real 429 still throttles the key. Without this the fix
/// above could pass by simply never throttling anything, which would be the
/// more dangerous defect.
#[test]
fn test_real_rate_limit_still_throttles_the_key() {
    let keys = vec![make_key_entry("key0", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    pool.mark_rate_limited(0, 60);

    assert_eq!(
        pool.status()[0].max_in_flight,
        1,
        "a real 429 is per-key evidence and must still halve the window"
    );
    assert!(
        pool.status()[0].cooldown_secs_remaining > 0,
        "a real 429 must still bench the key"
    );
    // And it is not miscounted as a stall.
    assert_eq!(pool.status()[0].stalls, 0);
}

/// The death spiral itself, as an executable scenario: the old code path
/// (stall → throttle) collapsed the gateway to zero capacity, while the new
/// one keeps it serving. This is the invariant that the live fleet violates.
///
/// The discriminating assertion is the cooldown, not the window: a stalling key
/// must stay leasable. Under the old behavior every key ended up benched for
/// 60 s, so `next_key_with_permit` failed for all four and the gateway
/// returned 503/429 to the whole fleet. (Window capacity alone is not enough
/// to tell the two apart — the successes in the loop restore it either way.)
#[test]
fn test_repeated_stalls_do_not_collapse_gateway_capacity() {
    let keys = vec![
        make_key_entry("key0", "a"),
        make_key_entry("key1", "b"),
        make_key_entry("key2", "c"),
        make_key_entry("key3", "d"),
    ];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    // 20 rounds of "every key stalls once" — a sustained upstream degradation.
    for _ in 0..20 {
        for idx in 0..4 {
            pool.note_upstream_stall(idx);
            pool.record_success(idx);
        }
    }

    let total_capacity: usize = pool.status().iter().map(|s| s.max_in_flight).sum();
    assert_eq!(
        total_capacity, 8,
        "sustained upstream stalls must not erode aggregate capacity (got {total_capacity})"
    );

    // Every key should have recovered to its ceiling by now.
    for s in pool.status() {
        assert_eq!(s.max_in_flight, 2);
    }

    // The load-bearing part: a stall must never bench a key, so the gateway
    // can still hand out leases. Under the old behavior all four keys were in
    // a 60 s cooldown and this loop fails immediately.
    for _ in 0..4 {
        assert!(
            pool.next_key_with_permit().is_ok(),
            "a key that merely stalled must stay leasable, otherwise the fleet is locked out"
        );
    }
}

/// S6: a `status()` read must be consistent across the cooldown, the window,
/// and the in-flight count. Before the read was made atomic, a write that
/// landed between two of those reads produced a state that never existed: a
/// key shown active while its window was already shrinking, or a stall counted
/// against a key that was simultaneously benched.
///
/// This cannot directly observe the interleaving — a race window this small
/// does not reproduce reliably in a unit test — so it pins the stronger
/// invariant that the read is *internally coherent*: the window and the
/// cooldown it reports always come from the same point in time.
#[test]
fn test_status_is_internally_coherent_under_writes() {
    use std::sync::Arc;
    let keys = vec![
        make_key_entry("key0", "a"),
        make_key_entry("key1", "b"),
        make_key_entry("key2", "c"),
        make_key_entry("key3", "d"),
    ];
    let pool = Arc::new(KeyPool::with_max_in_flight(keys, 2));

    // Hammer the pool with writers from another thread while reading status.
    let writer = Arc::clone(&pool);
    let handle = std::thread::spawn(move || {
        for _ in 0..50 {
            for idx in 0..4 {
                writer.note_upstream_stall(idx);
                writer.record_success(idx);
            }
        }
    });

    // Read repeatedly. Every snapshot must be self-consistent: a key that is
    // active has no cooldown, and its window is a legal value (never 0, never
    // above the ceiling). A torn read would show a key benched with a
    // zero-window, or active with a window the writer had already shrunk
    // below zero.
    for _ in 0..100 {
        for s in pool.status() {
            assert!(
                s.max_in_flight >= 1,
                "a key's window must never drop to zero: {s:?}"
            );
            assert!(
                s.max_in_flight <= 2,
                "a key's window must never exceed the ceiling: {s:?}"
            );
            assert!(
                s.cooldown_secs_remaining <= 60,
                "a cooldown must be bounded: {s:?}"
            );
            if s.active {
                assert_eq!(
                    s.cooldown_secs_remaining, 0,
                    "an active key must not report a cooldown: {s:?}"
                );
            }
        }
    }
    handle.join().unwrap();
}

/// Cooling and auth-quarantine used to share one `Instant`, so an operator
/// could not ask "which failure is this key benched for?". These are different
/// actions — wait a 429 out, vs replace a revoked key — and mixing them is the
/// failure mode that made a real auth quarantine invisible behind what looked
/// like transient throttling.
#[test]
fn test_cooling_and_auth_quarantine_are_distinguishable() {
    let keys = vec![make_key_entry("key0", "a"), make_key_entry("key1", "b")];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    pool.mark_rate_limited(0, 60);
    pool.mark_auth_failed(1, Duration::from_secs(60));

    let status = pool.status();
    assert_eq!(
        status[0].bench_reason,
        Some(crate::key_pool::BenchReason::RateLimit),
        "a 429 is a RateLimit"
    );
    assert_eq!(
        status[1].bench_reason,
        Some(crate::key_pool::BenchReason::AuthFailure),
        "a 401/403 is an AuthFailure, not a RateLimit"
    );
    // And an auth failure is counted separately, so the two failure modes are
    // visible independently.
    assert!(status[1].auth_failures > 0);
    assert_eq!(status[0].auth_failures, 0);
}

/// A retry must land on a *different* key than the one that just stalled.
/// Plain round-robin cannot promise that: once the pool wraps, the next slot
/// is the failed key again. The exclusion is what makes the retry
/// same-prompt-as-fresh-on-another-key rather than same-prompt-same-key.
#[test]
fn test_excluding_returns_a_different_key() {
    let keys = vec![make_key_entry("key0", "a"), make_key_entry("key1", "b")];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    let first = pool.next_key_with_permit().unwrap();
    let retry = pool
        .next_key_with_permit_excluding(Some(first.idx))
        .unwrap();
    assert_ne!(
        retry.idx, first.idx,
        "a retry that excludes the stalled key must land on the other key"
    );

    // Repeated exclusions keep missing the failed key: 10 retries, 10 times
    // the fresh key. (Each lease drops at the end of its iteration, so the
    // permits are not exhausted.)
    for _ in 0..10 {
        let again = pool
            .next_key_with_permit_excluding(Some(first.idx))
            .unwrap();
        assert_ne!(again.idx, first.idx);
    }
}

/// A single-key pool cannot satisfy an exclusion, so it degrades to the
/// unexcluded pick rather than refusing. Refusing would turn every stall
/// into an admission failure (429/503), which is worse than retrying on the
/// only key there is — and a stall never benches the key, so the lease is
/// still legal.
#[test]
fn test_excluding_with_a_single_key_still_yields_a_lease() {
    let keys = vec![make_key_entry("key0", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 2);

    let retry = pool.next_key_with_permit_excluding(Some(0)).unwrap();
    assert_eq!(retry.idx, 0);
}

/// The knob is OFF by default, and with it off the recovery curve is the
/// fixed one: `ceil(8 / deficit)` successes per slot, and the credit is
/// zeroed by a 429.
///
/// This is the test that would fail if the default path changed. Below the
/// ~29 % clamp point the adaptive bar equals the fixed one, so if the
/// default ever started reading the pressure ratio, the decayed credit —
/// which is never zeroed — is what would change the schedule, not the bar.
#[test]
fn test_fixed_rule_is_the_default_and_is_unchanged() {
    let mut keys = Vec::new();
    for i in 0..4 {
        keys.push(make_key_entry(&format!("key{}", i), &format!("k{}", i)));
    }
    let pool = KeyPool::with_max_in_flight(keys, 4);
    // The default constructor hard-codes AimdMode::Fixed; the flag is on the
    // knob, read at pool construction, not a pool accessor.

    for i in 0..4 {
        pool.record_rate_limited(i);
    }
    assert_eq!(pool.status()[0].max_in_flight, 2);

    // Three successes must NOT be enough, and the fourth must be: the fixed
    // bar for deficit 2 is ceil(8 / 2) = 4 either way, because below the
    // ~29 % clamp point the adaptive bar equals the fixed one. The guard is
    // the zeroing below.
    for _ in 0..3 {
        pool.record_success(0);
    }
    assert_eq!(
        pool.status()[0].max_in_flight,
        2,
        "the default rule must not recover a deficit-2 window in 3 successes"
    );

    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 3);

    // And the credit is zeroed, not decayed: after the next 429 the key sits
    // at window 1 against a ceiling of 4 (deficit 3, cost ceil(8 / 3) = 3),
    // and one success earns the first of the three — it does not move the
    // window, where the decayed-credit rule would have satisfied the slot
    // with the residue alone.
    pool.record_rate_limited(0);
    assert_eq!(pool.status()[0].max_in_flight, 1);
    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 1);
    pool.record_success(0);
    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 2);
}

/// The derived bar cannot beat the static constant below its ~29 % clamp
/// point: at no pressure and at the measured 18-23 % operating point it is 8,
/// the same cost. It pays strictly less only above the clamp point.
#[test]
fn test_adaptive_bar_matches_the_fixed_bar_under_no_pressure() {
    assert_eq!(super::aimd::pressure_bar(0.0), 8);
    assert_eq!(super::aimd::pressure_bar(0.05), 8);
    for deficit in 1..=8 {
        assert_eq!(
            super::aimd::pressure_success_cost(deficit, 0.05),
            super::aimd::static_success_cost(deficit),
            "deficit {deficit}: at 5% 429 the adaptive rule must cost the same as today"
        );
    }
    // Below the clamp point the derived bar cannot beat the static constant:
    // the ceiling, not the formula, sets the cost. So at today's measured
    // 22.64 % the bar is 8, exactly today's cost, and it is the decayed
    // credit — not a cheaper bar — that lets the adaptive rule bank a slot.
    let light = super::aimd::pressure_bar(0.05);
    let measured = super::aimd::pressure_bar(0.2264);
    assert_eq!(
        measured, light,
        "below the clamp point the derived bar cannot beat the static constant"
    );
    // Above the clamp point heavier pressure pays strictly less, never more.
    let heavy = super::aimd::pressure_bar(0.5);
    assert!(
        heavy < light,
        "above the clamp point a heavily throttled key must pay less than a lightly throttled one"
    );
}

/// The core fix, as a deterministic stream: a key under a sustained 22.64 %
/// 429 stream reaches a higher window under the adaptive rule than the fixed
/// rule allows in the same number of outcomes.
///
/// The stream interleaves the bench: every 429 is followed by a gap of
/// `bench_len` outcomes during which the key earns nothing, which is what the
/// live proxy does (a 429 benches the key, and a benched key is skipped by
/// `next_key`). A fixed bar cannot recover inside that pattern, because each
/// new 429 zeroes the credit before a full run is banked.
#[test]
fn test_adaptive_rule_recovers_where_the_fixed_rule_cannot() {
    let ceiling = 4;
    let n = 2000;
    // Every 4th outcome is a 429, and each 429 costs a bench of 6 idle
    // outcomes (60 s of cooldown against a ~0.1 req/s per-key arrival).
    let (rate_limit_period, bench_len) = (4, 6);
    // Fixed arm: build the same pool but force the fixed rule.
    let keys = vec![make_key_entry("key0", "a")];
    let fixed = KeyPool::with_max_in_flight(keys, ceiling);
    let adaptive = adaptive_pool(1, ceiling);

    for step in 0..n {
        // The bench: a window of outcomes where the key is benched and earns
        // nothing. It is modelled as "skip" rather than a timer, because the
        // window state does not change while a key is benched.
        if step % (rate_limit_period + bench_len) >= rate_limit_period {
            continue;
        }
        if step % rate_limit_period == rate_limit_period - 1 {
            fixed.record_rate_limited(0);
            adaptive.record_rate_limited(0);
        } else {
            fixed.record_success(0);
            adaptive.record_success(0);
        }
    }

    let fixed_window = fixed.status()[0].max_in_flight;
    let adaptive_window = adaptive.status()[0].max_in_flight;
    assert!(
        adaptive_window > fixed_window,
        "adaptive window {adaptive_window} must exceed the fixed window {fixed_window} \
         under a sustained high-429 stream"
    );
    // The pressure ratio is the input to the rule and must be non-zero, or
    // the adaptive arm would just be the fixed arm with extra bookkeeping.
    let ratio = adaptive.status()[0].rate_limit_ratio;
    assert!(
        ratio > 0.15 && ratio < 0.30,
        "the stream is a 1-in-4 429 pattern, so the ratio should sit near 0.25, got {ratio}"
    );
}

/// The complement of the recovery test: with NO 429 pressure the adaptive rule
/// is never *faster* than today's rule. Recovery credit is only ever earned
/// by successful first bytes, so a key that is not being throttled pays the
/// full cost.
///
/// Recovery credit is only ever earned by successful first bytes, so a key
/// that is not being throttled pays the full cost: at a ratio of 0 the derived
/// bar is `AIMD_BAR_MAX`, which is exactly `AIMD_SUCCESS_THRESHOLD`.
#[test]
fn test_adaptive_rule_is_not_faster_without_429_pressure() {
    // Cost-function level: identical at zero pressure for every deficit.
    for deficit in 1..=8 {
        assert_eq!(
            super::aimd::pressure_success_cost(deficit, 0.0),
            super::aimd::static_success_cost(deficit),
            "deficit {deficit}: no pressure must cost the same as today"
        );
    }

    // Stream level: an adaptive pool that has never been throttled charges
    // the same per-slot costs as a fixed one.
    let fixed = KeyPool::with_max_in_flight(vec![make_key_entry("key0", "a")], 4);
    let adaptive = adaptive_pool(1, 4);
    assert_eq!(fixed.status()[0].max_in_flight, 4);
    assert_eq!(adaptive.status()[0].max_in_flight, 4);

    // One shrink halves a ceiling-4 window to 2 (a halving can only reach 1
    // from 2), so both pools start the comparison from 2 on the same footing.
    fixed.record_rate_limited(0);
    adaptive.record_rate_limited(0);
    assert_eq!(fixed.status()[0].max_in_flight, 2);
    assert_eq!(adaptive.status()[0].max_in_flight, 2);

    // The schedule is identical either way: each success has the same deficit,
    // and the same deficit charges the same cost, so the two tracks cannot
    // diverge while the 429 generator stays quiet.
    for _ in 0..8 {
        fixed.record_success(0);
        adaptive.record_success(0);
        assert_eq!(
            fixed.status()[0].max_in_flight,
            adaptive.status()[0].max_in_flight,
            "with no 429 pressure the adaptive rule must track the fixed one step for step"
        );
    }
}

/// The credit survives a 429 under the adaptive rule, which is what makes the
/// recovery reachable across bench cycles. Under the fixed rule the same
/// sequence leaves the credit at zero.
///
/// Ceiling 8 so the deficits stay in the multi-step regime the adaptive bar
/// actually differentiates. A single 429 leaves the ratio near
/// `1/8 = 0.125`, so the bar is 7 and a deficit-4 window costs
/// `ceil(7 / 4) = 2` successes per slot.
#[test]
fn test_adaptive_credit_survives_a_rate_limit() {
    let pool = adaptive_pool(1, 8);
    pool.record_rate_limited(0); // 8 -> 4
    assert_eq!(pool.status()[0].max_in_flight, 4);

    // Two successes reach the deficit-4 cost, so the window grows to 5.
    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 4);
    pool.record_success(0);
    assert_eq!(pool.status()[0].max_in_flight, 5);

    // A 429 halves the window AND halves the credit, so the residue carries
    // forward. The growth at 5 spent the banked credit, so the second 429
    // divides 0 — and against a ceiling of 8 the window 2 faces a deficit of
    // 6, priced at ceil(8 / 6) = 2, so the slot completes on the SECOND
    // success, not the third as the deficit-3 arithmetic above would say.
    pool.record_rate_limited(0);
    assert_eq!(pool.status()[0].max_in_flight, 2);
    pool.record_success(0);
    pool.record_success(0);
    assert_eq!(
        pool.status()[0].max_in_flight,
        3,
        "at deficit 6 under a bar of 8 the cost is ceil(8 / 6) = 2"
    );
}

/// The knob's config surface: default off, and the accessors clamp.
fn write_limit_config(content: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut file = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file
}

#[test]
fn test_aimd_knob_defaults_off() {
    let file = write_limit_config(
        r#"
[[keys]]
key = "test"
"#,
    );
    let config = crate::config::load(file.path().to_str().unwrap()).unwrap();
    assert!(!config.aimd_pressure_adaptive());
    assert_eq!(config.aimd_max_success_bar(), 8);

    let file = write_limit_config(
        r#"
[limits]
aimd_pressure_adaptive = true
aimd_max_success_bar = 99

[[keys]]
key = "test"
"#,
    );
    let config = crate::config::load(file.path().to_str().unwrap()).unwrap();
    assert!(config.aimd_pressure_adaptive());
    assert_eq!(
        config.aimd_max_success_bar(),
        8,
        "an out-of-range bar is clamped"
    );
}

/// The bench cap is a MAXIMUM, never an extension: it can shorten a bench the
/// upstream asked to be long, but it must never lengthen one the upstream asked
/// to be short. With no cap configured the upstream's own duration is honoured
/// in full - which is today's behaviour exactly.
#[test]
fn test_rate_limit_bench_cap_shortens_but_never_extends() {
    let keys = vec![make_key_entry("key1", "a")];

    // No cap: the upstream's duration is honoured exactly.
    let pool =
        KeyPool::with_max_in_flight_aimd_and_bench_cap(keys.clone(), 4, AimdMode::Fixed, None);
    pool.mark_rate_limited(0, 120);
    let benched = pool.status()[0].cooldown_secs_remaining;
    // `cooldown_secs_remaining` is a floored `duration_since`, so a 120 s
    // bench reads back as 119 or 120 depending on sub-second rounding.
    // Either is the upstream's duration honoured; what would be wrong is a
    // capped or defaulted value.
    assert!(
        (119..=120).contains(&benched),
        "with no cap the upstream's 120 s must be honoured, got {benched}s"
    );

    // A cap BELOW the upstream's duration shortens it.
    let pool =
        KeyPool::with_max_in_flight_aimd_and_bench_cap(keys.clone(), 4, AimdMode::Fixed, Some(30));
    pool.mark_rate_limited(0, 120);
    let benched = pool.status()[0].cooldown_secs_remaining;
    assert!(
        benched <= 30,
        "the cap must shorten a 120 s bench, got {benched}s"
    );

    // A cap ABOVE the upstream's duration must NOT extend it.
    pool.mark_rate_limited(0, 5);
    let benched = pool.status()[0].cooldown_secs_remaining;
    assert!(
        benched <= 5,
        "the cap must never extend a bench, got {benched}s"
    );
}

/// `bench_key` is the COOLDOWN-ONLY half of a rate-limit reaction: it must
/// make the key unselectable for the requested duration without touching the
/// AIMD window, the 429 ratio, or the recovery credit.
///
/// The racing path depends on this distinction. A single racing leg can
/// produce a 429 while other legs are in flight; if the post-join bench also
/// applied the AIMD penalty, one upstream event would shrink the window more
/// than once.
#[test]
fn test_bench_key_applies_cooldown_without_aimd_penalty() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 4);

    let before = pool.status()[0].clone();
    assert_eq!(before.max_in_flight, 4);
    assert_eq!(before.rate_limit_ratio, 0.0);

    pool.bench_key(0, 60);

    let after = pool.status()[0].clone();
    assert!(
        !after.active,
        "bench_key must take the key out of selection"
    );
    assert!(
        after.cooldown_secs_remaining > 0 && after.cooldown_secs_remaining <= 60,
        "bench_key must apply the requested cooldown, got {}s",
        after.cooldown_secs_remaining
    );
    assert_eq!(
        after.bench_reason,
        Some(BenchReason::RateLimit),
        "bench_key must record a rate-limit bench"
    );
    assert_eq!(
        after.max_in_flight, before.max_in_flight,
        "bench_key must NOT halve the AIMD window"
    );
    assert_eq!(
        after.rate_limit_ratio, before.rate_limit_ratio,
        "bench_key must NOT fold a 429 into the pressure ratio"
    );
    assert_eq!(
        after.auth_failures, 0,
        "bench_key must not be counted as an auth failure"
    );
    assert_eq!(
        after.stalls, 0,
        "bench_key must not be counted as an upstream stall"
    );

    // The recovery credit must survive a bench as well. In the fixed rule the
    // credit is the number of banked successes below the ceiling, so it is
    // observable: shrink the window to 2 of 4 (cost: 4 successes for the next
    // slot), bank 2 successes, bench the key, and then 2 more successes must
    // be enough to reopen a slot. A bench that zeroed the credit would need 4.
    let credit_pool = KeyPool::with_max_in_flight(vec![make_key_entry("key2", "b")], 4);
    credit_pool.record_rate_limited(0);
    credit_pool.record_success(0);
    credit_pool.record_success(0);
    assert_eq!(
        credit_pool.status()[0].max_in_flight,
        2,
        "the two banked successes are not enough to reopen a slot yet"
    );

    let before_bench = credit_pool.status()[0].clone();
    credit_pool.bench_key(0, 60);
    let after_bench = credit_pool.status()[0].clone();
    assert_eq!(
        after_bench.max_in_flight, before_bench.max_in_flight,
        "bench_key must NOT halve the AIMD window"
    );
    assert_eq!(
        after_bench.rate_limit_ratio, before_bench.rate_limit_ratio,
        "bench_key must NOT fold a 429 into the pressure ratio"
    );

    credit_pool.record_success(0);
    credit_pool.record_success(0);
    assert_eq!(
        credit_pool.status()[0].max_in_flight,
        3,
        "the banked recovery credit must survive a bench-only cooldown"
    );
}

/// The complementary contract: `mark_rate_limited` applies EXACTLY ONE AIMD
/// penalty and one bench. Calling it twice must therefore halve the window
/// twice (4 -> 2 -> 1), never once more or less than that.
#[test]
fn test_mark_rate_limited_applies_exactly_one_aimd_penalty_and_bench() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 4);

    pool.mark_rate_limited(0, 60);

    let after = pool.status()[0].clone();
    assert_eq!(
        after.max_in_flight, 2,
        "one 429 must halve the window exactly once"
    );
    assert!(!after.active, "mark_rate_limited must bench the key");
    assert_eq!(
        after.bench_reason,
        Some(BenchReason::RateLimit),
        "mark_rate_limited must record a rate-limit bench"
    );
    assert!(
        after.cooldown_secs_remaining > 0 && after.cooldown_secs_remaining <= 60,
        "mark_rate_limited must apply the requested cooldown, got {}s",
        after.cooldown_secs_remaining
    );

    // A second event is a second penalty: 2 -> 1, floored at one slot.
    pool.mark_rate_limited(0, 60);
    assert_eq!(
        pool.status()[0].max_in_flight,
        1,
        "a second 429 must halve the window exactly once again"
    );
}

/// The bench cap applies to the cooldown-only operation too, and remains a
/// MAXIMUM: it shortens a long upstream-requested bench, preserves it when
/// unset, and never extends a short one.
#[test]
fn test_bench_key_honours_bench_cap_as_maximum() {
    let keys = vec![make_key_entry("key1", "a")];

    // No cap: the requested duration is preserved.
    let pool =
        KeyPool::with_max_in_flight_aimd_and_bench_cap(keys.clone(), 4, AimdMode::Fixed, None);
    pool.bench_key(0, 120);
    let benched = pool.status()[0].cooldown_secs_remaining;
    assert!(
        (119..=120).contains(&benched),
        "with no cap bench_key must preserve the requested 120 s, got {benched}s"
    );
    assert_eq!(
        pool.status()[0].max_in_flight,
        4,
        "bench_key must not change the AIMD window while applying a cap"
    );

    // A cap BELOW the requested duration shortens it.
    let pool =
        KeyPool::with_max_in_flight_aimd_and_bench_cap(keys.clone(), 4, AimdMode::Fixed, Some(30));
    pool.bench_key(0, 120);
    let benched = pool.status()[0].cooldown_secs_remaining;
    assert!(
        (29..=30).contains(&benched),
        "the cap must shorten a 120 s bench to 30 s, got {benched}s"
    );

    // A cap ABOVE the requested duration must NOT extend it.
    pool.bench_key(0, 5);
    let benched = pool.status()[0].cooldown_secs_remaining;
    assert!(
        (4..=5).contains(&benched),
        "the cap must never extend a bench, got {benched}s"
    );
}

/// An active auth quarantine is credential-level evidence. A later transient
/// rate-limit bench must not downgrade it to `RateLimit`, shorten its
/// duration to the configured bench cap, or misreport its reason.
#[test]
fn test_active_auth_quarantine_survives_later_bench_key() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight_aimd_and_bench_cap(keys, 4, AimdMode::Fixed, Some(30));
    pool.mark_auth_failed(0, Duration::from_secs(900));

    pool.bench_key(0, 30);

    let after = pool.status()[0].clone();
    assert_eq!(
        after.bench_reason,
        Some(BenchReason::AuthFailure),
        "an active auth quarantine must survive a later rate-limit bench"
    );
    assert!(
        after.cooldown_secs_remaining > 30,
        "an active auth quarantine must not be shortened to the rate-limit bench cap, got {}s",
        after.cooldown_secs_remaining
    );
    assert!(
        !after.active,
        "the preserved auth quarantine must keep the key inactive"
    );
}

/// With no existing quarantine, the cooldown-only operation still applies the
/// ordinary rate-limit bench.
#[test]
fn test_bench_key_without_quarantine_applies_rate_limit_bench() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 4);

    pool.bench_key(0, 60);

    let after = pool.status()[0].clone();
    assert_eq!(
        after.bench_reason,
        Some(BenchReason::RateLimit),
        "a key without a quarantine must receive a rate-limit bench"
    );
    assert!(
        !after.active,
        "the rate-limit bench must make the key inactive"
    );
    assert!(
        after.cooldown_secs_remaining > 0 && after.cooldown_secs_remaining <= 60,
        "the rate-limit bench must apply the requested cooldown, got {}s",
        after.cooldown_secs_remaining
    );
}

/// Once an auth quarantine has expired, it is no longer active and a
/// rate-limit bench may replace it.
#[test]
fn test_expired_auth_cooldown_can_be_replaced_by_rate_limit_bench() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 4);
    pool.mark_auth_failed(0, Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(10));
    assert!(
        pool.status()[0].active,
        "the short auth quarantine must be expired before the bench"
    );

    pool.bench_key(0, 60);

    let after = pool.status()[0].clone();
    assert_eq!(
        after.bench_reason,
        Some(BenchReason::RateLimit),
        "an expired auth cooldown must be replaceable by a rate-limit bench"
    );
    assert!(
        !after.active,
        "the replacement rate-limit bench must make the key inactive"
    );
    assert!(
        after.cooldown_secs_remaining > 0 && after.cooldown_secs_remaining <= 60,
        "the replacement rate-limit bench must apply the requested cooldown, got {}s",
        after.cooldown_secs_remaining
    );
}

/// Preserving the auth cooldown must not suppress the AIMD penalty:
/// `mark_rate_limited` still applies exactly one penalty (4 -> 2 slots).
#[test]
fn test_mark_rate_limited_applies_exactly_one_aimd_penalty_with_active_auth_quarantine() {
    let keys = vec![make_key_entry("key1", "a")];
    let pool = KeyPool::with_max_in_flight(keys, 4);
    pool.mark_auth_failed(0, Duration::from_secs(900));

    pool.mark_rate_limited(0, 60);

    let after = pool.status()[0].clone();
    assert_eq!(
        after.max_in_flight, 2,
        "mark_rate_limited must apply exactly one AIMD penalty while preserving auth quarantine"
    );
    assert_eq!(
        after.bench_reason,
        Some(BenchReason::AuthFailure),
        "the AIMD penalty must not change which cooldown survives"
    );
}
