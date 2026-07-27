//! One bot actor — a persona playing over the protocol interface
//! (ADR-0014 §3, §6): standing events, courtship, session play, arbiter
//! invocation, stars. Everything a bot does, a human client could do.
//!
//! The actor owns its relay client and subscriptions, and services its
//! sessions **serially** (the per-bot concurrency caps make this natural —
//! §7 of ADR-0015 assumes one `choose` at a time). Timed behavior (think
//! delays, correspondence pacing, win-on-time wakes) is scheduled through a
//! coarse periodic tick rather than per-event sleeps, so the notification
//! loop never blocks.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use nostr_sdk::prelude::*;
use tokio::sync::broadcast::error::RecvError;

use sashite_sanki_arbiter::event as arb;
use sashite_sanki_engine::domain::half_move::Move as EngineMove;
use sashite_sanki_engine::domain::status::{Outcome3, Status};
use sashite_sanki_engine::domain::time_control::TimeControl;
use sashite_sanki_player::{choose, Context as PlayerContext, Limits, Strength};

use crate::chain::{predicted_verdict, session_view, SessionView};
use crate::config::{BotConfig, DrawOffer, FleetSection};
use crate::courtship::{self, AcceptPlan, PoolCandidate};
use crate::fleet::Ledger;
use crate::mapping;
use crate::prng::SplitMix64;
use crate::publish::{publish_self_timed, RelayClock};
use crate::rematch;
use crate::tags;

const OPEN_CHALLENGE_KIND: u16 = 6418;
const DIRECT_CHALLENGE_KIND: u16 = 6420;
const ACCEPTED_CHALLENGE_KIND: u16 = 6421;
const GAME_SESSION_KIND: u16 = 6422;
const PLY_KIND: u16 = 6423;
const ADJUDICATION_REQUEST_KIND: u16 = 6424;
const ADJUDICATION_KIND: u16 = 6425;
const CHALLENGE_POLICY_KIND: u16 = 30420;
const MUTE_LIST_KIND: u16 = 10000;
const CONTACTS_KIND: u16 = 3;

/// Relay fetch budget.
const FETCH_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// How far back a chain of rematches may be walked when resolving a session's
/// inherited terms. A rematch of a rematch is legitimate; a relay serving a
/// cycle of them is not, and only a bound tells the two apart.
const MAX_REMATCH_HOPS: usize = 32;

/// The actor's periodic heartbeat (schedules thinks, deadline wakes, courtship).
const TICK_SECS: u64 = 5;

/// Safety margin σ reserved before the flag (mining + latency + buffer) — §6.5.
const SIGMA_LIVE_SECS: u64 = 2;
const SIGMA_CORRESPONDENCE_SECS: u64 = 60;

/// A pool entry's minimum residual life for us to court it, and the
/// `accept_until` window of our own entries (§6.2).
const COURT_MARGIN_SECS: u64 = 30;
const OWN_ENTRY_WINDOW_SECS: u64 = 180;

/// The `accept_until` window of a rematch offer (§9). The bot keeps ONE LIVE
/// offer per game (proactive or in reply), so this is the opponent's window to
/// answer and let the arbiter found the rematch. Sized for a human lingering on
/// the game-over screen — five minutes proved too short (an expired proactive
/// offer used to silence the reply path; that guard is now deadline-aware, and
/// the window generous).
const REMATCH_WINDOW_SECS: u64 = 900;

/// How far back the self-subscription replays when the bot starts. The
/// subscription carries *live* traffic; the state a restart needs is rebuilt by
/// the explicit recovery fetches, which are not bounded by this. Unbounded, the
/// relay replays the bot's entire history the moment it connects — every Ply,
/// verdict and offer it ever received — and the handlers work through a year of
/// finished games as though they had just arrived. The window is not zero
/// because `created_at` is the publisher's clock, not ours: the longest-lived
/// thing that arrives here and still deserves acting on is a Rematch Offer
/// inside its acceptance window, and three of those windows absorb both the skew
/// and a slow start.
const SELF_REPLAY_LOOKBACK_SECS: u64 = 3 * REMATCH_WINDOW_SECS;

/// Grace before locally abandoning a session whose arbiter stays silent.
const SILENT_ARBITER_GRACE_SECS: u64 = 15 * 60;

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
    /// The configured arbiter.
    pub arbiter: PublicKey,
    /// The shared fleet ledger.
    pub ledger: Arc<Ledger>,
    /// This bot's derived seed (from its pubkey).
    pub bot_seed: u64,
    /// Shutdown signal (true = stop courting, finish and disconnect).
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

/// A tracked session.
struct SessionMeta {
    session: Event,
    params: sashite_sanki_arbiter::session::SessionParams,
    opponent: PublicKey,
    correspondence: bool,
    /// When we first saw it (silent-arbiter bookkeeping happens upstream —
    /// a tracked session always has its 6422).
    my_side_evals: VecDeque<i32>,
    /// Earliest instant we may act on the pending duty (think pacing).
    next_action_at: u64,
    /// The half-move the pending think was scheduled for (re-planned when
    /// the chain moves under a premoveless bot).
    planned_half_move: u32,
    /// The ply we last published, keyed by the half-move it filled — the
    /// same-slot idempotence guard (§6.5). See the guard in `service_session`.
    published: Option<PublishedPly>,
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

/// Grace before re-sending an already-published ply whose echo has not come
/// back (three coarse ticks): long enough for any realistic relay round trip,
/// short enough never to threaten a clock even on a fast cadence.
const REPUBLISH_GRACE_SECS: u64 = 15;

/// Run the actor until shutdown. Errors bubble only for unrecoverable
/// startup conditions; per-event errors are logged and absorbed.
pub async fn run(mut ctx: BotContext) -> Result<()> {
    let me = ctx.keys.public_key();
    let span = tracing::info_span!("bot", bot = %ctx.name);
    let _enter = span.enter();

    let client = Client::builder().signer(ctx.keys.clone()).build();
    client
        .add_relay(ctx.fleet.relay_url.as_str())
        .await
        .with_context(|| format!("failed to add relay {}", ctx.fleet.relay_url))?;
    client.connect().await;

    let relay_clock = RelayClock::new();
    let mut rng = SplitMix64::new(ctx.bot_seed);
    let mut sessions: BTreeMap<EventId, SessionMeta> = BTreeMap::new();
    let mut courted_entries: BTreeSet<EventId> = BTreeSet::new();
    let mut accepted_challenges: BTreeSet<EventId> = BTreeSet::new();
    // Games we have already offered a rematch for, keyed to OUR offer's
    // `accept_until` — one LIVE offer per game, whether it went out proactively
    // (on the verdict) or in reply to the opponent's; a lapsed offer may be
    // renewed when the opponent offers after it expired.
    let mut offered_rematches: BTreeMap<EventId, u64> = BTreeMap::new();
    let mut starred_today: u64 = 0;

    reconcile_standing_events(&client, &ctx, &relay_clock).await;

    // Fixed-size subscriptions (§3): everything that p-tags the bot, plus the
    // pool feed on the matchmaker. Session-scoped 6424/6425 coverage comes
    // from the per-service fetch (the tick), not a mutable subscription — the
    // bounded-subscription variant is an optimization this loop can add later.
    let self_filter = Filter::new()
        .kinds(
            [
                DIRECT_CHALLENGE_KIND,
                GAME_SESSION_KIND,
                PLY_KIND,
                ADJUDICATION_KIND,
                rematch::REMATCH_OFFER_KIND,
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
    client
        .subscribe(self_filter, None)
        .await
        .context("subscribing to self kinds")?;
    client
        .subscribe(pool_filter, None)
        .await
        .context("subscribing to the pool")?;

    // Stateless restart (§9): rebuild active sessions from relay replay.
    if let Err(error) = recover_sessions(&client, &ctx, &mut sessions, me).await {
        tracing::warn!(error = %error, "session recovery incomplete");
    }
    // …and rebuild what we have already offered, for the same reason: the
    // one-offer-per-game gate lives in memory, so a restart would otherwise
    // reopen every game the bot ever finished.
    if let Err(error) = recover_offered_rematches(&client, &mut offered_rematches, me).await {
        tracing::warn!(error = %error, "rematch-offer recovery incomplete");
    }
    tracing::info!(sessions = sessions.len(), "bot up");

    let mut notifications = client.notifications();
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
                    &mut starred_today, &mut offered_rematches,
                )
                .await;
            }
            notification = notifications.recv() => match notification {
                Ok(RelayPoolNotification::Event { event, .. }) => {
                    handle_event(
                        &client, &ctx, &relay_clock, &mut sessions, &mut courted_entries,
                        &mut accepted_challenges, &mut offered_rematches, &mut rng, me, *event,
                    ).await;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "notifications lagged");
                }
                Err(RecvError::Closed) => break,
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
        0, // standing events are not player kinds; no PoW prescribed
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
        0,
        move |_created_at| {
            (
                vec![
                    Tag::identifier(game.clone()),
                    Tag::custom(TagKind::custom("mode"), [mode.clone()]),
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

/// Rebuild session tracking from the relay (§9): every Game Session tagging
/// the bot without a canonical Adjudication.
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
            .pubkey(me),
    )
    .await?;
    for session in game_sessions {
        let verdicts = fetch_many(
            client,
            Filter::new()
                .kind(Kind::Custom(ADJUDICATION_KIND))
                .author(ctx.arbiter)
                .event(session.id),
        )
        .await
        .unwrap_or_default();
        if !verdicts.is_empty() {
            continue; // terminated; nothing to resume
        }
        if let Err(error) = track_session(client, ctx, sessions, me, session).await {
            tracing::debug!(error = %error, "skipping unresumable session");
        }
    }
    Ok(())
}

/// Rebuild the set of games the bot has already offered a rematch for (§9,
/// stateless restart). The gate that keeps the bot to **one** offer per game is
/// a plain in-memory set; held only there it empties on every restart, and the
/// bot re-offers for each finished game the moment it sees the verdict again —
/// exactly the volley the gate exists to prevent. Our own Rematch Offers are the
/// durable record of what we already offered, so they are what is read back.
///
/// A failure here is warned about, not fatal: the cost is a possible duplicate
/// offer, which the arbiter's `(rematch, G)` idempotence absorbs — where the
/// cost of refusing to start is that the bot plays nothing at all.
async fn recover_offered_rematches(
    client: &Client,
    offered_rematches: &mut BTreeMap<EventId, u64>,
    me: PublicKey,
) -> Result<()> {
    let offers = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(rematch::REMATCH_OFFER_KIND))
            .author(me),
    )
    .await?;
    for offer in &offers {
        if let Some(concluded) = tags::sole_event_with_marker(offer, "rematch_of") {
            let deadline: u64 = tags::accept_until(offer)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| {
                    offer
                        .created_at
                        .as_secs()
                        .saturating_add(REMATCH_WINDOW_SECS)
                });
            let entry = offered_rematches.entry(concluded).or_insert(0);
            *entry = (*entry).max(deadline);
        }
    }
    tracing::debug!(
        games = offered_rematches.len(),
        "recovered the already-offered rematch set"
    );
    Ok(())
}

/// The terms a Game Session inherits from its founding: the timing mode (a
/// `timestamper`, present iff attested), the time control, and the raw
/// `time_control` rows the courtship layer compares byte-for-byte.
///
/// A session is founded one of three ways (kind `6422` §Founding reference): a
/// directed challenge (`accepted_challenge` → the Direct Challenge behind it), a
/// matchmade one (`pairing`), or a **rematch** — exactly two `rematch_offer`
/// references, which restate no terms at all and so must be followed back to the
/// concluded session and, recursively, to *its* founding. `hops` bounds that
/// recursion: a relay can serve a cycle of sessions that each claim to rematch
/// the next, and an unbounded walk would never return.
async fn resolve_founding(
    client: &Client,
    session: &Event,
    hops: usize,
) -> Result<(Option<PublicKey>, TimeControl, Vec<Vec<String>>)> {
    if let Some(accepted_id) = tags::event_with_marker(session, "accepted_challenge") {
        let accepted = fetch_event(client, accepted_id).await?;
        let direct_id = tags::first_event_ref(&accepted)
            .ok_or_else(|| anyhow!("acceptance references no challenge"))?;
        let direct = fetch_event(client, direct_id).await?;
        return Ok((
            tags::pubkey_with_role(&accepted, "timestamper"),
            mapping::time_control(&direct)?,
            tags::time_control_rows(&direct),
        ));
    }
    if let Some(pairing_id) = tags::event_with_marker(session, "pairing") {
        let pairing = fetch_event(client, pairing_id).await?;
        return Ok((
            tags::pubkey_with_role(&pairing, "timestamper"),
            mapping::time_control(&pairing)?,
            tags::time_control_rows(&pairing),
        ));
    }

    let Some(offer_ids) = tags::events_with_marker(session, "rematch_offer") else {
        bail!("session references no founding");
    };
    let [first_id, second_id] = <[EventId; 2]>::try_from(offer_ids)
        .map_err(|_| anyhow!("rematch session does not reference exactly two offers"))?;
    let hops = hops
        .checked_sub(1)
        .ok_or_else(|| anyhow!("rematch chain longer than {MAX_REMATCH_HOPS} hops"))?;
    let first = fetch_event(client, first_id).await?;
    let second = fetch_event(client, second_id).await?;
    let pair = rematch::founding_pair([&first, &second], &session.pubkey)
        .map_err(|reason| anyhow!("non-conforming rematch founding: {reason}"))?;
    let concluded = fetch_event(client, pair.concluded).await?;
    if concluded.kind != Kind::Custom(GAME_SESSION_KIND) {
        bail!("rematch_of is not a Game Session");
    }
    if concluded.pubkey != session.pubkey {
        bail!("rematch replays another arbiter's session");
    }
    // The terms come from the concluded session's OWN chain, never from the
    // offers (which restate none). The mode is then read back against what the
    // pair claims — kind `6430` §Semantic constraints 6. Trusting the offers
    // instead would let a pair claiming `None` found a session the arbiter can
    // only rule as attested, which the bot would then play blind.
    let (mode, time_control, rows) = Box::pin(resolve_founding(client, &concluded, hops)).await?;
    if pair.timestamper != mode {
        bail!("rematch offers do not mirror the concluded session's timing mode");
    }
    Ok((mode, time_control, rows))
}

/// Start tracking a Game Session: resolve its founding chain (time control,
/// timing mode) and assemble the arbiter-grade `SessionParams` — the same
/// walk the arbiter service performs before ruling (§6.4).
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
    if session.pubkey != ctx.arbiter {
        bail!("session from an arbiter we do not play under");
    }
    let players = tags::pubkeys_with_role(&session, "player");
    if !players.contains(&me) {
        bail!("not our session");
    }
    let opponent = *players
        .iter()
        .find(|player| **player != me)
        .ok_or_else(|| anyhow!("no opponent in the session"))?;

    // Founding-chain resolution (time control + timing mode) — directed,
    // matchmade, or rematch (kind `6422` §Founding reference).
    let (timestamper, time_control, rows) =
        resolve_founding(client, &session, MAX_REMATCH_HOPS).await?;
    if timestamper.is_some() {
        bail!("attested session (v1 is self-timed) — abandoned");
    }

    let params = mapping::session_params(&session, None, time_control, None)?;
    let correspondence = courtship::is_correspondence(&rows);
    ctx.ledger
        .session_changed(&session.pubkey, &opponent, false); // no-op guard
    ctx.ledger.session_changed(&me, &opponent, true);
    tracing::info!(session = %session.id, %opponent, correspondence, "tracking session");
    sessions.insert(
        session.id,
        SessionMeta {
            session,
            params,
            opponent,
            correspondence,
            my_side_evals: VecDeque::new(),
            next_action_at: 0,
            planned_half_move: 0,
            published: None,
        },
    );
    Ok(())
}

/// Route one delivered event.
#[allow(clippy::too_many_arguments)]
async fn handle_event(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    sessions: &mut BTreeMap<EventId, SessionMeta>,
    courted_entries: &mut BTreeSet<EventId>,
    accepted_challenges: &mut BTreeSet<EventId>,
    offered_rematches: &mut BTreeMap<EventId, u64>,
    rng: &mut SplitMix64,
    me: PublicKey,
    event: Event,
) {
    match event.kind {
        Kind::Custom(OPEN_CHALLENGE_KIND) => {
            if courted_entries.insert(event.id) {
                if let Err(reason) =
                    court_pool_entry(client, ctx, relay_clock, rng, me, &event).await
                {
                    tracing::debug!(entry = %event.id, reason, "pool entry not courted");
                }
            }
        }
        Kind::Custom(DIRECT_CHALLENGE_KIND) => {
            if accepted_challenges.insert(event.id) {
                if let Err(reason) =
                    consider_direct_challenge(client, ctx, relay_clock, sessions, rng, me, &event)
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
        Kind::Custom(PLY_KIND) | Kind::Custom(ADJUDICATION_REQUEST_KIND) => {
            // Discovery triggers only (§6.4): the tick re-derives everything
            // from a session-scoped fetch. Nothing to do inline.
        }
        Kind::Custom(ADJUDICATION_KIND) => {
            if let Some(session_id) = tags::event_with_marker(&event, "game_session") {
                if event.pubkey == ctx.arbiter {
                    if let Some(meta) = sessions.remove(&session_id) {
                        tracing::info!(session = %session_id, status = %event.content, "session terminated");
                        terminate_session(
                            client,
                            ctx,
                            relay_clock,
                            me,
                            &session_id,
                            &event.id,
                            &meta.opponent,
                            offered_rematches,
                        )
                        .await;
                    }
                }
            }
        }
        Kind::Custom(rematch::REMATCH_OFFER_KIND) => {
            // A rematch offer addressed to us (§9, reciprocate).
            if let Err(reason) =
                consider_rematch_offer(client, ctx, relay_clock, me, &event, offered_rematches)
                    .await
            {
                tracing::debug!(offer = %event.id, reason, "rematch offer not answered");
            }
        }
        _ => {}
    }
}

/// Terminate a tracked session on its verdict: release the fleet budget slot,
/// then (if the persona is willing) proactively offer a rematch. BOTH paths
/// that observe the terminal 6425 — the notification arm and the tick's session
/// service — funnel through here, so the offer fires whichever wins the race to
/// `sessions.remove`; that guard keeps it to one offer per game. Both carry the
/// Adjudication that ended the session (`adjudication`): the offer must cite one
/// as its `concluded_by` proof, and the verdict we just observed is exactly it —
/// no re-fetch needed.
#[allow(clippy::too_many_arguments)]
async fn terminate_session(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    me: PublicKey,
    concluded: &EventId,
    adjudication: &EventId,
    opponent: &PublicKey,
    offered: &mut BTreeMap<EventId, u64>,
) {
    ctx.ledger.session_changed(&me, opponent, false);
    maybe_offer_rematch(
        client,
        ctx,
        relay_clock,
        concluded,
        adjudication,
        opponent,
        false,
        offered,
    )
    .await;
}

/// Offer `opponent` a rematch of `concluded`, unless our own offer for this
/// game is still LIVE, the persona is unwilling (a per-game decision the
/// `always` flag overrides — reciprocating a human's explicit offer is
/// unconditional), or — against a sibling bot — the bot-vs-bot budget is
/// spent. Shared by both triggers (the proactive offer on the verdict and the
/// reply to an incoming offer), which is why the one-live-offer-per-game guard
/// lives here. Best-effort: a failure is logged,
/// never propagated. The offer is self-timed (the bot never plays attested
/// games) and mined by the publish path like any founding.
///
/// `adjudication` is the 6425 the offer cites as its `concluded_by` proof that
/// `concluded` is over: the observed verdict on the proactive path, the (already
/// verified) one the opponent cited on the reply path. Both adjudicate the same
/// session, which is all the pair rule asks.
#[allow(clippy::too_many_arguments)]
async fn maybe_offer_rematch(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    concluded: &EventId,
    adjudication: &EventId,
    opponent: &PublicKey,
    always: bool,
    offered: &mut BTreeMap<EventId, u64>,
) {
    // One LIVE offer per game — whether it goes out proactively or in reply, so
    // two willing bots never volley mirrors forever. The guard carries our
    // offer's own `accept_until`: once that window lapses the pair can no longer
    // be founded from our side, so an opponent offering AFTER the lapse earns a
    // fresh mirror instead of silence (the previously assumed limitation).
    let now = Timestamp::now().as_secs();
    if offered
        .get(concluded)
        .is_some_and(|deadline| *deadline > now)
    {
        return;
    }
    // The per-game die gates only what the bot VOLUNTEERS. Reciprocating an
    // explicit offer from outside the fleet is unconditional (`always`): a
    // human who clicks Rematch is answered, never diced away.
    if !always
        && !rematch::wants_rematch(
            ctx.bot_seed,
            concluded,
            rematch::DEFAULT_REMATCH_PROBABILITY,
        )
    {
        return;
    }
    if ctx.ledger.is_member(opponent) && !ctx.ledger.may_court_sibling() {
        tracing::debug!(against = %opponent, "rematch not offered: bot-vs-bot budget spent");
        return;
    }
    let concluded = *concluded;
    let adjudication = *adjudication;
    let opponent = *opponent;
    let arbiter = ctx.arbiter;
    let result = publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(rematch::REMATCH_OFFER_KIND),
        ctx.fleet.pow_difficulty,
        move |created_at| {
            let accept_until = created_at.as_secs().saturating_add(REMATCH_WINDOW_SECS);
            // No timestamper: the bot only plays self-timed games (attested
            // challenges are declined in courtship), so the rematch mirrors that.
            let tags = rematch::build_offer_tags(
                &concluded,
                &adjudication,
                &opponent,
                &arbiter,
                None,
                accept_until,
            );
            (tags, String::new())
        },
    )
    .await;
    match result {
        Ok(offer) => {
            let deadline: u64 = tags::accept_until(&offer)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| now.saturating_add(REMATCH_WINDOW_SECS));
            offered.insert(concluded, deadline);
            tracing::info!(
                rematch_of = %concluded,
                against = %opponent,
                offer = %offer.id,
                "offered a rematch"
            );
        }
        Err(error) => tracing::warn!(error = %error, "rematch offer publish failed"),
    }
}

/// Answer an incoming Rematch Offer (kind 6430) addressed to us (§9,
/// reciprocate): verify it is a well-formed offer, from our arbiter, for a Game
/// Session that BOTH of us actually played and that its `concluded_by`
/// Adjudication really ended, then mirror it through the shared path (which
/// applies the one-live-offer-per-game and budget gates; the persona die is
/// bypassed for an offerer from outside the fleet — a human who clicks Rematch
/// is answered). Catches what the proactive offer misses — a restart that
/// skipped the verdict, the opponent offering first, or our own offer having
/// expired before the human answered.
async fn consider_rematch_offer(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    me: PublicKey,
    event: &Event,
    offered: &mut BTreeMap<EventId, u64>,
) -> std::result::Result<(), &'static str> {
    let offer = rematch::parse_incoming_offer(event).ok_or("malformed rematch offer")?;
    if offer.addressed_to != me {
        return Err("addressed to someone else");
    }
    if offer.arbiter != ctx.arbiter {
        return Err("foreign arbiter");
    }
    if offer.timestamper.is_some() {
        return Err("attested rematch — we only play self-timed");
    }
    if offer.accept_until <= Timestamp::now().as_secs() {
        return Err("already expired");
    }
    // The concluded session was purged on its verdict; re-fetch it and confirm
    // it is our arbiter's Game Session that BOTH of us played — never mine a
    // mirror for a game we did not play (spam defense).
    let concluded = fetch_event(client, offer.concluded)
        .await
        .map_err(|_| "concluded session unavailable")?;
    if concluded.kind != Kind::Custom(GAME_SESSION_KIND) || concluded.pubkey != ctx.arbiter {
        return Err("rematch_of is not our arbiter's game session");
    }
    if tags::seat_for(&concluded, &me).is_none()
        || tags::seat_for(&concluded, &offer.offerer).is_none()
    {
        return Err("not both players of the concluded session");
    }
    // The offer's proof that the game is over: its `concluded_by` must really be
    // an Adjudication OF that session, signed by the arbiter who signed it (kind
    // 6425 §Semantic constraints). Without this the reference is a free-form
    // pointer, and a stranger could dress any event up as a verdict. Re-checked
    // here even though our own arbiter is honest: what we mirror is what we saw,
    // not what we assume.
    let adjudication = fetch_event(client, offer.concluded_by)
        .await
        .map_err(|_| "concluded_by adjudication unavailable")?;
    if adjudication.kind != Kind::Custom(ADJUDICATION_KIND) {
        return Err("concluded_by is not an adjudication");
    }
    if tags::event_with_marker(&adjudication, "game_session") != Some(concluded.id) {
        return Err("concluded_by adjudicates another session");
    }
    if adjudication.pubkey != concluded.pubkey {
        return Err("concluded_by is not signed by the session's arbiter");
    }
    // Cite the very Adjudication the opponent cited: verified above, and the
    // pair matches on `rematch_of` regardless.
    maybe_offer_rematch(
        client,
        ctx,
        relay_clock,
        &offer.concluded,
        &offer.concluded_by,
        &offer.offerer,
        !ctx.ledger.is_member(&offer.offerer),
        offered,
    )
    .await;
    Ok(())
}

/// React to a live pool entry (§6.2): compatibility, fleet budget, async
/// checks (mute lists, `following` filter), a persona reaction delay, then
/// our own mirror entry.
async fn court_pool_entry(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    rng: &mut SplitMix64,
    me: PublicKey,
    entry: &Event,
) -> std::result::Result<(), &'static str> {
    let now = Timestamp::now().as_secs();
    if !crate::persona::is_present(&ctx.config.schedule, ctx.bot_seed, chrono::Utc::now()) {
        return Err("absent");
    }
    let candidate: PoolCandidate = courtship::evaluate_open_challenge(
        entry,
        &me,
        &ctx.matchmaker,
        &ctx.arbiter,
        &ctx.fleet.game,
        &ctx.config.play,
        now,
        COURT_MARGIN_SECS,
    )?;
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

    // Persona reaction delay (bounded by the entry's life).
    let delay = crate::persona::think_seconds(
        &ctx.config.tempo.think,
        rng,
        candidate
            .accept_until
            .saturating_sub(now)
            .saturating_sub(COURT_MARGIN_SECS),
    );
    tokio::time::sleep(StdDuration::from_secs(delay.min(30))).await;

    let variant = candidate.variant.clone();
    let spec = candidate.spec.clone();
    let matchmaker = ctx.matchmaker;
    let arbiter = ctx.arbiter;
    let game = ctx.fleet.game.clone();
    publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(OPEN_CHALLENGE_KIND),
        ctx.fleet.pow_difficulty,
        move |created_at| {
            let mut event_tags = vec![
                p_role(&matchmaker, "matchmaker"),
                p_role(&arbiter, "arbiter"),
                Tag::custom(TagKind::custom("game"), [game.clone()]),
                Tag::custom(
                    TagKind::custom("variant"),
                    ["self".to_owned(), variant.clone()],
                ),
                Tag::custom(
                    TagKind::custom("variant"),
                    ["opponent".to_owned(), variant.clone()],
                ),
                Tag::custom(
                    TagKind::custom("accept_until"),
                    [created_at
                        .as_secs()
                        .saturating_add(OWN_ENTRY_WINDOW_SECS)
                        .to_string()],
                ),
            ];
            for row in &spec {
                event_tags.push(Tag::custom(TagKind::custom("time_control"), row.clone()));
            }
            (event_tags, String::new())
        },
    )
    .await
    .map_err(|error| {
        tracing::warn!(error = %error, "pool entry publish failed");
        "publish failed"
    })?;
    ctx.ledger.pool_presence(me, true);
    tracing::info!(against = %entry.pubkey, "entered the pool (reactive)");
    Ok(())
}

/// Consider a Direct Challenge addressed to us (§6.3) and publish the
/// acceptance when the persona says yes.
#[allow(clippy::too_many_arguments)]
async fn consider_direct_challenge(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    sessions: &BTreeMap<EventId, SessionMeta>,
    rng: &mut SplitMix64,
    me: PublicKey,
    challenge: &Event,
) -> std::result::Result<(), &'static str> {
    let now = Timestamp::now().as_secs();
    let plan: AcceptPlan = courtship::evaluate_direct_challenge(
        challenge,
        &me,
        &ctx.arbiter,
        &ctx.fleet.game,
        &ctx.config.play,
        now,
        COURT_MARGIN_SECS,
        rng,
    )?;
    // Concurrency caps.
    let (live, correspondence) = sessions
        .values()
        .fold((0_u32, 0_u32), |(live, corr), meta| {
            if meta.correspondence {
                (live, corr.saturating_add(1))
            } else {
                (live.saturating_add(1), corr)
            }
        });
    if plan.correspondence {
        if correspondence >= ctx.config.play.max_correspondence {
            return Err("correspondence cap reached");
        }
    } else if live >= ctx.config.play.max_live {
        return Err("live cap reached");
    }
    // Acceptance timing by pace (resolved question 5): live only while
    // present; correspondence at any hour after a credible delay.
    if !plan.correspondence
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

    let delay = crate::persona::think_seconds(
        &ctx.config.tempo.think,
        rng,
        plan.accept_until
            .saturating_sub(now)
            .saturating_sub(COURT_MARGIN_SECS),
    );
    tokio::time::sleep(StdDuration::from_secs(delay.min(60))).await;

    let challenge_id = challenge.id;
    let challenger = plan.challenger;
    let arbiter = ctx.arbiter;
    let me_hex = me.to_hex();
    let supply_mine = plan.supply_my_variant.clone();
    let supply_their = plan.supply_challenger_variant.clone();
    let supply_seat = plan.supply_seat;
    publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(ACCEPTED_CHALLENGE_KIND),
        ctx.fleet.pow_difficulty,
        move |_created_at| {
            let mut event_tags = vec![
                Tag::custom(TagKind::e(), [challenge_id.to_hex()]),
                p_role(&challenger, "opponent"),
                // The arbiter subscribes on `#p = self` and founds the game only
                // from an Accepted Challenge that names it: without this tag the
                // acceptance is published and OK'd by the relay, but the arbiter
                // never sees it and no Game Session (6422) is ever founded.
                p_role(&arbiter, "arbiter"),
            ];
            // A player's variant is declared ONLY when the challenge left it open
            // (kind 6421 constraint 7): re-declaring a variant the challenge already
            // fixed puts the same tag in both events and invalidates the pair — the
            // arbiter then never founds the game.
            if let Some(mine) = &supply_mine {
                event_tags.push(Tag::custom(
                    TagKind::custom("variant"),
                    [me_hex.clone(), mine.clone()],
                ));
            }
            if let Some(theirs) = &supply_their {
                event_tags.push(Tag::custom(
                    TagKind::custom("variant"),
                    [challenger.to_hex(), theirs.clone()],
                ));
            }
            if let Some(seat) = supply_seat {
                event_tags.push(Tag::custom(TagKind::custom("seat"), [seat.to_owned()]));
            }
            (event_tags, String::new())
        },
    )
    .await
    .map_err(|error| {
        tracing::warn!(error = %error, "acceptance publish failed");
        "publish failed"
    })?;
    tracing::info!(challenge = %challenge_id, %challenger, "accepted a direct challenge");
    Ok(())
}

/// Service every tracked session (the tick body): compute the live view via
/// the arbiter crate, then act — play our ply, invoke the arbiter, star.
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
) {
    let ids: Vec<EventId> = sessions.keys().copied().collect();
    for id in ids {
        let Some(meta) = sessions.get_mut(&id) else {
            continue;
        };
        match service_session(client, ctx, relay_clock, meta, rng, me, starred_today).await {
            // The verdict that ended it — carried through, since the rematch
            // offer must cite it as its `concluded_by` proof.
            Ok(Some(verdict_id)) => {
                if let Some(meta) = sessions.remove(&id) {
                    terminate_session(
                        client,
                        ctx,
                        relay_clock,
                        me,
                        &id,
                        &verdict_id,
                        &meta.opponent,
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

/// Service one session. Returns `Ok(Some(verdict))` — the id of the Adjudication
/// that ended it — when the session is finished and may be dropped, `Ok(None)`
/// while it is still running.
#[allow(clippy::too_many_arguments)]
async fn service_session(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    meta: &mut SessionMeta,
    rng: &mut SplitMix64,
    me: PublicKey,
    starred_today: &mut u64,
) -> Result<Option<EventId>> {
    let session_id = meta.session.id;
    let now = Timestamp::now().as_secs();

    // The canonical verdict, if any: the session is over.
    let verdicts = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(ADJUDICATION_KIND))
            .author(ctx.arbiter)
            .event(session_id),
    )
    .await?;
    if let Some(verdict) = verdicts.first() {
        maybe_star(client, ctx, relay_clock, meta, rng, verdict, starred_today).await;
        tracing::info!(session = %session_id, status = %verdict.content, "verdict observed");
        return Ok(Some(verdict.id));
    }

    // The session's plies and requests (session-scoped `#e` fetch — §6.4).
    let ply_events = fetch_many(
        client,
        Filter::new().kind(Kind::Custom(PLY_KIND)).event(session_id),
    )
    .await?;
    let request_events = fetch_many(
        client,
        Filter::new()
            .kind(Kind::Custom(ADJUDICATION_REQUEST_KIND))
            .event(session_id),
    )
    .await?;
    let mut plies: Vec<arb::Ply> = Vec::new();
    for event in &ply_events {
        match mapping::ply(event) {
            Ok(ply) => plies.push(ply),
            Err(error) => tracing::debug!(id = %event.id, error = %error, "unmappable ply"),
        }
    }

    let my_key = mapping_key(me)?;
    let view = session_view(&meta.params, &plies, &[], my_key, now)?;

    // Terminal chain, or a predicted post-chain verdict worth invoking?
    if let Some(invocation) = invocation_decision(ctx, meta, &plies, my_key, me, &view, now) {
        // The opponent may already have invoked; then the arbiter will rule.
        let already_requested = request_events
            .iter()
            .any(|event| tags::pubkey_with_role(event, "arbiter").as_ref() == Some(&ctx.arbiter));
        if !already_requested {
            publish_request(client, ctx, relay_clock, session_id).await?;
            tracing::info!(session = %session_id, ?invocation, "invoked the arbiter");
        }
        return Ok(None); // dropped when the 6425 lands
    }

    if view.terminal || view.on_move != my_key {
        return Ok(None);
    }

    // Same-slot idempotence guard (§6.5). Our ply for this very half-move may
    // already be out while the session fetch has not caught up with it (the
    // relay indexes and echoes asynchronously; a second trigger can land within
    // the same second). Without the guard that re-entry SEARCHED AGAIN — a fresh
    // seed, so usually a DIFFERENT move — and published a second content for the
    // same slot, leaving every consumer's selection to race the two (observed:
    // c2c4+b2b4, c2c4+e2e4, c7c5+d7d5). One slot, one search: hold until the
    // chain advances past it, and after a grace period re-send the SAME content
    // — identical contents collapse to one candidate in the selection rule, so
    // the re-send is harmless where a fresh search is not, and a genuinely lost
    // publish cannot deadlock the seat.
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
    let sigma = if meta.correspondence {
        SIGMA_CORRESPONDENCE_SECS
    } else {
        SIGMA_LIVE_SECS
    };
    let deadline = view.anchor.saturating_add(view.affordable);
    let latest_start = deadline.saturating_sub(sigma);
    if meta.planned_half_move != view.next_half_move {
        // (Re)plan the reflection for this half-move.
        let think_cfg = if meta.correspondence {
            &ctx.config.tempo.correspondence_think
        } else {
            &ctx.config.tempo.think
        };
        let ceiling = latest_start.saturating_sub(now).max(1);
        let think = crate::persona::think_seconds(think_cfg, rng, ceiling);
        let mut play_at = now.saturating_add(think);
        // Correspondence realism: prefer the presence window, except under
        // clock pressure (§6.7: realism never outranks not flagging).
        if meta.correspondence
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
        // Defer to the coarse tick when the wake is still more than a tick away
        // (cheap, non-blocking), and always for correspondence (its 60 s σ dwarfs
        // the tick). But a LIVE game whose planned move falls WITHIN one tick must
        // not defer: the next coarse tick would fire up to `TICK_SECS` AFTER
        // `next_action_at`, overshooting the flag on a fast cadence (a 10 s/move
        // game has no room for a 5 s tick slip). Sleep the sub-tick residual and
        // play in-call so we hit the planned instant precisely — `play_ply` itself
        // clamps the search to `latest_start - now`, so publication still lands
        // before the flag. Bounded by `TICK_SECS`, so the loop never blocks for
        // more than one tick.
        if meta.correspondence || wait >= TICK_SECS {
            return Ok(None); // a later tick will come back
        }
        tokio::time::sleep(StdDuration::from_secs(wait)).await;
    }

    // Recompute `now` after any pacing sleep so `play_ply` sizes its search budget
    // against the time that actually remains, not the pre-sleep estimate.
    let play_now = Timestamp::now().as_secs();
    play_ply(
        client,
        ctx,
        relay_clock,
        meta,
        &view,
        rng,
        me,
        play_now,
        latest_start,
    )
    .await?;
    Ok(None)
}

/// What a 6424 published now would achieve, if it is worth publishing
/// (§6.6): the bot invokes ONLY when it wants the predicted verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Invocation {
    Ratify,
    WinOnTime,
    AcceptDraw,
    Resign,
}

fn invocation_decision(
    ctx: &BotContext,
    meta: &SessionMeta,
    plies: &[arb::Ply],
    my_key: arb::PublicKey,
    me: PublicKey,
    view: &SessionView,
    now: u64,
) -> Option<Invocation> {
    let verdict = predicted_verdict(&meta.params, plies, &[], my_key, now)?;
    let my_side = if meta
        .params
        .player(sashite_sanki_engine::domain::side::Side::First)
        == my_key
    {
        sashite_sanki_engine::domain::side::Side::First
    } else {
        sashite_sanki_engine::domain::side::Side::Second
    };
    let i_win = matches!(
        (verdict.result(), my_side),
        (
            Outcome3::FirstWins,
            sashite_sanki_engine::domain::side::Side::First
        ) | (
            Outcome3::SecondWins,
            sashite_sanki_engine::domain::side::Side::Second
        )
    );
    let _ = me;
    match verdict.status() {
        // A rule-system ending reached by the chain: either player may
        // safely invoke — ratify whatever the outcome (it is already fact).
        // `MoveCap` (engine 0.6): the absolute 300-full-move global cap — a
        // rule-system draw like `MoveLimit`, ratifiable the same way.
        Status::Checkmate
        | Status::Stalemate
        | Status::NoMove
        | Status::Insufficient
        | Status::Repetition
        | Status::MoveLimit
        | Status::MoveCap => Some(Invocation::Ratify),
        // Timeout: invoke only as the winner (publishing early risks
        // resigning; the loser never hurries their own flag).
        Status::Timeout => i_win.then_some(Invocation::WinOnTime),
        // Agreement: the opponent's standing draw offer — invoking IS the
        // acceptance. Gated by temperament and the standing assessment.
        Status::Agreement => {
            (view.last_ply_offers_draw && wants_draw(ctx, meta)).then_some(Invocation::AcceptDraw)
        }
        // Residual resignation (against the invoker): only when the persona
        // has decided to resign (sustained hopeless assessments).
        Status::Resignation => wants_to_resign(ctx, meta).then_some(Invocation::Resign),
    }
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
    publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(PLY_KIND),
        ctx.fleet.pow_difficulty,
        move |_created_at| {
            let mut event_tags = vec![
                Tag::custom(
                    TagKind::e(),
                    [
                        session_id.to_hex(),
                        String::new(),
                        "game_session".to_owned(),
                    ],
                ),
                p_role(&opponent, "opponent"),
                Tag::custom(TagKind::custom("step"), [step.to_string()]),
            ];
            if offer_draw {
                event_tags.push(Tag::custom(TagKind::custom("draw"), Vec::<String>::new()));
            }
            (event_tags, content.clone())
        },
    )
    .await?;
    Ok(())
}

/// Choose and publish our ply (§6.5).
#[allow(clippy::too_many_arguments)]
async fn play_ply(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    meta: &mut SessionMeta,
    view: &SessionView,
    rng: &mut SplitMix64,
    me: PublicKey,
    now: u64,
    latest_start: u64,
) -> Result<()> {
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
    let tip = view.tip.clone();
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
    .context("search task failed")?
    .ok_or_else(|| anyhow!("no legal move on our turn (stale view?)"))?;

    meta.my_side_evals.push_back(choice.eval_cp);
    if meta.my_side_evals.len() > 8 {
        meta.my_side_evals.pop_front();
    }

    // Draw offer temperament on our own ply (§6.6).
    let offer_draw = match ctx.config.play.draw_offer {
        DrawOffer::Never => false,
        DrawOffer::Balanced => choice.eval_cp.abs() <= 60 && view.chain_len >= 24,
        DrawOffer::Eager => choice.eval_cp.abs() <= 150 && view.chain_len >= 10,
    };

    let content = move_content(&choice.mv);
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
    let _ = me;
    tracing::info!(
        session = %session_id,
        half_move = view.next_half_move,
        eval = choice.eval_cp,
        depth = choice.depth,
        nodes = choice.nodes,
        offer_draw,
        "played"
    );
    Ok(())
}

/// Publish the Adjudication Request (kind 6424).
async fn publish_request(
    client: &Client,
    ctx: &BotContext,
    relay_clock: &RelayClock,
    session_id: EventId,
) -> Result<()> {
    let arbiter = ctx.arbiter;
    publish_self_timed(
        client,
        &ctx.keys,
        relay_clock,
        Kind::Custom(ADJUDICATION_REQUEST_KIND),
        ctx.fleet.pow_difficulty,
        move |_created_at| {
            (
                vec![
                    Tag::custom(
                        TagKind::e(),
                        [
                            session_id.to_hex(),
                            String::new(),
                            "game_session".to_owned(),
                        ],
                    ),
                    p_role(&arbiter, "arbiter"),
                ],
                String::new(),
            )
        },
    )
    .await?;
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
    verdict: &Event,
    starred_today: &mut u64,
) {
    if *starred_today >= 1 {
        return;
    }
    if !crate::persona::is_present(&ctx.config.schedule, ctx.bot_seed, chrono::Utc::now()) {
        return;
    }
    let chain_len = meta.my_side_evals.len(); // proxy: own turns witnessed
    let fast_mate = verdict.content == "checkmate" && chain_len <= 12;
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
        0,
        move |_created_at| {
            (
                vec![
                    Tag::custom(
                        TagKind::e(),
                        [
                            session_id.to_hex(),
                            String::new(),
                            "sashite:session".to_owned(),
                        ],
                    ),
                    Tag::custom(TagKind::custom("k"), [GAME_SESSION_KIND.to_string()]),
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

/// The kind-6423 `content` of an engine move.
fn move_content(mv: &EngineMove) -> String {
    match mv {
        EngineMove::Board { from, to, actor } => {
            let actor = actor
                .as_ref()
                .map_or("null".to_owned(), |name| format!("\"{}\"", name.as_str()));
            format!("[\"{from}\",\"{to}\",{actor}]")
        }
        EngineMove::Drop { piece, to } => format!("[null,\"{to}\",\"{}\"]", piece.as_str()),
    }
}

fn p_role(pubkey: &PublicKey, role: &str) -> Tag {
    Tag::custom(
        TagKind::p(),
        [pubkey.to_hex(), String::new(), role.to_owned()],
    )
}

fn mapping_key(pubkey: PublicKey) -> Result<arb::PublicKey> {
    arb::PublicKey::parse(&pubkey.to_hex()).ok_or_else(|| anyhow!("malformed pubkey"))
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
        .fetch_events(filter, FETCH_TIMEOUT)
        .await
        .context("fetch failed")?;
    Ok(events.into_iter().collect())
}

async fn fetch_event(client: &Client, id: EventId) -> Result<Event> {
    fetch_many(client, Filter::new().id(id))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("event {id} not found on the relay"))
}

/// Unused-for-now grace constant kept where the silent-arbiter logic will
/// live once courting tracks declared-but-unratified sessions.
const _: u64 = SILENT_ARBITER_GRACE_SECS;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn move_content_matches_the_kind_6423_format() {
        let board = EngineMove::parse(r#"["a1","a4",null]"#).unwrap();
        assert_eq!(move_content(&board), r#"["a1","a4",null]"#);
        let promo = EngineMove::parse(r#"["b7","b8","queen"]"#).unwrap();
        assert_eq!(move_content(&promo), r#"["b7","b8","queen"]"#);
        let drop = EngineMove::parse(r#"[null,"e5","fu"]"#).unwrap();
        assert_eq!(move_content(&drop), r#"[null,"e5","fu"]"#);
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
    }
}
