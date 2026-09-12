//! The fleet ledger — the supervisor's shared coordination state (ADR-0014
//! §8): the bot-vs-bot budget and pool occupancy, so the pool stays warm
//! without the matchmaker pairing bots endlessly, and a human is never
//! crowded out.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use nostr_sdk::prelude::{EventId, PublicKey};

/// Shared, low-contention counters. A [`Mutex`] is ample: courtship events
/// are seconds apart, never hot.
#[derive(Debug)]
pub struct Ledger {
    /// Fleet member pubkeys (to recognize a sibling's entry or session).
    members: BTreeSet<PublicKey>,
    /// Fleet-wide cap on concurrent bot-vs-bot sessions.
    max_bot_vs_bot: u32,
    state: Mutex<LedgerState>,
}

#[derive(Debug, Default)]
struct LedgerState {
    /// Active sessions in which BOTH players are fleet members.
    bot_vs_bot: u32,
    /// Fleet members currently idling in the pool (spontaneous entries are
    /// throttled while siblings already wait there).
    pool_idlers: BTreeSet<PublicKey>,
    /// Fully open pool entries claimed by a member — the entry's id to the
    /// claimant and the entry's `accept_until` (ADR-0040 §2): one member
    /// courts such an entry, not the whole fleet. Pruned as entries expire.
    open_claims: BTreeMap<EventId, (PublicKey, u64)>,
}

impl Ledger {
    /// A ledger over the fleet's member set.
    #[must_use]
    pub fn new(members: BTreeSet<PublicKey>, max_bot_vs_bot: u32) -> Self {
        Self {
            members,
            max_bot_vs_bot,
            state: Mutex::new(LedgerState::default()),
        }
    }

    /// Whether `pubkey` is a fleet member.
    #[must_use]
    pub fn is_member(&self, pubkey: &PublicKey) -> bool {
        self.members.contains(pubkey)
    }

    /// Whether courting a SIBLING's entry is currently within the budget.
    #[must_use]
    pub fn may_court_sibling(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.bot_vs_bot < self.max_bot_vs_bot)
            .unwrap_or(false)
    }

    /// Record a session opening/closing between `a` and `b`.
    pub fn session_changed(&self, a: &PublicKey, b: &PublicKey, opened: bool) {
        if !(self.is_member(a) && self.is_member(b)) {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.bot_vs_bot = if opened {
                state.bot_vs_bot.saturating_add(1)
            } else {
                state.bot_vs_bot.saturating_sub(1)
            };
        }
    }

    /// Record a bot entering/leaving the pool. Returns whether a SPONTANEOUS
    /// entry is advised (throttled while a sibling already idles there —
    /// reactive entries are never throttled by this).
    pub fn pool_presence(&self, bot: PublicKey, present: bool) -> bool {
        self.state
            .lock()
            .map(|mut state| {
                if present {
                    state.pool_idlers.insert(bot);
                } else {
                    state.pool_idlers.remove(&bot);
                }
                state.pool_idlers.len() <= 1
            })
            .unwrap_or(false)
    }

    /// Claim a FULLY OPEN pool entry (no variant term) for `bot`: `true` for
    /// the first member to ask, and again for the same member; `false` once
    /// a sibling holds it. `accept_until` bounds the claim's life — an
    /// expired claim is pruned, so the map never grows with the pool's
    /// history. Idempotent, so a replayed entry is answered consistently.
    pub fn claim_open_entry(&self, entry: EventId, bot: PublicKey, accept_until: u64) -> bool {
        self.state
            .lock()
            .map(|mut state| {
                state
                    .open_claims
                    .retain(|_, (_, until)| *until > accept_until.saturating_sub(86_400));
                match state.open_claims.get(&entry) {
                    Some((claimant, _)) => *claimant == bot,
                    None => {
                        state.open_claims.insert(entry, (bot, accept_until));
                        true
                    }
                }
            })
            .unwrap_or(false)
    }

    /// Whether a sibling (other than `bot`) already idles in the pool
    /// (spontaneous-entry throttling).
    #[allow(dead_code)]
    #[must_use]
    pub fn sibling_in_pool(&self, bot: &PublicKey) -> bool {
        self.state
            .lock()
            .map(|state| state.pool_idlers.iter().any(|idler| idler != bot))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use nostr_sdk::prelude::Keys;

    #[test]
    fn bot_vs_bot_budget_caps_sibling_courtship() {
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let human = Keys::generate().public_key();
        let ledger = Ledger::new(BTreeSet::from([a, b]), 1);

        assert!(ledger.may_court_sibling());
        ledger.session_changed(&a, &b, true);
        assert!(!ledger.may_court_sibling());
        // A bot-human session never counts against the budget.
        ledger.session_changed(&a, &human, true);
        assert!(!ledger.may_court_sibling());
        ledger.session_changed(&a, &b, false);
        assert!(ledger.may_court_sibling());
    }

    #[test]
    fn an_open_entry_goes_to_the_first_claimant_only() {
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let ledger = Ledger::new(BTreeSet::from([a, b]), 3);
        let entry = EventId::from_slice(&[1; 32]).unwrap();
        let other = EventId::from_slice(&[2; 32]).unwrap();
        assert!(ledger.claim_open_entry(entry, a, 1_000));
        assert!(ledger.claim_open_entry(entry, a, 1_000)); // idempotent
        assert!(!ledger.claim_open_entry(entry, b, 1_000));
        assert!(ledger.claim_open_entry(other, b, 1_000));
        // Long after the entry died, the claim has been pruned: a new claim
        // (a replayed id can only be the same entry, so this is harmless).
        assert!(ledger.claim_open_entry(entry, b, 1_000 + 2 * 86_400));
    }

    #[test]
    fn pool_throttling_watches_siblings() {
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let ledger = Ledger::new(BTreeSet::from([a, b]), 3);
        assert!(!ledger.sibling_in_pool(&a));
        ledger.pool_presence(b, true);
        assert!(ledger.sibling_in_pool(&a));
        assert!(!ledger.sibling_in_pool(&b));
        ledger.pool_presence(b, false);
        assert!(!ledger.sibling_in_pool(&a));
    }
}
