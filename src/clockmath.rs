//! Clock budget arithmetic — how long the bot may take over a ply.
//!
//! The rule itself is the module's (`natural_state` charges every Ply and
//! flags a clock — *Time Accounting — Sanki*, as the session's module
//! implements it); this module only answers the *planning* question the
//! module's session-level answers do not: the **largest elapsed** the mover
//! can afford past the anchor before the flag falls, across bank rollovers
//! (*Time Accounting — Sanki* §Period transitions). It is a pacing heuristic,
//! never a verdict — and it is pinned against the module's own `clock`
//! primitive by an exhaustive boundary test, so it can never quietly diverge
//! from what the rule system rules.

use crate::module::Clock;

/// A time control in the ABI's encoding: period triples
/// `[duration, increment | null, plies | null]` (Kernel ABI — Sanki §Values).
pub type TimeControl = [[Option<u64>; 3]];

/// The largest `elapsed` (seconds) the mover's clock accepts from `clock`
/// without flagging: the current bank plus every subsequent period's
/// affordance (bank durations roll over; a quota period ends the roll with
/// its duration + per-ply allowance).
#[must_use]
pub fn max_affordable(tc: &TimeControl, clock: Clock) -> u64 {
    let Some(index) = usize::try_from(clock.period).ok() else {
        return 0;
    };
    let Some([_, increment, plies]) = tc.get(index) else {
        return 0; // a stale index flags immediately — nothing affordable
    };
    let increment = increment.unwrap_or(0);
    if plies.is_some() {
        // Quota period: the allowance is available during the ply; overspend
        // never rolls.
        return clock.remaining.saturating_add(increment);
    }
    // Bank period: spendable here, then the overspend rolls into the next.
    clock
        .remaining
        .saturating_add(affordable_from_fresh(tc, index.saturating_add(1)))
}

/// The affordance of entering period `index` fresh (bank reset to its
/// duration), rolling further while banks follow. Iterative: the number of
/// periods is the founding's.
fn affordable_from_fresh(tc: &TimeControl, mut index: usize) -> u64 {
    let mut total: u64 = 0;
    while let Some([duration, increment, plies]) = tc.get(index) {
        let duration = duration.unwrap_or(0);
        let increment = increment.unwrap_or(0);
        total = total.saturating_add(duration);
        if plies.is_some() {
            return total.saturating_add(increment);
        }
        index = index.saturating_add(1);
    }
    total
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use crate::module::{ask, native::Native};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Tick {
        kind: String,
    }

    /// One tick of the module's `clock` primitive: whether `elapsed` flags.
    fn flags(tc: &TimeControl, clock: Clock, elapsed: u64) -> bool {
        let time_control: Vec<serde_json::Value> = tc
            .iter()
            .map(|[d, i, p]| serde_json::json!([d, i, p]))
            .collect();
        let tick: Tick = ask(
            &mut Native,
            &serde_json::json!({
                "op": "clock",
                "time_control": time_control,
                "clock": {
                    "period": clock.period,
                    "plies_in_period": clock.plies_in_period,
                    "remaining": clock.remaining,
                },
                "elapsed": elapsed,
            }),
        )
        .unwrap();
        tick.kind == "flagged"
    }

    const fn clock(remaining: u64, period: u32, plies_in_period: u32) -> Clock {
        Clock {
            period,
            plies_in_period,
            remaining,
        }
    }

    /// The boundary pin: the module accepts exactly `max_affordable` and flags
    /// one second past it — across Fischer, plain banks, quota periods,
    /// rollover chains, and mid-quota states.
    #[test]
    fn boundary_agrees_with_the_module_tick() {
        let cases: Vec<(Vec<[Option<u64>; 3]>, Clock)> = vec![
            (vec![[Some(300), Some(3), None]], clock(300, 0, 0)),
            (vec![[Some(600), None, None]], clock(50, 0, 0)),
            (vec![[Some(0), Some(30), Some(1)]], clock(0, 0, 0)),
            (vec![[Some(60), Some(10), Some(3)]], clock(25, 0, 1)),
            (
                vec![[Some(3600), None, None], [Some(0), Some(30), Some(1)]],
                clock(10, 0, 0),
            ),
            (
                vec![
                    [Some(100), None, None],
                    [Some(50), None, None],
                    [Some(40), Some(5), None],
                ],
                clock(10, 0, 0),
            ),
            (
                vec![
                    [Some(5400), Some(30), Some(40)],
                    [Some(1800), Some(30), None],
                ],
                clock(1000, 0, 39),
            ),
        ];
        for (control, clock) in cases {
            let afford = max_affordable(&control, clock);
            assert!(
                !flags(&control, clock, afford),
                "affordable {afford} must not flag ({control:?})"
            );
            assert!(
                flags(&control, clock, afford + 1),
                "affordable {afford} + 1 must flag ({control:?})"
            );
        }
    }

    #[test]
    fn stale_period_affords_nothing() {
        let control = vec![[Some(300), Some(3), None]];
        assert_eq!(max_affordable(&control, clock(10, 3, 0)), 0);
        assert!(flags(&control, clock(10, 3, 0), 0));
    }
}
