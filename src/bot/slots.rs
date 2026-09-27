// SPDX-License-Identifier: Apache-2.0
//! The slots (ADR-0045 §4 `[play.max_concurrent]`, §5): per family, the
//! games open plus the reservations held. A reservation is taken when a
//! Direct Challenge passes the local checks (released unless consumed by
//! the founding, with `not_after = accept_until`), or when the bot sends
//! one (held until the runtime confirms that no session answers it). Two
//! challenges can never claim one slot.
//!
//! A founded session always takes its place, one over the cap if need be
//! (a session answering a lapsed challenge, a person founding with the
//! key): the bot cannot leave a founded session to the clock.
//!
//! Rule 8: an incoming challenge from a target the bot has challenged
//! **consumes** the bot's own reservation — the slot is the incoming
//! challenge's now, and the outgoing challenge is left to lapse. It still
//! holds no slot but is still *pending*: no other challenge goes to that
//! target until the query of §5 confirms that no session answers it.

use std::collections::BTreeMap;

use nostr_sdk::prelude::*;
use sashite_sanki_client::cadence::Cadence;

use crate::admit::LocalState;

/// A slot held for a challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    /// The family.
    pub cadence: Cadence,
    /// The other player.
    pub peer: PublicKey,
    /// The bot sent the challenge (else it received it).
    pub outgoing: bool,
    /// When the challenge lapses: its `accept_until`.
    pub accept_until: u64,
    /// An outgoing reservation consumed by the target's own challenge
    /// (rule 8): holds no slot, still pending.
    pub consumed: bool,
}

/// The slots of a bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    caps: BTreeMap<Cadence, u32>,
    /// Open sessions: their family and the opponent.
    open: BTreeMap<EventId, (Cadence, PublicKey)>,
    /// Reservations, by challenge id.
    reservations: BTreeMap<EventId, Reservation>,
}

impl Slots {
    /// Slots under `caps` (a family absent has a cap of zero).
    #[must_use]
    pub const fn new(caps: BTreeMap<Cadence, u32>) -> Self {
        Self {
            caps,
            open: BTreeMap::new(),
            reservations: BTreeMap::new(),
        }
    }

    /// The family's cap.
    #[must_use]
    pub fn cap(&self, cadence: Cadence) -> u32 {
        self.caps.get(&cadence).copied().unwrap_or(0)
    }

    /// The games open plus the reservations held, in the family.
    #[must_use]
    pub fn held(&self, cadence: Cadence) -> u32 {
        let open = self.open.values().filter(|(c, _)| *c == cadence).count();
        let reserved = self
            .reservations
            .values()
            .filter(|r| r.cadence == cadence && !r.consumed)
            .count();
        u32::try_from(open.saturating_add(reserved)).unwrap_or(u32::MAX)
    }

    /// Whether a slot is free in the family.
    #[must_use]
    pub fn free(&self, cadence: Cadence) -> bool {
        self.held(cadence) < self.cap(cadence)
    }

    /// What [`crate::admit::admit`] needs, for a challenge from `challenger`.
    #[must_use]
    pub fn local_state(&self, halted: bool, challenger: &PublicKey) -> LocalState {
        let held = Cadence::ALL
            .into_iter()
            .map(|c| (c, self.held(c)))
            .collect();
        let held_for_challenger = self
            .reservations
            .values()
            .find(|r| r.outgoing && r.peer == *challenger && !r.consumed)
            .map(|r| r.cadence);
        LocalState {
            halted,
            held,
            held_for_challenger,
        }
    }

    /// Takes a reservation for `challenge`; `false` when one is held for it
    /// already.
    pub fn reserve(&mut self, challenge: EventId, reservation: Reservation) -> bool {
        if self.reservations.contains_key(&challenge) {
            return false;
        }
        self.reservations.insert(challenge, reservation);
        true
    }

    /// Releases the reservation of `challenge`, if any.
    pub fn release(&mut self, challenge: &EventId) -> Option<Reservation> {
        self.reservations.remove(challenge)
    }

    /// Rule 8: the bot's outgoing reservation to `peer` is consumed by the
    /// peer's own challenge — it holds no slot from now on, and stays
    /// pending until released. Whether one was consumed.
    pub fn consume_outgoing(&mut self, peer: &PublicKey) -> bool {
        match self
            .reservations
            .values_mut()
            .find(|r| r.outgoing && r.peer == *peer && !r.consumed)
        {
            Some(reservation) => {
                reservation.consumed = true;
                true
            }
            None => false,
        }
    }

    /// The reservation of `challenge`.
    #[must_use]
    pub fn reservation(&self, challenge: &EventId) -> Option<&Reservation> {
        self.reservations.get(challenge)
    }

    /// The reservations, by challenge id.
    pub fn reservations(&self) -> impl Iterator<Item = (&EventId, &Reservation)> {
        self.reservations.iter()
    }

    /// The bot's own pending challenge to `peer`, if any.
    #[must_use]
    pub fn pending_to(&self, peer: &PublicKey) -> Option<EventId> {
        self.reservations
            .iter()
            .find(|(_, r)| r.outgoing && r.peer == *peer)
            .map(|(id, _)| *id)
    }

    /// A pending challenge from `peer`, if any.
    #[must_use]
    pub fn pending_from(&self, peer: &PublicKey) -> Option<EventId> {
        self.reservations
            .iter()
            .find(|(_, r)| !r.outgoing && r.peer == *peer)
            .map(|(id, _)| *id)
    }

    /// The incoming reservations lapsed at `now`: `accept_until` passed by
    /// more than the relay's past tolerance (a session stamped at
    /// `accept_until` is accepted until then). Outgoing reservations are the
    /// runtime's to release, after the query §5 prescribes.
    pub fn lapse_incoming(&mut self, now: u64, past_tolerance: u64) -> Vec<EventId> {
        let lapsed: Vec<EventId> = self
            .reservations
            .iter()
            .filter(|(_, r)| !r.outgoing && now > r.accept_until.saturating_add(past_tolerance))
            .map(|(id, _)| *id)
            .collect();
        for id in &lapsed {
            self.reservations.remove(id);
        }
        lapsed
    }

    /// A session opened on `challenge` (its reservation consumed, if any),
    /// or on no challenge of the bot's.
    pub fn open_session(
        &mut self,
        session: EventId,
        cadence: Cadence,
        opponent: PublicKey,
        challenge: Option<&EventId>,
    ) {
        if let Some(challenge) = challenge {
            self.reservations.remove(challenge);
        }
        self.open.insert(session, (cadence, opponent));
    }

    /// The session closed.
    pub fn close_session(&mut self, session: &EventId) -> Option<(Cadence, PublicKey)> {
        self.open.remove(session)
    }

    /// Whether a session with `peer` is open.
    #[must_use]
    pub fn session_open_with(&self, peer: &PublicKey) -> bool {
        self.open.values().any(|(_, p)| p == peer)
    }

    /// Whether `session` is open.
    #[must_use]
    pub fn is_open(&self, session: &EventId) -> bool {
        self.open.contains_key(session)
    }

    /// The open sessions.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.open.len()
    }
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

    fn id(n: u8) -> EventId {
        EventId::from_byte_array([n; 32])
    }

    #[test]
    fn reservations_and_sessions_hold_the_family() {
        let mut slots = Slots::new([(Cadence::Blitz, 1), (Cadence::Rapid, 2)].into());
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        assert!(slots.free(Cadence::Blitz));
        assert!(!slots.free(Cadence::Byoyomi), "no cap");
        assert!(slots.reserve(
            id(1),
            Reservation {
                cadence: Cadence::Blitz,
                peer: a,
                outgoing: true,
                accept_until: 1_100,
                consumed: false,
            }
        ));
        assert!(!slots.reserve(
            id(1),
            Reservation {
                cadence: Cadence::Blitz,
                peer: a,
                outgoing: true,
                accept_until: 1_100,
                consumed: false,
            }
        ));
        assert!(!slots.free(Cadence::Blitz));
        // The challenger the bot challenged finds the slot free for it.
        let local = slots.local_state(false, &a);
        assert_eq!(local.held_for_challenger, Some(Cadence::Blitz));
        assert_eq!(local.held[&Cadence::Blitz], 1);
        assert_eq!(slots.local_state(false, &b).held_for_challenger, None);
        assert_eq!(slots.pending_to(&a), Some(id(1)));
        assert_eq!(slots.pending_from(&a), None);
        // Rule 8: `a` challenges back; the bot's reservation is consumed,
        // the incoming one takes the slot, and the bot's challenge stays
        // pending.
        assert!(slots.consume_outgoing(&a));
        assert!(!slots.consume_outgoing(&a), "once");
        assert_eq!(slots.held(Cadence::Blitz), 0);
        assert_eq!(slots.pending_to(&a), Some(id(1)));
        assert_eq!(slots.local_state(false, &a).held_for_challenger, None);
        slots.reserve(
            id(2),
            Reservation {
                cadence: Cadence::Blitz,
                peer: a,
                outgoing: false,
                accept_until: 1_100,
                consumed: false,
            },
        );
        assert_eq!(slots.held(Cadence::Blitz), 1);
        assert_eq!(slots.pending_from(&a), Some(id(2)));
        // The founding consumes the incoming reservation; the session
        // takes the slot; the outgoing one lapses later.
        slots.open_session(id(9), Cadence::Blitz, a, Some(&id(2)));
        assert!(slots.reservation(&id(2)).is_none());
        assert!(slots.reservation(&id(1)).is_some());
        assert_eq!(slots.held(Cadence::Blitz), 1);
        assert!(slots.release(&id(1)).is_some());
        assert!(slots.session_open_with(&a));
        // One over the cap is still recorded.
        slots.open_session(id(10), Cadence::Blitz, b, None);
        assert_eq!(slots.held(Cadence::Blitz), 2);
        assert_eq!(slots.close_session(&id(9)), Some((Cadence::Blitz, a)));
        assert!(!slots.free(Cadence::Blitz));
        slots.close_session(&id(10));
        assert!(slots.free(Cadence::Blitz));
    }

    #[test]
    fn incoming_reservations_lapse_after_the_tolerance() {
        let mut slots = Slots::new([(Cadence::Blitz, 2)].into());
        let a = Keys::generate().public_key();
        slots.reserve(
            id(1),
            Reservation {
                cadence: Cadence::Blitz,
                peer: a,
                outgoing: false,
                accept_until: 1_000,
                consumed: false,
            },
        );
        slots.reserve(
            id(2),
            Reservation {
                cadence: Cadence::Blitz,
                peer: a,
                outgoing: true,
                accept_until: 1_000,
                consumed: false,
            },
        );
        assert!(slots.lapse_incoming(1_001, 1).is_empty());
        assert_eq!(slots.lapse_incoming(1_002, 1), vec![id(1)]);
        assert!(
            slots.reservation(&id(2)).is_some(),
            "outgoing: the runtime's"
        );
    }
}
