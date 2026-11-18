//! The token-bucket arithmetic of `world::move_guard`, with an explicit clock.

use super::move_guard::*;

const LIMITS: Limits = Limits {
    max_speed: 30.0,
    max_up_speed: 20.0,
    burst_seconds: 3.0,
};
const NO_KICK: KickPolicy = KickPolicy {
    after: 0,
    window_ms: 60_000,
};
const SEED: Seed = Seed {
    teleport_seq: 0,
    instance_id: 0,
};

fn flat(m: f32) -> Cost {
    Cost {
        horizontal: m,
        up: 0.0,
    }
}

fn ok(g: &MoveGuard, now: i64, cost: Cost) -> bool {
    g.check(1, SEED, now, cost, LIMITS, NO_KICK) == Verdict::Ok
}

#[test]
fn a_new_entry_holds_the_full_burst() {
    let g = MoveGuard::default();
    assert!(ok(&g, 0, flat(90.0)));
    let g = MoveGuard::default();
    assert!(!ok(&g, 0, flat(90.5)));
}

#[test]
fn the_budget_refills_with_server_time_and_is_capped() {
    let g = MoveGuard::default();
    assert!(ok(&g, 0, flat(90.0)));
    // Empty: nothing fits.
    assert!(!ok(&g, 0, flat(1.0)));
    // One second later 30 m are back.
    assert!(ok(&g, 1_000, flat(30.0)));
    assert!(!ok(&g, 1_000, flat(1.0)));
    // A long pause does not bank more than the burst.
    assert!(!ok(&g, 1_000_000, flat(90.5)));
    assert!(ok(&g, 2_000_000, flat(90.0)));
}

#[test]
fn burst_then_sustained_speed() {
    let g = MoveGuard::default();
    // 100 ms ticks at 60 m/s (twice the limit): the 90 m burst lasts 3 s, then the speed
    // settles at the limit and the rest is refused.
    let mut accepted = 0.0;
    for tick in 0..100 {
        if ok(&g, tick * 100, flat(6.0)) {
            accepted += 6.0;
        }
    }
    // 10 s of server time: at most burst + 10 s * 30 m/s.
    assert!(accepted <= 90.0 + 300.0 + 6.0, "{accepted}");
    assert!(accepted >= 300.0, "{accepted}");
    // At exactly the limit nothing is ever refused.
    let g = MoveGuard::default();
    for tick in 0..200 {
        assert!(ok(&g, tick * 100, flat(3.0)), "tick {tick}");
    }
}

#[test]
fn the_upward_budget_is_separate_and_falling_is_free() {
    let g = MoveGuard::default();
    let up = |m: f32| Cost {
        horizontal: 0.0,
        up: m,
    };
    // 20 m/s * 3 s.
    assert!(ok(&g, 0, up(60.0)));
    assert!(!ok(&g, 0, up(1.0)));
    // Horizontal movement is unaffected by the empty upward budget.
    assert!(ok(&g, 0, flat(90.0)));
    // Falling never costs: cost_between clamps the height gain at zero.
    let fall = cost_between((0.0, 0.0, 500.0), (0.0, 0.0, 0.0));
    assert_eq!(fall.up, 0.0);
    assert!(ok(&g, 0, fall));
    let climb = cost_between((0.0, 0.0, 0.0), (3.0, 4.0, 2.0));
    assert_eq!((climb.horizontal, climb.up), (5.0, 2.0));
}

#[test]
fn a_refused_move_is_not_charged() {
    let g = MoveGuard::default();
    assert!(!ok(&g, 0, flat(500.0)));
    assert!(ok(&g, 0, flat(90.0)));
}

#[test]
fn a_changed_seed_starts_a_full_budget() {
    let g = MoveGuard::default();
    assert!(ok(&g, 0, flat(90.0)));
    assert!(!ok(&g, 0, flat(30.0)));
    let tp = Seed {
        teleport_seq: 1,
        ..SEED
    };
    assert_eq!(g.check(1, tp, 0, flat(90.0), LIMITS, NO_KICK), Verdict::Ok);
    let moved = Seed {
        instance_id: 5,
        ..tp
    };
    assert_eq!(
        g.check(1, moved, 0, flat(90.0), LIMITS, NO_KICK),
        Verdict::Ok
    );
    // Re-basing (the guard's own correction teleport) keeps the budget.
    let g = MoveGuard::default();
    assert!(ok(&g, 0, flat(90.0)));
    g.rebase(1, tp);
    assert!(matches!(
        g.check(1, tp, 0, flat(30.0), LIMITS, NO_KICK),
        Verdict::Reject(_)
    ));
}

#[test]
fn non_finite_costs_are_violations() {
    let g = MoveGuard::default();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(!ok(&g, 0, flat(bad)));
        let c = Cost {
            horizontal: 0.0,
            up: bad,
        };
        assert!(!ok(&g, 0, c));
    }
}

#[test]
fn reports_and_corrections_are_rate_limited() {
    let g = MoveGuard::default();
    let reject = |now: i64| match g.check(1, SEED, now, flat(1e6), LIMITS, NO_KICK) {
        Verdict::Reject(r) => r,
        Verdict::Ok => panic!("accepted"),
    };
    let first = reject(0);
    assert_eq!((first.report, first.correct), (Some(0), true));
    let second = reject(500);
    assert_eq!((second.report, second.correct), (None, false));
    let third = reject(1_500);
    assert_eq!((third.report, third.correct), (None, true));
    let later = reject(10_000);
    assert_eq!(later.report, Some(2));
}

#[test]
fn the_kick_threshold_counts_violations_inside_the_window() {
    let g = MoveGuard::default();
    let kick = KickPolicy {
        after: 3,
        window_ms: 10_000,
    };
    let reject = |now: i64| match g.check(1, SEED, now, flat(1e6), LIMITS, kick) {
        Verdict::Reject(r) => r,
        Verdict::Ok => panic!("accepted"),
    };
    assert!(!reject(0).kick);
    assert!(!reject(1_000).kick);
    assert!(reject(2_000).kick);
    // Spread out over more than the window: never reaches the threshold.
    let g2 = MoveGuard::default();
    for i in 0..10 {
        match g2.check(2, SEED, i * 11_000, flat(1e6), LIMITS, kick) {
            Verdict::Reject(r) => assert!(!r.kick),
            Verdict::Ok => panic!("accepted"),
        }
    }
}

#[test]
fn idle_entries_are_pruned() {
    let g = MoveGuard::default();
    for key in 0..300 {
        let _ = g.check(key, SEED, 0, flat(1.0), LIMITS, NO_KICK);
    }
    assert_eq!(g.len(), 300);
    // Much later, a check of one unit drops the idle ones.
    let _ = g.check(0, SEED, 2_000_000, flat(1.0), LIMITS, NO_KICK);
    assert_eq!(g.len(), 1);
}

/// A Tiger Strike leap (20 m over the 400 ms of `skill_controllers` 2278) is a normal move:
/// from the full 90 m burst it passes, and even a bucket drained to 25 m still carries it.
/// Only a budget already spent below the leap's reach refuses it, which is the guard's general
/// rule for any move, so the leap gets no special credit.
#[test]
fn a_tiger_strike_leap_fits_a_normal_budget() {
    let g = MoveGuard::default();
    // 4 accepted steps of 5 m in 400 ms, the way the client moves itself.
    for step in 0..4 {
        assert!(ok(&g, i64::from(step) * 100, flat(5.0)), "step {step}");
    }
    // A 65 m sprint before the leap leaves 25 m of the 3 s burst: the 20 m leap still fits.
    assert!(ok(&g, 2_000, flat(65.0)));
    for step in 0..4 {
        assert!(
            ok(&g, 2_100 + i64::from(step) * 100, flat(5.0)),
            "leap step {step}"
        );
    }
    // At 20 m left a 40 m dash is refused.
    let g = MoveGuard::default();
    assert!(ok(&g, 0, flat(70.0)));
    assert!(!ok(&g, 100, flat(40.0)));
}
