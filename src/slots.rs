//! The cadence slots — the per-(bot, cadence) automaton of ADR-0040 §3 that
//! every founding of the bot is admitted through (ADR-0014 §8 as amended by
//! ADR-0039 §6 and ADR-0040).
//!
//! A family's cap (`[bot.play.max_concurrent]`) is a number of **holds**. A
//! hold is one of three states, each with a way out:
//!
//! ```text
//! Free              → Committed(until)       the bot commits: its own pool entry, or its acceptance
//! Committed(until)  → Committed(deadline)    a Pairing on the entry: `until` becomes its founding deadline
//! Committed(until)  → Playing(g)             the Game Session is tracked
//! Committed(until)  → Free                   `until` passed — nothing founded
//! Playing(g)        → Cooling(g, t_end + W)  a conforming Conclusion of g, canonical at t_end
//! Cooling(g, until) → Playing(g′)            g′ is a rematch of g, whoever proposed it
//! Cooling(g, until) → Free                   `until` passed
//! ```
//!
//! A founding at cadence `c` is admitted iff the family has a free hold
//! (`load < cap`), or the founding is a **rematch of `g`** and the family
//! holds `Cooling(g)`. Nothing else is admitted, so the bot never plays two
//! games at one cadence beyond its cap, and the slot stays with the pair for
//! `W` after their game — long enough for the Rematch button — and with
//! nobody else. `Committed` and `Cooling` expire on their own; `Playing` ends
//! with the game; no state can leak.
//!
//! Pure over ids and instants; the actor drives it and holds it behind a
//! mutex so the publish tasks can release a commitment they failed to make.

use nostr_sdk::prelude::EventId;

use crate::cadence::Cadence;

/// Why a founding is not admitted.
pub type Refusal = &'static str;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Hold {
    /// The bot has committed to a founding that is not tracked yet: its own
    /// pool entry (keyed by the entry it mirrors; `pool`), or its acceptance
    /// of a Direct Challenge (keyed by the challenge).
    Committed {
        cadence: Cadence,
        key: EventId,
        until: u64,
        pool: bool,
    },
    /// A tracked session.
    Playing { cadence: Cadence, session: EventId },
    /// A concluded session whose rematch is still possible: the hold stays
    /// with the pair until `until`.
    Cooling {
        cadence: Cadence,
        session: EventId,
        until: u64,
    },
}

impl Hold {
    const fn cadence(&self) -> Cadence {
        match self {
            Hold::Committed { cadence, .. }
            | Hold::Playing { cadence, .. }
            | Hold::Cooling { cadence, .. } => *cadence,
        }
    }

    /// Whether the hold has run out by itself at `now`.
    const fn expired(&self, now: u64) -> bool {
        match self {
            Hold::Committed { until, .. } | Hold::Cooling { until, .. } => *until <= now,
            Hold::Playing { .. } => false,
        }
    }
}

/// One bot's holds, every cadence together.
#[derive(Debug, Default)]
pub struct Slots {
    holds: Vec<Hold>,
}

impl Slots {
    /// Drop every hold that has run out at `now`.
    pub fn expire(&mut self, now: u64) {
        self.holds.retain(|hold| !hold.expired(now));
    }

    /// The family's load at `now`: its holds, expired ones dropped.
    pub fn load(&mut self, cadence: Cadence, now: u64) -> u32 {
        self.expire(now);
        self.holds
            .iter()
            .filter(|hold| hold.cadence() == cadence)
            .fold(0_u32, |n, _| n.saturating_add(1))
    }

    /// Whether the bot's own pool entry is still live in the family (one own
    /// entry per cadence at a time — ADR-0039 §7).
    pub fn pool_committed(&mut self, cadence: Cadence, now: u64) -> bool {
        self.expire(now);
        self.holds.iter().any(
            |hold| matches!(hold, Hold::Committed { cadence: c, pool: true, .. } if *c == cadence),
        )
    }

    /// Whether the family holds `Cooling(session)`.
    pub fn is_cooling(&mut self, session: &EventId, now: u64) -> bool {
        self.expire(now);
        self.holds
            .iter()
            .any(|hold| matches!(hold, Hold::Cooling { session: s, .. } if s == session))
    }

    /// Admit a fresh founding (a pool entry, a fresh Direct Challenge) at
    /// `cadence` under `cap`.
    pub fn admit_fresh(&mut self, cadence: Cadence, cap: u32, now: u64) -> Result<(), Refusal> {
        if self.load(cadence, now) < cap {
            Ok(())
        } else {
            Err("cap reached for this cadence")
        }
    }

    /// Admit a rematch of `concluded` at `cadence` under `cap`: through the
    /// cooling hold the concluded game left, or — once that has run out — as
    /// a fresh founding.
    pub fn admit_rematch(
        &mut self,
        cadence: Cadence,
        concluded: &EventId,
        cap: u32,
        now: u64,
    ) -> Result<(), Refusal> {
        if self.is_cooling(concluded, now) {
            Ok(())
        } else {
            self.admit_fresh(cadence, cap, now)
        }
    }

    /// Commit: the bot is about to publish its own pool entry (`pool`, keyed
    /// by the entry it mirrors) or its acceptance (keyed by the challenge),
    /// good until `until`. Re-committing a key refreshes it.
    pub fn commit(&mut self, cadence: Cadence, key: EventId, until: u64, pool: bool) {
        self.release(&key);
        self.holds.push(Hold::Committed {
            cadence,
            key,
            until,
            pool,
        });
    }

    /// Re-key a commitment: the bot's own pool entry, committed under the
    /// entry it mirrored before it existed, is keyed by its own id once
    /// published — the id any Pairing that concerns it references, whoever
    /// the matchmaker paired it with.
    pub fn rekey(&mut self, from: &EventId, to: EventId) {
        for hold in &mut self.holds {
            if let Hold::Committed { key, .. } = hold {
                if key == from {
                    *key = to;
                }
            }
        }
    }

    /// Release a commitment (its publish failed).
    pub fn release(&mut self, key: &EventId) {
        self.holds
            .retain(|hold| !matches!(hold, Hold::Committed { key: k, .. } if k == key));
    }

    /// A Pairing was observed on one of `keys` (the two entries it pairs):
    /// the commitment now lasts until the Pairing's founding deadline.
    pub fn extend_commitment(&mut self, keys: &[EventId], deadline: u64) {
        for hold in &mut self.holds {
            if let Hold::Committed { key, until, .. } = hold {
                if keys.contains(key) {
                    *until = (*until).max(deadline);
                }
            }
        }
    }

    /// A Game Session is tracked: the commitments under `keys` (the founding
    /// and, for a Pairing, the entries it pairs) and — for a rematch — the
    /// cooling hold of the concluded session it renews are consumed by
    /// `Playing(session)`.
    pub fn play(
        &mut self,
        cadence: Cadence,
        session: EventId,
        keys: &[EventId],
        rematch_of: Option<&EventId>,
    ) {
        self.holds.retain(|hold| match hold {
            Hold::Committed { key, .. } => !keys.contains(key),
            Hold::Playing { session: s, .. } => *s != session,
            Hold::Cooling { session: s, .. } => rematch_of != Some(s),
        });
        self.holds.push(Hold::Playing { cadence, session });
    }

    /// A tracked session is dropped without a Conclusion (superseded by the
    /// canonical Game Session of its slot).
    pub fn drop_session(&mut self, session: &EventId) {
        self.holds
            .retain(|hold| !matches!(hold, Hold::Playing { session: s, .. } if s == session));
    }

    /// The session concluded, canonically at `t_end`: its hold stays with the
    /// pair until `until` (`t_end + W`), open to a rematch of it only.
    pub fn cool(&mut self, session: &EventId, until: u64) {
        let Some(index) = self
            .holds
            .iter()
            .position(|hold| matches!(hold, Hold::Playing { session: s, .. } if s == session))
        else {
            return;
        };
        let Some(Hold::Playing { cadence, .. }) = self.holds.get(index).cloned() else {
            return;
        };
        self.holds.swap_remove(index);
        self.holds.push(Hold::Cooling {
            cadence,
            session: *session,
            until,
        });
    }

    /// A rematch challenge of `session` is live until `until` (theirs
    /// observed, or ours published): the cooling hold lasts at least as long,
    /// so a challenge published in the window's last second is honoured for
    /// its whole life.
    pub fn extend_cooling(&mut self, session: &EventId, until: u64) {
        for hold in &mut self.holds {
            if let Hold::Cooling {
                session: s,
                until: u,
                ..
            } = hold
            {
                if s == session {
                    *u = (*u).max(until);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn id(n: u8) -> EventId {
        EventId::from_slice(&[n; 32]).unwrap()
    }

    const W: u64 = 60;

    #[test]
    fn a_pool_courtship_holds_the_family_until_the_game_is_tracked() {
        let mut slots = Slots::default();
        let (entry, session) = (id(1), id(2));
        assert!(slots.admit_fresh(Cadence::Blitz, 1, 100).is_ok());
        slots.commit(Cadence::Blitz, entry, 100 + 180, true);
        // One own entry per cadence; the cap is reached; another family is free.
        assert!(slots.pool_committed(Cadence::Blitz, 101));
        assert_eq!(
            slots.admit_fresh(Cadence::Blitz, 1, 101),
            Err("cap reached for this cadence")
        );
        assert!(slots.admit_fresh(Cadence::Rapid, 1, 101).is_ok());
        // A Pairing lands in the entry's last second: the hold outlives it.
        slots.extend_commitment(&[entry], 100 + 180 + 60);
        assert_eq!(slots.load(Cadence::Blitz, 100 + 181), 1);
        // The session is tracked: Committed → Playing.
        slots.play(Cadence::Blitz, session, &[entry], None);
        assert!(!slots.pool_committed(Cadence::Blitz, 500));
        assert_eq!(slots.load(Cadence::Blitz, 10_000), 1);
    }

    #[test]
    fn a_pool_commitment_is_rekeyed_to_the_published_entry() {
        let mut slots = Slots::default();
        let (mirrored, ours, stranger, session) = (id(1), id(2), id(3), id(4));
        slots.commit(Cadence::Blitz, mirrored, 1_000, true);
        slots.rekey(&mirrored, ours);
        // The matchmaker paired OUR entry with a stranger's, not the one we
        // mirrored: the Pairing still names ours, and the hold follows it.
        slots.extend_commitment(&[stranger, ours], 2_000);
        assert_eq!(slots.load(Cadence::Blitz, 1_500), 1);
        slots.play(Cadence::Blitz, session, &[id(9), stranger, ours], None);
        assert!(!slots.pool_committed(Cadence::Blitz, 1_500));
        assert_eq!(slots.load(Cadence::Blitz, 1_500), 1);
    }

    #[test]
    fn a_commitment_expires_and_a_failed_publish_releases_it() {
        let mut slots = Slots::default();
        slots.commit(Cadence::Byoyomi, id(1), 200, false);
        assert_eq!(slots.load(Cadence::Byoyomi, 199), 1);
        assert_eq!(slots.load(Cadence::Byoyomi, 200), 0);
        slots.commit(Cadence::Byoyomi, id(3), 400, false);
        slots.release(&id(3));
        assert_eq!(slots.load(Cadence::Byoyomi, 201), 0);
    }

    #[test]
    fn cooling_admits_only_the_rematch_then_frees() {
        let mut slots = Slots::default();
        let (g, g2, other) = (id(1), id(2), id(9));
        slots.play(Cadence::Rapid, g, &[], None);
        slots.cool(&g, 1_000 + W);
        assert!(slots.is_cooling(&g, 1_000));
        // A stranger is refused for a minute; the rematch of g is admitted.
        assert_eq!(
            slots.admit_fresh(Cadence::Rapid, 1, 1_030),
            Err("cap reached for this cadence")
        );
        assert_eq!(
            slots.admit_rematch(Cadence::Rapid, &other, 1, 1_030),
            Err("cap reached for this cadence")
        );
        assert!(slots.admit_rematch(Cadence::Rapid, &g, 1, 1_030).is_ok());
        // The rematch is accepted: Committed beside the Cooling, both
        // consumed by the tracked session.
        slots.commit(Cadence::Rapid, id(5), 1_030 + 60, false);
        assert_eq!(slots.load(Cadence::Rapid, 1_031), 2);
        slots.play(Cadence::Rapid, g2, &[id(5)], Some(&g));
        assert_eq!(slots.load(Cadence::Rapid, 1_040), 1);
        assert!(!slots.is_cooling(&g, 1_040));
        // Without a rematch, the minute passes and the family is free.
        slots.cool(&g2, 2_000 + W);
        assert!(slots.admit_fresh(Cadence::Rapid, 1, 2_000 + W).is_ok());
        assert!(slots
            .admit_rematch(Cadence::Rapid, &g2, 1, 2_000 + W)
            .is_ok()); // as fresh
    }

    #[test]
    fn a_late_rematch_challenge_extends_the_cooling() {
        let mut slots = Slots::default();
        let g = id(1);
        slots.play(Cadence::Blitz, g, &[], None);
        slots.cool(&g, 1_000 + W);
        // Their challenge, published at t_end + 59, lives until t_end + 119.
        slots.extend_cooling(&g, 1_119);
        assert!(slots.is_cooling(&g, 1_100));
        assert_eq!(
            slots.admit_fresh(Cadence::Blitz, 1, 1_100),
            Err("cap reached for this cadence")
        );
        assert!(!slots.is_cooling(&g, 1_119));
    }

    #[test]
    fn a_superseded_session_is_dropped_and_cooling_an_unknown_session_is_a_no_op() {
        let mut slots = Slots::default();
        slots.play(Cadence::Correspondence, id(1), &[], None);
        slots.drop_session(&id(1));
        assert_eq!(slots.load(Cadence::Correspondence, 0), 0);
        slots.cool(&id(7), 100);
        assert_eq!(slots.load(Cadence::Correspondence, 0), 0);
    }

    #[test]
    fn caps_above_one_count_holds() {
        let mut slots = Slots::default();
        for n in 1..=4 {
            assert!(slots.admit_fresh(Cadence::Correspondence, 4, 0).is_ok());
            slots.play(Cadence::Correspondence, id(n), &[], None);
        }
        assert!(slots.admit_fresh(Cadence::Correspondence, 4, 0).is_err());
        slots.cool(&id(2), W);
        // Cooling still holds; the rematch of 2 goes through it.
        assert!(slots.admit_fresh(Cadence::Correspondence, 4, 10).is_err());
        assert!(slots
            .admit_rematch(Cadence::Correspondence, &id(2), 4, 10)
            .is_ok());
        assert!(slots.admit_fresh(Cadence::Correspondence, 4, W).is_ok());
    }
}
