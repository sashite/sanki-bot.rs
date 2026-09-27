// SPDX-License-Identifier: Apache-2.0
//! The session's clocks as SEI's `clock` (SEI §8.4 *Clock*; ADR-0045 §3):
//! `own` and `opp` from the natural state's clocks and the session's
//! periods, in milliseconds, **as the clock stands at the emission** of the
//! request.
//!
//! *Time Accounting — Sanki* and SEI's model agree on the shape of a clock
//! — periods in sequence, a bank that rolls its overspend into the next
//! period, a quota period that resets — and differ on one point: Sanki's
//! quota periods reset by discarding the residual (`carry: false`), and its
//! quota allowance is available *during* the ply. The mapping is:
//!
//! | Sanki period | SEI side |
//! |---|---|
//! | bank `[d, i?]` | `remaining` = budget − charged, `inc` = i; `deadline` = everything the mover can still afford, all rollovers included |
//! | quota `[d, i, n]` | `time` = d, `remaining` = budget − charged, `inc` = i, `togo` = n − plies in period, `carry: false`; `deadline` = budget + i − charged |
//! | the periods after | `next`, each `{time, inc, moves?, carry: false}` |
//!
//! A bank already exhausted at the emission has rolled into the next
//! period: the current period is the one the clock is in *then*. When
//! nothing is affordable any more — the mover has flagged — there is no
//! `clock` to send, and no search (ADR-0045 §3): the clock decides that turn.
//!
//! `deadline` is exact; the planning fields are as close as SEI's model
//! allows. In a quota period where the charge has already eaten into the
//! allowance, `remaining` is `0` and `inc` the full allowance, so the two
//! together overstate what is left by the excess — SEI §8.4: an engine that
//! respects `deadline` never loses on time, whatever the planning fields
//! say. `max_affordable` counts whole seconds, so `deadline` is conservative
//! by less than a second.

use sashite_sanki_client::clock::{max_affordable, TimeControl};
use sashite_sanki_client::module::Clock;
use serde_json::{json, Map, Value};

/// A side of the clock, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    /// `own` only: the time left before the mover loses on time.
    pub deadline: Option<u64>,
    /// The budget left in the current period.
    pub remaining: u64,
    /// The increment of the current period.
    pub inc: u64,
    /// Moves left in the current period, for a quota period.
    pub togo: Option<u64>,
    /// The nominal duration of a quota period.
    pub time: Option<u64>,
    /// The periods that follow.
    pub next: Vec<Period>,
}

/// A period of `next`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Period {
    /// Its budget when it begins.
    pub time: u64,
    /// Its increment.
    pub inc: u64,
    /// Its quota, for a quota period.
    pub moves: Option<u64>,
}

impl Side {
    /// The side as SEI writes it.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        if let Some(deadline) = self.deadline {
            object.insert("deadline".to_owned(), json!(deadline));
        }
        object.insert("remaining".to_owned(), json!(self.remaining));
        if self.inc > 0 {
            object.insert("inc".to_owned(), json!(self.inc));
        }
        if let (Some(togo), Some(time)) = (self.togo, self.time) {
            object.insert("togo".to_owned(), json!(togo));
            object.insert("time".to_owned(), json!(time));
            object.insert("carry".to_owned(), json!(false));
        }
        if !self.next.is_empty() {
            let next: Vec<Value> = self
                .next
                .iter()
                .map(|period| {
                    let mut p = Map::new();
                    p.insert("time".to_owned(), json!(period.time));
                    if period.inc > 0 {
                        p.insert("inc".to_owned(), json!(period.inc));
                    }
                    if let Some(moves) = period.moves {
                        p.insert("moves".to_owned(), json!(moves));
                        p.insert("carry".to_owned(), json!(false));
                    }
                    Value::Object(p)
                })
                .collect();
            object.insert("next".to_owned(), Value::Array(next));
        }
        Value::Object(object)
    }
}

/// Seconds to milliseconds, saturating.
const fn ms(seconds: u64) -> u64 {
    seconds.saturating_mul(1000)
}

/// The side of a player whose clock is `clock` at the anchor, `charged_ms`
/// after it (`0` for the player not on move), under the periods `tc`.
/// `None` when the player has flagged: nothing is affordable any more.
#[must_use]
pub fn side(tc: &TimeControl, clock: Clock, charged_ms: u64, own: bool) -> Option<Side> {
    let affordable = ms(max_affordable(tc, clock));
    if affordable <= charged_ms && (own || affordable == 0) {
        return None;
    }
    let deadline = own.then(|| affordable.saturating_sub(charged_ms));

    // Roll an exhausted bank into the next period, as the module will.
    let mut index = usize::try_from(clock.period).ok()?;
    let mut remaining = ms(clock.remaining);
    let mut plies_in_period = u64::from(clock.plies_in_period);
    let mut charged = charged_ms;
    loop {
        let [duration, increment, plies] = tc.get(index)?;
        let increment = ms(increment.unwrap_or(0));
        if plies.is_none() && charged > remaining {
            charged = charged.saturating_sub(remaining);
            index = index.saturating_add(1);
            let [next_duration, _, _] = tc.get(index)?;
            remaining = ms(next_duration.unwrap_or(0));
            plies_in_period = 0;
            continue;
        }
        let quota = plies.map(|quota| {
            let quota = quota.max(1);
            (quota, quota.saturating_sub(plies_in_period).max(1))
        });
        let next = tc
            .iter()
            .skip(index.saturating_add(1))
            .map(|[d, i, n]| Period {
                time: ms(d.unwrap_or(0)),
                inc: ms(i.unwrap_or(0)),
                moves: n.map(|n| n.max(1)),
            })
            .collect();
        return Some(Side {
            deadline,
            remaining: remaining.saturating_sub(charged),
            inc: increment,
            togo: quota.map(|(_, togo)| togo),
            time: quota.map(|_| ms(duration.unwrap_or(0))),
            next,
        });
    }
}

/// The `clock` of a `search`: `own` for the mover, charged `charged_ms`
/// since the anchor, `opp` for the other player, and `overhead` (ADR-0045
/// §3: `margin_ms + engine_rtt`). `None` when the mover has flagged.
#[must_use]
pub fn search_clock(
    tc: &TimeControl,
    own: Clock,
    opp: Clock,
    charged_ms: u64,
    overhead_ms: u64,
) -> Option<Value> {
    let own = side(tc, own, charged_ms, true)?;
    let mut object = Map::new();
    object.insert("own".to_owned(), own.to_json());
    if let Some(opp) = side(tc, opp, 0, false) {
        object.insert("opp".to_owned(), opp.to_json());
    }
    object.insert("overhead".to_owned(), json!(overhead_ms));
    Some(Value::Object(object))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    const fn clock(period: u32, plies: u32, remaining: u64) -> Clock {
        Clock {
            period,
            plies_in_period: plies,
            remaining,
        }
    }

    #[test]
    fn fischer() {
        // 5 + 3, 10 s already charged.
        let tc = [[Some(300), Some(3), None]];
        let s = side(&tc, clock(0, 4, 293), 10_000, true).unwrap();
        assert_eq!(s.deadline, Some(283_000));
        assert_eq!(s.remaining, 283_000);
        assert_eq!(s.inc, 3000);
        assert_eq!(s.togo, None);
        assert!(s.next.is_empty());
        assert_eq!(
            s.to_json(),
            json!({"deadline": 283000, "remaining": 283000, "inc": 3000})
        );
        // The opponent: no deadline, nothing charged.
        let o = side(&tc, clock(0, 4, 120), 0, false).unwrap();
        assert_eq!(o.deadline, None);
        assert_eq!(o.remaining, 120_000);
        // Flagged.
        assert!(side(&tc, clock(0, 4, 5), 5_000, true).is_none());
        assert!(side(&tc, clock(0, 4, 5), 4_999, true).is_some());
    }

    #[test]
    fn byoyomi() {
        // 10 s per move, 3 s charged: SEI's byōyomi row.
        let tc = [[Some(0), Some(10), Some(1)]];
        let s = side(&tc, clock(0, 0, 0), 3_000, true).unwrap();
        assert_eq!(
            s.to_json(),
            json!({"deadline": 7000, "remaining": 0, "inc": 10000, "togo": 1, "time": 0, "carry": false})
        );
        assert!(side(&tc, clock(0, 0, 0), 10_000, true).is_none());
    }

    #[test]
    fn main_bank_then_overtime() {
        // 3600 s bank, then 30 s per move (kind 3420 example 5).
        let tc = [[Some(3600), None, None], [Some(0), Some(30), Some(1)]];
        let s = side(&tc, clock(0, 12, 10), 0, true).unwrap();
        // Deadline: the bank plus the first overtime allowance.
        assert_eq!(s.deadline, Some(40_000));
        assert_eq!(s.remaining, 10_000);
        assert_eq!(
            s.next,
            vec![Period {
                time: 0,
                inc: 30_000,
                moves: Some(1)
            }]
        );
        assert_eq!(
            s.to_json()["next"],
            json!([{"time": 0, "inc": 30000, "moves": 1, "carry": false}])
        );
        // 25 s charged: the bank is exhausted, the clock is in the overtime
        // with 15 s of overspend charged.
        let rolled = side(&tc, clock(0, 12, 10), 25_000, true).unwrap();
        assert_eq!(rolled.deadline, Some(15_000));
        assert_eq!(rolled.togo, Some(1));
        assert_eq!(rolled.time, Some(0));
        assert_eq!(rolled.inc, 30_000);
        assert_eq!(rolled.remaining, 0);
        assert!(rolled.next.is_empty());
        // 50 s charged: flagged.
        assert!(side(&tc, clock(0, 12, 10), 50_000, true).is_none());
    }

    #[test]
    fn classical_two_periods() {
        // 40 moves in 90 min + 30 s, then 30 min + 30 s.
        let tc = [
            [Some(5400), Some(30), Some(40)],
            [Some(1800), Some(30), None],
        ];
        let s = side(&tc, clock(0, 37, 1000), 5_000, true).unwrap();
        assert_eq!(s.togo, Some(3));
        assert_eq!(s.time, Some(5_400_000));
        assert_eq!(s.remaining, 995_000);
        assert_eq!(s.deadline, Some(1_025_000)); // 1000 + 30 − 5
        assert_eq!(s.next.len(), 1);
        assert_eq!(s.next[0].moves, None);
        // In the second period: plain Fischer.
        let second = side(&tc, clock(1, 3, 1700), 0, true).unwrap();
        assert_eq!(second.togo, None);
        assert_eq!(second.deadline, Some(1_700_000));
    }

    #[test]
    fn the_search_clock() {
        let tc = [[Some(180), Some(2), None]];
        let value = search_clock(&tc, clock(0, 0, 180), clock(0, 0, 180), 1_500, 400).unwrap();
        assert_eq!(value["own"]["deadline"], 178_500);
        assert_eq!(value["opp"]["remaining"], 180_000);
        assert!(value["opp"].get("deadline").is_none());
        assert_eq!(value["overhead"], 400);
        assert!(search_clock(&tc, clock(0, 0, 1), clock(0, 0, 180), 1_000, 400).is_none());
    }
}
