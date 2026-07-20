//! Translate received Nostr suite events into the abstract types that
//! `sashite-sanki-arbiter` reasons about.
//!
//! These are **pure** functions: each takes already-received,
//! signature-verified events (the relay I/O lives in `main.rs`) and produces the
//! arbiter's typed model, or an error if a required tag or field is missing or
//! malformed. The arbiter's identity newtypes are referenced through the `event`
//! module path (`event::EventId`, `event::PublicKey`) to avoid clashing with the
//! Nostr `EventId` / `PublicKey` brought in by the prelude.

use crate::tags;
use anyhow::{anyhow, bail, Context, Result};
use nostr_sdk::prelude::*;
use sashite_sanki_arbiter::event::{self, AdjudicationRequest, Attestation, Ply};
use sashite_sanki_arbiter::session::SessionParams;
use sashite_sanki_engine::domain::time::Duration;
use sashite_sanki_engine::domain::time_control::{Period, TimeControl};
use sashite_sanki_engine::position::Position;

/// Map a Ply (kind `6423`) to the arbiter's [`Ply`].
pub fn ply(event: &Event) -> Result<Ply> {
    let session = tags::event_with_marker(event, "game_session")
        .ok_or_else(|| anyhow!("Ply references no Game Session"))?;
    Ok(Ply::new(
        arbiter_id(event.id)?,
        arbiter_key(event.pubkey)?,
        arbiter_id(session)?,
        step(event)?,
        has_tag(event, "draw"),
        event.content.clone(),
        // The relay-enforced created_at — the canonical timing when self-timed.
        engine_timestamp(event.created_at)?,
    ))
}

/// Map an Event Timestamp Attestation (kind `1041`) to the arbiter's
/// [`Attestation`]. Authority (is it the designated timestamper?) is the
/// arbiter's concern, not this mapping's. (Unused while v1 is self-timed —
/// kept for the attested-mode extension, ADR-0014 §13.)
#[allow(dead_code)]
pub fn attestation(event: &Event) -> Result<Attestation> {
    let attests = tags::event_with_marker(event, "attests")
        .ok_or_else(|| anyhow!("Attestation has no `attests` reference"))?;
    Ok(Attestation::new(
        arbiter_id(event.id)?,
        arbiter_key(event.pubkey)?,
        arbiter_id(attests)?,
        engine_timestamp(event.created_at)?,
    ))
}

/// Map an Adjudication Request (kind `6424`) to the arbiter's
/// [`AdjudicationRequest`]. (Unused while the bot predicts through its
/// synthetic probe; kept for parity with the sibling services.)
#[allow(dead_code)]
pub fn request(event: &Event) -> Result<AdjudicationRequest> {
    let session = tags::event_with_marker(event, "game_session")
        .ok_or_else(|| anyhow!("Adjudication Request references no Game Session"))?;
    let arbiter = tags::pubkey_with_role(event, "arbiter")
        .ok_or_else(|| anyhow!("Adjudication Request names no arbiter"))?;
    Ok(AdjudicationRequest::new(
        arbiter_id(event.id)?,
        arbiter_key(event.pubkey)?,
        arbiter_id(session)?,
        arbiter_key(arbiter)?,
        // The relay-enforced created_at — the canonical cutoff timing when self-timed.
        engine_timestamp(event.created_at)?,
    ))
}

/// Assemble [`SessionParams`] from the Game Session and the founding values the
/// caller has already resolved. Founding-agnostic, so it serves both the directed
/// (Accepted Challenge / Direct Challenge) and the matchmade (Pairing) paths:
/// - `session` — the Game Session (kind `6422`): players, seats, arbiter
///   (its signer), initial position (its content), and id;
/// - `timestamper` — the designated timestamper, or `None` when the founding
///   designates none (self-timed — the default; attestation is a dormant
///   capability): from the Accepted Challenge (kind `6421`) on the directed path,
///   or from the Pairing (kind `6419`) on the matchmade path;
/// - `time_control` — the agreed time control: from the Direct Challenge
///   (kind `6420`), or from the Pairing (kind `6419`);
/// - `session_attestation` — in attested mode, the timestamper's Attestation
///   (kind `1041`) of the Game Session, whose `created_at` is t₀. `None` when
///   self-timed, in which case t₀ is the Game Session's own `created_at`.
pub fn session_params(
    session: &Event,
    timestamper: Option<PublicKey>,
    time_control: TimeControl,
    session_attestation: Option<&Event>,
) -> Result<SessionParams> {
    let players = tags::pubkeys_with_role(session, "player");
    let (one, other) = match players.as_slice() {
        [one, other] => (*one, *other),
        _ => bail!(
            "Game Session must reference exactly two players, found {}",
            players.len()
        ),
    };

    let one_seat = tags::seat_for(session, &one)
        .ok_or_else(|| anyhow!("Game Session declares no seat for a player"))?;
    let other_seat = tags::seat_for(session, &other)
        .ok_or_else(|| anyhow!("Game Session declares no seat for a player"))?;
    let (first, second) = match (one_seat, other_seat) {
        ("first", "second") => (one, other),
        ("second", "first") => (other, one),
        _ => bail!("Game Session seats are not a first/second pair"),
    };

    // The Game Session is signed by the arbiter.
    let arbiter = session.pubkey;

    // t₀ (the anchor): in attested mode, the timestamper's attestation of THIS
    // session; when self-timed, the Game Session's own (relay-enforced) created_at.
    let anchor = match (timestamper, session_attestation) {
        (Some(ts), Some(attestation)) => {
            let attested = tags::event_with_marker(attestation, "attests")
                .ok_or_else(|| anyhow!("session attestation has no `attests` reference"))?;
            if attested != session.id {
                bail!("session attestation does not attest the Game Session");
            }
            if attestation.pubkey != ts {
                bail!("session attestation is not signed by the designated timestamper");
            }
            engine_timestamp(attestation.created_at)?
        }
        (None, _) => engine_timestamp(session.created_at)?,
        (Some(_), None) => bail!("attested session but no session attestation provided"),
    };

    let initial_position = Position::parse(&session.content)
        .map_err(|e| anyhow!("Game Session content is not a valid FEEN: {e}"))?;

    Ok(SessionParams::new(
        arbiter_id(session.id)?,
        arbiter_key(arbiter)?,
        timestamper.map(arbiter_key).transpose()?,
        arbiter_key(first)?,
        arbiter_key(second)?,
        time_control,
        initial_position,
        anchor,
    ))
}

/// Build a [`TimeControl`] from the `time_control` tag(s) of a Direct Challenge
/// (kind `6420`) or a Pairing (kind `6419`) — both carry the same tag format.
pub fn time_control(event: &Event) -> Result<TimeControl> {
    let mut periods = Vec::new();
    for tag in event.tags.iter() {
        let slice = tag.as_slice();
        if slice.first().map(String::as_str) != Some("time_control") {
            continue;
        }
        let duration = slice
            .get(1)
            .ok_or_else(|| anyhow!("time_control tag has no duration"))?
            .parse::<u64>()
            .context("time_control duration is not an integer")?;
        let increment = match slice.get(2) {
            Some(value) if !value.is_empty() => Some(Duration::from_secs(
                value
                    .parse::<u64>()
                    .context("time_control increment is not an integer")?,
            )),
            _ => None,
        };
        let plies = match slice.get(3) {
            Some(value) if !value.is_empty() => Some(
                value
                    .parse::<u32>()
                    .context("time_control plies is not an integer")?,
            ),
            _ => None,
        };
        let period = Period::new(Duration::from_secs(duration), increment, plies)
            .map_err(|e| anyhow!("invalid time_control period: {e}"))?;
        periods.push(period);
    }
    TimeControl::from_periods(periods).map_err(|e| anyhow!("invalid time control: {e}"))
}

/// The `step` of a Ply: the signer's own move ordinal (a positive integer);
/// the arbiter interprets it per kind `6423` §Step semantics and play order.
fn step(event: &Event) -> Result<u32> {
    let value = tag_value(event, "step").ok_or_else(|| anyhow!("Ply has no `step` tag"))?;
    let step = value
        .parse::<u32>()
        .context("Ply step is not a non-negative integer")?;
    if step == 0 {
        bail!("Ply step must be >= 1");
    }
    Ok(step)
}

/// The value (second element) of the first tag named `name`.
fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        match slice.first() {
            Some(key) if key == name => slice.get(1).map(String::as_str),
            _ => None,
        }
    })
}

/// Whether a (flag) tag named `name` is present.
fn has_tag(event: &Event, name: &str) -> bool {
    event
        .tags
        .iter()
        .any(|tag| tag.as_slice().first().map(String::as_str) == Some(name))
}

fn arbiter_id(id: EventId) -> Result<event::EventId> {
    event::EventId::parse(&id.to_hex()).ok_or_else(|| anyhow!("malformed event id {id}"))
}

fn arbiter_key(pubkey: PublicKey) -> Result<event::PublicKey> {
    event::PublicKey::parse(&pubkey.to_hex()).ok_or_else(|| anyhow!("malformed pubkey {pubkey}"))
}

fn engine_timestamp(
    created_at: Timestamp,
) -> Result<sashite_sanki_engine::domain::time::Timestamp> {
    let secs = i64::try_from(created_at.as_secs()).context("created_at out of range")?;
    Ok(sashite_sanki_engine::domain::time::Timestamp::from_unix(
        secs,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use sashite_sanki_engine::domain::side::Side;

    const CHESS_START: &str =
        "-rnbqk^bn-r/+p+p+p+p+p+p+p+p/8/8/8/8/+P+P+P+P+P+P+P+P/-RNBQK^BN-R / W/w";

    fn sign(kind: u16, content: &str, tags: Vec<Tag>, signer: &Keys) -> Event {
        EventBuilder::new(Kind::Custom(kind), content)
            .tags(tags)
            .sign_with_keys(signer)
            .expect("sign test event")
    }

    fn dummy_id() -> EventId {
        EventId::parse(&"a".repeat(64)).expect("valid event id")
    }

    fn marked_e(id: &EventId, marker: &str) -> Tag {
        Tag::custom(
            TagKind::e(),
            [id.to_hex(), String::new(), marker.to_string()],
        )
    }

    fn p_role(pubkey: &PublicKey, role: &str) -> Tag {
        Tag::custom(
            TagKind::p(),
            [pubkey.to_hex(), String::new(), role.to_string()],
        )
    }

    fn player(pubkey: &PublicKey) -> Tag {
        Tag::custom(
            TagKind::p(),
            [pubkey.to_hex(), String::new(), "player".to_string()],
        )
    }

    fn keyed(name: &str, pubkey: &PublicKey, value: &str) -> Tag {
        Tag::custom(TagKind::custom(name), [pubkey.to_hex(), value.to_string()])
    }

    #[test]
    fn maps_a_ply() {
        let mover = Keys::generate();
        let session_id = dummy_id();
        let event = sign(
            6423,
            "e4",
            vec![
                marked_e(&session_id, "game_session"),
                Tag::custom(TagKind::custom("step"), ["3"]),
            ],
            &mover,
        );

        let mapped = ply(&event).unwrap();
        assert_eq!(mapped.step, 3);
        assert!(!mapped.draw);
        assert_eq!(mapped.content, "e4");
        assert_eq!(mapped.signer.to_string(), mover.public_key().to_hex());
        assert_eq!(mapped.session.to_string(), session_id.to_hex());
        assert_eq!(mapped.id.to_string(), event.id.to_hex());
    }

    #[test]
    fn ply_reads_the_draw_flag_and_rejects_step_zero() {
        let mover = Keys::generate();
        let session_id = dummy_id();
        let with_draw = sign(
            6423,
            "e4",
            vec![
                marked_e(&session_id, "game_session"),
                Tag::custom(TagKind::custom("step"), ["1"]),
                Tag::custom(TagKind::custom("draw"), Vec::<String>::new()),
            ],
            &mover,
        );
        assert!(ply(&with_draw).unwrap().draw);

        let step_zero = sign(
            6423,
            "e4",
            vec![
                marked_e(&session_id, "game_session"),
                Tag::custom(TagKind::custom("step"), ["0"]),
            ],
            &mover,
        );
        assert!(ply(&step_zero).is_err());
    }

    #[test]
    fn maps_an_attestation() {
        let timestamper = Keys::generate();
        let attested = dummy_id();
        let event = sign(1041, "", vec![marked_e(&attested, "attests")], &timestamper);

        let mapped = attestation(&event).unwrap();
        assert_eq!(mapped.attests.to_string(), attested.to_hex());
        assert_eq!(mapped.signer.to_string(), timestamper.public_key().to_hex());
        assert_eq!(
            mapped.created_at.as_unix() as u64,
            event.created_at.as_secs()
        );
    }

    #[test]
    fn maps_a_request() {
        let invoker = Keys::generate();
        let arbiter = Keys::generate().public_key();
        let session_id = dummy_id();
        let event = sign(
            6424,
            "",
            vec![
                marked_e(&session_id, "game_session"),
                p_role(&arbiter, "arbiter"),
            ],
            &invoker,
        );

        let mapped = request(&event).unwrap();
        assert_eq!(mapped.session.to_string(), session_id.to_hex());
        assert_eq!(mapped.arbiter.to_string(), arbiter.to_hex());
        assert_eq!(mapped.signer.to_string(), invoker.public_key().to_hex());
    }

    #[test]
    fn parses_a_single_period_time_control() {
        let challenger = Keys::generate();
        let direct = sign(
            6420,
            "",
            vec![Tag::custom(TagKind::custom("time_control"), ["300", "5"])],
            &challenger,
        );
        let tc = time_control(&direct).unwrap();
        assert_eq!(tc.first().duration().as_secs(), 300);
        assert_eq!(tc.first().increment(), Some(Duration::from_secs(5)));
        assert_eq!(tc.period_count(), 1);
    }

    #[test]
    fn assembles_session_params_from_the_founding_chain() {
        let arbiter = Keys::generate();
        let timestamper = Keys::generate();
        let alice = Keys::generate(); // first
        let bob = Keys::generate(); // second

        let direct = sign(
            6420,
            "",
            vec![Tag::custom(TagKind::custom("time_control"), ["300", "5"])],
            &alice,
        );
        let session = sign(
            6422,
            CHESS_START,
            vec![
                player(&alice.public_key()),
                player(&bob.public_key()),
                keyed("seat", &alice.public_key(), "first"),
                keyed("seat", &bob.public_key(), "second"),
            ],
            &arbiter,
        );
        let session_attestation = sign(
            1041,
            "",
            vec![marked_e(&session.id, "attests")],
            &timestamper,
        );

        let tc = time_control(&direct).unwrap();
        let params = session_params(
            &session,
            Some(timestamper.public_key()),
            tc,
            Some(&session_attestation),
        )
        .unwrap();

        assert_eq!(params.session().to_string(), session.id.to_hex());
        assert_eq!(params.arbiter().to_string(), arbiter.public_key().to_hex());
        assert_eq!(
            params.timestamper().map(|t| t.to_string()),
            Some(timestamper.public_key().to_hex())
        );
        assert_eq!(
            params.player(Side::First).to_string(),
            alice.public_key().to_hex()
        );
        assert_eq!(
            params.player(Side::Second).to_string(),
            bob.public_key().to_hex()
        );
        assert_eq!(
            params.anchor().as_unix() as u64,
            session_attestation.created_at.as_secs()
        );
        assert_eq!(params.time_control().first().duration().as_secs(), 300);
    }

    #[test]
    fn session_params_self_timed_uses_game_session_created_at() {
        // No timestamper and no session attestation: t₀ is the Game Session's own
        // created_at, and the params carry no timestamper.
        let arbiter = Keys::generate();
        let alice = Keys::generate();
        let bob = Keys::generate();
        let direct = sign(
            6420,
            "",
            vec![Tag::custom(TagKind::custom("time_control"), ["300", "5"])],
            &alice,
        );
        let session = sign(
            6422,
            CHESS_START,
            vec![
                player(&alice.public_key()),
                player(&bob.public_key()),
                keyed("seat", &alice.public_key(), "first"),
                keyed("seat", &bob.public_key(), "second"),
            ],
            &arbiter,
        );
        let tc = time_control(&direct).unwrap();
        let params = session_params(&session, None, tc, None).unwrap();
        assert_eq!(params.timestamper(), None);
        assert_eq!(
            params.anchor().as_unix() as u64,
            session.created_at.as_secs()
        );
    }

    #[test]
    fn session_params_rejects_a_foreign_attestation() {
        let arbiter = Keys::generate();
        let timestamper = Keys::generate();
        let alice = Keys::generate();
        let bob = Keys::generate();

        let direct = sign(
            6420,
            "",
            vec![Tag::custom(TagKind::custom("time_control"), ["300", "5"])],
            &alice,
        );
        let session = sign(
            6422,
            CHESS_START,
            vec![
                player(&alice.public_key()),
                player(&bob.public_key()),
                keyed("seat", &alice.public_key(), "first"),
                keyed("seat", &bob.public_key(), "second"),
            ],
            &arbiter,
        );

        // An attestation of some OTHER event, not this session.
        let wrong_target = sign(
            1041,
            "",
            vec![marked_e(&dummy_id(), "attests")],
            &timestamper,
        );
        assert!(session_params(
            &session,
            Some(timestamper.public_key()),
            time_control(&direct).unwrap(),
            Some(&wrong_target)
        )
        .is_err());

        // An attestation of this session, but signed by a non-timestamper.
        let impostor = sign(1041, "", vec![marked_e(&session.id, "attests")], &alice);
        assert!(session_params(
            &session,
            Some(timestamper.public_key()),
            time_control(&direct).unwrap(),
            Some(&impostor)
        )
        .is_err());
    }
}
