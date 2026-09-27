// SPDX-License-Identifier: Apache-2.0
//! The rebuild at start (ADR-0045 §7 *Start*, step 7), from queries on the
//! bot's key: its Game Sessions, back to a `since` derived from the longest
//! game its families allow; for each, its Plies and Conclusions; its Direct
//! Challenges of the last day and of the longest game, and **every Game
//! Session answering any of them** — a session a target founded is the
//! bot's game as much as one it founded; the Direct Challenges to the bot
//! still within `accept_until`, for the runtime to re-admit.
//!
//! Open sessions are those without a canonical Conclusion. A session is
//! ignored (and logged) when its founding cannot be read, its terms do not
//! hold, its rules or relay are not the configured ones, or its timing is
//! attested: the bot plays self-timed sessions under one rule system.
//!
//! **The longest game.** Six hundred half-moves at the family's per-move
//! allowance: a day for correspondence, an hour otherwise — the bound this
//! bot reads into the ADR's "longest per-move allowance", the families'
//! own bounds (a day's increment short of correspondence) being formal.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nostr_sdk::prelude::*;
use sashite_sanki_client::cadence::Cadence;
use sashite_sanki_client::chain;
use sashite_sanki_client::module;
use sashite_sanki_client::readers::{self, DirectChallenge, Founding};
use sashite_sanki_client::session::{
    self, Events, SessionTerms, Timing, KIND_CONCLUSION, KIND_DIRECT_CHALLENGE, KIND_GAME_SESSION,
    KIND_PLY,
};
use sashite_sanki_client::tags::norm_relay;

use super::reads;
use crate::config::Config;
use crate::game::{SharedOracle, SKEW_ALLOWANCE_SECS};

/// The longest game, in half-moves.
const LONGEST_GAME_HALF_MOVES: u64 = 600;

/// One day, in seconds.
const DAY: u64 = 86_400;

/// One hour, in seconds.
const HOUR: u64 = 3_600;

/// How many ids one query carries.
const IDS_PER_QUERY: usize = 100;

/// An open session, rebuilt.
#[derive(Debug, Clone)]
pub struct OpenSession {
    /// The Game Session.
    pub session: Event,
    /// Its founding: a Direct Challenge, or a Pairing.
    pub founding: Event,
    /// Its terms.
    pub terms: SessionTerms,
    /// t₀: the Game Session's `created_at` (self-timed).
    pub start: u64,
    /// The family.
    pub cadence: Cadence,
    /// Its Plies and Conclusions so far.
    pub events: Vec<Event>,
}

/// A Direct Challenge the bot sent, unanswered and not lapsed beyond the
/// relay's tolerance: its reservation stands until the runtime's query
/// confirms that no session answers it.
#[derive(Debug, Clone)]
pub struct SentChallenge {
    /// The event.
    pub event: Event,
    /// Read.
    pub challenge: DirectChallenge,
}

/// What the relay says of the bot.
#[derive(Debug, Clone, Default)]
pub struct Rebuilt {
    /// The open sessions.
    pub open: Vec<OpenSession>,
    /// How many sessions were found closed.
    pub closed: usize,
    /// How many sessions the module could not judge: not resumed.
    pub unverified: usize,
    /// The bot's own challenges still pending.
    pub pending: Vec<SentChallenge>,
    /// The challenges to the bot within `accept_until`, unanswered by a
    /// session of the bot's, for the runtime to re-admit.
    pub incoming: Vec<(Event, DirectChallenge)>,
    /// Per target, the `created_at` of the bot's newest challenge to it.
    pub last_sent: BTreeMap<PublicKey, u64>,
    /// The bot's challenges of the last day, to any target.
    pub sent_last_day: u32,
    /// Their stamps, oldest first.
    pub sent_stamps: Vec<u64>,
}

/// Why the rebuild could not be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildError {
    /// The relay did not answer a query of this kind.
    UnknownRead(u16),
}

impl std::fmt::Display for RebuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRead(kind) => {
                write!(f, "the relay did not answer the read of kind {kind}")
            }
        }
    }
}

/// The per-move allowance of a family, in seconds.
#[must_use]
pub const fn per_move_allowance(cadence: Cadence) -> u64 {
    match cadence {
        Cadence::Correspondence => DAY,
        Cadence::Byoyomi | Cadence::Blitz | Cadence::Rapid => HOUR,
    }
}

/// The longest game the configuration's families allow, in seconds. (A
/// session founded under a family the configuration no longer plays and
/// older than this is not resumed: the ADR's bound is the families', and
/// six hundred days of a busy key would cost the start one query per
/// session.)
#[must_use]
pub fn longest_game_secs(config: &Config) -> u64 {
    Cadence::ALL
        .into_iter()
        .filter(|c| config.play().cap(*c) > 0)
        .map(per_move_allowance)
        .max()
        .unwrap_or(HOUR)
        .saturating_mul(LONGEST_GAME_HALF_MOVES)
}

/// The family of a session's terms: its first period's.
#[must_use]
pub fn cadence_of(terms: &SessionTerms) -> Option<Cadence> {
    let [duration, increment, plies] = terms.time_control.first()?;
    let duration = (*duration)?;
    if duration == 0 && plies.is_none() {
        return None;
    }
    Some(Cadence::of_period(duration, increment.unwrap_or(0)))
}

/// What the rebuild reads for.
#[derive(Debug, Clone)]
pub struct Scope<'a> {
    /// The bot's key.
    pub me: PublicKey,
    /// The configuration.
    pub config: &'a Config,
    /// The relay to query — the configured one, which the terms are
    /// compared with.
    pub relay: &'a RelayUrl,
    /// The module's `max_step`.
    pub max_step: u32,
    /// The relay's clock now.
    pub now: u64,
    /// The relay's past tolerance `L`.
    pub past_tolerance: u64,
}

/// Runs the queries and sorts what they return.
///
/// # Errors
///
/// A query the relay did not answer.
pub async fn rebuild(
    client: &Client,
    scope: &Scope<'_>,
    oracle: &SharedOracle,
) -> Result<Rebuilt, RebuildError> {
    let Scope {
        me,
        config,
        relay,
        max_step,
        now,
        past_tolerance,
    } = *scope;
    let longest = longest_game_secs(config);
    let since = Timestamp::from(now.saturating_sub(longest));
    let mut out = Rebuilt::default();

    // The bot's own sessions.
    let mine = query(
        client,
        relay,
        Filter::new()
            .author(me)
            .kind(Kind::Custom(KIND_GAME_SESSION))
            .since(since),
        KIND_GAME_SESSION,
    )
    .await?;

    // The bot's own challenges, and every session answering them.
    let challenge_since = Timestamp::from(now.saturating_sub(longest.max(DAY)));
    let sent = query(
        client,
        relay,
        Filter::new()
            .author(me)
            .kind(Kind::Custom(KIND_DIRECT_CHALLENGE))
            .since(challenge_since),
        KIND_DIRECT_CHALLENGE,
    )
    .await?;
    let sent: Vec<(Event, DirectChallenge)> = sent
        .into_iter()
        .filter_map(|event| match readers::direct_challenge(&event) {
            Ok(challenge) => Some((event, challenge)),
            Err(why) => {
                tracing::warn!(id = %event.id, %why, "a challenge of ours the reader refuses");
                None
            }
        })
        .collect();
    let day_ago = now.saturating_sub(DAY);
    for (event, challenge) in &sent {
        let stamp = event.created_at.as_secs();
        if stamp >= day_ago {
            out.sent_last_day = out.sent_last_day.saturating_add(1);
            out.sent_stamps.push(stamp);
        }
        let last = out.last_sent.entry(challenge.opponent).or_insert(stamp);
        *last = (*last).max(stamp);
    }
    let answering = query_by_ids(
        client,
        relay,
        Kind::Custom(KIND_GAME_SESSION),
        sent.iter().map(|(e, _)| e.id),
        |filter, ids| filter.events(ids),
        KIND_GAME_SESSION,
    )
    .await?;

    // The foundings of the bot's own sessions.
    let foundings_wanted: BTreeSet<EventId> = mine
        .iter()
        .filter_map(|s| readers::founding_of(s).ok().map(Founding::id))
        .collect();
    let mut foundings: HashMap<EventId, Event> = query_by_ids(
        client,
        relay,
        Kind::Custom(KIND_DIRECT_CHALLENGE),
        foundings_wanted.iter().copied(),
        |filter, ids| {
            filter.ids(ids).kinds([
                Kind::Custom(KIND_DIRECT_CHALLENGE),
                Kind::Custom(session::KIND_PAIRING),
            ])
        },
        KIND_DIRECT_CHALLENGE,
    )
    .await?
    .into_iter()
    .map(|e| (e.id, e))
    .collect();
    for (event, _) in &sent {
        foundings.insert(event.id, event.clone());
    }

    // The candidate sessions, with their foundings, checked; one per
    // founding: the earliest conforming.
    let mut by_founding: BTreeMap<EventId, (Event, Event, SessionTerms)> = BTreeMap::new();
    for session in mine.into_iter().chain(answering) {
        let Ok(founding_ref) = readers::founding_of(&session) else {
            continue;
        };
        let Some(founding) = foundings.get(&founding_ref.id()) else {
            tracing::warn!(session = %session.id, "a session whose founding the relay does not hold");
            continue;
        };
        let terms = match session::terms(&session, founding) {
            Ok(terms) => terms,
            Err(why) => {
                tracing::warn!(session = %session.id, ?why, "a session whose terms do not hold");
                continue;
            }
        };
        if !terms_are_ours(&terms, config, me) {
            tracing::warn!(session = %session.id, "a session not on our terms");
            continue;
        }
        if let Ok(challenge) = readers::direct_challenge(founding) {
            if session.created_at.as_secs() > challenge.accept_until {
                tracing::warn!(session = %session.id, "a session stamped after the challenge's accept_until");
                continue;
            }
        }
        let earlier = by_founding
            .get(&founding_ref.id())
            .is_some_and(|(s, _, _)| (s.created_at, s.id) <= (session.created_at, session.id));
        if !earlier {
            by_founding.insert(founding_ref.id(), (session, founding.clone(), terms));
        }
    }

    // Their Plies and Conclusions, one query per session (a chain may run
    // to six hundred half-moves); open or closed.
    let session_ids: Vec<EventId> = by_founding.values().map(|(s, _, _)| s.id).collect();
    let mut events_by_session: HashMap<EventId, Vec<Event>> = HashMap::new();
    for session_id in session_ids {
        let filter = Filter::new()
            .kinds([Kind::Custom(KIND_PLY), Kind::Custom(KIND_CONCLUSION)])
            .event(session_id);
        let events = query(client, relay, filter, KIND_PLY).await?;
        events_by_session.insert(session_id, events);
    }
    let mut answered: BTreeSet<EventId> = BTreeSet::new();
    for (founding_id, (session, founding, terms)) in by_founding {
        answered.insert(founding_id);
        let events = events_by_session.remove(&session.id).unwrap_or_default();
        let start = session.created_at.as_secs();
        let assembled = Events::from_relay(events.iter(), &terms, max_step);
        let request = chain::session_request(&terms, start, &assembled);
        let conclusions: Vec<_> = assembled
            .conclusions
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let closed = if conclusions.is_empty() {
            false
        } else {
            let selected = {
                let mut oracle = oracle
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                module::select_conclusion(&mut *oracle, &request, &conclusions)
            };
            match selected {
                Ok(canonical) => canonical.is_some(),
                Err(why) => {
                    // A guessed state is no state: neither open nor closed,
                    // no engine (ADR-0045 §8).
                    tracing::error!(session = %session.id, %why, "the module could not select a conclusion; the session is not resumed");
                    out.unverified = out.unverified.saturating_add(1);
                    continue;
                }
            }
        };
        if closed {
            out.closed = out.closed.saturating_add(1);
            continue;
        }
        let Some(cadence) = cadence_of(&terms) else {
            tracing::warn!(session = %session.id, "a session without a cadence");
            continue;
        };
        out.open.push(OpenSession {
            session,
            founding,
            terms,
            start,
            cadence,
            events,
        });
    }

    // The bot's challenges still pending: unanswered, not lapsed beyond
    // the tolerance (the runtime confirms with the query §5 prescribes).
    for (event, challenge) in sent {
        if answered.contains(&event.id) {
            continue;
        }
        let lapsed = challenge
            .accept_until
            .saturating_add(past_tolerance)
            .saturating_add(SKEW_ALLOWANCE_SECS);
        if now > lapsed {
            continue;
        }
        out.pending.push(SentChallenge { event, challenge });
    }

    // The challenges to the bot, within `accept_until`, not founded by a
    // session of ours.
    let to_me = query(
        client,
        relay,
        Filter::new()
            .pubkey(me)
            .kind(Kind::Custom(KIND_DIRECT_CHALLENGE))
            .since(Timestamp::from(now.saturating_sub(longest.max(DAY)))),
        KIND_DIRECT_CHALLENGE,
    )
    .await?;
    for event in to_me {
        if answered.contains(&event.id) {
            continue;
        }
        match readers::direct_challenge(&event) {
            Ok(challenge) if challenge.opponent == me && challenge.accept_until > now => {
                out.incoming.push((event, challenge));
            }
            Ok(_) => {}
            Err(why) => tracing::debug!(id = %event.id, %why, "a challenge the reader refuses"),
        }
    }
    out.incoming
        .sort_by_key(|(event, _)| (event.created_at, event.id));
    Ok(out)
}

/// Whether the session is played under the configured rules, on the
/// configured relay, self-timed, with the bot at the board.
fn terms_are_ours(terms: &SessionTerms, config: &Config, me: PublicKey) -> bool {
    let connection = config.connection();
    terms.game == "sanki"
        && terms.rules == connection.rules
        && terms.is_player(&me)
        && match &terms.timing {
            Timing::SelfTimed(url) => norm_relay(url) == norm_relay(&connection.relay.to_string()),
            Timing::Attested(_) => false,
        }
}

/// A proven, paged query.
async fn query(
    client: &Client,
    relay: &RelayUrl,
    filter: Filter,
    kind: u16,
) -> Result<Vec<Event>, RebuildError> {
    reads::paged(client, relay, filter)
        .await
        .ok_or(RebuildError::UnknownRead(kind))
}

/// A query over many ids, in batches.
async fn query_by_ids(
    client: &Client,
    relay: &RelayUrl,
    kind: Kind,
    ids: impl Iterator<Item = EventId>,
    shape: impl Fn(Filter, Vec<EventId>) -> Filter,
    kind_code: u16,
) -> Result<Vec<Event>, RebuildError> {
    let ids: Vec<EventId> = ids.collect();
    let mut out = Vec::new();
    for batch in ids.chunks(IDS_PER_QUERY) {
        let filter = shape(Filter::new().kind(kind), batch.to_vec());
        out.extend(query(client, relay, filter, kind_code).await?);
    }
    Ok(out)
}
