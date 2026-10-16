use super::*;
use std::time::Duration;

fn make_key_entry(key: &str, label: &str) -> KeyEntry {
    KeyEntry {
        key: key.to_string(),
        label: Some(label.to_string()),
        disabled: false,
    }
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
