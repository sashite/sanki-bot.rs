//! Rematch offers (kind 3430) — the pure layer.
//!
//! When a game ends, either player may offer a **rematch** of the concluded
//! session by publishing a kind-3430 Rematch Offer (an untimed founding
//! designation, like an Open or Direct Challenge). Two *mutually-addressed*
//! offers of the same concluded session let the arbiter found ONE new Game
//! Session with the seats swapped and the terms inherited. An offer proves the
//! game is over by pointing, besides its `rematch_of` session, at an
//! Adjudication (kind 3425) of that same session — its `concluded_by`
//! reference, REQUIRED and exactly one (kind `3430` §Semantic constraints). The
//! two offers of a pair need not cite the *same* Adjudication: a session may
//! carry several, so `concluded_by` is a proof, never a pair identifier — the
//! pair is still keyed by `rematch_of`. This module is the bot's side of that,
//! with no I/O:
//!
//! - [`parse_incoming_offer`] reads an incoming 3430 into the handful of fields
//!   the bot needs to decide and mirror it;
//! - [`wants_rematch`] is the persona's *per-game* willingness — decided once
//!   per concluded game so the bot's VOLUNTEERED offers are stable (the
//!   proactive offer on the verdict, and bot-vs-bot mirrors); answering an
//!   explicit offer from outside the fleet bypasses it (the actor's `always`
//!   flag) — a human who clicks Rematch is answered, never diced away;
//! - [`build_offer_tags`] builds the tags of the bot's OWN mirror offer.
//!
//! The actor wires these in the next sub-tasks: the 3425 handler offers
//! proactively (initiate), and a 3430 subscription answers an opponent's offer
//! (reciprocate). The `nonce` proof-of-work tag is added by the publish path's
//! miner (`publish::publish_self_timed`), not here.

use nostr_sdk::prelude::*;

use crate::prng::SplitMix64;
use crate::tags;

/// Rematch Offer — the untimed founding a player publishes to rematch a
/// concluded session (in the manner of an Open Challenge 3418 or Direct
/// Challenge 3420).
pub const REMATCH_OFFER_KIND: u16 = 3430;

/// Default share of concluded games a persona is willing to rematch. The
/// caller passes it to [`wants_rematch`]; it can move to per-persona config
/// later without touching this layer.
pub const DEFAULT_REMATCH_PROBABILITY: f64 = 0.75;

/// A parsed incoming Rematch Offer — what the bot needs to decide and mirror.
#[derive(Debug, Clone)]
pub struct IncomingOffer {
    /// The signer of the offer — the OTHER player, our prospective opponent.
    pub offerer: PublicKey,
    /// Whom the offer addresses (its `opponent` role) — expected to be us.
    pub addressed_to: PublicKey,
    /// The concluded session this offer rematches (its `rematch_of` reference).
    pub concluded: EventId,
    /// The Adjudication (kind 3425) the offer cites as proof that session ended
    /// (its `concluded_by` reference). That it really adjudicates `concluded`,
    /// and is signed by that session's arbiter, is the caller's check: it needs
    /// the event itself, which only the relay has.
    pub concluded_by: EventId,
    /// The designated arbiter (expected to match the concluded game's).
    pub arbiter: PublicKey,
    /// The timestamper, present iff the concluded game was attested — the
    /// timing mode the bot's own offer must mirror.
    pub timestamper: Option<PublicKey>,
    /// The offer's own acceptance deadline (unix seconds).
    pub accept_until: u64,
}

/// Read an event as an incoming Rematch Offer, extracting the fields the bot
/// needs. Returns `None` unless it is a structurally well-formed 3430: exactly
/// one `rematch_of` reference, exactly one `concluded_by` reference, an `opponent`
/// distinct from the signer, an `arbiter`, and a parseable `accept_until` lying
/// in the offer's own future.
///
/// The *contextual* checks — that we are the addressee, that the arbiter is our
/// fleet's, that the window still has time left against the current clock, that
/// the cited `concluded_by` really is an Adjudication of the concluded session
/// signed by its arbiter — are the caller's: they need the world, and are not
/// structural.
pub fn parse_incoming_offer(event: &Event) -> Option<IncomingOffer> {
    if event.kind != Kind::Custom(REMATCH_OFFER_KIND) {
        return None;
    }
    // REQUIRED, and exactly one (kind 3430 §Semantic constraints, constraint 1):
    // an offer naming two sessions does not say which one it replays, and taking
    // the first would let tag order decide what we are being invited to.
    let concluded = tags::sole_event_with_marker(event, "rematch_of")?;
    // REQUIRED, and exactly one: a second `concluded_by` leaves which
    // Adjudication is being cited undetermined, so the offer is malformed
    // rather than ambiguous-but-usable.
    let concluded_by = tags::sole_event_with_marker(event, "concluded_by")?;
    let addressed_to = tags::pubkey_with_role(event, "opponent")?;
    let arbiter = tags::pubkey_with_role(event, "arbiter")?;
    let offerer = event.pubkey;
    // A player cannot rematch themselves: the opponent is the other seat.
    if addressed_to == offerer {
        return None;
    }
    let accept_until: u64 = tags::accept_until(event)?.parse().ok()?;
    // A deadline at or before the offer's own timestamp is already void.
    if accept_until <= event.created_at.as_secs() {
        return None;
    }
    let timestamper = tags::pubkey_with_role(event, "timestamper");
    Some(IncomingOffer {
        offerer,
        addressed_to,
        concluded,
        concluded_by,
        arbiter,
        timestamper,
        accept_until,
    })
}

/// The founding a rematch-founded Game Session inherits, as read from its **pair**
/// of Rematch Offers (kind `3422` §Founding reference: exactly two
/// `rematch_offer`-marked references). A rematch session restates none of the
/// terms (kind `3430` §Inherited terms), so the only things the pair itself
/// settles are *which* session is being replayed and in *which* timing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundingPair {
    /// The concluded Game Session both offers replay (their common `rematch_of`).
    pub concluded: EventId,
    /// The timing mode both offers claim, present iff attested (constraint 6).
    pub timestamper: Option<PublicKey>,
}

/// Read the founding pair of a rematch-founded Game Session: the two offers must
/// be conforming 3430s under `arbiter`, distinct, signed by two different
/// players, mutually addressed, replaying the same session, and agreeing on the
/// timing mode.
///
/// The two offers need NOT cite the same `concluded_by` Adjudication — a session
/// may carry several, and the pair is keyed by `rematch_of` alone. What the
/// caller must still check against the world (it needs the events): that
/// `concluded` really is a Game Session of this arbiter, and that the mode the
/// pair claims is the one that session actually ran in.
///
/// # Errors
///
/// A short static reason, for the caller to wrap into its own context.
pub fn founding_pair(
    offers: [&Event; 2],
    arbiter: &PublicKey,
) -> Result<FoundingPair, &'static str> {
    let [first, second] = offers;
    for offer in offers {
        if offer.kind != Kind::Custom(REMATCH_OFFER_KIND) {
            return Err("founding reference is not a rematch offer");
        }
        if tags::pubkey_with_role(offer, "arbiter").as_ref() != Some(arbiter) {
            return Err("offer designates another arbiter");
        }
    }
    if first.id == second.id {
        return Err("the two founding references are one offer");
    }
    if first.pubkey == second.pubkey {
        return Err("both offers signed by the same player");
    }
    if tags::pubkey_with_role(first, "opponent") != Some(second.pubkey)
        || tags::pubkey_with_role(second, "opponent") != Some(first.pubkey)
    {
        return Err("offers are not mutually addressed");
    }
    // Strict readers: a doubled `rematch_of` leaves which session is replayed
    // undetermined, and first-match-wins would found a session on a guess.
    let concluded =
        tags::sole_event_with_marker(first, "rematch_of").ok_or("offer has no sole rematch_of")?;
    if tags::sole_event_with_marker(second, "rematch_of") != Some(concluded) {
        return Err("offers replay different concluded sessions");
    }
    let timestamper = tags::pubkey_with_role(first, "timestamper");
    if tags::pubkey_with_role(second, "timestamper") != timestamper {
        return Err("offers disagree on the timing mode");
    }
    Ok(FoundingPair {
        concluded,
        timestamper,
    })
}

/// Whether the persona is willing to rematch `concluded`, decided ONCE per game
/// so the initiate path (on the verdict) and the reciprocate path (on an
/// incoming offer) always reach the same answer for the same game.
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

/// Build the tags of the bot's OWN Rematch Offer of `concluded`: proving that
/// session over with `adjudication` (the 3425 the bot observed, its
/// `concluded_by`), addressed to `opponent`, designating `arbiter`, and — iff
/// the concluded game was attested — the same `timestamper` (the offer mirrors
/// the concluded game's timing mode). `accept_until` is an absolute unix-seconds
/// deadline; the caller sets it from the stamped `created_at` plus its window.
/// The `nonce` (NIP-13) is added by the publish path's miner, not here; the
/// content is empty, as for a 3418/3420 founding.
///
/// The caller passes whichever Adjudication of `concluded` it holds; it need not
/// be the one the opponent's mirror cites, and the pair still matches on
/// `rematch_of`.
#[must_use]
pub fn build_offer_tags(
    concluded: &EventId,
    adjudication: &EventId,
    opponent: &PublicKey,
    arbiter: &PublicKey,
    timestamper: Option<&PublicKey>,
    accept_until: u64,
) -> Vec<Tag> {
    let mut tags = vec![
        marked_e(concluded, "rematch_of"),
        marked_e(adjudication, "concluded_by"),
        p_role(opponent, "opponent"),
        p_role(arbiter, "arbiter"),
    ];
    if let Some(ts) = timestamper {
        tags.push(p_role(ts, "timestamper"));
    }
    tags.push(single("accept_until", &accept_until.to_string()));
    tags
}

/// A `["p", <pubkey>, "", <role>]` tag.
fn p_role(pubkey: &PublicKey, role: &str) -> Tag {
    Tag::custom("p", [pubkey.to_hex(), String::new(), role.to_string()])
}

/// A `["e", <id>, "", <marker>]` tag.
fn marked_e(id: &EventId, marker: &str) -> Tag {
    Tag::custom("e", [id.to_hex(), String::new(), marker.to_string()])
}

/// A `[<name>, <value>]` singleton tag.
fn single(name: &str, value: &str) -> Tag {
    Tag::custom(name, [value.to_string()])
}

#[cfg(test)]
mod tests {
    // Concise `expect` on values statically known to be present; the
    // panic-avoidance lints target production paths, not tests.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn keys() -> Keys {
        Keys::generate()
    }

    /// A real Game Session id to rematch (only its id is used).
    fn a_concluded_id() -> EventId {
        EventBuilder::new(Kind::Custom(GAME_SESSION_KIND), "feen")
            .finalize(&keys())
            .expect("sign")
            .id
    }

    /// A real Adjudication id to cite as `concluded_by` (only its id is used).
    fn an_adjudication_id() -> EventId {
        EventBuilder::new(Kind::Custom(ADJUDICATION_KIND), "checkmate")
            .finalize(&keys())
            .expect("sign")
            .id
    }

    const GAME_SESSION_KIND: u16 = 3422;
    const DIRECT_CHALLENGE_KIND: u16 = 3420;
    const ADJUDICATION_KIND: u16 = 3425;

    /// Build a signed 3430 the way the app or the load-test harness would, with
    /// `created_at` set an hour before its deadline (a live window).
    fn offer_event(
        signer: &Keys,
        opponent: &PublicKey,
        concluded: &EventId,
        adjudication: &EventId,
        arbiter: &PublicKey,
        timestamper: Option<&PublicKey>,
        accept_until: u64,
    ) -> Event {
        let tags = build_offer_tags(
            concluded,
            adjudication,
            opponent,
            arbiter,
            timestamper,
            accept_until,
        );
        EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from_secs(accept_until.saturating_sub(3_600)))
            .finalize(signer)
            .expect("sign")
    }

    #[test]
    fn parses_a_conforming_self_timed_offer() {
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let adjudication = an_adjudication_id();
        let event = offer_event(
            &signer,
            &me,
            &concluded,
            &adjudication,
            &arbiter,
            None,
            2_000_000_000,
        );

        let parsed = parse_incoming_offer(&event).expect("well-formed offer parses");
        assert_eq!(parsed.offerer, signer.public_key());
        assert_eq!(parsed.addressed_to, me);
        assert_eq!(parsed.concluded, concluded);
        assert_eq!(parsed.concluded_by, adjudication);
        assert_eq!(parsed.arbiter, arbiter);
        assert_eq!(parsed.timestamper, None);
        assert_eq!(parsed.accept_until, 2_000_000_000);
    }

    #[test]
    fn parses_the_attested_timestamper() {
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let ts = keys().public_key();
        let concluded = a_concluded_id();
        let event = offer_event(
            &signer,
            &me,
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            Some(&ts),
            2_000_000_000,
        );
        assert_eq!(
            parse_incoming_offer(&event).expect("parses").timestamper,
            Some(ts)
        );
    }

    #[test]
    fn rejects_the_wrong_kind() {
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        // The same tag shape carried by a Direct Challenge kind is not a 3430.
        let tags = build_offer_tags(
            &concluded,
            &an_adjudication_id(),
            &me,
            &arbiter,
            None,
            2_000_000_000,
        );
        let event = EventBuilder::new(Kind::Custom(DIRECT_CHALLENGE_KIND), "")
            .tags(tags)
            .finalize(&signer)
            .expect("sign");
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn rejects_a_missing_opponent() {
        let signer = keys();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let event = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(vec![
                marked_e(&concluded, "rematch_of"),
                marked_e(&an_adjudication_id(), "concluded_by"),
                p_role(&arbiter, "arbiter"),
                single("accept_until", "2000000000"),
            ])
            .finalize(&signer)
            .expect("sign");
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn rejects_a_missing_concluded_by() {
        // An offer that names no Adjudication proves nothing about the game
        // being over: the reference is REQUIRED, so the offer is malformed.
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let event = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(vec![
                marked_e(&concluded, "rematch_of"),
                p_role(&me, "opponent"),
                p_role(&arbiter, "arbiter"),
                single("accept_until", "2000000000"),
            ])
            .custom_created_at(Timestamp::from_secs(1_999_996_400))
            .finalize(&signer)
            .expect("sign");
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn rejects_a_repeated_concluded_by() {
        // Two cited Adjudications leave which one is being cited undetermined —
        // "exactly one" is the requirement, and first-match-wins would hide it.
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let event = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(vec![
                marked_e(&concluded, "rematch_of"),
                marked_e(&an_adjudication_id(), "concluded_by"),
                marked_e(&an_adjudication_id(), "concluded_by"),
                p_role(&me, "opponent"),
                p_role(&arbiter, "arbiter"),
                single("accept_until", "2000000000"),
            ])
            .custom_created_at(Timestamp::from_secs(1_999_996_400))
            .finalize(&signer)
            .expect("sign");
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn rejects_a_repeated_rematch_of() {
        // Two named sessions leave which one is being replayed undetermined; tag
        // order is not a designation (constraint 1).
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let event = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(vec![
                marked_e(&a_concluded_id(), "rematch_of"),
                marked_e(&a_concluded_id(), "rematch_of"),
                marked_e(&an_adjudication_id(), "concluded_by"),
                p_role(&me, "opponent"),
                p_role(&arbiter, "arbiter"),
                single("accept_until", "2000000000"),
            ])
            .custom_created_at(Timestamp::from_secs(1_999_996_400))
            .finalize(&signer)
            .expect("sign");
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn rejects_a_self_addressed_offer() {
        let signer = keys();
        let me = signer.public_key(); // opponent == signer
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let event = offer_event(
            &signer,
            &me,
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn rejects_a_deadline_not_in_the_future() {
        let signer = keys();
        let me = keys().public_key();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        // created_at == accept_until: the deadline is reached at publication.
        let event = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(build_offer_tags(
                &concluded,
                &an_adjudication_id(),
                &me,
                &arbiter,
                None,
                1_000,
            ))
            .custom_created_at(Timestamp::from_secs(1_000))
            .finalize(&signer)
            .expect("sign");
        assert!(parse_incoming_offer(&event).is_none());
    }

    #[test]
    fn a_built_offer_round_trips_through_the_parser() {
        // What the bot builds, the bot (and the arbiter) can read back.
        let signer = keys();
        let opponent = keys().public_key();
        let arbiter = keys().public_key();
        let ts = keys().public_key();
        let concluded = a_concluded_id();
        let adjudication = an_adjudication_id();
        let event = offer_event(
            &signer,
            &opponent,
            &concluded,
            &adjudication,
            &arbiter,
            Some(&ts),
            2_000_000_000,
        );
        let parsed = parse_incoming_offer(&event).expect("round-trips");
        assert_eq!(parsed.concluded, concluded);
        assert_eq!(parsed.concluded_by, adjudication);
        assert_eq!(parsed.addressed_to, opponent);
        assert_eq!(parsed.arbiter, arbiter);
        assert_eq!(parsed.timestamper, Some(ts));
    }

    #[test]
    fn a_built_offer_carries_no_inherited_terms() {
        let opponent = keys().public_key();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let adjudication = an_adjudication_id();
        let tags = build_offer_tags(
            &concluded,
            &adjudication,
            &opponent,
            &arbiter,
            None,
            2_000_000_000,
        );
        let event = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(tags)
            .finalize(&keys())
            .expect("sign");
        assert_eq!(
            tags::event_with_marker(&event, "rematch_of"),
            Some(concluded)
        );
        // Exactly one `concluded_by`, distinct from the session it rematches.
        assert_eq!(
            tags::sole_event_with_marker(&event, "concluded_by"),
            Some(adjudication)
        );
        assert_eq!(tags::pubkey_with_role(&event, "opponent"), Some(opponent));
        assert_eq!(tags::pubkey_with_role(&event, "timestamper"), None);
        assert_eq!(tags::accept_until(&event), Some("2000000000"));
        // The terms live on the concluded session, never restated on the offer.
        assert_eq!(tags::game(&event), None);
        assert!(tags::seat(&event).is_none());
        assert!(tags::time_control_rows(&event).is_empty());
    }

    /// The two mutually-addressed offers of a conforming pair, plus the arbiter
    /// they both designate.
    fn a_pair(concluded: &EventId, timestamper: Option<&PublicKey>) -> (Event, Event, PublicKey) {
        let alice = keys();
        let bob = keys();
        let arbiter = keys().public_key();
        let first = offer_event(
            &alice,
            &bob.public_key(),
            concluded,
            &an_adjudication_id(),
            &arbiter,
            timestamper,
            2_000_000_000,
        );
        let second = offer_event(
            &bob,
            &alice.public_key(),
            concluded,
            // A DIFFERENT Adjudication of the same session — a session may carry
            // several, and the pair is keyed by `rematch_of` alone.
            &an_adjudication_id(),
            &arbiter,
            timestamper,
            2_000_000_000,
        );
        (first, second, arbiter)
    }

    #[test]
    fn pairs_two_mutual_offers_citing_different_adjudications() {
        let concluded = a_concluded_id();
        let (first, second, arbiter) = a_pair(&concluded, None);
        assert_ne!(
            tags::sole_event_with_marker(&first, "concluded_by"),
            tags::sole_event_with_marker(&second, "concluded_by"),
            "the fixture must exercise the different-Adjudication case"
        );
        let pair = founding_pair([&first, &second], &arbiter).expect("a conforming pair");
        assert_eq!(pair.concluded, concluded);
        assert_eq!(pair.timestamper, None);
        // Order between the two offers is immaterial.
        assert_eq!(
            founding_pair([&second, &first], &arbiter).expect("either order"),
            pair
        );
    }

    #[test]
    fn carries_the_attested_mode_both_offers_claim() {
        let ts = keys().public_key();
        let concluded = a_concluded_id();
        let (first, second, arbiter) = a_pair(&concluded, Some(&ts));
        let pair = founding_pair([&first, &second], &arbiter).expect("a conforming pair");
        assert_eq!(pair.timestamper, Some(ts));
    }

    #[test]
    fn rejects_a_pair_under_another_arbiter() {
        let concluded = a_concluded_id();
        let (first, second, _) = a_pair(&concluded, None);
        let stranger = keys().public_key();
        assert_eq!(
            founding_pair([&first, &second], &stranger),
            Err("offer designates another arbiter")
        );
    }

    #[test]
    fn rejects_a_non_offer_reference() {
        let concluded = a_concluded_id();
        let (first, second, arbiter) = a_pair(&concluded, None);
        let impostor = EventBuilder::new(Kind::Custom(DIRECT_CHALLENGE_KIND), "")
            .tags(second.tags.to_vec())
            .finalize(&keys())
            .expect("sign");
        assert_eq!(
            founding_pair([&first, &impostor], &arbiter),
            Err("founding reference is not a rematch offer")
        );
    }

    #[test]
    fn rejects_one_offer_counted_twice() {
        // A session naming the same offer in both slots has one consent, not two.
        let concluded = a_concluded_id();
        let (first, _, arbiter) = a_pair(&concluded, None);
        assert_eq!(
            founding_pair([&first, &first], &arbiter),
            Err("the two founding references are one offer")
        );
    }

    #[test]
    fn rejects_two_offers_from_the_same_player() {
        // Distinct events, but one player consenting twice: still not a pair.
        let concluded = a_concluded_id();
        let alice = keys();
        let bob = keys().public_key();
        let arbiter = keys().public_key();
        let first = offer_event(
            &alice,
            &bob,
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        let again = offer_event(
            &alice,
            &bob,
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            1_999_999_999,
        );
        assert_ne!(first.id, again.id);
        assert_eq!(
            founding_pair([&first, &again], &arbiter),
            Err("both offers signed by the same player")
        );
    }

    #[test]
    fn rejects_offers_that_do_not_address_each_other() {
        let concluded = a_concluded_id();
        let alice = keys();
        let bob = keys();
        let carol = keys().public_key();
        let arbiter = keys().public_key();
        let first = offer_event(
            &alice,
            &bob.public_key(),
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        // Bob answers a third player, not Alice.
        let second = offer_event(
            &bob,
            &carol,
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        assert_eq!(
            founding_pair([&first, &second], &arbiter),
            Err("offers are not mutually addressed")
        );
    }

    #[test]
    fn rejects_offers_replaying_different_sessions() {
        let alice = keys();
        let bob = keys();
        let arbiter = keys().public_key();
        let first = offer_event(
            &alice,
            &bob.public_key(),
            &a_concluded_id(),
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        let second = offer_event(
            &bob,
            &alice.public_key(),
            &a_concluded_id(),
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        assert_eq!(
            founding_pair([&first, &second], &arbiter),
            Err("offers replay different concluded sessions")
        );
    }

    #[test]
    fn rejects_a_doubled_rematch_of() {
        // Which session is replayed is undetermined; first-match-wins would
        // found a session on a guess.
        let alice = keys();
        let bob = keys();
        let arbiter = keys().public_key();
        let concluded = a_concluded_id();
        let first = EventBuilder::new(Kind::Custom(REMATCH_OFFER_KIND), "")
            .tags(vec![
                marked_e(&concluded, "rematch_of"),
                marked_e(&a_concluded_id(), "rematch_of"),
                marked_e(&an_adjudication_id(), "concluded_by"),
                p_role(&bob.public_key(), "opponent"),
                p_role(&arbiter, "arbiter"),
                single("accept_until", "2000000000"),
            ])
            .finalize(&alice)
            .expect("sign");
        let second = offer_event(
            &bob,
            &alice.public_key(),
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        assert_eq!(
            founding_pair([&first, &second], &arbiter),
            Err("offer has no sole rematch_of")
        );
    }

    #[test]
    fn rejects_offers_disagreeing_on_the_timing_mode() {
        // One offer claims attestation, the other self-timing: the pair founds a
        // session nobody could rule (kind 3430 §Semantic constraints 6).
        let alice = keys();
        let bob = keys();
        let arbiter = keys().public_key();
        let ts = keys().public_key();
        let concluded = a_concluded_id();
        let first = offer_event(
            &alice,
            &bob.public_key(),
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            Some(&ts),
            2_000_000_000,
        );
        let second = offer_event(
            &bob,
            &alice.public_key(),
            &concluded,
            &an_adjudication_id(),
            &arbiter,
            None,
            2_000_000_000,
        );
        assert_eq!(
            founding_pair([&first, &second], &arbiter),
            Err("offers disagree on the timing mode")
        );
    }

    #[test]
    fn willingness_is_deterministic_per_game() {
        let g = a_concluded_id();
        assert_eq!(wants_rematch(42, &g, 0.75), wants_rematch(42, &g, 0.75));
        // The extremes are total.
        assert!(wants_rematch(42, &g, 1.0)); // a unit draw is always < 1.0
        assert!(!wants_rematch(42, &g, 0.0)); // and never < 0.0
    }

    #[test]
    fn willingness_depends_on_the_seed() {
        // Deterministic inputs (no ambient RNG): a fixed game and a fixed seed
        // range. The verdict must not be constant across seeds — the seed is
        // folded in, so different personas judge the same game differently.
        let hex = "da".repeat(32); // 64 hex chars → a 32-byte id
        let g = EventId::parse(&hex).expect("valid id");
        let (mut seen_true, mut seen_false) = (false, false);
        for seed in 0..64_u64 {
            if wants_rematch(seed, &g, 0.5) {
                seen_true = true;
            } else {
                seen_false = true;
            }
        }
        assert!(seen_true && seen_false, "willingness ignores the seed");
    }

    #[test]
    fn willingness_is_roughly_the_configured_share() {
        // Over many distinct games at a fixed seed, about the configured share
        // (the 0.75 default) are accepted.
        let mut accepted = 0_u32;
        let total = 2_000_u32;
        for i in 0..total {
            // Deterministic distinct ids (no ambient RNG): i as a 64-hex id.
            let g = EventId::parse(&format!("{i:064x}")).expect("valid id");
            if wants_rematch(7, &g, DEFAULT_REMATCH_PROBABILITY) {
                accepted = accepted.saturating_add(1);
            }
        }
        // 0.75 × 2000 = 1500; a generous band absorbs the finite sample.
        assert!(
            (1_350..=1_650).contains(&accepted),
            "accepted {accepted}/{total}"
        );
    }
}
