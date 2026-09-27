// SPDX-License-Identifier: Apache-2.0
//! The runtime (ADR-0045 §7): one loop over the relay's notifications, the
//! games' endings, the challenge work's results and a one-second tick —
//! until told to stop.
//!
//! - An event of the session kinds signed with the bot's key is read by the
//!   echo detector first: another instance of the library halts the bot.
//! - A Direct Challenge to the bot goes through the local checks; admitted,
//!   it takes its reservation and its network checks and founding run in
//!   a task of their own.
//! - A Game Session naming the bot — founded by a target on the bot's
//!   challenge, by the bot's own founding, or by a person acting with the
//!   key — opens a game, its engine launched, one over the cap if need be.
//! - Plies and Conclusions go to their session's game.
//! - On the tick: incoming reservations lapse; the bot's pending challenges
//!   are settled by the query §5 prescribes; a due target is challenged at
//!   its jittered instant; a halted bot logs its state each minute.
//! - On stop (`SIGTERM`): no more admissions, every game told to stop,
//!   the publications in flight given five seconds, the engines ended.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use nostr_sdk::prelude::*;
use sashite_sanki_client::cadence::Cadence;
use sashite_sanki_client::publisher::Resolution;
use sashite_sanki_client::readers::{self, DirectChallenge, Founding};
use sashite_sanki_client::session::{
    self, SessionTerms, KIND_CONCLUSION, KIND_DIRECT_CHALLENGE, KIND_GAME_SESSION, KIND_PLY,
};
use sashite_sanki_client::tags::events_with_marker;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::challenges::{self, Answer, Founded, Opened, Sent};
use super::echo::{Origin, Watch};
use super::rebuild::{cadence_of, OpenSession, Rebuilt};
use super::slots::{Reservation, Slots};
use super::start::Started;
use crate::admit;
use crate::game::{Game, GameContext, GameEnd, GameInput, SKEW_ALLOWANCE_SECS};
use crate::outgoing::{self, SendingState, TargetState};

/// How long publications in flight get on stop.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often a halted bot says so.
const HALTED_LOG_SECS: u64 = 60;

/// How long an unanswered query on a pending challenge waits before it is
/// asked again.
const ASK_AGAIN_SECS: u64 = 60;

/// One day, in seconds.
const DAY: u64 = 86_400;

/// A game's input channel: a replay of a long chain must fit.
const GAME_CHANNEL: usize = 2_048;

/// How many events are held for sessions not open yet.
const BUFFERED_EVENTS: usize = 4_096;

/// A founding never acknowledged: the session as signed, the challenge it
/// answers, the challenger.
#[derive(Clone)]
struct UnresolvedFounding {
    session: Event,
    challenge: Box<Event>,
    challenger: PublicKey,
}

/// A game running.
struct Running {
    inputs: mpsc::Sender<GameInput>,
}

/// What a task of the runtime's came back with.
enum Work {
    /// A founding ended.
    Founded {
        challenge: EventId,
        challenger: PublicKey,
        founded: Founded,
    },
    /// A sending ended — or a resolution of an unacknowledged sending.
    Sent {
        target: PublicKey,
        cadence: Cadence,
        accept_secs: u64,
        sent: Sent,
        resolution: bool,
    },
    /// The query on a pending challenge of the bot's answered.
    Answered { challenge: EventId, answer: Answer },
    /// A Game Session naming the bot was checked against its founding.
    Session {
        session: Box<Event>,
        founding: Option<Box<(Event, SessionTerms)>>,
    },
}

/// The runtime's state.
pub struct Runtime {
    ctx: Arc<GameContext>,
    watch: Watch,
    slots: Slots,
    games: BTreeMap<EventId, Running>,
    game_tasks: JoinSet<(EventId, GameEnd)>,
    work: JoinSet<Work>,
    halted: bool,
    halted_logged_at: Option<u64>,
    stopping: bool,
    /// The challenges considered, incoming or outgoing.
    seen_challenges: BTreeSet<EventId>,
    /// The sessions opened or found closed.
    seen_sessions: BTreeSet<EventId>,
    /// The bot's own challenges, by id, while pending.
    outgoing_events: BTreeMap<EventId, Event>,
    /// The bot's own challenges sent and never acknowledged: resolved by
    /// id before their target is challenged again.
    unresolved: BTreeMap<EventId, (Event, PublicKey, Cadence, u64)>,
    /// Pending challenges whose query proved nothing: asked again at.
    ask_again: BTreeMap<EventId, u64>,
    /// Foundings never acknowledged: the session as signed, the challenge
    /// it answers, the challenger; resolved by id.
    unresolved_foundings: BTreeMap<EventId, UnresolvedFounding>,
    /// Plies and Conclusions of sessions not open yet, by session id:
    /// handed to the game when it opens (bounded).
    buffered: BTreeMap<EventId, Vec<Event>>,
    /// Whether the relay was connected at the last tick.
    was_connected: bool,
    last_sent: BTreeMap<PublicKey, u64>,
    sent_stamps: VecDeque<u64>,
    last_sent_any: Option<u64>,
    next_fire: Option<(PublicKey, u64)>,
    sending: bool,
    jitter: bool,
}

impl Runtime {
    /// The runtime from what the start left: the rebuilt sessions opened,
    /// the pending challenges reserved, the incoming ones re-admitted.
    /// `jitter` is whether outgoing challenges fire at their jittered
    /// instant (a test fires at once).
    #[must_use]
    pub fn new(started: &Started, jitter: bool) -> Self {
        let ctx = Arc::clone(&started.ctx);
        let mut runtime = Self {
            slots: Slots::new(ctx.config.play().max_concurrent.clone()),
            ctx,
            watch: started.watch,
            games: BTreeMap::new(),
            game_tasks: JoinSet::new(),
            work: JoinSet::new(),
            halted: false,
            halted_logged_at: None,
            stopping: false,
            seen_challenges: BTreeSet::new(),
            seen_sessions: BTreeSet::new(),
            outgoing_events: BTreeMap::new(),
            unresolved: BTreeMap::new(),
            ask_again: BTreeMap::new(),
            unresolved_foundings: BTreeMap::new(),
            buffered: BTreeMap::new(),
            was_connected: true,
            last_sent: BTreeMap::new(),
            sent_stamps: VecDeque::new(),
            last_sent_any: None,
            next_fire: None,
            sending: false,
            jitter,
        };
        runtime.absorb_rebuilt(&started.rebuilt);
        runtime
    }

    fn absorb_rebuilt(&mut self, rebuilt: &Rebuilt) {
        for open in &rebuilt.open {
            self.open_rebuilt(open);
        }
        for pending in &rebuilt.pending {
            let cadence = Cadence::of_rows(&pending.challenge.rows).unwrap_or(Cadence::Blitz);
            self.seen_challenges.insert(pending.event.id);
            self.slots.reserve(
                pending.event.id,
                Reservation {
                    cadence,
                    peer: pending.challenge.opponent,
                    outgoing: true,
                    accept_until: pending.challenge.accept_until,
                    consumed: false,
                },
            );
            self.outgoing_events
                .insert(pending.event.id, pending.event.clone());
        }
        self.last_sent = rebuilt.last_sent.clone();
        self.sent_stamps = rebuilt.sent_stamps.iter().copied().collect();
        self.last_sent_any = self.last_sent.values().copied().max();
        for (event, challenge) in &rebuilt.incoming {
            self.consider_incoming(event.clone(), challenge.clone());
        }
    }

    fn open_rebuilt(&mut self, open: &OpenSession) {
        let opponent = open.terms.opponent_of(&self.ctx.me).unwrap_or(self.ctx.me);
        self.open_game(
            open.session.clone(),
            open.terms.clone(),
            open.cadence,
            opponent,
            Some(open.founding.id),
            open.events.clone(),
        );
    }

    /// Opens a game on `session`, its reservation consumed if any.
    fn open_game(
        &mut self,
        session: Event,
        terms: SessionTerms,
        cadence: Cadence,
        opponent: PublicKey,
        founding: Option<EventId>,
        events: Vec<Event>,
    ) {
        if !self.seen_sessions.insert(session.id) {
            return;
        }
        if self.stopping {
            return;
        }
        let start = session.created_at.as_secs();
        self.slots
            .open_session(session.id, cadence, opponent, founding.as_ref());
        if let Some(founding) = founding {
            self.outgoing_events.remove(&founding);
            self.ask_again.remove(&founding);
        }
        // From the rebuild, the relay was read; opened at runtime, the
        // game reads it first, and what arrived meanwhile is handed over.
        let fresh = events.is_empty();
        let mut events = events;
        events.extend(self.buffered.remove(&session.id).unwrap_or_default());
        let game = Game::new(&self.ctx, terms, start, events);
        let (tx, rx) = mpsc::channel(GAME_CHANNEL);
        let ctx = Arc::clone(&self.ctx);
        let id = session.id;
        tracing::info!(session = %id, ?cadence, opponent = %opponent, fresh, "game open");
        self.game_tasks
            .spawn(async move { (id, game.run(ctx, rx, fresh).await) });
        self.games.insert(id, Running { inputs: tx });
    }

    /// Runs until `stop` resolves (`SIGTERM`), then stops everything.
    pub async fn run(mut self, mut started: Started, stop: impl std::future::Future<Output = ()>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                biased;
                () = &mut stop => break,
                notification = started.notifications.next() => match notification {
                    Some(ClientNotification::Event { relay_url, event, .. })
                        if relay_url == self.ctx.publisher.settings().relay =>
                    {
                        self.on_event(*event).await;
                    }
                    Some(ClientNotification::Shutdown) | None => break,
                    Some(_) => {}
                },
                Some(ended) = self.game_tasks.join_next(), if !self.game_tasks.is_empty() => {
                    match ended {
                        Ok((id, end)) => self.on_game_end(id, end),
                        Err(e) => tracing::error!(%e, "a game task failed"),
                    }
                }
                Some(work) = self.work.join_next(), if !self.work.is_empty() => {
                    match work {
                        Ok(work) => self.on_work(work).await,
                        Err(e) => {
                            tracing::error!(%e, "a task failed");
                            // Whatever it was, its gates are lifted.
                            self.sending = false;
                            self.ask_again.retain(|_, at| *at != u64::MAX);
                        }
                    }
                }
                _ = tick.tick() => self.on_tick().await,
            }
        }
        self.stop().await;
    }

    /// Stops: no more admissions or challenges, every game told to stop,
    /// the publications in flight given their grace, the engines ended.
    async fn stop(&mut self) {
        self.stopping = true;
        for running in self.games.values() {
            let _ = running.inputs.try_send(GameInput::Stop);
        }
        let grace = tokio::time::sleep(STOP_GRACE);
        tokio::pin!(grace);
        while !self.game_tasks.is_empty() {
            tokio::select! {
                _ = &mut grace => break,
                Some(ended) = self.game_tasks.join_next() => {
                    if let Ok((id, end)) = ended {
                        self.on_game_end(id, end);
                    }
                }
            }
        }
        self.game_tasks.abort_all();
        self.work.abort_all();
        self.ctx.publisher.close();
    }

    // ---- events ----

    async fn on_event(&mut self, event: Event) {
        if event.verify().is_err() {
            return;
        }
        let signed_here = self.ctx.publisher.signed(&event.id);
        match self.watch.origin(&event, signed_here) {
            Origin::Library => {
                self.halt(&event);
                return;
            }
            Origin::Person => {
                tracing::info!(id = %event.id, kind = event.kind.as_u16(), "a person acting with the key");
            }
            Origin::Ours | Origin::Ignored => {}
        }
        match event.kind.as_u16() {
            KIND_DIRECT_CHALLENGE => {
                if let Ok(challenge) = readers::direct_challenge(&event) {
                    if challenge.opponent == self.ctx.me {
                        self.consider_incoming(event, challenge);
                    } else if challenge.challenger == self.ctx.me && !signed_here {
                        // A person challenging with the key: pending, so
                        // that the answer opens a game.
                        self.track_outgoing(event, challenge);
                    }
                }
            }
            KIND_GAME_SESSION => self.on_session(event),
            KIND_PLY | KIND_CONCLUSION => {
                for id in events_with_marker(&event, "game_session") {
                    match self.games.get(&id) {
                        Some(running) => {
                            if running
                                .inputs
                                .try_send(GameInput::Event(event.clone()))
                                .is_err()
                            {
                                tracing::error!(session = %id, id = %event.id, "a game's channel is full; the event is dropped (the game reads the relay at its next claim)");
                            }
                        }
                        None if !self.seen_sessions.contains(&id) => self.buffer(id, event.clone()),
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Halted (ADR-0045 §2): no admission, no outgoing challenge, no
    /// publication; reservations released; open sessions left to the
    /// clock — every game told to stop, its engine ended; the work in
    /// flight aborted.
    fn halt(&mut self, event: &Event) {
        if self.halted {
            return;
        }
        tracing::error!(id = %event.id, "another instance of the library holds the key: halted");
        self.halted = true;
        self.halted_logged_at = Some(self.ctx.publisher.now());
        let held: Vec<EventId> = self.slots.reservations().map(|(id, _)| *id).collect();
        for id in held {
            self.slots.release(&id);
        }
        self.outgoing_events.clear();
        self.ask_again.clear();
        self.unresolved.clear();
        self.unresolved_foundings.clear();
        self.buffered.clear();
        self.next_fire = None;
        self.sending = false;
        self.work.abort_all();
        for running in self.games.values() {
            let _ = running.inputs.try_send(GameInput::Stop);
        }
    }

    /// Holds an event of a session not open yet, within the bound.
    fn buffer(&mut self, session: EventId, event: Event) {
        let held: usize = self.buffered.values().map(Vec::len).sum();
        if held >= BUFFERED_EVENTS {
            // The oldest session's events go first.
            if let Some(oldest) = self.buffered.keys().next().copied() {
                self.buffered.remove(&oldest);
            }
        }
        self.buffered.entry(session).or_default().push(event);
    }

    /// A Game Session naming the bot: checked against its founding in a
    /// task (the founding may have to be read).
    fn on_session(&mut self, session: Event) {
        if self.seen_sessions.contains(&session.id) {
            return;
        }
        let Ok(founding_ref) = readers::founding_of(&session) else {
            return;
        };
        let known = self.outgoing_events.get(&founding_ref.id()).cloned();
        let ctx = Arc::clone(&self.ctx);
        self.work.spawn(async move {
            let founding = match known {
                Some(event) => Some(event),
                None => {
                    let kind = match founding_ref {
                        Founding::DirectChallenge(_) => KIND_DIRECT_CHALLENGE,
                        Founding::Pairing(_) => session::KIND_PAIRING,
                    };
                    let filter = Filter::new().id(founding_ref.id()).kind(Kind::Custom(kind));
                    super::reads::fetch(&ctx.client, &ctx.publisher.settings().relay, vec![filter])
                        .await
                        .and_then(|events| events.into_iter().next())
                }
            };
            let checked = founding.and_then(|founding| {
                let terms = session::terms(&session, &founding).ok()?;
                if !challenges::terms_are_ours(&ctx, &terms) {
                    return None;
                }
                if let Ok(challenge) = readers::direct_challenge(&founding) {
                    if session.created_at.as_secs() > challenge.accept_until {
                        return None;
                    }
                }
                Some((founding, terms))
            });
            Work::Session {
                session: Box::new(session),
                founding: checked.map(Box::new),
            }
        });
    }

    // ---- challenges ----

    /// The local checks, the reservation, then the network checks and the
    /// founding in a task.
    fn consider_incoming(&mut self, event: Event, challenge: DirectChallenge) {
        if !self.seen_challenges.insert(challenge.id) || self.stopping {
            return;
        }
        let now = self.ctx.publisher.now();
        let local = self.slots.local_state(self.halted, &challenge.challenger);
        let open_seat_first = self.ctx.publisher.signer().open_seat_first(&challenge.id);
        let admission =
            match admit::admit(&challenge, &self.ctx.config, &local, now, open_seat_first) {
                Ok(admission) => admission,
                Err(refusal) => {
                    tracing::debug!(challenge = %challenge.id, ?refusal, "challenge refused");
                    return;
                }
            };
        // Rule 8: the bot's own challenge to this challenger is consumed.
        if local.held_for_challenger == Some(admission.cadence) {
            self.slots.consume_outgoing(&challenge.challenger);
        }
        self.slots.reserve(
            challenge.id,
            Reservation {
                cadence: admission.cadence,
                peer: challenge.challenger,
                outgoing: false,
                accept_until: admission.accept_until,
                consumed: false,
            },
        );
        tracing::info!(challenge = %challenge.id, challenger = %challenge.challenger, ?admission.cadence, "challenge admitted; founding");
        let ctx = Arc::clone(&self.ctx);
        let challenger = challenge.challenger;
        let id = challenge.id;
        self.work.spawn(async move {
            let founded = challenges::found(ctx, event, challenge, admission).await;
            Work::Founded {
                challenge: id,
                challenger,
                founded,
            }
        });
    }

    /// A challenge of the bot's on the relay: its reservation, until the
    /// query settles it.
    fn track_outgoing(&mut self, event: Event, challenge: DirectChallenge) {
        if !self.seen_challenges.insert(challenge.id) {
            return;
        }
        let cadence = Cadence::of_rows(&challenge.rows).unwrap_or(Cadence::Blitz);
        self.slots.reserve(
            challenge.id,
            Reservation {
                cadence,
                peer: challenge.opponent,
                outgoing: true,
                accept_until: challenge.accept_until,
                consumed: false,
            },
        );
        let stamp = event.created_at.as_secs();
        let last = self.last_sent.entry(challenge.opponent).or_insert(stamp);
        *last = (*last).max(stamp);
        self.sent_stamps.push_back(stamp);
        self.last_sent_any = Some(self.last_sent_any.map_or(stamp, |l| l.max(stamp)));
        self.outgoing_events.insert(challenge.id, event);
    }

    async fn on_work(&mut self, work: Work) {
        if self.halted {
            tracing::debug!("halted: a task's result is dropped");
            self.sending = false;
            return;
        }
        match work {
            Work::Founded {
                challenge,
                challenger,
                founded,
            } => match founded {
                Founded::Session(opened) => {
                    let Opened { session, terms } = *opened;
                    let cadence = cadence_of(&terms).unwrap_or(Cadence::Blitz);
                    self.open_game(
                        session,
                        terms,
                        cadence,
                        challenger,
                        Some(challenge),
                        Vec::new(),
                    );
                }
                Founded::Unknown {
                    session,
                    challenge: challenge_event,
                } => {
                    tracing::warn!(challenge = %challenge, "founding not acknowledged; resolving later");
                    self.unresolved_foundings.insert(
                        session.id,
                        UnresolvedFounding {
                            session,
                            challenge: challenge_event,
                            challenger,
                        },
                    );
                }
                Founded::Refused(why) => {
                    tracing::info!(challenge = %challenge, why, "not founded");
                    self.slots.release(&challenge);
                }
            },
            Work::Sent {
                target,
                cadence,
                accept_secs,
                sent,
                resolution,
            } => {
                self.sending = false;
                match sent {
                    // On the relay: recorded once — a resolution's `Found`
                    // confirms what the first `Unknown` recorded.
                    Sent::Challenge(event) if !resolution => {
                        self.record_sent(event, target, cadence, accept_secs);
                    }
                    Sent::Challenge(event) => {
                        tracing::info!(target = %target, "challenge resolved: on the relay");
                        self.outgoing_events.insert(event.id, event);
                    }
                    // Not acknowledged: recorded and reserved as sent (the
                    // relay may hold it), resolved by id before the next.
                    Sent::Unknown(event) => {
                        tracing::warn!(target = %target, "challenge not acknowledged; resolving before the next");
                        if !resolution {
                            self.record_sent(event.clone(), target, cadence, accept_secs);
                        }
                        self.unresolved
                            .insert(event.id, (event, target, cadence, accept_secs));
                    }
                    Sent::Failed(why) if resolution => {
                        // Absent after all: the reservation goes.
                        tracing::warn!(target = %target, why, "challenge resolved: absent");
                        let absent: Vec<EventId> = self
                            .slots
                            .reservations()
                            .filter(|(_, r)| r.outgoing && r.peer == target)
                            .map(|(id, _)| *id)
                            .collect();
                        for id in absent {
                            self.slots.release(&id);
                            self.outgoing_events.remove(&id);
                        }
                    }
                    Sent::Failed(why) => {
                        tracing::error!(target = %target, why, "challenge not sent")
                    }
                }
            }
            Work::Answered { challenge, answer } => match answer {
                Answer::Session(opened) => {
                    let Opened { session, terms } = *opened;
                    let cadence = cadence_of(&terms).unwrap_or(Cadence::Blitz);
                    let opponent = terms.opponent_of(&self.ctx.me).unwrap_or(self.ctx.me);
                    // Played all the same, one over the cap if need be.
                    self.open_game(
                        session,
                        terms,
                        cadence,
                        opponent,
                        Some(challenge),
                        Vec::new(),
                    );
                }
                Answer::None => {
                    tracing::info!(challenge = %challenge, "challenge lapsed unanswered");
                    self.slots.release(&challenge);
                    self.outgoing_events.remove(&challenge);
                    self.ask_again.remove(&challenge);
                }
                Answer::Unknown => {
                    self.ask_again.insert(
                        challenge,
                        self.ctx.publisher.now().saturating_add(ASK_AGAIN_SECS),
                    );
                }
            },
            Work::Session { session, founding } => match founding.map(|b| *b) {
                Some((founding, terms)) => {
                    let cadence = cadence_of(&terms).unwrap_or(Cadence::Blitz);
                    let opponent = terms.opponent_of(&self.ctx.me).unwrap_or(self.ctx.me);
                    self.open_game(
                        *session,
                        terms,
                        cadence,
                        opponent,
                        Some(founding.id),
                        Vec::new(),
                    );
                }
                None => {
                    tracing::warn!(session = %session.id, "a session naming the bot, not on its terms; ignored");
                }
            },
        }
    }

    fn record_sent(&mut self, event: Event, target: PublicKey, cadence: Cadence, accept_secs: u64) {
        let stamp = event.created_at.as_secs();
        self.seen_challenges.insert(event.id);
        self.slots.reserve(
            event.id,
            Reservation {
                cadence,
                peer: target,
                outgoing: true,
                accept_until: stamp.saturating_add(accept_secs),
                consumed: false,
            },
        );
        self.last_sent.insert(target, stamp);
        self.sent_stamps.push_back(stamp);
        self.last_sent_any = Some(stamp);
        self.outgoing_events.insert(event.id, event);
        tracing::info!(target = %target, "challenge sent");
    }

    fn on_game_end(&mut self, id: EventId, end: GameEnd) {
        self.games.remove(&id);
        let freed = self.slots.close_session(&id);
        match end {
            GameEnd::Closed(verdict) => {
                tracing::info!(session = %id, status = %verdict.status, ?freed, "game closed");
            }
            GameEnd::Unverified(why) => {
                tracing::error!(session = %id, why, "game unverified; no further acts");
            }
            GameEnd::Stopped => tracing::info!(session = %id, "game stopped"),
        }
    }

    // ---- the tick ----

    async fn on_tick(&mut self) {
        if self.stopping {
            return;
        }
        let now = self.ctx.publisher.now();
        let past_tolerance = self.ctx.publisher.settings().past_tolerance;

        if self.halted {
            if self
                .halted_logged_at
                .is_none_or(|at| now >= at.saturating_add(HALTED_LOG_SECS))
            {
                tracing::error!("halted: another instance of the library holds the key");
                self.halted_logged_at = Some(now);
            }
            return;
        }

        // After a reconnection, every open session is re-derived from a
        // fresh read, and the pending challenges asked again.
        let connected = self
            .ctx
            .client
            .relay(&self.ctx.publisher.settings().relay)
            .await
            .is_ok_and(|relay| relay.is_some_and(|r| r.status().is_connected()));
        if connected && !self.was_connected {
            tracing::warn!("reconnected: every open session is read again");
            for running in self.games.values() {
                let _ = running.inputs.try_send(GameInput::Reread);
            }
            for at in self.ask_again.values_mut().filter(|at| **at != u64::MAX) {
                *at = now;
            }
        }
        self.was_connected = connected;

        // Incoming reservations lapse.
        for id in self.slots.lapse_incoming(now, past_tolerance) {
            tracing::debug!(challenge = %id, "incoming challenge lapsed");
        }

        // Foundings never acknowledged: resolved by id.
        self.resolve_foundings();

        // Challenges sent and never acknowledged: resolved by id.
        self.resolve_unresolved();

        // Pending challenges past `accept_until + L + skew`: the query.
        let due: Vec<EventId> = self
            .slots
            .reservations()
            .filter(|(id, r)| {
                r.outgoing
                    && now
                        > r.accept_until
                            .saturating_add(past_tolerance)
                            .saturating_add(SKEW_ALLOWANCE_SECS)
                    && self.ask_again.get(id).is_none_or(|at| now >= *at)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            if let Some(event) = self.outgoing_events.get(&id).cloned() {
                // Not asked again before the answer.
                self.ask_again.insert(id, u64::MAX);
                let ctx = Arc::clone(&self.ctx);
                self.work.spawn(async move {
                    let answer = challenges::answered(ctx, event).await;
                    Work::Answered {
                        challenge: id,
                        answer,
                    }
                });
            } else {
                self.slots.release(&id);
            }
        }

        // Outgoing challenges.
        self.consider_sending(now);

        // The day's window.
        let day_ago = now.saturating_sub(DAY);
        while self.sent_stamps.front().is_some_and(|s| *s < day_ago) {
            self.sent_stamps.pop_front();
        }
    }

    fn resolve_foundings(&mut self) {
        let pending: Vec<(EventId, UnresolvedFounding)> = self
            .unresolved_foundings
            .iter()
            .map(|(id, v)| (*id, v.clone()))
            .collect();
        for (id, unresolved) in pending {
            self.unresolved_foundings.remove(&id);
            let UnresolvedFounding {
                session,
                challenge: challenge_event,
                challenger,
            } = unresolved;
            let ctx = Arc::clone(&self.ctx);
            let challenge = challenge_event.id;
            self.work.spawn(async move {
                let founded = match ctx.publisher.resolve(id).await {
                    Resolution::Found(found) => match session::terms(&found, &challenge_event) {
                        Ok(terms) => Founded::Session(Box::new(Opened {
                            session: found,
                            terms,
                        })),
                        Err(why) => Founded::Refused(format!(
                            "the founded session's terms do not hold: {why:?}"
                        )),
                    },
                    Resolution::ConfirmedAbsent => Founded::Refused("founding absent".to_owned()),
                    Resolution::Unknown => Founded::Unknown {
                        session,
                        challenge: challenge_event,
                    },
                };
                Work::Founded {
                    challenge,
                    challenger,
                    founded,
                }
            });
        }
    }

    fn resolve_unresolved(&mut self) {
        let pending: Vec<(EventId, (Event, PublicKey, Cadence, u64))> = self
            .unresolved
            .iter()
            .map(|(id, v)| (*id, v.clone()))
            .collect();
        for (id, (event, target, cadence, accept_secs)) in pending {
            self.unresolved.remove(&id);
            let ctx = Arc::clone(&self.ctx);
            self.sending = true;
            self.work.spawn(async move {
                let sent = match ctx.publisher.resolve(id).await {
                    Resolution::Found(found) => Sent::Challenge(found),
                    Resolution::ConfirmedAbsent => Sent::Failed("absent after all".to_owned()),
                    Resolution::Unknown => Sent::Unknown(event),
                };
                Work::Sent {
                    target,
                    cadence,
                    accept_secs,
                    sent,
                    resolution: true,
                }
            });
        }
    }

    fn consider_sending(&mut self, now: u64) {
        let Some(outgoing) = self.ctx.config.challenges().outgoing.clone() else {
            return;
        };
        if self.sending || !self.unresolved.is_empty() {
            return;
        }
        let day_ago = now.saturating_sub(DAY);
        let state = SendingState {
            targets: outgoing
                .targets
                .as_slice()
                .iter()
                .map(|t| {
                    (
                        *t,
                        TargetState {
                            session_open: self.slots.session_open_with(t),
                            pending_to: self.slots.pending_to(t).is_some(),
                            pending_from: self.slots.pending_from(t).is_some(),
                            last_sent: self.last_sent.get(t).copied(),
                        },
                    )
                })
                .collect(),
            sent_last_day: u32::try_from(
                self.sent_stamps.iter().filter(|s| **s >= day_ago).count(),
            )
            .unwrap_or(u32::MAX),
            last_sent_any: self.last_sent_any,
            slot_free: self.slots.free(outgoing.cadence),
        };
        match self.next_fire {
            None => {
                if let Some(target) = outgoing::next_due(&outgoing, &state, now) {
                    let at = if self.jitter {
                        let jitter = self
                            .ctx
                            .publisher
                            .signer()
                            .jitter_secs(&target, now.checked_div(3600).unwrap_or(0));
                        outgoing::fire_at(now, jitter)
                    } else {
                        now
                    };
                    self.next_fire = Some((target, at));
                }
            }
            Some((target, at)) if now >= at => {
                self.next_fire = None;
                // Still due at the instant?
                if outgoing::next_due(&outgoing, &state, now) != Some(target) {
                    return;
                }
                self.sending = true;
                self.last_sent_any = Some(now);
                let ctx = Arc::clone(&self.ctx);
                let cadence = outgoing.cadence;
                let accept_secs = outgoing.accept_secs;
                self.work.spawn(async move {
                    let sent = challenges::send(ctx, outgoing, target).await;
                    Work::Sent {
                        target,
                        cadence,
                        accept_secs,
                        sent,
                        resolution: false,
                    }
                });
            }
            Some(_) => {}
        }
    }
}
