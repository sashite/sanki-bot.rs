//! Rematches — the pure layer (kind 3420 §Rematch challenge, ADR-0033).
//!
//! When a game ends, either player may propose a **rematch** of the concluded
//! session by publishing a Direct Challenge that references it (`rematch_of`)
//! and a Conclusion of it (`concluded_by`, the proof it is over), with the
//! seats swapped and every other term inherited. The challenged player
//! accepts as they accept any Direct Challenge: by founding the Game Session
//! on it (kind `3422`). One concluded session founds at most one rematch,
//! whichever player's challenge is accepted first. This module is the bot's
//! side of that, with no I/O:
//!
//! - [`wants_rematch`] is the persona's *per-game* willingness — decided once
//!   per concluded game so the bot's VOLUNTEERED challenges are stable (the
//!   proactive challenge on the verdict, and bot-vs-bot); answering an
//!   explicit rematch challenge from outside the fleet bypasses it (the
//!   actor's `always` flag) — a human who clicks Rematch is answered, never
//!   diced away;
//! - [`rematch_challenge_tags`] builds the tags of the bot's OWN rematch
//!   challenge from the concluded session's terms.
//!
//! An incoming rematch challenge is evaluated by the courtship layer like any
//! Direct Challenge, then checked against the concluded session by
//! `founding::rematch_terms_ok`. The `nonce` proof-of-work tag is added by
//! the publish path's miner, not here.

use nostr_sdk::prelude::*;

use crate::prng::SplitMix64;
use crate::session::{SessionTerms, Timing};

/// Default share of concluded games a persona is willing to rematch. The
/// caller passes it to [`wants_rematch`]; it can move to per-persona config
/// later without touching this layer.
pub const DEFAULT_REMATCH_PROBABILITY: f64 = 0.75;

/// The persona's willingness to rematch a concluded game (§9).
///
/// Deterministic and stateless: the concluded id is folded into the bot's seed
/// (FNV-1a) to seed one `SplitMix64` draw, so the verdict never depends on when
/// — or how often — the question is asked, and a fixed `bot_seed` keeps the
/// whole persona reproducible (ADR-0014 §4). `probability` is the share of
/// games to accept in `[0, 1]` (≥ 1 always, ≤ 0 never).
#[must_use]
pub fn wants_rematch(bot_seed: u64, concluded: &EventId, probability: f64) -> bool {
    let hex = concluded.to_hex();
    let mut mix = bot_seed;
    for byte in hex.as_bytes() {
        // FNV-1a step: bitwise xor then a wrapping multiply — neither can panic,
        // so the panic-avoidance lints are satisfied without saturation.
        mix = (mix ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    SplitMix64::new(mix).next_unit() < probability
}

/// The tags of the bot's OWN rematch challenge of the session `concluded`,
/// signed by `me` (a player of it), proving it over with `conclusion` (a
/// Conclusion of it): the opponent is the other player, the seat is the
/// other value than the one `me` held, both variants, the time control, the
/// game, the rule system and the timing designation are the concluded
/// session's. `accept_until` is an absolute unix-seconds deadline; the caller
/// sets it from the stamped `created_at` plus its window. `None` when `me`
/// did not play the session.
#[must_use]
pub fn rematch_challenge_tags(
    concluded: &SessionTerms,
    conclusion: &EventId,
    me: &PublicKey,
    accept_until: u64,
    relay_hint: &str,
) -> Option<Vec<Tag>> {
    let held = concluded.seat_of(me)?;
    let opponent = concluded.player(held.other());
    let mut tags = vec![
        Tag::custom(
            "p",
            [
                opponent.to_hex(),
                relay_hint.to_owned(),
                "opponent".to_owned(),
            ],
        ),
        Tag::custom(
            "e",
            [
                concluded.rules.to_hex(),
                relay_hint.to_owned(),
                "rules".to_owned(),
            ],
        ),
        Tag::custom(
            "e",
            [
                concluded.id.to_hex(),
                relay_hint.to_owned(),
                "rematch_of".to_owned(),
            ],
        ),
        Tag::custom(
            "e",
            [
                conclusion.to_hex(),
                relay_hint.to_owned(),
                "concluded_by".to_owned(),
            ],
        ),
        Tag::custom("game", [concluded.game.clone()]),
    ];
    match &concluded.timing {
        Timing::SelfTimed(relay) => tags.push(Tag::custom("timing_relay", [relay.clone()])),
        Timing::Attested(ts) => tags.push(Tag::custom(
            "p",
            [ts.to_hex(), relay_hint.to_owned(), "timestamper".to_owned()],
        )),
    }
    for [duration, increment, plies] in &concluded.time_control {
        let mut row = vec![duration.unwrap_or(0).to_string()];
        if let Some(increment) = increment {
            row.push(increment.to_string());
            if let Some(plies) = plies {
                row.push(plies.to_string());
            }
        }
        tags.push(Tag::custom("time_control", row));
    }
    tags.push(Tag::custom(
        "variant",
        [concluded.first.to_hex(), concluded.first_variant.clone()],
    ));
    tags.push(Tag::custom(
        "variant",
        [concluded.second.to_hex(), concluded.second_variant.clone()],
    ));
    tags.push(Tag::custom("seat", [held.other().name().to_owned()]));
    tags.push(Tag::custom("accept_until", [accept_until.to_string()]));
    Some(tags)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::founding::rematch_terms_ok;
    use crate::session::fixtures::World;
    use crate::session::{self, KIND_DIRECT_CHALLENGE};

    #[test]
    fn willingness_is_stable_per_game_and_follows_the_probability() {
        let game = EventId::from_hex(&"a".repeat(64)).unwrap();
        let first = wants_rematch(7, &game, 0.5);
        for _ in 0..10 {
            assert_eq!(wants_rematch(7, &game, 0.5), first);
        }
        assert!(wants_rematch(7, &game, 1.0));
        assert!(!wants_rematch(7, &game, 0.0));
        // Over many games the share approaches the probability.
        let accepted = (0..2000u32)
            .filter(|i| {
                let id = EventId::from_hex(&format!("{i:064x}")).unwrap();
                wants_rematch(42, &id, 0.75)
            })
            .count();
        assert!((1350..=1650).contains(&accepted), "{accepted}");
    }

    #[test]
    fn the_challenge_built_here_is_a_conforming_rematch_of_the_session() {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_700_000_100);
        let concluded = session::terms(&session, &pairing).unwrap();
        let conclusion = w.conclusion(&session.id, &w.second, "checkmate", 100, 0, 1_700_001_000);
        // The second player (seat `second`) proposes: they claim `first`.
        let tags = rematch_challenge_tags(
            &concluded,
            &conclusion.id,
            &w.second.public_key(),
            2_000_000_000,
            "",
        )
        .unwrap();
        let challenge = EventBuilder::new(Kind::Custom(KIND_DIRECT_CHALLENGE), "")
            .tags(tags)
            .tag(Tag::parse(["nonce", "0", "0"]).unwrap())
            .finalize(&w.second)
            .unwrap();
        assert_eq!(
            rematch_terms_ok(&challenge, &concluded, &conclusion),
            Ok(())
        );
        assert_eq!(session::exactly_one(&challenge, "seat"), Some("first"));
        assert_eq!(
            session::time_control_of(&challenge).unwrap(),
            concluded.time_control
        );
        // A stranger cannot build one.
        assert!(rematch_challenge_tags(
            &concluded,
            &conclusion.id,
            &Keys::generate().public_key(),
            2_000_000_000,
            ""
        )
        .is_none());
    }
}
