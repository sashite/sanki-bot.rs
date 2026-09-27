// SPDX-License-Identifier: Apache-2.0
//! Outgoing challenges (ADR-0045 §5 *Sending one*): with
//! `[challenges.outgoing]`, the bot challenges its targets so that two bots
//! meet without a person. The recipient founds the session; the bot only
//! sends and waits.
//!
//! A target is **due** when: no session between the bot and the target is
//! open; no challenge of the bot to the target is pending; no challenge
//! from the target to the bot is pending; the last challenge to the target
//! is older than `every_secs`; fewer than `max_per_day` challenges were
//! sent in the last 24 h; and a slot is free in the time control's family.
//! The bot walks its targets in a fixed order and sends to the first due
//! one, at most one challenge a minute, at an instant **jittered** within
//! the minute by `HMAC(k_jitter, target ‖ hour)`, so that two bots
//! targeting each other do not fire in phase.
//!
//! Pure: the runtime feeds it what it knows and fires when told.

use nostr_sdk::prelude::*;

use crate::config::Outgoing;

/// One day, in seconds.
const DAY: u64 = 86_400;

/// What the runtime knows about a target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TargetState {
    /// A session between the bot and the target is open.
    pub session_open: bool,
    /// A challenge of the bot to the target is pending (its `accept_until`
    /// plus the relay's tolerance not passed, or a query still owed).
    pub pending_to: bool,
    /// A challenge from the target to the bot is pending.
    pub pending_from: bool,
    /// When the bot last challenged the target, in relay seconds.
    pub last_sent: Option<u64>,
}

/// What the runtime knows when it considers sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendingState {
    /// The targets' states, in the configured order.
    pub targets: Vec<(PublicKey, TargetState)>,
    /// Challenges sent in the last 24 h, to any target.
    pub sent_last_day: u32,
    /// When the bot last sent a challenge to anyone.
    pub last_sent_any: Option<u64>,
    /// Whether a slot is free in the outgoing time control's family.
    pub slot_free: bool,
}

/// Whether `target` is due at `now`.
#[must_use]
pub fn is_due(outgoing: &Outgoing, state: &TargetState, now: u64) -> bool {
    !state.session_open
        && !state.pending_to
        && !state.pending_from
        && state
            .last_sent
            .is_none_or(|last| now >= last.saturating_add(outgoing.every_secs))
}

/// The first due target, in the configured order — or none: no slot, the
/// day's quota spent, a challenge sent less than a minute ago, or no
/// target due.
#[must_use]
pub fn next_due(outgoing: &Outgoing, state: &SendingState, now: u64) -> Option<PublicKey> {
    if !state.slot_free || state.sent_last_day >= outgoing.max_per_day {
        return None;
    }
    if state
        .last_sent_any
        .is_some_and(|last| now < last.saturating_add(60))
    {
        return None;
    }
    state
        .targets
        .iter()
        .find(|(_, target)| is_due(outgoing, target, now))
        .map(|(pubkey, _)| *pubkey)
}

/// The instant to fire at for `target`: the next instant, at or after
/// `now`, whose second within its minute is `jitter(target, hour)`. The
/// caller computes `jitter` with `Identity::jitter_secs(target, now / 3600)`.
#[must_use]
pub fn fire_at(now: u64, jitter_secs: u64) -> u64 {
    let minute = now.checked_div(60).unwrap_or(0).saturating_mul(60);
    let this = minute.saturating_add(jitter_secs.min(59));
    if this >= now {
        this
    } else {
        this.saturating_add(60)
    }
}

/// The instant, `DAY` back from `now`, from which challenges count toward
/// `max_per_day`.
#[must_use]
pub const fn day_since(now: u64) -> u64 {
    now.saturating_sub(DAY)
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
    use crate::config::{NonEmpty, Variant};
    use sashite_sanki_client::cadence::Cadence;

    fn outgoing(targets: Vec<PublicKey>) -> Outgoing {
        Outgoing {
            targets: NonEmpty::test(targets),
            time_control: vec![[Some(180), Some(2), None]],
            cadence: Cadence::Blitz,
            variant: Variant::Chess,
            every_secs: 600,
            accept_secs: 120,
            max_per_day: 3,
        }
    }

    #[test]
    fn the_first_due_target_in_order() {
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let o = outgoing(vec![a, b]);
        let idle = TargetState::default();
        let state = SendingState {
            targets: vec![
                (
                    a,
                    TargetState {
                        session_open: true,
                        ..idle
                    },
                ),
                (b, idle),
            ],
            sent_last_day: 0,
            last_sent_any: None,
            slot_free: true,
        };
        assert_eq!(next_due(&o, &state, 10_000), Some(b));
        // Pending either way, or too recent: not due.
        for busy in [
            TargetState {
                pending_to: true,
                ..idle
            },
            TargetState {
                pending_from: true,
                ..idle
            },
            TargetState {
                last_sent: Some(9_500),
                ..idle
            },
        ] {
            assert!(!is_due(&o, &busy, 10_000));
        }
        assert!(is_due(
            &o,
            &TargetState {
                last_sent: Some(9_400),
                ..idle
            },
            10_000
        ));
    }

    #[test]
    fn the_global_gates() {
        let a = Keys::generate().public_key();
        let o = outgoing(vec![a]);
        let ready = SendingState {
            targets: vec![(a, TargetState::default())],
            sent_last_day: 0,
            last_sent_any: None,
            slot_free: true,
        };
        assert_eq!(next_due(&o, &ready, 10_000), Some(a));
        let no_slot = SendingState {
            slot_free: false,
            ..ready.clone()
        };
        assert_eq!(next_due(&o, &no_slot, 10_000), None);
        let quota = SendingState {
            sent_last_day: 3,
            ..ready.clone()
        };
        assert_eq!(next_due(&o, &quota, 10_000), None);
        let recent = SendingState {
            last_sent_any: Some(9_970),
            ..ready.clone()
        };
        assert_eq!(next_due(&o, &recent, 10_000), None);
        let minute_ago = SendingState {
            last_sent_any: Some(9_940),
            ..ready
        };
        assert_eq!(next_due(&o, &minute_ago, 10_000), Some(a));
    }

    #[test]
    fn the_firing_instant_is_jittered_within_the_minute() {
        assert_eq!(fire_at(10_000, 40), 10_000); // 10 000 = 166 × 60 + 40
        assert_eq!(fire_at(10_001, 40), 10_060);
        assert_eq!(fire_at(10_000, 0), 10_020); // 9 960 has passed
        assert_eq!(fire_at(10_000, 59), 10_019);
        assert_eq!(fire_at(10_000, 99), 10_019, "clamped to the minute");
        assert_eq!(day_since(100_000), 13_600);
    }
}
