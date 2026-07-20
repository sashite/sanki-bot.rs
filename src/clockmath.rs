//! Clock budget arithmetic — how long the bot may take over a ply.
//!
//! The rule itself lives in the engine (`clock::tick`, driven through the
//! arbiter's replay); this module only answers the *planning* question the
//! engine's step-wise API does not: the **largest elapsed** the mover can
//! afford before the flag falls, across bank rollovers (Time Accounting —
//! Sanki §Period transitions). Pinned against `clock::tick` by an
//! exhaustive boundary test, so it can never quietly diverge from what the
//! arbiter will rule.

use sashite_sanki_engine::domain::time_control::{Clock, TimeControl};

/// The largest `elapsed` (seconds) `tick` accepts from `clock` without
/// flagging: the current bank plus every subsequent period's affordance
/// (bank durations roll over; a quota period ends the roll with its
/// duration + per-ply allowance).
#[must_use]
pub fn max_affordable(tc: &TimeControl, clock: Clock) -> u64 {
    let Some(period) = tc.period(clock.period_index()) else {
        return 0; // a stale index flags immediately — nothing affordable
    };
    let increment = period.increment().map_or(0, |d| d.as_secs());
    if period.plies().is_some() {
        // Quota period: the allowance is available during the ply; overspend
        // never rolls.
        return clock.remaining().as_secs().saturating_add(increment);
    }
    // Bank period: spendable here, then the overspend rolls into the next.
    clock
        .remaining()
        .as_secs()
        .saturating_add(affordable_from_fresh(
            tc,
            clock.period_index().saturating_add(1),
        ))
}

/// The affordance of entering period `index` fresh (bank reset to its
/// duration), rolling further while banks follow.
fn affordable_from_fresh(tc: &TimeControl, index: usize) -> u64 {
    let Some(period) = tc.period(index) else {
        return 0;
    };
    let duration = period.duration().as_secs();
    let increment = period.increment().map_or(0, |d| d.as_secs());
    if period.plies().is_some() {
        duration.saturating_add(increment)
    } else {
        duration.saturating_add(affordable_from_fresh(tc, index.saturating_add(1)))
    }
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
    use sashite_sanki_engine::clock::{tick, Tick};
    use sashite_sanki_engine::domain::time::Duration;
    use sashite_sanki_engine::domain::time_control::Period;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn period(duration: u64, increment: Option<u64>, plies: Option<u32>) -> Period {
        Period::new(secs(duration), increment.map(secs), plies).expect("valid period")
    }

    fn tc(periods: Vec<Period>) -> TimeControl {
        TimeControl::from_periods(periods).expect("valid tc")
    }

    /// The boundary pin: `tick` accepts exactly `max_affordable` and flags one
    /// second past it — across Fischer, plain banks, quota periods, rollover
    /// chains, and mid-quota states.
    #[test]
    fn boundary_agrees_with_the_engine_tick() {
        let cases = [
            (
                tc(vec![period(300, Some(3), None)]),
                Clock::new(secs(300), 0, 0),
            ),
            (
                tc(vec![period(600, None, None)]),
                Clock::new(secs(50), 0, 0),
            ),
            (
                tc(vec![period(0, Some(30), Some(1))]),
                Clock::new(secs(0), 0, 0),
            ),
            (
                tc(vec![period(60, Some(10), Some(3))]),
                Clock::new(secs(25), 0, 1),
            ),
            (
                tc(vec![period(3600, None, None), period(0, Some(30), Some(1))]),
                Clock::new(secs(10), 0, 0),
            ),
            (
                tc(vec![
                    period(100, None, None),
                    period(50, None, None),
                    period(40, Some(5), None),
                ]),
                Clock::new(secs(10), 0, 0),
            ),
            (
                tc(vec![
                    period(5400, Some(30), Some(40)),
                    period(1800, Some(30), None),
                ]),
                Clock::new(secs(1000), 0, 39),
            ),
        ];
        for (control, clock) in cases {
            let afford = max_affordable(&control, clock);
            assert!(
                !matches!(tick(&control, clock, secs(afford)), Tick::Flagged),
                "affordable {afford} must not flag"
            );
            assert!(
                matches!(tick(&control, clock, secs(afford + 1)), Tick::Flagged),
                "affordable {afford} + 1 must flag"
            );
        }
    }

    #[test]
    fn stale_period_affords_nothing() {
        let control = tc(vec![period(300, Some(3), None)]);
        assert_eq!(max_affordable(&control, Clock::new(secs(10), 3, 0)), 0);
    }
}
