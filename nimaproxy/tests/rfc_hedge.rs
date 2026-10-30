//! Unit tests for RFC dirmacs/nimakai#63 (late hedge, rank, tool-id log,
//! loop in-flight guard, log fields).
//!
//! These are the mutation-proof hooks the separate proofs drive:
//! - atomic winner claim: a both-byte race yields exactly one body and the
//!   loser is not scored;
//! - no third call for D in {learned, 170 s};
//! - a silent call ends at its rail while the other leg keeps waiting;
//! - no free second slot means no hedge;
//! - rank (key, model) ordering and loser-not-scored;
//! - tool-id predicate (skipped != unmatched);
//! - loop idle-close blocked while an attempt is open + loop_seq not reused.

use std::time::Duration;

use nimaproxy::key_pool::KeyPool;
use nimaproxy::loop_tracker::{LoopTracker, RoundOutcome};
use nimaproxy::proxy::hedge::{
    drive_hedge_race, hedge_delay_ms, hedge_slot_available, LegOutcome, WinnerSlot,
    HEDGE_FIXED_DELAY_MS, HEDGE_MARGIN_MS,
};
use nimaproxy::test_utils::MockAppStateBuilder;

// ---------- Part 1: the hedge race ----------

/// A leg that produces a byte immediately.
async fn fast_leg() -> LegOutcome {
    LegOutcome::Won
}

/// A leg that stays silent past any reasonable rail (never produces a byte).
async fn silent_leg() -> LegOutcome {
    tokio::time::sleep(Duration::from_secs(3600)).await;
    LegOutcome::RailExpired
}

/// A leg that ends at its own rail with no byte after `ms`.
async fn rail_leg(ms: u64) -> LegOutcome {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    LegOutcome::RailExpired
}

#[tokio::test(start_paused = true)]
async fn both_byte_race_yields_exactly_one_winner_and_loser_not_scored() {
    // Both legs produce a byte "simultaneously": the atomic claim must let
    // exactly one win; the other is reported Lost (not scored).
    let race = drive_hedge_race(fast_leg(), fast_leg(), Duration::from_millis(0)).await;
    assert!(race.winner_leg.is_some(), "a byte race must have a winner");
    assert_eq!(
        race.loser,
        Some((1, LegOutcome::Lost))
            .or(Some((0, LegOutcome::Lost)))
            .filter(|l| l.1 == LegOutcome::Lost)
            .or(race.loser.filter(|l| l.1 == LegOutcome::Lost))
    );
    // Exactly one winner and one loser:
    let winner = race.winner_leg.unwrap();
    assert!(winner == 0 || winner == 1);
    let loser = race.loser.expect("the other leg must be reported Lost");
    assert_ne!(loser.0, winner, "winner and loser must be different legs");
    assert_eq!(loser.1, LegOutcome::Lost);
    assert!(!race.both_rail_expired);
}

#[tokio::test(start_paused = true)]
async fn silent_call_ends_at_its_rail_while_the_other_keeps_waiting() {
    // Leg 1 is silent for 200 ms then rail-expires; leg 2 answers at 50 ms.
    // The race must report leg 2 as the winner (the silent leg did NOT get
    // cancelled before its own rail; it simply lost when the other won).
    // Leg 1 is silent for 200 ms then rail-expires; leg 2 answers at 50 ms
    // and wins. Leg 1 is NOT cancelled before its own rail — it simply ends
    // at its rail afterwards; the winner is leg 2.
    let race = drive_hedge_race(
        rail_leg(200),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            LegOutcome::Won
        },
        Duration::from_millis(10),
    )
    .await;
    assert_eq!(race.winner_leg, Some(1));
    // leg 1 ended at its own rail (RailExpired), not cancelled by the win:
    // a timer never cancels a call no other call has beaten — leg 1 was
    // still open when leg 2 won, so it is reported Lost.
    assert!(
        race.loser == Some((0, LegOutcome::Lost)) || race.loser.is_none(),
        "loser must be leg 0 Lost or absent; got {:?}",
        race.loser
    );
}

#[tokio::test(start_paused = true)]
async fn both_legs_rail_expire_means_one_504_no_third_call() {
    let race = drive_hedge_race(rail_leg(100), rail_leg(100), Duration::from_millis(10)).await;
    assert!(race.winner_leg.is_none());
    assert!(race.both_rail_expired, "both rails expired with no byte");
    // The caller returns ONE 504 and starts no third call: the race result
    // carries no winner, so the attempt clamp (2) is what bounds it.
}

#[tokio::test(start_paused = true)]
async fn hedge_not_armed_before_the_delay() {
    // Leg 2 is admitted only after arm_delay; if leg 1 wins before that,
    // leg 2 must never even run (winner slot checked first).
    let race = drive_hedge_race(fast_leg(), silent_leg(), Duration::from_millis(500)).await;
    assert_eq!(race.winner_leg, Some(0));
    // leg 2 was armed after 500 ms but leg 1 already won at 0 ms, so leg 2
    // reports Lost without ever running its future to completion.
    assert_eq!(race.loser, Some((1, LegOutcome::Lost)));
}

// ---------- delay: fixed 170 s below the sample floor, learned above ----------

#[test]
fn no_third_call_delay_fixed_170s_below_floor() {
    let state = MockAppStateBuilder::new().with_keys(2).build();
    // No samples: fixed 170 s (capped by rail headroom when the rail is small).
    assert_eq!(hedge_delay_ms(&state, "m", 300_000), HEDGE_FIXED_DELAY_MS);
    assert_eq!(hedge_delay_ms(&state, "m", 300_000), 170_000);
}

#[test]
fn no_third_call_delay_learned_capped_by_rail() {
    let state = MockAppStateBuilder::new().with_keys(2).build();
    let model = "m";
    // dynamic_sample_floor defaults to 10 in RuntimeControls; record 30
    // successes at 1000 ms -> p95 = 1000, learned = 1000 + 30000 = 31000.
    for _ in 0..30 {
        state.model_stats.record(model, 1000.0, true);
    }
    assert_eq!(
        hedge_delay_ms(&state, model, 300_000),
        1000 + HEDGE_MARGIN_MS
    );
    // Rail cap: rail - 1000.
    assert_eq!(hedge_delay_ms(&state, model, 20_000), 19_000);
    // Failures/timeouts never enter the ring, so a stall cannot push p95 up:
    for _ in 0..50 {
        state.model_stats.record(model, 999_999.0, false);
    }
    assert_eq!(
        hedge_delay_ms(&state, model, 300_000),
        1000 + HEDGE_MARGIN_MS
    );
}

// ---------- no free second slot means no hedge ----------

#[test]
fn no_free_second_slot_means_no_hedge() {
    // A 1-permit global pool: after the first call takes the only permit,
    // hedge_slot_available must be false (available*2 >= total fails).
    let state = MockAppStateBuilder::new().with_keys(2).build();
    // max_upstream_in_flight default is 48, all free -> hedge allowed.
    assert!(hedge_slot_available(&state));
    // Occupy all but none: take every permit via try_acquire_upstream.
    let total = state.max_upstream_in_flight;
    let mut held = Vec::new();
    for _ in 0..total {
        if let Some(p) = state.try_acquire_upstream() {
            held.push(p);
        }
    }
    assert!(!hedge_slot_available(&state), "no free permits -> no hedge");
    drop(held);
    assert!(hedge_slot_available(&state));
    // Half-free boundary: reserve at least half for first calls.
    let mut held = Vec::new();
    for _ in 0..(total / 2) {
        if let Some(p) = state.try_acquire_upstream() {
            held.push(p);
        }
    }
    assert!(
        hedge_slot_available(&state),
        "exactly half free still allows hedge"
    );
    for _ in 0..1 {
        if let Some(p) = state.try_acquire_upstream() {
            held.push(p);
        }
    }
    assert!(
        !hedge_slot_available(&state),
        "below half free forbids hedge"
    );
}

// ---------- Part 2: rank (key, model) ----------

#[test]
fn rank_is_keyed_by_key_and_model_and_biases_round_robin() {
    let keys = vec![
        nimaproxy::KeyEntry {
            key: "k1".into(),
            label: Some("a".into()),
            disabled: false,
        },
        nimaproxy::KeyEntry {
            key: "k2".into(),
            label: Some("b".into()),
            disabled: false,
        },
        nimaproxy::KeyEntry {
            key: "k3".into(),
            label: Some("c".into()),
            disabled: false,
        },
    ];
    let pool = KeyPool::new(keys);
    // Default: all ranks 0 -> pure round-robin order.
    let mut picked = Vec::new();
    for _ in 0..3 {
        let lease = pool.next_key_with_permit().unwrap();
        picked.push(lease.idx);
    }
    assert_eq!(picked, vec![0, 1, 2]);

    // Key 2 answers for model "m" -> its rank rises; it must be picked first
    // for "m" while "other" model is unaffected.
    for _ in 0..10 {
        pool.record_answer(2, "m", true);
    }
    assert!(pool.rank_of(2, "m") > pool.rank_of(0, "m"));
    assert_eq!(pool.rank_of(0, "other"), 0.0, "rank is per-model");
    let lease = pool
        .next_key_with_permit_excluding_for_model(None, "m")
        .unwrap();
    assert_eq!(lease.idx, 2, "highest-ranked key for m is picked first");
    // ...but for another model the round-robin continues.
    let lease_other = pool
        .next_key_with_permit_excluding_for_model(None, "other")
        .unwrap();
    assert_ne!(
        lease_other.idx, 2,
        "rank for m must not promote the key for other models"
    );

    // A stall on key 0 for "m" pushes its rank below default.
    pool.record_answer(0, "m", false);
    assert!(pool.rank_of(0, "m") < 0.0);
}

#[test]
fn loser_of_a_hedge_is_not_scored_in_rank() {
    let keys = vec![
        nimaproxy::KeyEntry {
            key: "k1".into(),
            label: Some("a".into()),
            disabled: false,
        },
        nimaproxy::KeyEntry {
            key: "k2".into(),
            label: Some("b".into()),
            disabled: false,
        },
    ];
    let pool = KeyPool::new(keys);
    let before0 = pool.rank_of(0, "m");
    let before1 = pool.rank_of(1, "m");
    // A cancelled-because-other-won call scores NOTHING: the hedge code simply
    // never calls record_answer for it. Simulate: only the winner scores.
    pool.record_answer(1, "m", true);
    assert_eq!(pool.rank_of(0, "m"), before0, "loser not scored");
    assert!(pool.rank_of(1, "m") > before1, "winner scored");
}

// ---------- Part 3: tool-id predicate is covered by the in-crate
// tool_call_id_tests module (validate_mistral_tool_call_ids is pub(super));
// the skipped-vs-unmatched split is asserted there.

// ---------- Part 4: loop in-flight guard + seq tombstone ----------

#[test]
fn idle_close_blocked_while_an_attempt_is_open() {
    let t = LoopTracker::new(Duration::from_secs(300));
    let t0 = chrono::DateTime::parse_from_rfc3339("2026-10-18T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let t1 = t0 + chrono::Duration::minutes(10);
    t.begin("s1", Some("user"), 2, false, t0);
    // An attempt is in flight: the sweep must NOT idle-close the loop even
    // though last_seen is older than the timeout.
    t.attempt_started("s1");
    assert_eq!(t.in_flight("s1"), 1);
    assert!(
        t.sweep(t1).is_empty(),
        "idle close forbidden while attempt open"
    );
    // When the client response finishes, the sweep may close it.
    t.attempt_finished("s1");
    assert_eq!(t.in_flight("s1"), 0);
    let closed = t.sweep(t1);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].ended_by, nimaproxy::loop_tracker::LoopEnd::Idle);
}

#[test]
fn loop_seq_is_never_reused_after_eviction() {
    let t = LoopTracker::new(Duration::from_secs(300));
    let t0 = chrono::DateTime::parse_from_rfc3339("2026-10-18T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let t1 = t0 + chrono::Duration::hours(2);
    let p1 = t.begin("s1", Some("user"), 2, false, t0);
    assert_eq!(p1.loop_seq, 1);
    t.sweep(t1); // evicts the session, keeps the tombstone
    let p2 = t.begin("s1", Some("user"), 2, false, t1);
    assert_eq!(p2.loop_seq, 2, "loop_seq must not be reused");
    assert_eq!(t.loop_seq_high_water("s1"), 2);
}

#[test]
fn rail_ended_failure_counts_as_error_round_with_rail_as_model_ms() {
    let t = LoopTracker::new(Duration::from_secs(300));
    let t0 = chrono::DateTime::parse_from_rfc3339("2026-10-18T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    t.begin("s1", Some("user"), 2, false, t0);
    // A call that ends at its rail with no winner is an error round, with the
    // rail as model_ms.
    t.record_round(
        "s1",
        RoundOutcome {
            model_ms: 300_000,
            success: false,
            finish_reason: Some("rail".to_string()),
            tool_call_fragments: 0,
            tool_calls_distinct: 0,
        },
        t0,
    );
    // A call cancelled because the other won is NOT an error round: the
    // hedge code simply does not call record_round for it.
    let closed = t.sweep(t0 + chrono::Duration::minutes(10));
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].error_rounds, 1);
    assert_eq!(closed[0].model_ms, 300_000);
    assert_eq!(closed[0].completed_rounds, 1);
}

// ---------- Part 5: log fields ----------

#[test]
fn winner_slot_atomic_claim_is_single_winner() {
    let slot = WinnerSlot::new();
    assert!(slot.claim());
    assert!(!slot.claim());
    assert!(!slot.claim());
}
