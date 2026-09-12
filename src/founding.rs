//! Founding a session — the Game Session (kind 3422) the bot publishes, on
//! either path (kind `3422` §Signing party): to **accept** a Direct Challenge
//! addressed to it (the Game Session IS the acceptance), or to **found** the
//! session a canonical Pairing declared (either player may). Pure over
//! borrowed events, the persona's plan and what the module's `describe`
//! reports; the actor publishes.
//!
//! Nothing here is the bot's choice beyond what the challenge delegated (an
//! open variant, an open seat — settled by the courtship layer): every other
//! term is mirrored from the founding event, and the initial position is the
//! one the rule system **prescribes** for the pairing of variants
//! (`describe.positions`, kind `3422` §Content) — a Game Session that
//! departed from either would found nothing.

use nostr_sdk::prelude::*;

use crate::courtship::{AcceptPlan, Skip};
use crate::module::Describe;
use crate::session::{self, Seat, SessionTerms, Timing};
use crate::tags;

/// The founding marker a Game Session carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Founding {
    /// A Direct Challenge (fresh or rematch): the acceptance.
    DirectChallenge,
    /// A canonical Pairing: the founding by one of its players.
    Pairing,
}

impl Founding {
    const fn marker(self) -> &'static str {
        match self {
            Self::DirectChallenge => "direct_challenge",
            Self::Pairing => "pairing",
        }
    }
}

/// Everything a Game Session states, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameSessionPlan {
    /// The founding path.
    pub founding: Founding,
    /// The founding event's id.
    pub founding_id: EventId,
    /// The game.
    pub game: String,
    /// The Rule System event, mirrored.
    pub rules: EventId,
    /// The timing relay, mirrored verbatim (self-timed mode).
    pub timing_relay: String,
    /// The player seated `first`.
    pub first: PublicKey,
    /// The player seated `second`.
    pub second: PublicKey,
    /// The `first` player's variant.
    pub first_variant: String,
    /// The `second` player's variant.
    pub second_variant: String,
    /// The initial position the rule system prescribes (the `content`).
    pub position: String,
}

impl GameSessionPlan {
    /// The tags of the Game Session (kind `3422` §Tags). The `nonce` is the
    /// publish path's; the content is [`Self::position`].
    #[must_use]
    pub fn tags(&self, relay_hint: &str) -> Vec<Tag> {
        vec![
            Tag::custom(
                "e",
                [
                    self.founding_id.to_hex(),
                    relay_hint.to_owned(),
                    self.founding.marker().to_owned(),
                ],
            ),
            Tag::custom("timing_relay", [self.timing_relay.clone()]),
            Tag::custom("game", [self.game.clone()]),
            Tag::custom(
                "e",
                [
                    self.rules.to_hex(),
                    relay_hint.to_owned(),
                    "rules".to_owned(),
                ],
            ),
            p_role(&self.first, "player", relay_hint),
            p_role(&self.second, "player", relay_hint),
            Tag::custom("seat", [self.first.to_hex(), "first".to_owned()]),
            Tag::custom("seat", [self.second.to_hex(), "second".to_owned()]),
            Tag::custom("variant", [self.first.to_hex(), self.first_variant.clone()]),
            Tag::custom(
                "variant",
                [self.second.to_hex(), self.second_variant.clone()],
            ),
        ]
    }
}

fn p_role(pubkey: &PublicKey, role: &str, relay_hint: &str) -> Tag {
    Tag::custom(
        "p",
        [pubkey.to_hex(), relay_hint.to_owned(), role.to_owned()],
    )
}

/// The initial position the rule system prescribes for `(first, second)`
/// variants, from the module's `describe`.
fn prescribed_position(describe: &Describe, first: &str, second: &str) -> Result<String, Skip> {
    describe
        .positions
        .get(&format!("{first}/{second}"))
        .cloned()
        .ok_or("the rule system defines no position for this pairing of variants")
}

/// The Game Session accepting `challenge` per `plan` (kind `3422` §Seats,
/// §Variants: the challenge's terms mirrored, the delegated ones supplied).
///
/// # Errors
///
/// A [`Skip`] when the challenge's designation is not the self-timed form
/// (it was vetted), or the rule system prescribes no position for the
/// variants.
pub fn accept_direct_challenge(
    challenge: &Event,
    plan: &AcceptPlan,
    me: PublicKey,
    describe: &Describe,
) -> Result<GameSessionPlan, Skip> {
    let Some(Timing::SelfTimed(timing_relay)) = session::timing_of(challenge) else {
        return Err("the challenge is not self-timed");
    };
    let game = tags::game(challenge).ok_or("no game")?.to_owned();
    let rules = session::rules_ref(challenge).ok_or("no rules reference")?;
    let (first, second, first_variant, second_variant) = match plan.my_seat {
        Seat::First => (
            me,
            plan.challenger,
            plan.my_variant.clone(),
            plan.their_variant.clone(),
        ),
        Seat::Second => (
            plan.challenger,
            me,
            plan.their_variant.clone(),
            plan.my_variant.clone(),
        ),
    };
    let position = prescribed_position(describe, &first_variant, &second_variant)?;
    Ok(GameSessionPlan {
        founding: Founding::DirectChallenge,
        founding_id: challenge.id,
        game,
        rules,
        timing_relay,
        first,
        second,
        first_variant,
        second_variant,
        position,
    })
}

/// The Game Session founding the session `pairing` declared — fully
/// determined by the Pairing (kind `3419` §From Pairing to Game Session). The
/// Pairing is checked for what the bot can decide: it names the bot as a
/// player, our rule system, our relay, both seats and variants, and its
/// `found_until` still leaves `margin_secs`.
///
/// # Errors
///
/// A [`Skip`] naming the first term that does not fit.
#[allow(clippy::too_many_arguments)]
pub fn found_on_pairing(
    pairing: &Event,
    me: PublicKey,
    relay_url: &str,
    rules: &EventId,
    game: &str,
    describe: &Describe,
    now: u64,
    margin_secs: u64,
) -> Result<GameSessionPlan, Skip> {
    if pairing.kind != Kind::Custom(session::KIND_PAIRING) {
        return Err("not a Pairing");
    }
    let players = tags::pubkeys_with_role(pairing, "player");
    let [a, b] = players.as_slice() else {
        return Err("not exactly two players");
    };
    if a == b || (*a != me && *b != me) {
        return Err("not our Pairing");
    }
    if tags::game(pairing) != Some(game) {
        return Err("other game");
    }
    if session::rules_ref(pairing) != Some(*rules) {
        return Err("another rule system");
    }
    let timing_relay = match session::timing_of(pairing) {
        Some(Timing::SelfTimed(relay))
            if tags::norm_relay(&relay) == tags::norm_relay(relay_url) =>
        {
            relay
        }
        Some(Timing::SelfTimed(_)) => return Err("timing_relay designation is not our relay"),
        Some(Timing::Attested(_)) => return Err("attested mode (v1 is self-timed)"),
        None => return Err("no single timing designation"),
    };
    let found_until = session::exactly_one(pairing, "found_until")
        .and_then(session::decimal)
        .ok_or("missing found_until")?;
    if found_until <= now.saturating_add(margin_secs) {
        return Err("found_until too close");
    }
    let (first, second) = match (
        tags::seat_for(pairing, a).and_then(Seat::parse),
        tags::seat_for(pairing, b).and_then(Seat::parse),
    ) {
        (Some(Seat::First), Some(Seat::Second)) => (*a, *b),
        (Some(Seat::Second), Some(Seat::First)) => (*b, *a),
        _ => return Err("seats are not one first and one second"),
    };
    let first_variant = tags::variant_for(pairing, &first)
        .filter(|v| session::is_identifier(v))
        .ok_or("no variant for the first player")?
        .to_owned();
    let second_variant = tags::variant_for(pairing, &second)
        .filter(|v| session::is_identifier(v))
        .ok_or("no variant for the second player")?
        .to_owned();
    let position = prescribed_position(describe, &first_variant, &second_variant)?;
    Ok(GameSessionPlan {
        founding: Founding::Pairing,
        founding_id: pairing.id,
        game: game.to_owned(),
        rules: *rules,
        timing_relay,
        first,
        second,
        first_variant,
        second_variant,
        position,
    })
}

/// The canonical Game Session of a founding's slot among `candidates`, in
/// self-timed mode (kind `3422` §Idempotence and race resolution): among the
/// candidates referencing `founding` whose terms conform to it, the smallest
/// `(created_at, id)`. With its terms.
#[must_use]
pub fn canonical_session<'a>(
    candidates: impl IntoIterator<Item = &'a Event>,
    founding: &Event,
) -> Option<(&'a Event, SessionTerms)> {
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let terms = session::terms(candidate, founding).ok()?;
            Some((candidate, terms))
        })
        .min_by_key(|(candidate, _)| (candidate.created_at.as_secs(), candidate.id))
}

/// Whether a rematch challenge's constrained terms are those of the
/// concluded session (kind `3420` §Rematch challenge): the two players, the
/// game, the rule system, the time control, both variants, the timing
/// designation, and the seat — the other value than the one the challenger
/// held. `conclusion` is the `concluded_by` event, which must be a
/// structurally conforming Conclusion of that session (its correctness under
/// the rule system is the caller's, through the module).
///
/// # Errors
///
/// A [`Skip`] naming the first constraint violated.
pub fn rematch_terms_ok(
    challenge: &Event,
    concluded: &SessionTerms,
    conclusion: &Event,
) -> Result<(), Skip> {
    let challenger = challenge.pubkey;
    let Some(opponent) = concluded.opponent_of(&challenger) else {
        return Err("the challenger did not play the concluded session");
    };
    if tags::pubkeys_with_role(challenge, "opponent") != [opponent] {
        return Err("the opponent is not the other player of the concluded session");
    }
    if session::conclusion(conclusion, concluded).is_none() {
        return Err("concluded_by is not a conforming Conclusion of the concluded session");
    }
    if tags::game(challenge) != Some(concluded.game.as_str()) {
        return Err("the game differs from the concluded session's");
    }
    if session::rules_ref(challenge) != Some(concluded.rules) {
        return Err("the rule system differs from the concluded session's");
    }
    if session::time_control_of(challenge).ok().as_deref() != Some(&concluded.time_control) {
        return Err("the time control differs from the concluded session's");
    }
    if tags::variant_for(challenge, &concluded.first) != Some(concluded.first_variant.as_str())
        || tags::variant_for(challenge, &concluded.second)
            != Some(concluded.second_variant.as_str())
    {
        return Err("the variants differ from the concluded session's");
    }
    if session::timing_of(challenge).as_ref() != Some(&concluded.timing) {
        return Err("the timing designation differs from the concluded session's");
    }
    let held = concluded
        .seat_of(&challenger)
        .ok_or("the challenger did not play the concluded session")?;
    match session::exactly_one(challenge, "seat").and_then(Seat::parse) {
        Some(declared) if declared == held.other() => Ok(()),
        _ => Err("the seat is not swapped"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;
    use crate::courtship::RematchRefs;
    use crate::module::native::Native;
    use crate::session::fixtures::{World, CHESS_CHESS};

    fn describe() -> Describe {
        crate::module::describe(&mut Native).unwrap()
    }

    #[test]
    fn founds_on_a_pairing_exactly_as_the_pairing_says() {
        let w = World::new();
        let pairing = w.pairing();
        let plan = found_on_pairing(
            &pairing,
            w.second.public_key(),
            "wss://relay.example.com/",
            &w.rules,
            "sanki",
            &describe(),
            1_700_000_000,
            30,
        )
        .unwrap();
        assert_eq!(plan.founding, Founding::Pairing);
        assert_eq!(plan.first, w.first.public_key());
        assert_eq!(plan.second, w.second.public_key());
        assert_eq!(plan.position, CHESS_CHESS);
        // The published event conforms to its founding: its terms parse.
        let event = EventBuilder::new(Kind::Custom(session::KIND_GAME_SESSION), &plan.position)
            .tags(plan.tags("wss://relay.example.com"))
            .finalize(&w.second)
            .unwrap();
        let terms = session::terms(&event, &pairing).unwrap();
        assert_eq!(terms.first_variant, "chess");
        assert_eq!(terms.position, CHESS_CHESS);
        // A stranger, another rule system, a lapsed window: refused.
        assert!(found_on_pairing(
            &pairing,
            Keys::generate().public_key(),
            "wss://relay.example.com",
            &w.rules,
            "sanki",
            &describe(),
            1_700_000_000,
            30
        )
        .is_err());
        assert!(found_on_pairing(
            &pairing,
            w.first.public_key(),
            "wss://relay.example.com",
            &EventId::from_hex(&"8".repeat(64)).unwrap(),
            "sanki",
            &describe(),
            1_700_000_000,
            30
        )
        .is_err());
        assert!(found_on_pairing(
            &pairing,
            w.first.public_key(),
            "wss://relay.example.com",
            &w.rules,
            "sanki",
            &describe(),
            2_000_000_000,
            30
        )
        .is_err());
    }

    #[test]
    fn accepts_a_direct_challenge_with_the_prescribed_position() {
        let w = World::new();
        let challenger = w.first.clone();
        let me = w.second.public_key();
        let challenge = EventBuilder::new(Kind::Custom(session::KIND_DIRECT_CHALLENGE), "")
            .tags(vec![
                World::p(&me, "opponent"),
                World::e(&w.rules, "rules"),
                Tag::parse(["timing_relay", "wss://relay.example.com"]).unwrap(),
                Tag::parse(["game", "sanki"]).unwrap(),
                Tag::parse(["time_control", "300", "3"]).unwrap(),
                Tag::parse(["accept_until", "2000000300"]).unwrap(),
                Tag::parse(["nonce", "0", "0"]).unwrap(),
            ])
            .finalize(&challenger)
            .unwrap();
        let plan = AcceptPlan {
            challenger: challenger.public_key(),
            my_variant: "ogi".to_owned(),
            their_variant: "chess".to_owned(),
            my_seat: Seat::Second,
            accept_until: 2_000_000_300,
            cadence: crate::cadence::Cadence::Blitz,
            rematch: None,
            premium_imposition: false,
        };
        let session_plan = accept_direct_challenge(&challenge, &plan, me, &describe()).unwrap();
        assert_eq!(session_plan.first, challenger.public_key());
        assert_eq!(session_plan.second, me);
        assert_eq!(session_plan.first_variant, "chess");
        assert_eq!(session_plan.second_variant, "ogi");
        assert_eq!(
            session_plan.position,
            describe().positions["chess/ogi"],
            "the position is the one the rule system prescribes"
        );
        let event = EventBuilder::new(
            Kind::Custom(session::KIND_GAME_SESSION),
            &session_plan.position,
        )
        .tags(session_plan.tags(""))
        .finalize(&w.second)
        .unwrap();
        let terms = session::terms(&event, &challenge).unwrap();
        assert_eq!(terms.pairing(), "chess/ogi");
        assert_eq!(terms.second, me);
    }

    #[test]
    fn the_canonical_session_is_the_earliest_conforming_one() {
        let w = World::new();
        let pairing = w.pairing();
        let by_first = w.session_with(&pairing, 1_700_000_200, CHESS_CHESS, &w.first);
        let by_second = w.session_with(&pairing, 1_700_000_150, CHESS_CHESS, &w.second);
        let stranger = w.session_with(&pairing, 1_700_000_100, CHESS_CHESS, &Keys::generate());
        let (canonical, terms) =
            canonical_session([&by_first, &by_second, &stranger], &pairing).unwrap();
        assert_eq!(canonical.id, by_second.id);
        assert_eq!(terms.id, by_second.id);
        assert!(canonical_session([&stranger], &pairing).is_none());
    }

    #[test]
    fn a_rematch_challenge_must_mirror_the_concluded_session_with_seats_swapped() {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_700_000_100);
        let concluded = session::terms(&session, &pairing).unwrap();
        let conclusion = w.conclusion(&session.id, &w.first, "resignation", 0, 100, 1_700_001_000);
        let refs = RematchRefs {
            concluded: session.id,
            concluded_by: conclusion.id,
        };
        let challenge = |seat: &str, tc: &[&str], fv: &str| {
            EventBuilder::new(Kind::Custom(session::KIND_DIRECT_CHALLENGE), "")
                .tags(vec![
                    World::p(&w.second.public_key(), "opponent"),
                    World::e(&w.rules, "rules"),
                    World::e(&refs.concluded, "rematch_of"),
                    World::e(&refs.concluded_by, "concluded_by"),
                    Tag::parse(["timing_relay", "wss://relay.example.com"]).unwrap(),
                    Tag::parse(["game", "sanki"]).unwrap(),
                    Tag::parse(["time_control"].into_iter().chain(tc.iter().copied())).unwrap(),
                    Tag::parse(["variant", &w.first.public_key().to_hex(), fv]).unwrap(),
                    Tag::parse(["variant", &w.second.public_key().to_hex(), "chess"]).unwrap(),
                    Tag::parse(["seat", seat]).unwrap(),
                    Tag::parse(["accept_until", "2000000300"]).unwrap(),
                    Tag::parse(["nonce", "0", "0"]).unwrap(),
                ])
                .finalize(&w.first)
                .unwrap()
        };
        // The first player held `first`; a rematch claims `second`.
        assert_eq!(
            rematch_terms_ok(
                &challenge("second", &["300", "3"], "chess"),
                &concluded,
                &conclusion
            ),
            Ok(())
        );
        assert!(rematch_terms_ok(
            &challenge("first", &["300", "3"], "chess"),
            &concluded,
            &conclusion
        )
        .is_err());
        assert!(rematch_terms_ok(
            &challenge("second", &["300", "0"], "chess"),
            &concluded,
            &conclusion
        )
        .is_err());
        assert!(rematch_terms_ok(
            &challenge("second", &["300", "3"], "ogi"),
            &concluded,
            &conclusion
        )
        .is_err());
        // A Conclusion of another session is no proof.
        let other = w.conclusion(
            &EventId::from_hex(&"9".repeat(64)).unwrap(),
            &w.first,
            "resignation",
            0,
            100,
            1,
        );
        assert!(rematch_terms_ok(
            &challenge("second", &["300", "3"], "chess"),
            &concluded,
            &other
        )
        .is_err());
    }
}
