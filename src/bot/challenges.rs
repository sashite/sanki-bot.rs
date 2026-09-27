// SPDX-License-Identifier: Apache-2.0
//! The I/O around Direct Challenges (ADR-0045 §5): after the local checks
//! and the reservation, the **network checks** — not already founded, the
//! rating — each a proven read, each fail-closed; then the **founding**;
//! and, the other way, the **sending** of a challenge to a target, and the
//! query that releases a pending challenge's reservation.
//!
//! Every function here runs in its own task: the runtime's loop takes the
//! reservation, spawns the work, and reads its result.

use std::sync::Arc;

use nostr_sdk::prelude::*;
use sashite_sanki_client::cadence::Cadence;
use sashite_sanki_client::drafts;
use sashite_sanki_client::publisher::Outcome;
use sashite_sanki_client::readers::{self, DirectChallenge, Founding, KIND_RATING_ATTESTATION};
use sashite_sanki_client::session::{self, SessionTerms, Timing, KIND_GAME_SESSION};
use sashite_sanki_client::tags::norm_relay;

use super::reads::{self, Read};
use crate::admit::Admission;
use crate::config::{Outgoing, Policy};
use crate::game::GameContext;

/// The game.
pub const GAME: &str = "sanki";

/// A Game Session with its terms.
#[derive(Debug, Clone)]
pub struct Opened {
    /// The Game Session.
    pub session: Event,
    /// Its terms.
    pub terms: SessionTerms,
}

/// How a founding ended.
#[derive(Debug)]
pub enum Founded {
    /// The Game Session is on the relay.
    Session(Box<Opened>),
    /// Sent and never acknowledged, even after the publisher's resolution:
    /// the reservation stands until a resolution settles it. Nothing this
    /// publisher signs comes back as a notification, so the challenge
    /// event is kept with it, for the terms.
    Unknown {
        /// The Game Session as signed.
        session: Event,
        /// The challenge it answers.
        challenge: Box<Event>,
    },
    /// Not founded: a network check refused, or the publication failed.
    Refused(String),
}

/// The network checks (rules 11 and 12), then the founding.
pub async fn found(
    ctx: Arc<GameContext>,
    challenge_event: Event,
    challenge: DirectChallenge,
    admission: Admission,
) -> Founded {
    let relay = &ctx.publisher.settings().relay;
    let me = ctx.me;

    // 11. Not already founded: no Game Session of the bot references it.
    let filter = Filter::new()
        .author(me)
        .kind(Kind::Custom(KIND_GAME_SESSION))
        .event(challenge.id);
    match reads::read(&ctx.client, relay, filter).await {
        Read::ConfirmedAbsent => {}
        Read::Found(_) => return Founded::Refused("already founded".to_owned()),
        Read::Unknown => return Founded::Refused("the founding check was not answered".to_owned()),
    }

    // 12. The rating, under `rating`.
    if let Policy::Rating {
        max_delta,
        authority,
    } = &ctx.config.challenges().policy
    {
        match within_rating(&ctx, *authority, *max_delta, challenge.challenger).await {
            Ok(true) => {}
            Ok(false) => return Founded::Refused("outside the rating window".to_owned()),
            Err(why) => return Founded::Refused(why),
        }
    }

    // The founding.
    let (first, second) = admission.seats(me);
    let (first_variant, second_variant) = admission.variants();
    let pairing = format!("{}/{}", first_variant.name(), second_variant.name());
    let Some(position) = ctx.describe.positions.get(&pairing).cloned() else {
        return Founded::Refused(format!("no initial position for {pairing}"));
    };
    let draft = drafts::GameSession {
        plan: drafts::SessionPlan {
            founding: Founding::DirectChallenge(challenge.id),
            game: GAME.to_owned(),
            rules: ctx.config.connection().rules,
            timing_relay: ctx.config.connection().relay.to_string(),
            first,
            second,
            first_variant: first_variant.name().to_owned(),
            second_variant: second_variant.name().to_owned(),
            position,
        },
        not_before: challenge.created_at,
        not_after: admission.accept_until,
    };
    match ctx.publisher.publish(draft).await {
        Outcome::Accepted(session) => match session::terms(&session, &challenge_event) {
            Ok(terms) => Founded::Session(Box::new(Opened { session, terms })),
            Err(why) => {
                Founded::Refused(format!("the founded session's terms do not hold: {why:?}"))
            }
        },
        Outcome::Unknown(session) => Founded::Unknown {
            session,
            challenge: Box::new(challenge_event),
        },
        Outcome::Rejected(rejection) => Founded::Refused(format!("rejected: {rejection}")),
        Outcome::Withheld(why) => Founded::Refused(format!("withheld: {why}")),
        Outcome::Failed(reason) => Founded::Refused(format!("failed: {reason}")),
        Outcome::Closed => Founded::Refused("the publisher is closed".to_owned()),
    }
}

/// Whether both players are rated by `authority` in the `sanki` pool and
/// within `max_delta` (rule 12): an unrated player, or an unknown read,
/// refuses.
async fn within_rating(
    ctx: &GameContext,
    authority: PublicKey,
    max_delta: u32,
    challenger: PublicKey,
) -> Result<bool, String> {
    let relay = &ctx.publisher.settings().relay;
    let filter = Filter::new()
        .author(authority)
        .kind(Kind::Custom(KIND_RATING_ATTESTATION))
        .pubkeys([ctx.me, challenger]);
    let events = reads::paged(&ctx.client, relay, filter)
        .await
        .ok_or_else(|| "the rating read was not answered".to_owned())?;
    let latest = |player: PublicKey| -> Option<f64> {
        events
            .iter()
            .filter_map(|e| readers::rating_attestation(e).ok())
            .filter(|a| a.authority == authority && a.player == player && a.game == GAME)
            .max_by_key(|a| a.created_at)
            .map(|a| a.elo_post)
    };
    let (Some(mine), Some(theirs)) = (latest(ctx.me), latest(challenger)) else {
        return Ok(false);
    };
    Ok((mine - theirs).abs() <= f64::from(max_delta))
}

/// How a sending ended.
#[derive(Debug)]
pub enum Sent {
    /// On the relay.
    Challenge(Event),
    /// Sent and never acknowledged: resolved by id before the target is
    /// challenged again (`NotIdempotent`).
    Unknown(Event),
    /// Not sent.
    Failed(String),
}

/// Sends a challenge to `target` (§5 *Sending one*): both variants fixed
/// to the configured one, the periods, `accept_until = created_at +
/// accept_secs`, the draft's `not_after` its rank in the queue.
pub async fn send(ctx: Arc<GameContext>, outgoing: Outgoing, target: PublicKey) -> Sent {
    let now = ctx.publisher.now();
    let Some(accept_secs) = std::num::NonZeroU64::new(outgoing.accept_secs) else {
        return Sent::Failed("accept_secs is zero".to_owned());
    };
    let draft = drafts::DirectChallenge {
        me: ctx.me,
        target,
        game: GAME.to_owned(),
        rules: ctx.config.connection().rules,
        timing_relay: ctx.config.connection().relay.to_string(),
        time_control: outgoing.time_control.clone(),
        variant: outgoing.variant.name().to_owned(),
        accept_secs,
        not_after: Some(now.saturating_add(outgoing.accept_secs)),
        content: String::new(),
    };
    match ctx.publisher.publish(draft).await {
        Outcome::Accepted(event) => Sent::Challenge(event),
        Outcome::Unknown(event) => Sent::Unknown(event),
        Outcome::Rejected(rejection) => Sent::Failed(format!("rejected: {rejection}")),
        Outcome::Withheld(why) => Sent::Failed(format!("withheld: {why}")),
        Outcome::Failed(reason) => Sent::Failed(format!("failed: {reason}")),
        Outcome::Closed => Sent::Failed("the publisher is closed".to_owned()),
    }
}

/// What the query on a pending challenge of the bot's found.
#[derive(Debug)]
pub enum Answer {
    /// A conforming session by the target: the bot's game.
    Session(Box<Opened>),
    /// No session answers it: the reservation is released.
    None,
    /// The read proved nothing: asked again later.
    Unknown,
}

/// Asks whether a session answers the bot's challenge (§5 *The
/// reservation*): the earliest conforming one by the target, stamped by
/// `accept_until`; a non-conforming session is ignored and logged at
/// `warn`.
pub async fn answered(ctx: Arc<GameContext>, challenge_event: Event) -> Answer {
    let Ok(challenge) = readers::direct_challenge(&challenge_event) else {
        return Answer::None;
    };
    let relay = &ctx.publisher.settings().relay;
    let filter = Filter::new()
        .kind(Kind::Custom(KIND_GAME_SESSION))
        .event(challenge.id);
    let Some(sessions) = reads::fetch(&ctx.client, relay, vec![filter]).await else {
        return Answer::Unknown;
    };
    let mut best: Option<(Event, SessionTerms)> = None;
    for session in sessions {
        match conforming(&ctx, &session, &challenge_event, &challenge) {
            Ok(terms) => {
                let earlier = best
                    .as_ref()
                    .is_some_and(|(b, _)| (b.created_at, b.id) <= (session.created_at, session.id));
                if !earlier {
                    best = Some((session, terms));
                }
            }
            Err(why) => {
                tracing::warn!(session = %session.id, challenge = %challenge.id, why, "a non-conforming session answering our challenge; ignored");
            }
        }
    }
    match best {
        Some((session, terms)) => Answer::Session(Box::new(Opened { session, terms })),
        None => Answer::None,
    }
}

/// Whether `session` answers the bot's `challenge` on its terms.
pub fn conforming(
    ctx: &GameContext,
    session: &Event,
    challenge_event: &Event,
    challenge: &DirectChallenge,
) -> Result<SessionTerms, &'static str> {
    if session.pubkey != challenge.opponent {
        return Err("not signed by the target");
    }
    if session.created_at.as_secs() > challenge.accept_until {
        return Err("stamped after accept_until");
    }
    let terms = session::terms(session, challenge_event).map_err(|_| "terms do not hold")?;
    if !terms_are_ours(ctx, &terms) {
        return Err("not on our terms");
    }
    Ok(terms)
}

/// Whether the session is played under the configured rules, on the
/// configured relay, self-timed, with the bot at the board.
pub fn terms_are_ours(ctx: &GameContext, terms: &SessionTerms) -> bool {
    let connection = ctx.config.connection();
    terms.game == GAME
        && terms.rules == connection.rules
        && terms.is_player(&ctx.me)
        && match &terms.timing {
            Timing::SelfTimed(url) => norm_relay(url) == norm_relay(&connection.relay.to_string()),
            Timing::Attested(_) => false,
        }
}

/// The family of a session's terms: its first period's.
#[must_use]
pub fn cadence_of(terms: &SessionTerms) -> Option<Cadence> {
    super::rebuild::cadence_of(terms)
}
