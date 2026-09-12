//! One bot actor — a persona playing over the protocol interface
//! (ADR-0014 §3, §6; ADR-0033/0034): standing events, courtship, founding,
//! session play, conclusion, rematches, stars. Everything a bot does, a human
//! client could do — and nothing a bot does on the rules axis is its own:
//! the session's **module** (the rule system named by the fleet's Rule
//! System event) answers every question of state, legality and verdict.
//!
//! The actor owns its relay client and subscriptions, and services its
//! sessions **serially** (the per-bot concurrency caps make this natural —
//! §7 of ADR-0015 assumes one `choose` at a time). Timed behavior (think
//! delays, correspondence pacing, win-on-time wakes, the timeout courtesy)
//! is scheduled through a coarse periodic tick rather than per-event sleeps,
//! so the notification loop never blocks.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use nostr_sdk::prelude::*;

use sashite_sanki_player::{choose, Context as PlayerContext, Limits, Move, Position, Strength};

use crate::cadence::Cadence;
use crate::chain::{self, predicted_verdict, session_view, SessionView};
use crate::conclusion::{self, Conclude};
use crate::config::{BotConfig, DrawOffer, FleetSection};
use crate::courtship::{self, AcceptPlan, PoolCandidate};
use crate::fleet::Ledger;
use crate::founding;
use crate::module::{self, Check, Describe, Runtime};
use crate::prng::SplitMix64;
use crate::publish::{publish_self_timed, Pow, RelayClock};
use crate::rematch;
use crate::rules::LoadedRuleSystem;
use crate::session::{self, Events, Seat, SessionTerms, Timing};
use crate::tags;

const OPEN_CHALLENGE_KIND: u16 = 3418;
const PAIRING_KIND: u16 = session::KIND_PAIRING;
const DIRECT_CHALLENGE_KIND: u16 = session::KIND_DIRECT_CHALLENGE;
const GAME_SESSION_KIND: u16 = session::KIND_GAME_SESSION;
const PLY_KIND: u16 = session::KIND_PLY;
const CONCLUSION_KIND: u16 = session::KIND_CONCLUSION;
const CHALLENGE_POLICY_KIND: u16 = 30420;
const MUTE_LIST_KIND: u16 = 10000;
const CONTACTS_KIND: u16 = 3;

/// Relay fetch budget.
const FETCH_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// The actor's periodic heartbeat (schedules thinks, deadline wakes, courtship).
const TICK_SECS: u64 = 5;

/// Safety margin σ reserved before the flag (mining + latency + buffer) — §6.5.
const SIGMA_LIVE_SECS: u64 = 2;
const SIGMA_CORRESPONDENCE_SECS: u64 = 60;

/// A pool entry's minimum residual life for us to court it, a Pairing's
/// minimum residual founding window, the `accept_until` window of our own
/// entries (§6.2).
const COURT_MARGIN_SECS: u64 = 30;
const OWN_ENTRY_WINDOW_SECS: u64 = 180;

/// The `accept_until` window of a rematch challenge (§9). The bot keeps ONE
/// LIVE challenge per concluded game, so this is the opponent's window to
/// accept — sized for a human lingering on the game-over screen.
const REMATCH_WINDOW_SECS: u64 = 900;

/// How far back the self-subscription replays when the bot starts. The
/// subscription carries *live* traffic; the state a restart needs is rebuilt by
/// the explicit recovery fetches. The window is not zero because `created_at`
/// is the publisher's clock, not ours: the longest-lived thing that arrives
/// here and still deserves acting on is a rematch challenge inside its
/// acceptance window, and three of those windows absorb both the skew and a
/// slow start.
const SELF_REPLAY_LOOKBACK_SECS: u64 = 3 * REMATCH_WINDOW_SECS;

/// How far back the session recovery looks for Game Sessions naming the bot
/// (§9): a correspondence game outlives a restart, a season-old one does not.
/// Every recovered session costs one reconstruction on the first tick.
const RECOVERY_LOOKBACK_SECS: u64 = 30 * 86_400;

/// How far back the recovery looks for Pairings naming the bot: their
/// founding window is a minute or two (kind 3419 §Lifecycle tags), so an
/// older one has lapsed.
const PAIRING_RECOVERY_LOOKBACK_SECS: u64 = 600;

/// Grace before re-sending an already-published Ply or Conclusion whose echo
/// has not come back (three coarse ticks): long enough for any realistic
/// relay round trip, short enough never to threaten a clock even on a fast
/// cadence.
const REPUBLISH_GRACE_SECS: u64 = 15;

/// Everything one bot actor needs, assembled by the supervisor.
pub struct BotContext {
    /// Persona name (tracing span).
    pub name: String,
    /// The bot's identity.
    pub keys: Keys,
    /// The persona.
    pub config: BotConfig,
    /// Fleet-wide settings.
    pub fleet: FleetSection,
    /// The configured matchmaker.
    pub matchmaker: PublicKey,
    /// The rule system the fleet plays under: its event id, and its module —
    /// shared by every persona of the process, one request at a time.
    pub rules: EventId,
    /// The loaded rule system (the module, instantiated).
    pub module: Arc<Mutex<LoadedRuleSystem>>,
    /// What the module's `describe` reports (the normative members).
    pub describe: Describe,
    /// The shared fleet ledger.
    pub ledger: Arc<Ledger>,
    /// This bot's derived seed (from its pubkey).
    pub bot_seed: u64,
    /// Shutdown signal (true = stop courting, finish and disconnect).
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

impl BotContext {
    /// One synchronous exchange with the module.
    fn with_module<T>(&self, f: impl FnOnce(&mut Runtime) -> T) -> Result<T> {
        let mut guard = self
            .module
            .lock()
            .map_err(|_| anyhow!("the module's lock is poisoned"))?;
        Ok(f(&mut guard.runtime))
    }
}

/// A tracked session.
struct SessionMeta {
    /// The canonical Game Session.
    session: Event,
    /// The slot it occupies (kind 3422 §Idempotence and race resolution):
    /// the founding's id, or the concluded session's for a rematch. One
    /// tracked session per slot: a canonical one evicts a stale one.
    slot: EventId,
    /// Its terms.
    terms: SessionTerms,
    /// t₀.
    start: u64,
    /// The bot's seat.
    my_seat: Seat,
    /// The opponent.
    opponent: PublicKey,
    /// The session's cadence family (*Cadence — Sanki*): what its slot is
    /// counted under; its correspondence bit paces the think and the
    /// presence rules.
    cadence: Cadence,
    my_side_evals: VecDeque<i32>,
    /// Earliest instant we may act on the pending duty (think pacing).
    next_action_at: u64,
    /// The half-move the pending think was scheduled for (re-planned when
    /// the chain moves under a premoveless bot).
    planned_half_move: u32,
    /// The ply we last published, keyed by the half-move it filled — the
    /// same-slot idempotence guard (§6.5). See the guard in `service_session`.
    published: Option<PublishedPly>,
    /// When the win on time was first predicted — the courtesy delay runs
    /// from here (§6.6).
    timeout_seen_at: Option<u64>,
    /// The Conclusion we published and when — held until its echo comes
    /// back, re-sent after the grace.
    concluded: Option<(Event, u64)>,
    /// Conclusions already logged as non-conforming (the client/kernel
    /// divergence metric), so each is reported once.
    warned: BTreeSet<EventId>,
}

/// The record behind `SessionMeta::published`: enough to RE-SEND the very
/// same ply (never to search a new one) while the session fetch has not yet
/// caught up with it.
struct PublishedPly {
    /// The half-move ordinal our ply filled (`view.next_half_move` at publish).
    half_move: u32,
    /// Our own step for that slot (the `step` tag).
    step: u32,
    /// The exact move content published.
    content: String,
    /// Whether the ply carried the `draw` offer tag.
    offer_draw: bool,
    /// When it was (last) sent — paces the lost-publish re-send.
    at: u64,
}

/// Run the actor until shutdown. Errors bubble only for unrecoverable
/// startup conditions; per-event errors are logged and absorbed.
pub async fn run(mut ctx: BotContext) -> Result<()> {
    let me = ctx.keys.public_key();
    let span = tracing::info_span!("bot", bot = %ctx.name);
    let _enter = span.enter();

    // No signer on the client: this bot signs every event itself before
    // sending it (see `publish`).
    let client = Client::builder().build();
    client
        .add_relay(ctx.fleet.relay_url.as_str())
        .await
        .with_context(|| format!("failed to add relay {}", ctx.fleet.relay_url))?;
    client.connect().await;

    let relay_clock = Arc::new(RelayClock::new());
    let mut rng = SplitMix64::new(ctx.bot_seed);
    let mut sessions: BTreeMap<EventId, SessionMeta> = BTreeMap::new();
    let mut courted_entries: BTreeSet<EventId> = BTreeSet::new();
    let mut considered_challenges: BTreeSet<EventId> = BTreeSet::new();
    let mut founded_pairings: BTreeSet<EventId> = BTreeSet::new();
    // Games we have already challenged to a rematch, keyed to OUR challenge's
    // `accept_until` — one LIVE challenge per game, whether it went out
    // proactively (on the verdict) or in reply to the opponent's; a lapsed
    // one may be renewed when the opponent proposes after it expired.
    let mut offered_rematches: BTreeMap<EventId, u64> = BTreeMap::new();
    // Our own live pool entries' `accept_until`, ONE PER CADENCE (ADR-0039
    // §7): one entry at a time per family, so N compatible strangers entering
    // together at one cadence do not become N sessions, while a byōyomi
    // entry never blocks a blitz one.
    let own_entries: Arc<PoolLocks> = Arc::new(PoolLocks::default());
    let mut starred_today: u64 = 0;

    reconcile_standing_events(&client, &ctx, &relay_clock).await;

    // Fixed-size subscriptions (§3): everything that p-tags the bot, plus the
    // pool feed on the matchmaker. Session-scoped Ply/Conclusion coverage
    // comes from the per-service fetch (the tick), not a mutable subscription.
    let self_filter = Filter::new()
        .kinds(
            [
                PAIRING_KIND,
                DIRECT_CHALLENGE_KIND,
                GAME_SESSION_KIND,
                PLY_KIND,
                CONCLUSION_KIND,
            ]
            .into_iter()
            .map(Kind::Custom),
        )
        .pubkey(me)
        .since(Timestamp::from(
            Timestamp::now()
                .as_secs()
                .saturating_sub(SELF_REPLAY_LOOKBACK_SECS),
        ));
    let pool_filter = Filter::new()
        .kind(Kind::Custom(OPEN_CHALLENGE_KIND))
        .pubkey(ctx.matchmaker);
    // The notification stream is opened BEFORE the subscriptions: it is a
    // broadcast channel, and what the relay replays on REQ before the stream
    // exists is lost to it — a Pairing that landed while the bot was
    // starting, for one.
    let mut notifications = client.notifications();
    client
        .subscribe(self_filter)
        .await
        .context("subscribing to self kinds")?;
    client
        .subscribe(pool_filter)
        .await
        .context("subscribing to the pool")?;

    // Stateless restart (§9): rebuild active sessions from relay replay.
    if let Err(error) = recover_sessions(&client, &ctx, &mut sessions, me).await {
        tracing::warn!(error = %error, "session recovery incomplete");
    }
    // …and rebuild what we have already challenged, for the same reason: the
    // one-challenge-per-game gate lives in memory, so a restart would
    // otherwise reopen every game the bot ever finished.
    if let Err(error) = recover_offered_rematches(&client, &mut offered_rematches, me).await {
        tracing::warn!(error = %error, "rematch recovery incomplete");
    }
    // …and found the Pairings that landed while the bot was down, while
    // their founding window is open (a Pairing has no other trigger).
    match fetch_many(
        &client,
        Filter::new()
            .kind(Kind::Custom(PAIRING_KIND))
            .pubkey(me)
            .author(ctx.matchmaker)
            .since(Timestamp::from(
                Timestamp::now()
                    .as_secs()
                    .saturating_sub(PAIRING_RECOVERY_LOOKBACK_SECS),
            )),
    )
    .await
    {
        Ok(pairings) => {
            for pairing in pairings {
                if founded_pairings.insert(pairing.id) {
                    if let Err(reason) =
                        consider_pairing(&client, &ctx, &relay_clock, &mut sessions, me, &pairing)
                            .await
                    {
                        tracing::debug!(pairing = %pairing.id, reason, "recovered pairing not founded");
                    }
                }
            }
        }
        Err(error) => tracing::warn!(error = %error, "pairing recovery incomplete"),
    }
    tracing::info!(sessions = sessions.len(), "bot up");

    let mut tick = tokio::time::interval(StdDuration::from_secs(TICK_SECS));
    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.changed() => {
                if *ctx.shutdown.borrow() {
                    tracing::info!("shutdown; leaving the pool and disconnecting");
                    break;
                }
            }
            _ = tick.tick() => {
                service_all(
                    &client, &ctx, &relay_clock, &mut sessions, &mut rng, me,
                    &mut starred_today, &mut offered_rematches, None,
                )
                .await;
            }
            notification = notifications.next() => match notification {
                Some(ClientNotification::Event { event, .. }) => {
                    handle_event(
                        &client, &ctx, &relay_clock, &mut sessions, &mut courted_entries,
                        &mut considered_challenges, &mut founded_pairings,
                        &mut offered_rematches, &own_entries, &mut starred_today,
                        &mut rng, me, *event,
                    ).await;
                }
                Some(_) => {}
                // The stream ends when the client shuts down. (0.45 drops the
                // lag error rather than surfacing it; the tick re-services
                // every session on a timer, so a missed event is noticed late
                // rather than never.)
                None => break,
            },
        }
    }
    client.disconnect().await;
    Ok(())
}

/// Publish/refresh the standing events (§6.1): the kind-0 profile with the
/// NIP-24 `bot: true` flag, and the kind-30420 policy for the game.
async fn reconcile_standing_events(client: &Client, ctx: &BotContext, relay_clock: &RelayClock) {
    let profile = &ctx.config.profile;
    let metadata = format!(
        r#"{{"name":{},"about":{},"bot":true{}}}"#,
        json_string(&profile.display_name),
        json_string(&profile.about),
        profile
            .picture
            .as_deref()
            .map(|url| format!(r#","picture":{}"#, json_string(url)))
            .unwrap_or_default(),
    );
    let result = publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Metadata,
        Pow::None,
        |_created_at| (Vec::new(), metadata.clone()),
    )
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, "profile publish failed");
    }

    let mode = ctx.config.play.challenge_policy.clone();
    let game = ctx.fleet.game.clone();
    let result = publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(CHALLENGE_POLICY_KIND),
        Pow::None,
        move |_created_at| {
            (
                vec![
                    Tag::identifier(game.clone()),
                    Tag::custom("mode", [mode.clone()]),
                ],
                String::new(),
            )
        },
    )
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, "challenge-policy publish failed");
    }
}

/// Rebuild session tracking from the relay (§9): every recent Game Session
/// tagging the bot; the first tick drops the concluded ones (the module
/// selects their canonical Conclusion) and resumes the rest.
async fn recover_sessions(
    client: &Client,
    ctx: &BotContext,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    me: PublicKey,
) -> Result<()> {
    let game_sessions = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(GAME_SESSION_KIND))
            .pubkey(me)
            .since(Timestamp::from(
                Timestamp::now()
                    .as_secs()
                    .saturating_sub(RECOVERY_LOOKBACK_SECS),
            )),
    )
    .await?;
    for session in game_sessions {
        if let Err(error) = track_session(client, ctx, sessions, me, session).await {
            tracing::debug!(error = %error, "skipping unresumable session");
        }
    }
    Ok(())
}

/// Rebuild the set of games the bot has already challenged to a rematch (§9,
/// stateless restart). Our own rematch challenges are the durable record of
/// what we already proposed, so they are what is read back. A failure here is
/// warned about, not fatal: the cost is a possible duplicate challenge, which
/// the `(rematch, concluded session)` slot absorbs.
async fn recover_offered_rematches(
    client: &Client,
    offered_rematches: &mut BTreeMap<EventId, u64>,
    me: PublicKey,
) -> Result<()> {
    let challenges = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(DIRECT_CHALLENGE_KIND))
            .author(me),
    )
    .await?;
    for challenge in &challenges {
        if let [concluded] = tags::events_with_marker(challenge, "rematch_of").as_slice() {
            let deadline: u64 = tags::accept_until(challenge)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| {
                    challenge
                        .created_at
                        .as_secs()
                        .saturating_add(REMATCH_WINDOW_SECS)
                });
            let entry = offered_rematches.entry(*concluded).or_insert(0);
            *entry = (*entry).max(deadline);
        }
    }
    tracing::debug!(
        games = offered_rematches.len(),
        "recovered the already-challenged rematch set"
    );
    Ok(())
}

/// The founding event a Game Session references, fetched: a Pairing or a
/// Direct Challenge, by the marker.
async fn fetch_founding(client: &Client, session: &Event) -> Result<Event> {
    let (kind, id) = match (
        tags::events_with_marker(session, "pairing").as_slice(),
        tags::events_with_marker(session, "direct_challenge").as_slice(),
    ) {
        ([id], []) => (PAIRING_KIND, *id),
        ([], [id]) => (DIRECT_CHALLENGE_KIND, *id),
        _ => bail!("the Game Session does not reference exactly one founding"),
    };
    let founding = fetch_event(client, id).await?;
    if founding.kind != Kind::Custom(kind) {
        bail!("the founding is not of the kind its marker announces");
    }
    if founding.verify().is_err() {
        bail!("the founding's signature is invalid");
    }
    Ok(founding)
}

/// The checks on a founding event that are the bot's to decide before it
/// trusts a Game Session on it: a Pairing is the configured matchmaker's, a
/// Direct Challenge honours its nonce (kind 3420 constraint 9). Nothing
/// beyond what the terms check (`session::terms`) already mirrors.
fn founding_ok(ctx: &BotContext, founding: &Event) -> Result<()> {
    if founding.kind == Kind::Custom(PAIRING_KIND) {
        if founding.pubkey != ctx.matchmaker {
            bail!("the Pairing is not the configured matchmaker's");
        }
    } else if !session::pow_ok(founding) {
        bail!("the Direct Challenge carries no honoured nonce");
    }
    Ok(())
}

/// The slot a founding's Game Sessions compete for: the concluded session for
/// a rematch challenge, the founding itself otherwise.
fn slot_of(founding: &Event) -> EventId {
    match tags::events_with_marker(founding, "rematch_of").as_slice() {
        [concluded] => *concluded,
        _ => founding.id,
    }
}

/// The founding's deadline for a Game Session's canonical timing (kind 3422
/// constraint 9): the Direct Challenge's `accept_until`, the Pairing's
/// `found_until`.
fn founding_deadline(founding: &Event) -> Option<u64> {
    let name = if founding.kind == Kind::Custom(PAIRING_KIND) {
        "found_until"
    } else {
        "accept_until"
    };
    session::exactly_one(founding, name).and_then(session::decimal)
}

/// The Game Sessions competing for `founding`'s slot, each with its own
/// founding (kind 3422 §Idempotence and race resolution): those referencing
/// `founding` — and, for a rematch challenge, those referencing any rematch
/// challenge of the same concluded session, which is the slot
/// `(rematch, concluded session)`.
async fn slot_candidates(client: &Client, founding: &Event) -> Result<Vec<(Event, Event)>> {
    let mut foundings = vec![founding.clone()];
    if let [concluded] = tags::events_with_marker(founding, "rematch_of").as_slice() {
        let siblings = fetch_many(
            client,
            Filter::new()
                .kind(Kind::Custom(DIRECT_CHALLENGE_KIND))
                .event(*concluded),
        )
        .await?;
        foundings.extend(siblings.into_iter().filter(|sibling| {
            sibling.id != founding.id
                && sibling.verify().is_ok()
                && tags::events_with_marker(sibling, "rematch_of").as_slice() == [*concluded]
        }));
    }
    let mut candidates = Vec::new();
    for sibling in foundings {
        let sessions = fetch_many(
            client,
            Filter::new()
                .kind(Kind::Custom(GAME_SESSION_KIND))
                .event(sibling.id),
        )
        .await?;
        candidates.extend(
            sessions
                .into_iter()
                .map(|session| (session, sibling.clone())),
        );
    }
    Ok(candidates)
}

/// The canonical Game Session of a slot among `candidates` — each with its
/// founding — in self-timed mode: the earliest `(created_at, id)` among those
/// whose terms conform to their founding. With its terms and founding.
fn canonical_of_slot(candidates: Vec<(Event, Event)>) -> Option<(Event, SessionTerms, Event)> {
    candidates
        .into_iter()
        .filter_map(|(candidate, founding)| {
            let terms = session::terms(&candidate, &founding).ok()?;
            Some((candidate, terms, founding))
        })
        .min_by_key(|(candidate, _, _)| (candidate.created_at.as_secs(), candidate.id))
}

/// Start tracking a Game Session naming the bot: resolve its founding, check
/// its terms against it and against the rule system (the prescribed initial
/// position), establish the canonical Game Session of its slot, and t₀.
async fn track_session(
    client: &Client,
    ctx: &BotContext,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    me: PublicKey,
    session: Event,
) -> Result<()> {
    if sessions.contains_key(&session.id) {
        return Ok(());
    }
    if session.verify().is_err() {
        bail!("invalid signature");
    }
    let founding = fetch_founding(client, &session).await?;
    // The canonical Game Session of the slot may be another than the one
    // delivered (both players of a Pairing founding; a double publish; both
    // players' rematch challenges accepted).
    let mut candidates = slot_candidates(client, &founding).await?;
    if !candidates
        .iter()
        .any(|(candidate, _)| candidate.id == session.id)
    {
        candidates.push((session, founding));
    }
    // Only Game Sessions naming the bot compete: a stranger's event on a
    // sibling founding cannot displace ours (on the directed path the
    // acceptor signs, on a Pairing the matchmaker names the players).
    let candidates: Vec<(Event, Event)> = candidates
        .into_iter()
        .filter(|(candidate, founding)| {
            founding_ok(ctx, founding).is_ok()
                && tags::pubkeys_with_role(candidate, "player").contains(&me)
        })
        .collect();
    let Some((canonical, terms, founding)) = canonical_of_slot(candidates) else {
        bail!("no conforming Game Session on this founding");
    };
    if sessions.contains_key(&canonical.id) {
        return Ok(());
    }
    if !terms.is_player(&me) {
        bail!("not our session");
    }
    if terms.rules != ctx.rules {
        bail!("session under another rule system");
    }
    match &terms.timing {
        Timing::SelfTimed(relay)
            if tags::norm_relay(relay) == tags::norm_relay(&ctx.fleet.relay_url) => {}
        Timing::SelfTimed(_) => bail!("session timed by another relay — unverifiable here"),
        Timing::Attested(_) => bail!("attested session (v1 is self-timed) — abandoned"),
    }
    // The initial position the rule system prescribes (constraint 7).
    if ctx.describe.positions.get(&terms.pairing()) != Some(&terms.position) {
        bail!("the Game Session's position is not the one the rule system prescribes");
    }
    // The founding's deadline (constraint 9).
    let created_at = canonical.created_at.as_secs();
    if founding_deadline(&founding).is_some_and(|deadline| created_at > deadline) {
        bail!("the Game Session was founded after the founding's deadline");
    }
    let start = terms.start_at.map_or(created_at, |at| at.max(created_at));
    let my_seat = terms
        .seat_of(&me)
        .ok_or_else(|| anyhow!("no seat for us"))?;
    let opponent = terms.player(my_seat.other());
    // The session's cadence, read off the founding's first period (*Cadence —
    // Sanki*): a founding without one is non-conforming, and the bot was
    // never admitted into it — it cannot be served under any cap.
    let cadence = Cadence::of_rows(&tags::time_control_rows(&founding))
        .ok_or_else(|| anyhow!("the founding has no cadence (malformed time_control)"))?;
    // One tracked session per slot: a canonical Game Session arriving after
    // a sibling (both players of a Pairing founding, both rematch
    // challenges accepted) evicts the stale one, whose events would never
    // come — the bot would otherwise claim a win on time in a phantom.
    let slot = slot_of(&founding);
    let stale: Vec<EventId> = sessions
        .iter()
        .filter(|(_, meta)| meta.slot == slot)
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some(meta) = sessions.remove(&id) {
            ctx.ledger.session_changed(&me, &meta.opponent, false);
            tracing::info!(session = %id, canonical = %canonical.id, "superseded by the canonical Game Session of its slot");
        }
    }
    ctx.ledger.session_changed(&me, &opponent, true);
    tracing::info!(session = %canonical.id, %opponent, seat = my_seat.name(), cadence = cadence.token(), "tracking session");
    sessions.insert(
        canonical.id,
        SessionMeta {
            session: canonical,
            slot,
            terms,
            start,
            my_seat,
            opponent,
            cadence,
            my_side_evals: VecDeque::new(),
            next_action_at: 0,
            planned_half_move: 0,
            published: None,
            timeout_seen_at: None,
            concluded: None,
            warned: BTreeSet::new(),
        },
    );
    Ok(())
}

/// Route one delivered event.
#[allow(clippy::too_many_arguments)]
async fn handle_event(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &Arc<RelayClock>,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    courted_entries: &mut BTreeSet<EventId>,
    considered_challenges: &mut BTreeSet<EventId>,
    founded_pairings: &mut BTreeSet<EventId>,
    offered_rematches: &mut BTreeMap<EventId, u64>,
    own_entries: &Arc<PoolLocks>,
    starred_today: &mut u64,
    rng: &mut SplitMix64,
    me: PublicKey,
    event: Event,
) {
    match event.kind {
        Kind::Custom(OPEN_CHALLENGE_KIND) => {
            if courted_entries.insert(event.id) {
                if let Err(reason) = court_pool_entry(
                    client,
                    ctx,
                    relay_clock,
                    sessions,
                    own_entries,
                    rng,
                    me,
                    &event,
                )
                .await
                {
                    tracing::debug!(entry = %event.id, reason, "pool entry not courted");
                }
            }
        }
        Kind::Custom(PAIRING_KIND) => {
            if founded_pairings.insert(event.id) {
                if let Err(reason) =
                    consider_pairing(client, ctx, relay_clock, sessions, me, &event).await
                {
                    tracing::debug!(pairing = %event.id, reason, "pairing not founded");
                }
            }
        }
        Kind::Custom(DIRECT_CHALLENGE_KIND) => {
            if considered_challenges.insert(event.id) {
                if let Err(reason) = consider_direct_challenge(
                    client,
                    ctx,
                    relay_clock,
                    sessions,
                    offered_rematches,
                    rng,
                    me,
                    &event,
                )
                .await
                {
                    tracing::debug!(challenge = %event.id, reason, "challenge not accepted");
                }
            }
        }
        Kind::Custom(GAME_SESSION_KIND) => {
            if let Err(error) = track_session(client, ctx, sessions, me, event).await {
                tracing::debug!(error = %error, "session not tracked");
            }
        }
        Kind::Custom(PLY_KIND) => {
            // A discovery trigger only (§6.4): the tick re-derives everything
            // from a session-scoped fetch. Nothing to do inline.
        }
        Kind::Custom(CONCLUSION_KIND) => {
            // A Conclusion of a tracked session: service it now rather than
            // at the next tick — the module decides whether it conforms.
            if let Some(session_id) = tags::event_with_marker(&event, "game_session") {
                if sessions.contains_key(&session_id) {
                    service_all(
                        client,
                        ctx,
                        relay_clock,
                        sessions,
                        rng,
                        me,
                        starred_today,
                        offered_rematches,
                        Some(session_id),
                    )
                    .await;
                }
            }
        }
        _ => {}
    }
}

/// Terminate a tracked session on its canonical Conclusion: release the
/// fleet budget slot, then (if the persona is willing) propose a rematch.
#[allow(clippy::too_many_arguments)]
async fn terminate_session(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    me: PublicKey,
    meta: &SessionMeta,
    conclusion: &EventId,
    concluded_at: u64,
    offered: &mut BTreeMap<EventId, u64>,
) {
    // A game that ended long ago — re-observed after a restart — is not
    // proposed a rematch of: the offer is for a player still at the board.
    if relay_clock.now_secs().saturating_sub(concluded_at) > REMATCH_WINDOW_SECS {
        ctx.ledger.session_changed(&me, &meta.opponent, false);
        return;
    }
    ctx.ledger.session_changed(&me, &meta.opponent, false);
    maybe_offer_rematch(
        client,
        ctx,
        relay_clock,
        &meta.terms,
        conclusion,
        false,
        offered,
    )
    .await;
}

/// Challenge the opponent to a rematch of `concluded`, unless our own
/// challenge for this game is still LIVE, the persona is unwilling (a
/// per-game decision the `always` flag overrides — reciprocating a human's
/// explicit proposal is unconditional), or — against a sibling bot — the
/// bot-vs-bot budget is spent. Shared by both triggers (the proactive
/// challenge on the verdict and the reply to an incoming one), which is why
/// the one-live-challenge-per-game guard lives here. Best-effort: a failure
/// is logged, never propagated.
async fn maybe_offer_rematch(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    concluded: &SessionTerms,
    conclusion: &EventId,
    always: bool,
    offered: &mut BTreeMap<EventId, u64>,
) {
    let me = ctx.keys.public_key();
    let Some(opponent) = concluded.opponent_of(&me) else {
        return;
    };
    let now = Timestamp::now().as_secs();
    if offered
        .get(&concluded.id)
        .is_some_and(|deadline| *deadline > now)
    {
        return;
    }
    if !always
        && !rematch::wants_rematch(
            ctx.bot_seed,
            &concluded.id,
            rematch::DEFAULT_REMATCH_PROBABILITY,
        )
    {
        return;
    }
    if ctx.ledger.is_member(&opponent) && !ctx.ledger.may_court_sibling() {
        tracing::debug!(against = %opponent, "rematch not proposed: bot-vs-bot budget spent");
        return;
    }
    let terms = concluded.clone();
    let conclusion = *conclusion;
    let hint = tags::norm_relay(&ctx.fleet.relay_url).to_owned();
    let result = publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(DIRECT_CHALLENGE_KIND),
        Pow::Mined(ctx.fleet.pow_difficulty),
        move |created_at| {
            let accept_until = created_at.as_secs().saturating_add(REMATCH_WINDOW_SECS);
            let tags =
                rematch::rematch_challenge_tags(&terms, &conclusion, &me, accept_until, &hint)
                    .unwrap_or_default();
            (tags, String::new())
        },
    )
    .await;
    match result {
        Ok(challenge) => {
            let deadline: u64 = tags::accept_until(&challenge)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| now.saturating_add(REMATCH_WINDOW_SECS));
            offered.insert(concluded.id, deadline);
            tracing::info!(
                rematch_of = %concluded.id,
                against = %opponent,
                challenge = %challenge.id,
                "proposed a rematch"
            );
        }
        Err(error) => tracing::warn!(error = %error, "rematch challenge publish failed"),
    }
}

/// React to a live pool entry (§6.2): compatibility, fleet budget, async
/// checks (mute lists, `following` filter), a persona reaction delay, then
/// our own mirror entry.
#[allow(clippy::too_many_arguments)]
async fn court_pool_entry(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &Arc<RelayClock>,
    sessions: &BTreeMap<EventId, SessionMeta>,
    own_entries: &Arc<PoolLocks>,
    rng: &mut SplitMix64,
    me: PublicKey,
    entry: &Event,
) -> std::result::Result<(), &'static str> {
    let now = relay_clock.now_secs();
    if !crate::persona::is_present(&ctx.config.schedule, ctx.bot_seed, chrono::Utc::now()) {
        return Err("absent");
    }
    let candidate: PoolCandidate = courtship::evaluate_open_challenge(
        entry,
        &me,
        &ctx.fleet.relay_url,
        &ctx.matchmaker,
        &ctx.rules,
        &ctx.fleet.game,
        &ctx.config.play,
        now,
        COURT_MARGIN_SECS,
    )?;
    // The per-cadence cap, and one own entry at a time PER CADENCE: a
    // Pairing founds a session the bot must then serve, under that cap.
    let cadence = candidate.cadence;
    if load_of(sessions, cadence) >= ctx.config.play.max_concurrent.cap(cadence) {
        return Err("cap reached for this cadence");
    }
    if own_entries.until(cadence) > now {
        return Err("our own entry at this cadence is still live");
    }
    if ctx.ledger.is_member(&candidate.challenger) && !ctx.ledger.may_court_sibling() {
        return Err("bot-vs-bot budget spent");
    }
    if is_publicly_blocked(client, &candidate.challenger, &me).await
        || is_publicly_blocked(client, &me, &candidate.challenger).await
    {
        return Err("public block on either side");
    }
    if candidate.needs_following_check && !follows(client, &candidate.challenger, &me).await {
        return Err("their following filter excludes us (fail-closed)");
    }

    // Persona reaction delay (bounded by the entry's life) — then the entry,
    // in a task of its own: the actor's loop must keep serving its games
    // meanwhile (a live game on a fast cadence cannot wait 30 s).
    let delay = crate::persona::think_seconds(
        &ctx.config.tempo.think,
        rng,
        candidate
            .accept_until
            .saturating_sub(now)
            .saturating_sub(COURT_MARGIN_SECS),
    )
    .min(30);
    own_entries.hold(
        cadence,
        now.saturating_add(delay)
            .saturating_add(OWN_ENTRY_WINDOW_SECS),
    );
    let client = client.clone();
    let keys = ctx.keys.clone();
    let relay_clock = Arc::clone(relay_clock);
    let ledger = Arc::clone(&ctx.ledger);
    let own_entries = Arc::clone(own_entries);
    let pow = Pow::Mined(ctx.fleet.pow_difficulty);
    let variant = candidate.variant.clone();
    let mirror = candidate.mirror;
    let spec = candidate.spec.clone();
    let matchmaker = ctx.matchmaker;
    let rules = ctx.rules;
    let game = ctx.fleet.game.clone();
    let against = entry.pubkey;
    // Self-timed designation: the configured relay, normalized (Canonical
    // Timing NIP §Timing modes and mode selection) — also the relay hint of
    // the rules reference.
    let timing_relay = tags::norm_relay(&ctx.fleet.relay_url).to_owned();
    tokio::spawn(async move {
        tokio::time::sleep(StdDuration::from_secs(delay)).await;
        let result = publish_self_timed(
            &client,
            &keys,
            &relay_clock,
            Kind::Custom(OPEN_CHALLENGE_KIND),
            pow,
            move |created_at| {
                let mut event_tags = vec![
                    p_role(&matchmaker, "matchmaker"),
                    Tag::custom(
                        "e",
                        [rules.to_hex(), timing_relay.clone(), "rules".to_owned()],
                    ),
                    Tag::custom("timing_relay", [timing_relay.clone()]),
                    Tag::custom("game", [game.clone()]),
                    Tag::custom("variant", ["self".to_owned(), variant.clone()]),
                ];
                // The free MIRROR form imposes the shared variant back; courting
                // an ASYMMETRIC (premium) entry, our entry must leave the
                // opponent unconstrained (see `courtship::PoolCandidate::mirror`).
                if mirror {
                    event_tags.push(Tag::custom(
                        "variant",
                        ["opponent".to_owned(), variant.clone()],
                    ));
                }
                event_tags.push(Tag::custom(
                    "accept_until",
                    [created_at
                        .as_secs()
                        .saturating_add(OWN_ENTRY_WINDOW_SECS)
                        .to_string()],
                ));
                for row in &spec {
                    event_tags.push(Tag::custom("time_control", row.clone()));
                }
                (event_tags, String::new())
            },
        )
        .await;
        match result {
            Ok(_) => {
                ledger.pool_presence(me, true);
                tracing::info!(%against, "entered the pool (reactive)");
            }
            Err(error) => {
                own_entries.release(cadence);
                tracing::warn!(error = %error, "pool entry publish failed");
            }
        }
    });
    Ok(())
}

/// The load of one cadence family: how many tracked sessions it holds (the
/// per-family caps, ADR-0039 §6).
fn load_of(sessions: &BTreeMap<EventId, SessionMeta>, cadence: Cadence) -> u32 {
    sessions
        .values()
        .filter(|meta| meta.cadence == cadence)
        .fold(0_u32, |n, _| n.saturating_add(1))
}

/// The pool's one-entry lock, per cadence (ADR-0039 §7): the `accept_until`
/// of the bot's own live entry in each family, `0` when none. Shared with the
/// publish task, which releases the lock on a failed publish.
#[derive(Debug, Default)]
struct PoolLocks {
    byoyomi: std::sync::atomic::AtomicU64,
    blitz: std::sync::atomic::AtomicU64,
    rapid: std::sync::atomic::AtomicU64,
    correspondence: std::sync::atomic::AtomicU64,
}

impl PoolLocks {
    const fn slot(&self, cadence: Cadence) -> &std::sync::atomic::AtomicU64 {
        match cadence {
            Cadence::Byoyomi => &self.byoyomi,
            Cadence::Blitz => &self.blitz,
            Cadence::Rapid => &self.rapid,
            Cadence::Correspondence => &self.correspondence,
        }
    }

    /// The family's live entry deadline (unix seconds), `0` when none.
    fn until(&self, cadence: Cadence) -> u64 {
        self.slot(cadence)
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Hold the family until `until`.
    fn hold(&self, cadence: Cadence, until: u64) {
        self.slot(cadence)
            .store(until, std::sync::atomic::Ordering::Relaxed);
    }

    /// Release the family (a failed publish).
    fn release(&self, cadence: Cadence) {
        self.hold(cadence, 0);
    }
}

/// Found the session a Pairing naming us declares (kind 3422 §Signing party,
/// matchmaking path): as soon as it is observed, unless a canonical Game
/// Session for it already exists.
async fn consider_pairing(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    me: PublicKey,
    pairing: &Event,
) -> std::result::Result<(), &'static str> {
    if pairing.pubkey != ctx.matchmaker {
        return Err("not our matchmaker's Pairing");
    }
    if pairing.verify().is_err() {
        return Err("invalid signature");
    }
    let now = Timestamp::now().as_secs();
    let plan = founding::found_on_pairing(
        pairing,
        me,
        &ctx.fleet.relay_url,
        &ctx.rules,
        &ctx.fleet.game,
        &ctx.describe,
        now,
        COURT_MARGIN_SECS.min(10),
    )?;
    ctx.ledger.pool_presence(me, false);
    // Do not publish if a canonical Game Session for the Pairing is already
    // observed (the other player founded first): track it instead.
    let existing = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(GAME_SESSION_KIND))
            .event(pairing.id),
    )
    .await
    .unwrap_or_default();
    if let Some((canonical, _)) = founding::canonical_session(existing.iter(), pairing) {
        let canonical = canonical.clone();
        if let Err(error) = track_session(client, ctx, sessions, me, canonical).await {
            tracing::debug!(error = %error, "the existing Game Session is not trackable");
        }
        return Err("already founded by the other player");
    }
    let session = publish_game_session(client, ctx, relay_clock, &plan)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "Game Session publish failed");
            "publish failed"
        })?;
    tracing::info!(pairing = %pairing.id, session = %session.id, "founded the session on the Pairing");
    if let Err(error) = track_session(client, ctx, sessions, me, session).await {
        tracing::debug!(error = %error, "our Game Session is not tracked yet");
    }
    Ok(())
}

/// Publish a Game Session per `plan` — no proof of work is prescribed on
/// kind 3422.
async fn publish_game_session(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    plan: &founding::GameSessionPlan,
) -> Result<Event> {
    let hint = tags::norm_relay(&ctx.fleet.relay_url).to_owned();
    let plan = plan.clone();
    publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(GAME_SESSION_KIND),
        Pow::None,
        move |_created_at| (plan.tags(&hint), plan.position.clone()),
    )
    .await
}

/// Consider a Direct Challenge addressed to us (§6.3) — fresh or rematch —
/// and accept it by founding the Game Session when the persona says yes.
#[allow(clippy::too_many_arguments)]
async fn consider_direct_challenge(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &Arc<RelayClock>,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    offered_rematches: &mut BTreeMap<EventId, u64>,
    rng: &mut SplitMix64,
    me: PublicKey,
    challenge: &Event,
) -> std::result::Result<(), &'static str> {
    if challenge.verify().is_err() {
        return Err("invalid signature");
    }
    let now = relay_clock.now_secs();
    let plan: AcceptPlan = courtship::evaluate_direct_challenge(
        challenge,
        &me,
        &ctx.fleet.relay_url,
        &ctx.rules,
        &ctx.fleet.game,
        &ctx.config.play,
        now,
        COURT_MARGIN_SECS,
        rng,
    )?;
    // A rematch challenge: its constraints against the concluded session,
    // and the persona's per-game willingness (a human's explicit proposal is
    // answered unconditionally).
    if let Some(refs) = plan.rematch {
        verify_rematch(client, ctx, challenge, refs).await?;
        let human = !ctx.ledger.is_member(&plan.challenger);
        if !human
            && !rematch::wants_rematch(
                ctx.bot_seed,
                &refs.concluded,
                rematch::DEFAULT_REMATCH_PROBABILITY,
            )
        {
            return Err("rematch declined (persona)");
        }
    }
    // The per-cadence cap.
    if load_of(sessions, plan.cadence) >= ctx.config.play.max_concurrent.cap(plan.cadence) {
        return Err("cap reached for this cadence");
    }
    // Acceptance timing by pace (resolved question 5): live only while
    // present; correspondence at any hour after a credible delay.
    if !plan.cadence.is_correspondence()
        && !crate::persona::is_present(&ctx.config.schedule, ctx.bot_seed, chrono::Utc::now())
    {
        return Err("absent (live challenge)");
    }
    if is_publicly_blocked(client, &plan.challenger, &me).await
        || is_publicly_blocked(client, &me, &plan.challenger).await
    {
        return Err("public block on either side");
    }
    if ctx.ledger.is_member(&plan.challenger) && !ctx.ledger.may_court_sibling() {
        return Err("bot-vs-bot budget spent");
    }
    // A challenge replayed after a restart may already be accepted (kind
    // 3422 §Idempotence: one Game Session per slot): track what exists
    // instead of founding twice.
    let existing = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(GAME_SESSION_KIND))
            .event(challenge.id),
    )
    .await
    .unwrap_or_default();
    if let Some((accepted, _)) = founding::canonical_session(existing.iter(), challenge) {
        let accepted = accepted.clone();
        if let Err(error) = track_session(client, ctx, sessions, me, accepted).await {
            tracing::debug!(error = %error, "the existing acceptance is not trackable");
        }
        return Err("already accepted");
    }

    // The acceptance IS the Game Session (kind 3422 §Signing party) — after
    // the persona's reflection, in a task of its own so the actor's loop
    // keeps serving its games meanwhile. The Game Session reaches the loop
    // through the subscription (it names the bot) and is tracked then.
    let session_plan = founding::accept_direct_challenge(challenge, &plan, me, &ctx.describe)?;
    if let Some(refs) = plan.rematch {
        // Accepting theirs supersedes proposing ours.
        offered_rematches.insert(refs.concluded, u64::MAX);
    }
    let delay = crate::persona::think_seconds(
        &ctx.config.tempo.think,
        rng,
        plan.accept_until
            .saturating_sub(now)
            .saturating_sub(COURT_MARGIN_SECS),
    )
    .min(60);
    let client = client.clone();
    let keys = ctx.keys.clone();
    let relay_clock = Arc::clone(relay_clock);
    let hint = tags::norm_relay(&ctx.fleet.relay_url).to_owned();
    let challenge_id = challenge.id;
    let challenger = plan.challenger;
    let rematch = plan.rematch.is_some();
    tokio::spawn(async move {
        tokio::time::sleep(StdDuration::from_secs(delay)).await;
        let result = publish_self_timed(
            &client,
            &keys,
            &relay_clock,
            Kind::Custom(GAME_SESSION_KIND),
            Pow::None,
            move |_created_at| (session_plan.tags(&hint), session_plan.position.clone()),
        )
        .await;
        match result {
            Ok(session) => tracing::info!(
                challenge = %challenge_id,
                %challenger,
                session = %session.id,
                rematch,
                "accepted a direct challenge by founding the session"
            ),
            Err(error) => tracing::warn!(error = %error, "acceptance publish failed"),
        }
    });
    Ok(())
}

/// Verify a rematch challenge against the concluded session it names (kind
/// 3420 §Rematch challenge): the session is one we played, its Conclusion
/// really is one the rule system yields (`check`, through the module — never
/// believed), and every constrained term mirrors it.
async fn verify_rematch(
    client: &Client,
    ctx: &BotContext,
    challenge: &Event,
    refs: courtship::RematchRefs,
) -> std::result::Result<(), &'static str> {
    let concluded = fetch_event(client, refs.concluded)
        .await
        .map_err(|_| "concluded session unavailable")?;
    if concluded.kind != Kind::Custom(GAME_SESSION_KIND) || concluded.verify().is_err() {
        return Err("rematch_of is not a Game Session");
    }
    let founding = fetch_founding(client, &concluded)
        .await
        .map_err(|_| "the concluded session's founding is unavailable")?;
    let terms =
        session::terms(&concluded, &founding).map_err(|_| "non-conforming concluded session")?;
    if !terms.is_player(&ctx.keys.public_key()) {
        return Err("we did not play the concluded session");
    }
    let conclusion = fetch_event(client, refs.concluded_by)
        .await
        .map_err(|_| "concluded_by unavailable")?;
    if conclusion.verify().is_err() {
        return Err("concluded_by has an invalid signature");
    }
    founding::rematch_terms_ok(challenge, &terms, &conclusion)?;
    // One rematch per concluded session: a Game Session already founded on
    // any rematch challenge of it makes this one moot.
    let siblings = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(DIRECT_CHALLENGE_KIND))
            .event(concluded.id),
    )
    .await
    .map_err(|_| "the concluded session's rematch challenges are unavailable")?;
    for sibling in siblings.iter().filter(|sibling| {
        tags::events_with_marker(sibling, "rematch_of").as_slice() == [concluded.id]
    }) {
        let founded = fetch_many(
            client,
            Filter::new()
                .kind(Kind::Custom(GAME_SESSION_KIND))
                .event(sibling.id),
        )
        .await
        .unwrap_or_default();
        if founding::canonical_session(founded.iter(), sibling).is_some() {
            return Err("a rematch of this session is already founded");
        }
    }
    // The proof of conclusion, checked by the module: the Conclusion must
    // conform at its cutoff.
    let created_at = concluded.created_at.as_secs();
    let start = terms.start_at.map_or(created_at, |at| at.max(created_at));
    let raw = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(PLY_KIND))
            .event(concluded.id),
    )
    .await
    .map_err(|_| "the concluded session's plies are unavailable")?;
    let events = Events::from_relay(raw.iter(), &terms, ctx.describe.max_step);
    let claim = session::conclusion(&conclusion, &terms).ok_or("malformed concluded_by")?;
    let request = chain::session_request(&terms, start, &events);
    let verdict = ctx
        .with_module(|oracle| module::check(oracle, &request, &claim))
        .map_err(|_| "module unavailable")?
        .map_err(|_| "the module gave no answer")?;
    match verdict {
        Check::Conforming(_) => Ok(()),
        Check::Wrong { .. } => Err("concluded_by does not conform to the rule system"),
        Check::NoVerdict(_) => Err("concluded_by is out of reach"),
    }
}

/// Service every tracked session (the tick body), or only `only`: compute
/// the live view through the module, then act — conclude, play our ply,
/// star.
#[allow(clippy::too_many_arguments)]
async fn service_all(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    rng: &mut SplitMix64,
    me: PublicKey,
    starred_today: &mut u64,
    offered_rematches: &mut BTreeMap<EventId, u64>,
    only: Option<EventId>,
) {
    let ids: Vec<EventId> = match only {
        Some(id) => vec![id],
        None => sessions.keys().copied().collect(),
    };
    for id in ids {
        let Some(meta) = sessions.get_mut(&id) else {
            continue;
        };
        match service_session(client, ctx, relay_clock, meta, rng, starred_today).await {
            // The canonical Conclusion that ended it — carried through, since
            // the rematch challenge cites it as its `concluded_by` proof.
            Ok(Some((conclusion_id, concluded_at))) => {
                if let Some(meta) = sessions.remove(&id) {
                    terminate_session(
                        client,
                        ctx,
                        relay_clock,
                        me,
                        &meta,
                        &conclusion_id,
                        concluded_at,
                        offered_rematches,
                    )
                    .await;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(session = %id, error = %error, "session service hiccup");
            }
        }
    }
}

/// Service one session. Returns `Ok(Some((conclusion, cutoff)))` — the id
/// and the cutoff of the canonical Conclusion that ended it — when the
/// session is finished and may be dropped, `Ok(None)` while it is still
/// running.
#[allow(clippy::too_many_arguments)]
async fn service_session(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    meta: &mut SessionMeta,
    rng: &mut SplitMix64,
    starred_today: &mut u64,
) -> Result<Option<(EventId, u64)>> {
    let session_id = meta.session.id;
    // The relay's clock, as estimated: the cutoff of every question asked
    // of the module, and what deadlines are measured against.
    let now = relay_clock.now_secs();

    // The session's Plies and Conclusions (session-scoped `#e` fetch — §6.4),
    // filtered as the ABI prescribes.
    let raw = fetch_many(
        client,
        Filter::new()
            .kinds([Kind::Custom(PLY_KIND), Kind::Custom(CONCLUSION_KIND)])
            .event(session_id),
    )
    .await?;
    let events = Events::from_relay(raw.iter(), &meta.terms, ctx.describe.max_step);
    let request = chain::session_request(&meta.terms, meta.start, &events);

    // The canonical Conclusion, if any: the session is over. Every other
    // Conclusion is checked once and a wrong claim logged — the client/kernel
    // divergence metric an operator watches.
    let conclusions: Vec<serde_json::Value> =
        events.conclusions.iter().map(|(_, c)| c.clone()).collect();
    let canonical =
        ctx.with_module(|oracle| module::select_conclusion(oracle, &request, &conclusions))??;
    for (id, claim) in &events.conclusions {
        if meta.warned.contains(id) || canonical.as_ref().is_some_and(|c| c.id == id.to_hex()) {
            continue;
        }
        let verdict = ctx.with_module(|oracle| module::check(oracle, &request, claim))??;
        if let Check::Wrong { claimed, expected } = verdict {
            meta.warned.insert(*id);
            tracing::warn!(
                session = %session_id,
                conclusion = %id,
                %claimed,
                expected = ?expected,
                "non-conforming Conclusion (client/kernel divergence)"
            );
        }
    }
    if let Some(canonical) = canonical {
        let conclusion_id = EventId::from_hex(&canonical.id)?;
        if now.saturating_sub(canonical.cutoff) <= REMATCH_WINDOW_SECS {
            maybe_star(
                client,
                ctx,
                relay_clock,
                meta,
                rng,
                &canonical.verdict.status,
                starred_today,
            )
            .await;
        }
        tracing::info!(
            session = %session_id,
            status = %canonical.verdict.status,
            first = canonical.verdict.result.first,
            second = canonical.verdict.result.second,
            "session concluded"
        );
        return Ok(Some((conclusion_id, canonical.cutoff)));
    }

    let view =
        ctx.with_module(|oracle| session_view(oracle, &meta.terms, meta.start, &events, now))??;

    // A Conclusion we published: once its echo is back and it is not the
    // canonical one, it did not conform (the opponent moved between the
    // prediction and the cutoff, say) — the session goes on, and a fresh
    // prediction may conclude it again. While the echo has not come back,
    // re-send it after the grace, never publish another.
    if let Some((ours, at)) = &meta.concluded {
        if events.conclusions.iter().any(|(id, _)| *id == ours.id) {
            tracing::warn!(session = %session_id, conclusion = %ours.id, "our Conclusion did not conform; the session goes on");
            meta.concluded = None;
        } else if now.saturating_sub(*at) >= REPUBLISH_GRACE_SECS {
            // Its stamp is stale for a strict timing relay by now: let the
            // decision below publish a fresh one — a second conforming
            // Conclusion by the same signer is harmless (the earliest rules).
            tracing::info!(session = %session_id, "the Conclusion's echo never came back; concluding afresh");
            meta.concluded = None;
        } else {
            return Ok(None);
        }
    }

    // A wanted verdict? The prediction is the module's, at this instant, with
    // our seat as the invoker.
    let predicted = ctx.with_module(|oracle| {
        predicted_verdict(oracle, &meta.terms, meta.start, &events, meta.my_seat, now)
    })??;
    if let Some(verdict) = predicted {
        let decision = conclusion::decision(
            &verdict,
            meta.my_seat,
            &view,
            wants_draw(ctx, meta),
            wants_to_resign(ctx, meta),
        );
        if let Some(why) = decision {
            let due = match why {
                Conclude::WinOnTime => {
                    // The courtesy delay: a human whose clock just fell is not
                    // flagged to the second (§6.6).
                    let seen = *meta.timeout_seen_at.get_or_insert(now);
                    now.saturating_sub(seen) >= ctx.config.play.timeout_courtesy_secs
                }
                Conclude::Terminal | Conclude::AcceptDraw | Conclude::Resign => true,
            };
            if due {
                let (tags, content) = conclusion::conclusion_tags(
                    &meta.terms,
                    &verdict,
                    tags::norm_relay(&ctx.fleet.relay_url),
                );
                let event = publish_self_timed(
                    client,
                    &ctx.keys,
                    relay_clock,
                    Kind::Custom(CONCLUSION_KIND),
                    Pow::Mined(ctx.fleet.pow_difficulty),
                    move |_created_at| (tags.clone(), content.clone()),
                )
                .await?;
                meta.concluded = Some((event, now));
                tracing::info!(session = %session_id, ?why, status = %verdict.status, "concluded");
            }
            return Ok(None);
        }
    }
    meta.timeout_seen_at = None;

    if view.terminal || view.on_move != meta.my_seat {
        return Ok(None);
    }

    // Same-slot idempotence guard (§6.5). Our ply for this very half-move may
    // already be out while the session fetch has not caught up with it. One
    // slot, one search: hold until the chain advances past it, and after a
    // grace period re-send the SAME content — identical contents collapse to
    // one candidate in the selection rule, so the re-send is harmless where a
    // fresh search is not, and a genuinely lost publish cannot deadlock the
    // seat.
    if let Some(published) = &mut meta.published {
        if published.half_move == view.next_half_move {
            if now.saturating_sub(published.at) >= REPUBLISH_GRACE_SECS {
                publish_ply(
                    client,
                    ctx,
                    relay_clock,
                    session_id,
                    meta.opponent,
                    published.step,
                    published.content.clone(),
                    published.offer_draw,
                )
                .await?;
                published.at = now;
                tracing::info!(
                    session = %session_id,
                    half_move = view.next_half_move,
                    "re-sent the unacknowledged ply (same content)"
                );
            }
            return Ok(None);
        }
    }

    // Our turn. Pace it: presence for correspondence, think delay for all.
    let sigma = if meta.cadence.is_correspondence() {
        SIGMA_CORRESPONDENCE_SECS
    } else {
        SIGMA_LIVE_SECS
    };
    let deadline = view.anchor.saturating_add(view.affordable);
    let latest_start = deadline.saturating_sub(sigma);
    if meta.planned_half_move != view.next_half_move {
        // (Re)plan the reflection for this half-move.
        let think_cfg = if meta.cadence.is_correspondence() {
            &ctx.config.tempo.correspondence_think
        } else {
            &ctx.config.tempo.think
        };
        let ceiling = latest_start.saturating_sub(now).max(1);
        let think = crate::persona::think_seconds(think_cfg, rng, ceiling);
        let mut play_at = now.saturating_add(think);
        // Correspondence realism: prefer the presence window, except under
        // clock pressure (§6.7: realism never outranks not flagging).
        if meta.cadence.is_correspondence()
            && !crate::persona::is_present(&ctx.config.schedule, ctx.bot_seed, chrono::Utc::now())
        {
            let next_window = crate::persona::next_presence(
                &ctx.config.schedule,
                ctx.bot_seed,
                chrono::Utc::now(),
                48,
            )
            .map(|at| u64::try_from(at.timestamp()).unwrap_or(now));
            play_at = match next_window {
                Some(window_at) if window_at < latest_start => window_at.max(play_at),
                _ => play_at.min(latest_start), // sneak the move in from the phone
            };
        }
        meta.planned_half_move = view.next_half_move;
        meta.next_action_at = play_at.min(latest_start);
    }
    if now < meta.next_action_at {
        let wait = meta.next_action_at.saturating_sub(now);
        // Defer to the coarse tick when the wake is still more than a tick
        // away, and always for correspondence. A LIVE game whose planned move
        // falls WITHIN one tick must not defer: sleep the sub-tick residual
        // and play in-call so we hit the planned instant precisely.
        if meta.cadence.is_correspondence() || wait >= TICK_SECS {
            return Ok(None); // a later tick will come back
        }
        tokio::time::sleep(StdDuration::from_secs(wait)).await;
    }

    // Recompute `now` after any pacing sleep so `play_ply` sizes its search
    // budget against the time that actually remains.
    let play_now = relay_clock.now_secs();
    play_ply(
        client,
        ctx,
        relay_clock,
        meta,
        &view,
        rng,
        play_now,
        latest_start,
    )
    .await?;
    Ok(None)
}

/// Draw temperament (§6.6): accept when the standing assessment does not
/// say we are clearly better.
fn wants_draw(ctx: &BotContext, meta: &SessionMeta) -> bool {
    let latest = meta.my_side_evals.back().copied().unwrap_or(0);
    match ctx.config.play.draw_offer {
        DrawOffer::Never => latest <= -150, // accept only when worse
        DrawOffer::Balanced => latest <= 60,
        DrawOffer::Eager => latest <= 150,
    }
}

/// Resignation temperament (§6.6): assessment below the threshold for two
/// consecutive own turns.
fn wants_to_resign(ctx: &BotContext, meta: &SessionMeta) -> bool {
    let threshold = ctx.config.play.resign_threshold;
    let mut recent = meta.my_side_evals.iter().rev();
    matches!(
        (recent.next(), recent.next()),
        (Some(&a), Some(&b)) if a < threshold && b < threshold
    )
}

/// Publish a Ply carrying `content` for our `step` slot — the single
/// publication path, shared by `play_ply` (a fresh search) and the same-slot
/// guard's idempotent re-send.
#[allow(clippy::too_many_arguments)]
async fn publish_ply(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    session_id: EventId,
    opponent: PublicKey,
    step: u32,
    content: String,
    offer_draw: bool,
) -> Result<()> {
    let hint = tags::norm_relay(&ctx.fleet.relay_url).to_owned();
    publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(PLY_KIND),
        Pow::Mined(ctx.fleet.pow_difficulty),
        move |_created_at| {
            let mut event_tags = vec![
                Tag::custom(
                    "e",
                    [session_id.to_hex(), hint.clone(), "game_session".to_owned()],
                ),
                Tag::custom(
                    "p",
                    [opponent.to_hex(), hint.clone(), "opponent".to_owned()],
                ),
                Tag::custom("step", [step.to_string()]),
            ];
            if offer_draw {
                event_tags.push(Tag::custom("draw", Vec::<String>::new()));
            }
            (event_tags, content.clone())
        },
    )
    .await?;
    Ok(())
}

/// Choose and publish our ply (§6.5): the search over the tip the module
/// reached, then the module's `legal_moves` as the last word on what is
/// played.
#[allow(clippy::too_many_arguments)]
async fn play_ply(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    meta: &mut SessionMeta,
    view: &SessionView,
    rng: &mut SplitMix64,
    now: u64,
    latest_start: u64,
) -> Result<()> {
    // The module's legal moves in the tip: the rule, whatever the search
    // thinks (ADR-0034 §Normativity).
    let legal = ctx.with_module(|oracle| module::legal_moves(oracle, &view.tip))??;
    if legal.is_empty() {
        bail!("no legal move on our turn (the view should be terminal)");
    }
    // Search budget: the persona's allowance, clamped by the clock.
    let clock_allowance_ms = latest_start.saturating_sub(now).saturating_mul(1_000);
    let budget_ms = ctx
        .config
        .play
        .strength
        .time_ms
        .min(clock_allowance_ms.max(50));
    let strength = Strength {
        max_depth: ctx.config.play.strength.depth,
        tt_capacity: 200_000,
        weights: sashite_sanki_player::EvalWeights::default(),
        seed: rng.next_u64(),
    };
    let tip = Position::parse(&view.tip).map_err(|e| {
        anyhow!(
            "the search engine cannot parse the module's tip {}: {e:?}",
            view.tip
        )
    })?;
    let occurrences = view.occurrences.clone();
    let halfmove_clock = view.halfmove_clock;
    let choice = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now()
            .checked_add(StdDuration::from_millis(budget_ms))
            .unwrap_or_else(std::time::Instant::now);
        let stop = move || std::time::Instant::now() >= deadline;
        let ctx = PlayerContext {
            position: &tip,
            occurrences: &occurrences,
            halfmove_clock,
        };
        let limits = Limits {
            max_nodes: None,
            should_stop: Some(&stop),
        };
        choose(&ctx, &strength, &limits)
    })
    .await
    .context("search task failed")?;

    // The search's choice, if the module admits it; else — a divergence
    // between the search engine and the rule system, logged — a move the
    // module admits, drawn among its.
    let (content, eval_cp) = match choice {
        Some(choice) if legal.contains(&move_content(&choice.mv)) => {
            tracing::debug!(
                eval = choice.eval_cp,
                depth = choice.depth,
                nodes = choice.nodes,
                "searched"
            );
            (move_content(&choice.mv), choice.eval_cp)
        }
        other => {
            tracing::warn!(
                session = %meta.session.id,
                chosen = other.as_ref().map(|c| move_content(&c.mv)),
                "the search's move is not one the rule system admits; playing a legal one"
            );
            let index = rng.next_index(legal.len());
            (
                legal.get(index).cloned().unwrap_or_default(),
                meta.my_side_evals.back().copied().unwrap_or(0),
            )
        }
    };

    meta.my_side_evals.push_back(eval_cp);
    if meta.my_side_evals.len() > 8 {
        meta.my_side_evals.pop_front();
    }

    // Draw offer temperament on our own ply (§6.6).
    let offer_draw = match ctx.config.play.draw_offer {
        DrawOffer::Never => false,
        DrawOffer::Balanced => eval_cp.abs() <= 60 && view.chain_len >= 24,
        DrawOffer::Eager => eval_cp.abs() <= 150 && view.chain_len >= 10,
    };

    let session_id = meta.session.id;
    let opponent = meta.opponent;
    let step = view.step;
    publish_ply(
        client,
        ctx,
        relay_clock,
        session_id,
        opponent,
        step,
        content.clone(),
        offer_draw,
    )
    .await?;
    // Arm the same-slot idempotence guard: this half-move is played — a
    // re-entry before the echo must re-send THIS content, never search again.
    meta.published = Some(PublishedPly {
        half_move: view.next_half_move,
        step,
        content,
        offer_draw,
        at: now,
    });
    tracing::info!(
        session = %session_id,
        half_move = view.next_half_move,
        eval = eval_cp,
        offer_draw,
        "played"
    );
    Ok(())
}

/// Star a notable finished session (§6.8): a fast mate or a long struggle,
/// budgeted (a rare gesture) and only while present.
#[allow(clippy::too_many_arguments)]
async fn maybe_star(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    meta: &SessionMeta,
    rng: &mut SplitMix64,
    status: &str,
    starred_today: &mut u64,
) {
    if *starred_today >= 1 {
        return;
    }
    if !crate::persona::is_present(&ctx.config.schedule, ctx.bot_seed, chrono::Utc::now()) {
        return;
    }
    let chain_len = meta.my_side_evals.len(); // proxy: own turns witnessed
    let fast_mate = status == "checkmate" && chain_len <= 12;
    let long_struggle = chain_len >= 50;
    if !(fast_mate || long_struggle) || rng.next_index(4) != 0 {
        return;
    }
    let session_id = meta.session.id;
    let result = publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Reaction,
        Pow::None,
        move |_created_at| {
            (
                vec![
                    Tag::custom(
                        "e",
                        [
                            session_id.to_hex(),
                            String::new(),
                            "sashite:session".to_owned(),
                        ],
                    ),
                    Tag::custom("k", [GAME_SESSION_KIND.to_string()]),
                ],
                "+".to_owned(),
            )
        },
    )
    .await;
    if result.is_ok() {
        *starred_today = starred_today.saturating_add(1);
        tracing::info!(session = %session_id, "starred a notable game");
    }
}

/// Whether `blocker`'s public mute list (kind 10000) targets `blocked`.
async fn is_publicly_blocked(client: &Client, blocker: &PublicKey, blocked: &PublicKey) -> bool {
    let Ok(lists) = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(MUTE_LIST_KIND))
            .author(*blocker),
    )
    .await
    else {
        return false;
    };
    lists.iter().any(|list| {
        list.tags.iter().any(|tag| {
            let s = tag.as_slice();
            s.first().map(String::as_str) == Some("p")
                && s.get(1).and_then(|v| PublicKey::parse(v).ok()).as_ref() == Some(blocked)
        })
    })
}

/// Whether `follower`'s contact list (kind 3) includes `followed` — the
/// counterparty's `following` filter, checked fail-closed.
async fn follows(client: &Client, follower: &PublicKey, followed: &PublicKey) -> bool {
    let Ok(lists) = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(CONTACTS_KIND))
            .author(*follower),
    )
    .await
    else {
        return false;
    };
    lists.iter().any(|list| {
        list.tags.iter().any(|tag| {
            let s = tag.as_slice();
            s.first().map(String::as_str) == Some("p")
                && s.get(1).and_then(|v| PublicKey::parse(v).ok()).as_ref() == Some(followed)
        })
    })
}

/// The kind-3423 `content` of a search-engine move — the same form the
/// module's `legal_moves` lists (Move Encoding — Sanki §Content format).
fn move_content(mv: &Move) -> String {
    match mv {
        Move::Board { from, to, actor } => {
            let actor = actor
                .as_ref()
                .map_or("null".to_owned(), |name| format!("\"{}\"", name.as_str()));
            format!("[\"{from}\",\"{to}\",{actor}]")
        }
        Move::Drop { piece, to } => format!("[null,\"{to}\",\"{}\"]", piece.as_str()),
    }
}

fn p_role(pubkey: &PublicKey, role: &str) -> Tag {
    Tag::custom("p", [pubkey.to_hex(), String::new(), role.to_owned()])
}

/// Minimal JSON string escaping for the kind-0 metadata fields.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len().saturating_add(2));
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

async fn fetch_many(client: &Client, filter: Filter) -> Result<Vec<Event>> {
    let events = client
        .fetch_events(filter)
        .timeout(FETCH_TIMEOUT)
        .await
        .context("fetch failed")?;
    Ok(events.into_iter().collect())
}

async fn fetch_event(client: &Client, id: EventId) -> Result<Event> {
    fetch_many(client, Filter::new().id(id))
        .await?
        .into_iter()
        .find(|event| event.id == id)
        .ok_or_else(|| anyhow!("event {id} not found on the relay"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn move_content_matches_the_kind_3423_format() {
        let board = Move::parse(r#"["a1","a4",null]"#).unwrap();
        assert_eq!(move_content(&board), r#"["a1","a4",null]"#);
        let promo = Move::parse(r#"["b7","b8","queen"]"#).unwrap();
        assert_eq!(move_content(&promo), r#"["b7","b8","queen"]"#);
        let drop = Move::parse(r#"[null,"e5","fu"]"#).unwrap();
        assert_eq!(move_content(&drop), r#"[null,"e5","fu"]"#);
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
    }

    #[test]
    fn the_search_engine_and_the_module_agree_on_move_contents() {
        // The module's `legal_moves` lists the contents a Ply carries; the
        // search engine's moves, encoded by `move_content`, must be among
        // them — the guard `play_ply` applies, exercised on the start.
        use crate::module::native::Native;
        let describe = module::describe(&mut Native).unwrap();
        for (pairing, feen) in &describe.positions {
            let legal = module::legal_moves(&mut Native, feen).unwrap();
            let position = Position::parse(feen).unwrap_or_else(|e| panic!("{pairing}: {e:?}"));
            let strength = Strength {
                max_depth: 1,
                ..Strength::default()
            };
            let ctx = PlayerContext {
                position: &position,
                occurrences: &sashite_sanki_player::Occurrences::new(),
                halfmove_clock: 0,
            };
            let choice = choose(&ctx, &strength, &Limits::default()).unwrap();
            assert!(
                legal.contains(&move_content(&choice.mv)),
                "{pairing}: {} not among the module's legal moves",
                move_content(&choice.mv)
            );
        }
    }
}
