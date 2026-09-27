// SPDX-License-Identifier: Apache-2.0
//! Admitting a Direct Challenge (ADR-0045 §5 *Accepting one*): a pure
//! function decides, and I/O only feeds it. The local checks run in order,
//! synchronously, without I/O; each [`Refusal`] variant is a test, and the
//! caller logs each at `debug`. The network checks — not already founded,
//! the rating — are the runtime's, after the reservation.
//!
//! The variants (rule 6): the bot's own is the one the challenge fixes, or
//! else `preferred`, and must be in `variants`; the challenger's is the one
//! the challenge fixes, or else the bot's own (the mirror rule), and must be
//! in `opponents`. An **asymmetric imposition** — the challenge fixing the
//! bot's variant while leaving the challenger's open, or fixing it
//! differently — is always refused: the form is premium-only, and this bot
//! consults no admission service.

use std::collections::BTreeMap;

use nostr_sdk::prelude::*;
use sashite_sanki_client::cadence::Cadence;
use sashite_sanki_client::readers::DirectChallenge;
use sashite_sanki_client::session::{Seat, Timing};
use sashite_sanki_client::tags::norm_relay;

use crate::config::{per_move_share, Config, Policy, Variant};

/// The least time a challenge must leave before `accept_until` (rule 4).
pub const MIN_ACCEPT_LEAD_SECS: u64 = 10;

/// What the runtime knows when a challenge arrives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalState {
    /// The bot is `Halted` (another instance of the library holds the key).
    pub halted: bool,
    /// Per family: the games open plus the reservations held.
    pub held: BTreeMap<Cadence, u32>,
    /// The family of a reservation held by the bot's own pending challenge
    /// **to this very challenger**, if any: it is free for the challenge
    /// (rule 8), the bot's own challenge being left to lapse.
    pub held_for_challenger: Option<Cadence>,
}

/// Why a challenge is not admitted; in the order the checks run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Rule 1: the bot is `Halted`.
    Halted,
    /// Rule 2: not the game.
    OtherGame,
    /// Rule 2: not the configured Rule System.
    OtherRules,
    /// Rule 3: attested mode, or another timing relay.
    OtherTiming,
    /// Rule 4: `accept_until` too close.
    ExpiringSoon,
    /// Rule 5: a rematch challenge (out of scope).
    Rematch,
    /// Rule 6: the variant imposed on the bot is not one it plays.
    OwnVariantNotPlayed(String),
    /// Rule 6: the imposition is asymmetric.
    AsymmetricImposition,
    /// Rule 6: the challenger's variant is not one the bot plays against.
    OpponentVariantNotPlayed(String),
    /// Rule 7: the time control has no cadence.
    NoCadence,
    /// Rule 7: the family has no cap.
    FamilyNotPlayed(Cadence),
    /// Rule 7: the smallest per-move share, below `min_move_secs`.
    NotPlayable {
        /// The share, in seconds.
        share: u64,
    },
    /// Rule 8: no free slot in the family.
    NoFreeSlot(Cadence),
    /// Rule 9: the challenger is in `blocks`.
    Blocked,
    /// Rule 10: the policy refuses.
    Policy,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Halted => f.write_str("the bot is halted"),
            Self::OtherGame => f.write_str("another game"),
            Self::OtherRules => f.write_str("another rule system"),
            Self::OtherTiming => f.write_str("another timing designation"),
            Self::ExpiringSoon => f.write_str("accept_until too close"),
            Self::Rematch => f.write_str("a rematch challenge"),
            Self::OwnVariantNotPlayed(v) => write!(f, "the bot does not play {v}"),
            Self::AsymmetricImposition => f.write_str("an asymmetric imposition"),
            Self::OpponentVariantNotPlayed(v) => write!(f, "the bot does not play against {v}"),
            Self::NoCadence => f.write_str("no cadence"),
            Self::FamilyNotPlayed(c) => write!(f, "the {} family is not played", c.token()),
            Self::NotPlayable { share } => write!(f, "{share} s a move, below min_move_secs"),
            Self::NoFreeSlot(c) => write!(f, "no free {} slot", c.token()),
            Self::Blocked => f.write_str("the challenger is blocked"),
            Self::Policy => f.write_str("the policy refuses"),
        }
    }
}

/// What the local checks decided; the network checks and the founding
/// follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The challenger.
    pub challenger: PublicKey,
    /// The bot's seat: the other value than the challenger's declared one,
    /// or the one drawn when open.
    pub my_seat: Seat,
    /// Whether the seat was open in the challenge (drawn by the bot).
    pub seat_was_open: bool,
    /// The bot's variant.
    pub my_variant: Variant,
    /// The challenger's variant.
    pub their_variant: Variant,
    /// The family.
    pub cadence: Cadence,
    /// The challenge's `accept_until`: the reservation's `not_after`.
    pub accept_until: u64,
    /// The periods.
    pub time_control: Vec<[Option<u64>; 3]>,
}

impl Admission {
    /// The players by seat: `(first, second)`.
    #[must_use]
    pub fn seats(&self, me: PublicKey) -> (PublicKey, PublicKey) {
        match self.my_seat {
            Seat::First => (me, self.challenger),
            Seat::Second => (self.challenger, me),
        }
    }

    /// The variants by seat: `(first, second)`.
    #[must_use]
    pub const fn variants(&self) -> (Variant, Variant) {
        match self.my_seat {
            Seat::First => (self.my_variant, self.their_variant),
            Seat::Second => (self.their_variant, self.my_variant),
        }
    }
}

/// The local checks, in order. `open_seat_first` is the bot's draw for an
/// open seat (`Identity::open_seat_first`), computed by the caller.
///
/// # Errors
///
/// The first refusal.
pub fn admit(
    challenge: &DirectChallenge,
    config: &Config,
    local: &LocalState,
    now: u64,
    open_seat_first: bool,
) -> Result<Admission, Refusal> {
    // 1.
    if local.halted {
        return Err(Refusal::Halted);
    }
    // 2.
    if challenge.game != "sanki" {
        return Err(Refusal::OtherGame);
    }
    if challenge.rules != config.connection().rules {
        return Err(Refusal::OtherRules);
    }
    // 3.
    match &challenge.timing {
        Timing::SelfTimed(relay)
            if norm_relay(relay) == norm_relay(&config.connection().relay.to_string()) => {}
        _ => return Err(Refusal::OtherTiming),
    }
    // 4.
    if challenge.accept_until < now.saturating_add(MIN_ACCEPT_LEAD_SECS) {
        return Err(Refusal::ExpiringSoon);
    }
    // 5.
    if challenge.is_rematch() {
        return Err(Refusal::Rematch);
    }
    // 6.
    let play = config.play();
    let my_variant = match &challenge.opponent_variant {
        Some(imposed) => {
            let variant = Variant::parse(imposed)
                .filter(|v| play.variants.contains(v))
                .ok_or_else(|| Refusal::OwnVariantNotPlayed(imposed.clone()))?;
            if challenge.challenger_variant.as_deref() != Some(imposed.as_str()) {
                return Err(Refusal::AsymmetricImposition);
            }
            variant
        }
        None => play.preferred,
    };
    let their_variant = match &challenge.challenger_variant {
        Some(theirs) => Variant::parse(theirs)
            .filter(|v| play.opponents.contains(v))
            .ok_or_else(|| Refusal::OpponentVariantNotPlayed(theirs.clone()))?,
        None => {
            if !play.opponents.contains(&my_variant) {
                return Err(Refusal::OpponentVariantNotPlayed(
                    my_variant.name().to_owned(),
                ));
            }
            my_variant
        }
    };
    // 7.
    let cadence = Cadence::of_rows(&challenge.rows).ok_or(Refusal::NoCadence)?;
    if play.cap(cadence) == 0 {
        return Err(Refusal::FamilyNotPlayed(cadence));
    }
    let share = per_move_share(&challenge.time_control);
    if share < play.min_move_secs {
        return Err(Refusal::NotPlayable { share });
    }
    // 8.
    let held = local.held.get(&cadence).copied().unwrap_or(0);
    let freed = u32::from(local.held_for_challenger == Some(cadence));
    if held.saturating_sub(freed) >= play.cap(cadence) {
        return Err(Refusal::NoFreeSlot(cadence));
    }
    // 9.
    if config.challenges().blocks.contains(&challenge.challenger) {
        return Err(Refusal::Blocked);
    }
    // 10.
    match &config.challenges().policy {
        Policy::Nobody => return Err(Refusal::Policy),
        Policy::Following(follows) if !follows.contains(&challenge.challenger) => {
            return Err(Refusal::Policy);
        }
        Policy::Everyone | Policy::Following(_) | Policy::Rating { .. } => {}
    }

    let (my_seat, seat_was_open) = match challenge.challenger_seat {
        Some(theirs) => (theirs.other(), false),
        None => (
            if open_seat_first {
                Seat::First
            } else {
                Seat::Second
            },
            true,
        ),
    };
    Ok(Admission {
        challenger: challenge.challenger,
        my_seat,
        seat_was_open,
        my_variant,
        their_variant,
        cadence,
        accept_until: challenge.accept_until,
        time_control: challenge.time_control.clone(),
    })
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

    const RULES: &str = "7777777777777777777777777777777777777777777777777777777777777777";

    fn config(extra: &str) -> Config {
        Config::from_toml(&toml_with_challenges(&format!(
            "policy = \"everyone\"\nblocks = []\n{extra}"
        )))
        .unwrap()
    }

    fn challenge() -> DirectChallenge {
        DirectChallenge {
            id: EventId::from_slice(&[1; 32]).unwrap(),
            challenger: Keys::generate().public_key(),
            opponent: Keys::generate().public_key(),
            created_at: 1_000,
            game: "sanki".to_owned(),
            rules: EventId::from_hex(RULES).unwrap(),
            timing: Timing::SelfTimed("wss://relay.sanki.app".to_owned()),
            time_control: vec![[Some(180), Some(2), None]],
            rows: vec![vec!["180".to_owned(), "2".to_owned()]],
            challenger_variant: None,
            opponent_variant: None,
            challenger_seat: None,
            accept_until: 1_120,
            rematch: None,
            content: String::new(),
        }
    }

    #[test]
    fn a_free_challenge_is_admitted_with_the_delegated_terms_supplied() {
        let config = config("");
        let a = admit(&challenge(), &config, &LocalState::default(), 1_000, true).unwrap();
        assert_eq!(a.my_variant, Variant::Chess);
        assert_eq!(a.their_variant, Variant::Chess, "the mirror rule");
        assert_eq!(a.my_seat, Seat::First);
        assert!(a.seat_was_open);
        assert_eq!(a.cadence, Cadence::Blitz);
        let b = admit(&challenge(), &config, &LocalState::default(), 1_000, false).unwrap();
        assert_eq!(b.my_seat, Seat::Second);
        assert_eq!(b.seats(b.challenger).0, b.challenger);
    }

    #[test]
    fn the_checks_run_in_order() {
        let config = config("");
        let state = LocalState::default();
        let mut c = challenge();
        c.game = "go".to_owned();
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::OtherGame)
        );
        let mut c = challenge();
        c.rules = EventId::from_slice(&[9; 32]).unwrap();
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::OtherRules)
        );
        let mut c = challenge();
        c.timing = Timing::SelfTimed("wss://other.example".to_owned());
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::OtherTiming)
        );
        let mut c = challenge();
        c.timing = Timing::Attested(Keys::generate().public_key());
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::OtherTiming)
        );
        assert_eq!(
            admit(&challenge(), &config, &state, 1_111, true),
            Err(Refusal::ExpiringSoon)
        );
        assert!(admit(&challenge(), &config, &state, 1_110, true).is_ok());
        let mut c = challenge();
        c.rematch = Some(sashite_sanki_client::readers::RematchRefs {
            concluded: c.id,
            concluded_by: c.id,
        });
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::Rematch)
        );
        let halted = LocalState {
            halted: true,
            ..LocalState::default()
        };
        assert_eq!(
            admit(&c, &config, &halted, 1_000, true),
            Err(Refusal::Halted)
        );
    }

    #[test]
    fn variants_and_impositions() {
        let config = config("");
        let state = LocalState::default();
        // The challenger's own variant, ours open: cross-variant by
        // delegation is fine.
        let mut c = challenge();
        c.challenger_variant = Some("xiongqi".to_owned());
        let a = admit(&c, &config, &state, 1_000, true).unwrap();
        assert_eq!(
            (a.my_variant, a.their_variant),
            (Variant::Chess, Variant::Xiongqi)
        );
        // The mirror imposition: free.
        let mut c = challenge();
        c.challenger_variant = Some("ogi".to_owned());
        c.opponent_variant = Some("ogi".to_owned());
        let a = admit(&c, &config, &state, 1_000, true).unwrap();
        assert_eq!(
            (a.my_variant, a.their_variant),
            (Variant::Ogi, Variant::Ogi)
        );
        // An asymmetric imposition: ours fixed, theirs open or different.
        let mut c = challenge();
        c.opponent_variant = Some("ogi".to_owned());
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::AsymmetricImposition)
        );
        c.challenger_variant = Some("chess".to_owned());
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::AsymmetricImposition)
        );
        // A variant we do not play, imposed on us.
        let mut c = challenge();
        c.opponent_variant = Some("xiongqi".to_owned());
        c.challenger_variant = Some("xiongqi".to_owned());
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::OwnVariantNotPlayed("xiongqi".to_owned()))
        );
        // A variant we do not play against.
        let mut c = challenge();
        c.challenger_variant = Some("shogi".to_owned());
        assert_eq!(
            admit(&c, &config, &state, 1_000, true),
            Err(Refusal::OpponentVariantNotPlayed("shogi".to_owned()))
        );
    }

    #[test]
    fn cadence_playability_and_slots() {
        let config = config("");
        let mut c = challenge();
        c.time_control = vec![[Some(900), Some(10), None]];
        c.rows = vec![vec!["900".to_owned(), "10".to_owned()]];
        assert_eq!(
            admit(&c, &config, &LocalState::default(), 1_000, true),
            Err(Refusal::FamilyNotPlayed(Cadence::Rapid))
        );
        let mut c = challenge();
        c.time_control = vec![[Some(60), None, None]];
        c.rows = vec![vec!["60".to_owned()]];
        assert_eq!(
            admit(&c, &config, &LocalState::default(), 1_000, true),
            Err(Refusal::NotPlayable { share: 1 })
        );
        let mut c = challenge();
        c.rows = vec![vec!["".to_owned()]];
        assert_eq!(
            admit(&c, &config, &LocalState::default(), 1_000, true),
            Err(Refusal::NoCadence)
        );
        // The slot: held by a game, no; held by our own challenge to this
        // challenger, yes.
        let mut held = BTreeMap::new();
        held.insert(Cadence::Blitz, 1);
        let full = LocalState {
            halted: false,
            held: held.clone(),
            held_for_challenger: None,
        };
        assert_eq!(
            admit(&challenge(), &config, &full, 1_000, true),
            Err(Refusal::NoFreeSlot(Cadence::Blitz))
        );
        let reserved = LocalState {
            halted: false,
            held,
            held_for_challenger: Some(Cadence::Blitz),
        };
        assert!(admit(&challenge(), &config, &reserved, 1_000, true).is_ok());
    }

    #[test]
    fn blocks_and_policies() {
        let c = challenge();
        let hex = c.challenger.to_hex();
        let state = LocalState::default();
        let blocking = Config::from_toml(&toml_with_challenges(&format!(
            "policy = \"everyone\"\nblocks = [\"{hex}\"]"
        )))
        .unwrap();
        assert_eq!(
            admit(&c, &blocking, &state, 1_000, true),
            Err(Refusal::Blocked)
        );
        let nobody =
            Config::from_toml(&toml_with_challenges("policy = \"nobody\"\nblocks = []")).unwrap();
        assert_eq!(
            admit(&c, &nobody, &state, 1_000, true),
            Err(Refusal::Policy)
        );
        let other = Keys::generate().public_key().to_hex();
        let following = Config::from_toml(&toml_with_challenges(&format!(
            "policy = \"following\"\nfollows = [\"{other}\"]\nblocks = []"
        )))
        .unwrap();
        assert_eq!(
            admit(&c, &following, &state, 1_000, true),
            Err(Refusal::Policy)
        );
        let following_us = Config::from_toml(&toml_with_challenges(&format!(
            "policy = \"following\"\nfollows = [\"{hex}\"]\nblocks = []"
        )))
        .unwrap();
        assert!(admit(&c, &following_us, &state, 1_000, true).is_ok());
        // Rating: the local checks pass; the network check decides.
        let rating = Config::from_toml(&toml_with_challenges(&format!(
            "policy = \"rating\"\nmax_delta = 200\nrating_authority = \"{other}\"\nblocks = []"
        )))
        .unwrap();
        assert!(admit(&c, &rating, &state, 1_000, true).is_ok());
    }

    fn toml_with_challenges(challenges: &str) -> String {
        format!(
            r#"
schema = 1
[connection]
relay = "wss://relay.sanki.app"
rules = "{RULES}"
data_dir = "/var/lib/sanki-bot"
rate_per_minute = 30
[identity]
file = "/var/lib/sanki-bot/key.nsec"
[profile]
name = "kitsune"
about = ""
picture = "https://example.com/k.png"
[challenges]
{challenges}
[play]
variants = ["chess", "ogi"]
preferred = "chess"
opponents = ["chess", "ogi", "xiongqi"]
min_move_secs = 5
margin_ms = 300
[play.max_concurrent]
blitz = 1
byoyomi = 1
"#
        )
    }
}
