//! Persona sampling — presence windows, think times, preference draws
//! (ADR-0014 §4, §6.7). Pure and seedable: every draw flows through the
//! caller's [`SplitMix64`], so a fixed seed reproduces a bot's whole
//! behavior in tests.

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use chrono_tz::Tz;

use crate::config::{ScheduleConfig, ThinkConfig};
use crate::prng::SplitMix64;

/// Parse an IANA timezone name.
#[must_use]
pub fn parse_timezone(name: &str) -> Option<Tz> {
    name.parse::<Tz>().ok()
}

/// Parse a `days` range: a single lowercase day (`sat`) or an inclusive
/// range (`mon-fri`, wrapping allowed: `sat-sun`, `fri-mon`). Returns the
/// set of matching weekdays.
#[must_use]
pub fn parse_days(spec: &str) -> Option<Vec<Weekday>> {
    fn day(token: &str) -> Option<Weekday> {
        match token {
            "mon" => Some(Weekday::Mon),
            "tue" => Some(Weekday::Tue),
            "wed" => Some(Weekday::Wed),
            "thu" => Some(Weekday::Thu),
            "fri" => Some(Weekday::Fri),
            "sat" => Some(Weekday::Sat),
            "sun" => Some(Weekday::Sun),
            _ => None,
        }
    }
    match spec.split_once('-') {
        None => Some(vec![day(spec)?]),
        Some((from, to)) => {
            let from = day(from)?;
            let to = day(to)?;
            let mut days = Vec::new();
            let mut current = from;
            loop {
                days.push(current);
                if current == to {
                    break;
                }
                current = current.succ();
                if days.len() > 7 {
                    return None; // unreachable; defensive
                }
            }
            Some(days)
        }
    }
}

/// Parse `HH:MM` into minutes since local midnight.
#[must_use]
pub fn parse_hhmm(value: &str) -> Option<u32> {
    let (h, m) = value.split_once(':')?;
    let hours: u32 = h.parse().ok()?;
    let minutes: u32 = m.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    hours.checked_mul(60)?.checked_add(minutes)
}

/// A day's per-window jitter and the show-up decision, derived
/// deterministically from `(bot_seed, local date)` — stable for the whole
/// day, different across days (ADR-0014 §4: window edges randomized per
/// day; some evenings the bot just doesn't show up).
fn day_rng(bot_seed: u64, date_ordinal: i32) -> SplitMix64 {
    SplitMix64::new(bot_seed ^ (date_ordinal as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Whether the persona is present at `now` (UTC), per its schedule and
/// per-day jitter/probability. Pure.
#[must_use]
pub fn is_present(schedule: &ScheduleConfig, bot_seed: u64, now: DateTime<Utc>) -> bool {
    let Some(tz) = parse_timezone(&schedule.timezone) else {
        return false;
    };
    let local = now.with_timezone(&tz);
    let date_ordinal = local.date_naive().num_days_from_ce();
    let mut rng = day_rng(bot_seed, date_ordinal);

    // The show-up draw, once per local day.
    if rng.next_unit() >= schedule.presence_probability {
        return false;
    }

    let minute_of_day = local
        .hour()
        .saturating_mul(60)
        .saturating_add(local.minute());
    let weekday = local.weekday();
    let jitter_range = f64::from(schedule.jitter_minutes);

    for window in &schedule.windows {
        let Some(days) = parse_days(&window.days) else {
            continue;
        };
        if !days.contains(&weekday) {
            continue;
        }
        let (Some(from), Some(to)) = (parse_hhmm(&window.from), parse_hhmm(&window.to)) else {
            continue;
        };
        // Per-window daily jitter: each edge shifts by up to ± jitter.
        let from_shift = (rng.next_unit() * 2.0 - 1.0) * jitter_range;
        let to_shift = (rng.next_unit() * 2.0 - 1.0) * jitter_range;
        let from = add_minutes(from, from_shift);
        let to = add_minutes(to, to_shift);
        if from <= minute_of_day && minute_of_day < to {
            return true;
        }
    }
    false
}

/// The next UTC instant at or after `now` when the persona is present, probed
/// minute by minute over the coming `horizon_hours` (persona windows are
/// minute-grained). `None` when no window opens within the horizon (e.g. a
/// low presence-probability streak).
#[must_use]
pub fn next_presence(
    schedule: &ScheduleConfig,
    bot_seed: u64,
    now: DateTime<Utc>,
    horizon_hours: u32,
) -> Option<DateTime<Utc>> {
    let minutes = u64::from(horizon_hours).saturating_mul(60);
    let mut probe = now;
    for _ in 0..minutes {
        if is_present(schedule, bot_seed, probe) {
            return Some(probe);
        }
        probe = probe.checked_add_signed(chrono::Duration::minutes(1))?;
    }
    None
}

fn add_minutes(base: u32, shift: f64) -> u32 {
    let shifted = f64::from(base) + shift;
    if shifted <= 0.0 {
        0
    } else {
        let capped = shifted.min(f64::from(24 * 60));
        // Bounded to a day: the cast is total.
        capped as u32
    }
}

/// Sample a think time in seconds (log-normal, per the persona), clamped to
/// `[1, ceiling]` — the ceiling being the caller's deadline minus its
/// safety margin (§6.5: the simulated reflection is real elapsed time).
#[must_use]
pub fn think_seconds(think: &ThinkConfig, rng: &mut SplitMix64, ceiling: u64) -> u64 {
    let raw = rng.next_lognormal(think.median_s.max(0.1), think.sigma.max(0.0));
    let bounded = raw.clamp(1.0, f64::from(u32::MAX));
    // Total: bounded ∈ [1, u32::MAX].
    (bounded as u64).min(ceiling.max(1))
}

/// Draw a key from a weighted map (variant preferences).
#[must_use]
pub fn draw_weighted_key<'a>(
    weights: &'a std::collections::BTreeMap<String, f64>,
    rng: &mut SplitMix64,
) -> Option<&'a str> {
    let entries: Vec<(&str, f64)> = weights.iter().map(|(k, w)| (k.as_str(), *w)).collect();
    let values: Vec<f64> = entries.iter().map(|(_, w)| *w).collect();
    let index = rng.next_weighted(&values);
    entries.get(index).map(|(k, _)| *k)
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
    use crate::config::WindowConfig;
    use chrono::TimeZone;

    fn schedule(probability: f64, jitter: u32) -> ScheduleConfig {
        ScheduleConfig {
            timezone: "Asia/Tokyo".to_owned(),
            windows: vec![WindowConfig {
                days: "mon-sun".to_owned(),
                from: "20:30".to_owned(),
                to: "23:30".to_owned(),
            }],
            jitter_minutes: jitter,
            presence_probability: probability,
        }
    }

    fn tokyo_evening() -> DateTime<Utc> {
        // 2026-07-20 21:00 Asia/Tokyo == 12:00 UTC.
        Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap()
    }

    fn tokyo_morning() -> DateTime<Utc> {
        // 2026-07-20 09:00 Asia/Tokyo == 00:00 UTC.
        Utc.with_ymd_and_hms(2026, 7, 20, 0, 0, 0).unwrap()
    }

    #[test]
    fn present_inside_the_window_absent_outside() {
        let schedule = schedule(1.0, 0);
        assert!(is_present(&schedule, 7, tokyo_evening()));
        assert!(!is_present(&schedule, 7, tokyo_morning()));
    }

    #[test]
    fn day_seed_makes_presence_deterministic() {
        let schedule = schedule(0.5, 20);
        let now = tokyo_evening();
        assert_eq!(
            is_present(&schedule, 42, now),
            is_present(&schedule, 42, now)
        );
    }

    #[test]
    fn some_days_the_bot_does_not_show_up() {
        let schedule = schedule(0.5, 0);
        let mut present_days = 0;
        let mut absent_days = 0;
        for day in 1..=20 {
            let now = Utc.with_ymd_and_hms(2026, 7, day, 12, 0, 0).unwrap();
            if is_present(&schedule, 42, now) {
                present_days += 1;
            } else {
                absent_days += 1;
            }
        }
        assert!(
            present_days > 0 && absent_days > 0,
            "both outcomes occur across days"
        );
    }

    #[test]
    fn next_presence_finds_the_evening() {
        let schedule = schedule(1.0, 0);
        let from = tokyo_morning();
        let at = next_presence(&schedule, 7, from, 24).unwrap();
        assert!(is_present(&schedule, 7, at));
        assert!(at >= from);
    }

    #[test]
    fn think_time_is_clamped_by_the_ceiling() {
        let think = ThinkConfig {
            median_s: 3.0,
            sigma: 0.6,
        };
        let mut rng = SplitMix64::new(1);
        for _ in 0..100 {
            let sample = think_seconds(&think, &mut rng, 5);
            assert!((1..=5).contains(&sample));
        }
    }

    #[test]
    fn parse_days_handles_ranges_and_wraps() {
        assert_eq!(parse_days("sat").unwrap(), vec![Weekday::Sat]);
        assert_eq!(parse_days("mon-fri").unwrap().len(), 5);
        assert_eq!(parse_days("sat-sun").unwrap().len(), 2);
        assert_eq!(parse_days("fri-mon").unwrap().len(), 4); // wrapping
        assert!(parse_days("lundi").is_none());
    }
}
