// SPDX-License-Identifier: Apache-2.0
//! One open session (ADR-0045 §7 *The game runtime*): re-derived from the
//! relay through the module on every event it receives and at the timers
//! it sets, with one engine process of its own.
//!
//! **A turn**, per session:
//!
//! ```text
//! Idle → Searching → Answered(Ply draft) → InFlight(Signed) → Accepted
//!                                               ├─ Unknown → (Ply: resend the same content) → Accepted
//!                                               └─ Failed(Rejection | Expired)   — terminal for this step
//! ```
//!
//! - `Searching` starts when the module's tip is the bot's turn and the
//!   relay holds no Ply of the bot for that step: the `search` of §3 is sent.
//!   Events arriving during the search are absorbed; when the chain changed
//!   under the turn — a candidate re-selected, a Conclusion, a co-writer's
//!   Ply — the search is interrupted (`cancel`), the answer discarded, and a
//!   new turn opens for whatever slot is now the bot's (a **withdrawn**
//!   turn).
//! - `Answered` holds the validated answer — the engine's `best`, the
//!   provisional answer, or the fallback — and the scores of the turn.
//! - A Ply `Accepted` or `InFlight` is never followed by another content
//!   for its step: an `Unknown` Ply is resolved by id, and sent again with
//!   the same content while the window allows.
//! - `Failed` publishes nothing more for the step, and the clock decides.
//!
//! **Timers.** The next wake is the earliest of: the opponent's flag plus a
//! second; the bot's own flag plus `L` plus a second; a paced Ply's stamp;
//! the stamp of any held event still ahead of the relay's clock (a
//! conforming client stamps `floor(now) + 1`, so an opponent's Ply is
//! usually ahead of the estimate by a fraction of a second); a decided
//! act's instant; a resolution owed.
//!
//! **Concluding**: the bot drafts a Conclusion when `verdict_at(me, now)` is
//! a rule ending or a `timeout` — the opponent's flag, or its own (once its
//! Ply for the step, if any, is resolved absent). An `agreement` the bot did
//! not decide is never claimed, even when the module would yield it (the
//! opponent's offer standing at the bot's own flag). The draft is `Moot` at
//! any stamp where the verdict no longer holds. The session closes when
//! `select_conclusion` names a canonical Conclusion, whoever signed it.
//!
//! **Draws and resignation** (§7): decided on the current turn's evaluation
//! by [`crate::policy`]; an acceptance or a resignation is concluded no
//! earlier than `anchor + L + 1 s` plus the skew allowance, after a fresh
//! read of the session, and only if the module then yields the verdict the
//! act means.
//!
//! **`Unverified`**: a module failure on this session ends the game's acts —
//! never a guessed rule.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::*;
use sashite_sanki_client::chain::{self, SessionView};
use sashite_sanki_client::drafts;
use sashite_sanki_client::module::{self, Describe, Oracle, Verdict, VerdictAt};
use sashite_sanki_client::notation;
use sashite_sanki_client::publisher::{Outcome, Publisher, Resolution};
use sashite_sanki_client::query;
use sashite_sanki_client::session::{self, Events, Seat, SessionTerms};
use serde_json::Value;
use tokio::sync::{mpsc, Notify};

use crate::config::{Config, EngineConfig};
use crate::fallback::{self, FallbackKey};
use crate::policy::{self, Decision, Evaluation, Streaks};
use crate::sei::{self, Engine, Host, Probe, SearchRequest, Verdict as EngineVerdict};

/// The allowance for clock skew before concluding an acceptance or a
/// resignation (§7), in seconds.
pub const SKEW_ALLOWANCE_SECS: u64 = 2;

/// How long a fresh read of the session may take before the decision.
pub const REREAD_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times a Conclusion finally rejected or unbuildable is drafted
/// again for the same verdict, with a growing pause.
const MAX_CONCLUSION_ATTEMPTS: u32 = 3;

/// The shared oracle: one module instance for every game.
pub type SharedOracle = Arc<Mutex<Box<dyn Oracle + Send>>>;

/// What every game shares.
pub struct GameContext {
    /// The configuration.
    pub config: Arc<Config>,
    /// The bot's key.
    pub me: PublicKey,
    /// The fallback key.
    pub fallback_key: FallbackKey,
    /// The publisher.
    pub publisher: Arc<Publisher<crate::identity::Identity>>,
    /// The relay client, for the fresh reads.
    pub client: Client,
    /// The module.
    pub oracle: SharedOracle,
    /// What the module describes.
    pub describe: Describe,
    /// The engine's probe, when an engine is configured.
    pub probe: Option<Probe>,
}

impl GameContext {
    fn engine_config(&self) -> Option<&EngineConfig> {
        self.config.engine()
    }

    /// The relay's past tolerance `L`, in seconds, as the publisher learnt
    /// it.
    fn past_tolerance(&self) -> u64 {
        self.publisher.settings().past_tolerance
    }

    /// `overhead = margin_ms + engine_rtt` (§3).
    fn overhead_ms(&self) -> u64 {
        let rtt = self.probe.as_ref().map_or(0, |p| {
            u64::try_from(p.engine_rtt.as_millis()).unwrap_or(u64::MAX)
        });
        self.config.play().margin_ms.saturating_add(rtt)
    }

    /// The relay's clock now, in milliseconds: one read of the host's
    /// clock, plus the integer skew the publisher learnt. The host is read
    /// first: when the two reads straddle a second, the estimate errs one
    /// second late — more time charged, an earlier hard stop — never early.
    fn now_ms(&self) -> u64 {
        let host = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let host_ms = u64::try_from(host.as_millis()).unwrap_or(u64::MAX);
        let skew_secs = i128::from(self.publisher.now()).saturating_sub(i128::from(host.as_secs()));
        let with_skew = i128::from(host_ms).saturating_add(skew_secs.saturating_mul(1000));
        u64::try_from(with_skew.max(0)).unwrap_or(u64::MAX)
    }
}

/// The state of the current step's turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Turn {
    Idle,
    /// The answer for `step` on `tip`, waiting for its pace: withdrawn when
    /// the chain changes meanwhile, published when the pace comes.
    Answered {
        step: u32,
        tip: String,
        content: String,
        draw: bool,
        pace: u64,
    },
    /// A Ply for `step` is on the relay, or in flight: no other content
    /// for it.
    Committed {
        step: u32,
        accepted: bool,
        content: String,
        draw: bool,
        event: Box<Event>,
    },
    /// Nothing more is published for `step`.
    Failed {
        step: u32,
    },
}

/// The engine of this game.
enum EngineSlot {
    /// No engine configured: every turn is the fallback.
    None,
    /// Running.
    Running(Engine),
    /// Ended by a failure; relaunched while relaunches remain.
    Down,
    /// The engine and the bot disagree on the rules: kept, but every later
    /// turn is the fallback.
    Disagreeing(Engine),
}

/// Why the game ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameEnd {
    /// A canonical Conclusion names the verdict.
    Closed(Verdict),
    /// The module gave no usable answer: no further acts.
    Unverified(String),
    /// The runtime asked the game to stop (SIGTERM).
    Stopped,
}

/// What the runtime sends a game.
#[derive(Debug)]
pub enum GameInput {
    /// An event of this session, from the relay.
    Event(Event),
    /// Read the session again from the relay before the next pass: a
    /// reconnection, or a game opened at runtime (ADR-0045 §7
    /// *Reconciliation*).
    Reread,
    /// Stop: end the engine, publish nothing more.
    Stop,
}

/// An act the bot decided, awaiting its instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decided {
    AcceptDraw { step: u32, at: u64, flag: u64 },
    Resign { step: u32, at: u64 },
}

/// One open session.
pub struct Game {
    terms: SessionTerms,
    start: u64,
    seat: Seat,
    opponent: PublicKey,
    /// Every event of the session seen, by id.
    seen: BTreeSet<EventId>,
    events: Vec<Event>,
    engine: EngineSlot,
    relaunches: u8,
    searched: bool,
    turn: Turn,
    /// A Conclusion sent and never acknowledged: resolved before anything
    /// else, never believed.
    pending_conclusion: Option<Event>,
    conclusion_attempts: u32,
    streaks: Streaks,
    decided: Option<Decided>,
    stopped: bool,
    /// A fresh read owed before the next pass.
    reread_owed: bool,
}

impl Game {
    /// A game over `terms`, founded at `start` (t₀), with the session's
    /// events known so far.
    #[must_use]
    pub fn new(ctx: &GameContext, terms: SessionTerms, start: u64, events: Vec<Event>) -> Self {
        let seat = if terms.first == ctx.me {
            Seat::First
        } else {
            Seat::Second
        };
        let opponent = match seat {
            Seat::First => terms.second,
            Seat::Second => terms.first,
        };
        let mut game = Self {
            terms,
            start,
            seat,
            opponent,
            seen: BTreeSet::new(),
            events: Vec::new(),
            engine: if ctx.engine_config().is_some() {
                EngineSlot::Down
            } else {
                EngineSlot::None
            },
            relaunches: 0,
            searched: false,
            turn: Turn::Idle,
            pending_conclusion: None,
            conclusion_attempts: 0,
            streaks: Streaks::default(),
            decided: None,
            stopped: false,
            reread_owed: false,
        };
        for event in events {
            game.absorb(event);
        }
        game
    }

    /// The session's id.
    #[must_use]
    pub const fn id(&self) -> EventId {
        self.terms.id
    }

    /// Adds an event of the session, once.
    fn absorb(&mut self, event: Event) {
        if event.verify().is_err() || !self.seen.insert(event.id) {
            return;
        }
        self.events.push(event);
    }

    fn take(&mut self, input: GameInput) {
        match input {
            GameInput::Event(event) => self.absorb(event),
            GameInput::Reread => self.reread_owed = true,
            GameInput::Stop => self.stopped = true,
        }
    }

    fn assembled(&self, ctx: &GameContext) -> Events {
        Events::from_relay(self.events.iter(), &self.terms, ctx.describe.max_step)
    }

    fn request(&self, ctx: &GameContext) -> Value {
        chain::session_request(&self.terms, self.start, &self.assembled(ctx))
    }

    /// The earliest stamp of a held event still ahead of `now`: the view
    /// does not include it yet.
    fn pending_stamp(&self, now: u64) -> Option<u64> {
        self.events
            .iter()
            .map(|e| e.created_at.as_secs())
            .filter(|stamp| *stamp >= now)
            .min()
    }

    /// Runs the game to its end. A game opened at runtime (`fresh`: not
    /// from the rebuild, which read the relay) reads the session first.
    pub async fn run(
        mut self,
        ctx: Arc<GameContext>,
        mut inputs: mpsc::Receiver<GameInput>,
        fresh: bool,
    ) -> GameEnd {
        self.reread_owed = fresh;
        // The engine is launched as the session opens (ADR-0045 §5), off
        // the turn's clock.
        self.ensure_engine(&ctx, None).await;
        let end = loop {
            if self.stopped {
                break GameEnd::Stopped;
            }
            if self.reread_owed {
                // Owed until the relay answers: what is not seen is not
                // acted on.
                if self.reread(&ctx).await {
                    self.reread_owed = false;
                } else {
                    tracing::warn!(session = %self.terms.id, "the relay did not answer the read; again in a second");
                    tokio::select! {
                        input = inputs.recv() => match input {
                            Some(input) => self.take(input),
                            None => break GameEnd::Stopped,
                        },
                        () = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    continue;
                }
            }
            let wake = match self.reconcile(&ctx, &mut inputs).await {
                Ok(wake) => wake,
                Err(end) => break end,
            };
            if self.stopped {
                break GameEnd::Stopped;
            }
            let now = ctx.publisher.now();
            let wake = wake.unwrap_or_else(|| now.saturating_add(60));
            let wake = self
                .pending_stamp(now)
                .map_or(wake, |stamp| wake.min(stamp.saturating_add(1)));
            let sleep = Duration::from_millis(
                wake.saturating_sub(now)
                    .saturating_mul(1000)
                    .clamp(200, 3_600_000),
            );
            tokio::select! {
                input = inputs.recv() => match input {
                    Some(input) => self.take(input),
                    None => break GameEnd::Stopped,
                },
                () = tokio::time::sleep(sleep) => {}
            }
            while let Ok(input) = inputs.try_recv() {
                self.take(input);
            }
        };
        self.end_engine().await;
        end
    }

    async fn end_engine(&mut self) {
        match std::mem::replace(&mut self.engine, EngineSlot::Down) {
            EngineSlot::Running(engine) | EngineSlot::Disagreeing(engine) => engine.close().await,
            EngineSlot::None => self.engine = EngineSlot::None,
            EngineSlot::Down => {}
        }
    }

    /// Launches the engine when it is down and relaunches remain, bounded
    /// by `by` when given (a turn's hard stop).
    async fn ensure_engine(&mut self, ctx: &GameContext, by: Option<Instant>) {
        let Some(engine_config) = ctx.engine_config() else {
            return;
        };
        if !matches!(self.engine, EngineSlot::Down) {
            return;
        }
        if self.relaunches > engine_config.max_relaunches_per_game {
            return;
        }
        match self.launch(engine_config, by).await {
            Ok(engine) => self.engine = EngineSlot::Running(engine),
            Err(failure) => {
                tracing::error!(session = %self.terms.id, %failure, "the engine failed to open");
                self.relaunches = self.relaunches.saturating_add(1);
            }
        }
    }

    async fn launch(
        &self,
        engine_config: &EngineConfig,
        by: Option<Instant>,
    ) -> Result<Engine, sei::EngineFailure> {
        let mut engine = Engine::launch(&engine_config.launch)?;
        let mut deadline = engine
            .started()
            .checked_add(Duration::from_millis(engine_config.launch_ms))
            .unwrap_or_else(Instant::now);
        if let Some(by) = by {
            deadline = deadline.min(by);
        }
        let announcement = sei::open(
            &mut engine,
            &Host::default(),
            &engine_config.options,
            deadline,
        )
        .await?;
        if announcement.version != Some(1) {
            return Err(sei::EngineFailure::Opening(
                "no common SEI version".to_owned(),
            ));
        }
        Ok(engine)
    }

    /// One pass: the session as it stands, and what to do. Returns the next
    /// instant to wake at, in relay seconds (`None`: nothing in particular).
    async fn reconcile(
        &mut self,
        ctx: &Arc<GameContext>,
        inputs: &mut mpsc::Receiver<GameInput>,
    ) -> Result<Option<u64>, GameEnd> {
        let now = ctx.publisher.now();

        // A Conclusion of ours never acknowledged: resolved first.
        if let Some(pending) = self.pending_conclusion.clone() {
            match ctx.publisher.resolve(pending.id).await {
                Resolution::Found(found) => {
                    self.pending_conclusion = None;
                    self.absorb(found);
                }
                Resolution::ConfirmedAbsent => {
                    // A cycle spent: drafted again, as an attempt.
                    self.pending_conclusion = None;
                    self.conclusion_attempts = self.conclusion_attempts.saturating_add(1);
                }
                Resolution::Unknown => return Ok(Some(now.saturating_add(5))),
            }
        }

        let assembled = self.assembled(ctx);
        let request = chain::session_request(&self.terms, self.start, &assembled);

        // A canonical Conclusion closes the session, whoever signed it.
        let conclusions: Vec<Value> = assembled
            .conclusions
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        if !conclusions.is_empty() {
            let canonical = {
                let mut oracle = lock(&ctx.oracle);
                module::select_conclusion(&mut *oracle, &request, &conclusions)
            };
            match canonical {
                Ok(Some(canonical)) => return Err(GameEnd::Closed(canonical.verdict)),
                Ok(None) => {}
                Err(e) => return Err(GameEnd::Unverified(e.to_string())),
            }
        }

        let view = {
            let mut oracle = lock(&ctx.oracle);
            chain::session_view(&mut *oracle, &self.terms, self.start, &assembled, now)
        }
        .map_err(|e| GameEnd::Unverified(e.to_string()))?;
        tracing::debug!(
            session = %self.terms.id,
            chain_len = view.chain_len,
            on_move = ?view.on_move,
            step = view.step,
            anchor = view.anchor,
            affordable = view.affordable,
            events = self.events.len(),
            "reconcile"
        );
        let flag = view.anchor.saturating_add(view.affordable);

        // A rule ending, or a flag that fell: a Conclusion to claim, or
        // nothing until one is.
        if view.terminal || now > flag {
            return self
                .conclude_if_due(ctx, &view, &request, now)
                .await
                .map(Some);
        }

        if view.on_move != self.seat {
            // The opponent's turn. A step of ours the chain passed is over.
            let step_of_ours = chain::step_at(view.next_half_move.saturating_add(1));
            if let Turn::Answered { step, .. }
            | Turn::Committed { step, .. }
            | Turn::Failed { step } = &self.turn
            {
                if *step < step_of_ours {
                    self.turn = Turn::Idle;
                }
            }
            if self.decided.take().is_some() {
                self.streaks.reset();
            }
            // Their time is ours to relaunch an engine in.
            self.ensure_engine(ctx, None).await;
            return Ok(Some(flag.saturating_add(1)));
        }

        // Our turn.
        let step = view.step;
        match self.turn.clone() {
            Turn::Answered {
                step: s,
                tip,
                content,
                draw,
                pace,
            } if s == step => {
                if tip != view.tip {
                    // Re-selected under the answer: a withdrawn turn.
                    tracing::info!(session = %self.terms.id, step, "turn withdrawn: the tip changed");
                    self.turn = Turn::Idle;
                    self.streaks.reset();
                    return Ok(Some(now));
                }
                if now.saturating_add(1) < pace {
                    return Ok(Some(pace.saturating_sub(1)));
                }
                return self.publish_ply(ctx, &view, step, content, draw).await;
            }
            Turn::Committed {
                step: s,
                accepted,
                content,
                draw,
                event,
            } if s == step => {
                let stamp = event.created_at.as_secs();
                if stamp >= now {
                    // Paced ahead of the relay's clock: not in the view yet.
                    return Ok(Some(stamp.saturating_add(1)));
                }
                if accepted {
                    // On the relay, not selected: a race a candidate of the
                    // person acting with the key settles. Wait.
                    return Ok(Some(now.saturating_add(2)));
                }
                // Unknown: resolve, then send the same content again while
                // the window allows.
                return self
                    .resolve_ply(ctx, &view, step, content, draw, &event)
                    .await;
            }
            Turn::Failed { step: s } if s == step => {
                // Nothing more for this step: the clock decides. Wake to
                // claim our own flag if it comes to that.
                return Ok(Some(
                    flag.saturating_add(ctx.past_tolerance()).saturating_add(1),
                ));
            }
            _ => self.turn = Turn::Idle,
        }
        if self.own_ply_on_relay(ctx, step) {
            // Ours (a co-writer's, say) on the relay, not selected yet.
            return Ok(Some(now.saturating_add(2)));
        }

        // A decided acceptance or resignation awaiting its instant.
        if let Some(decided) = self.decided {
            return self
                .act_on_decision(ctx, &view, &request, decided, now)
                .await;
        }

        self.play_turn(ctx, inputs, view, step).await
    }

    /// Searches, decides, and publishes the Ply (or concludes).
    async fn play_turn(
        &mut self,
        ctx: &Arc<GameContext>,
        inputs: &mut mpsc::Receiver<GameInput>,
        view: SessionView,
        step: u32,
    ) -> Result<Option<u64>, GameEnd> {
        let legal = {
            let mut oracle = lock(&ctx.oracle);
            module::legal_moves(&mut *oracle, &view.tip)
        }
        .map_err(|e| GameEnd::Unverified(e.to_string()))?;
        if legal.is_empty() {
            return Err(GameEnd::Unverified(
                "no legal move on a non-terminal tip".to_owned(),
            ));
        }

        let flag = view.anchor.saturating_add(view.affordable);
        let hard_stop_ms = flag
            .saturating_mul(1000)
            .saturating_sub(ctx.config.play().margin_ms);
        let now_ms = ctx.now_ms();
        let searched = if now_ms >= hard_stop_ms {
            // The hard stop already past: no search; the fallback, while a
            // stamp within the flag exists (the publisher says).
            Some((None, None, false))
        } else {
            let hard_stop = Instant::now()
                .checked_add(Duration::from_millis(hard_stop_ms.saturating_sub(now_ms)))
                .unwrap_or_else(Instant::now);
            self.search(ctx, inputs, &view, &legal, hard_stop).await
        };
        if self.stopped {
            return Ok(None);
        }
        let Some((answer, evaluation, engine_expected)) = searched else {
            tracing::info!(session = %self.terms.id, step, "turn withdrawn: the chain changed");
            self.streaks.reset();
            return Ok(Some(ctx.publisher.now()));
        };

        let content = match answer {
            Some(content) => content,
            None => {
                let fallback =
                    fallback::choose(&ctx.fallback_key, self.terms.id.as_bytes(), step, &legal)
                        .cloned()
                        .ok_or_else(|| GameEnd::Unverified("no fallback".to_owned()))?;
                if engine_expected {
                    tracing::warn!(session = %self.terms.id, step, "fallback move");
                } else {
                    tracing::info!(session = %self.terms.id, step, "random move");
                }
                fallback
            }
        };

        let decision = policy::decide(
            ctx.config.play(),
            evaluation.as_ref(),
            view.next_half_move,
            view.last_ply_offers_draw,
            &mut self.streaks,
        );
        let now = ctx.publisher.now();
        let decision_at = view
            .anchor
            .saturating_add(ctx.past_tolerance())
            .saturating_add(1)
            .saturating_add(SKEW_ALLOWANCE_SECS)
            .max(now);
        match decision {
            Decision::AcceptDraw => {
                // Attempted only when its instant, the re-read and the
                // margin fall before our flag; otherwise the turn moves.
                let feasible = decision_at
                    .saturating_add(REREAD_TIMEOUT.as_secs())
                    .saturating_add(ctx.config.play().margin_ms.div_ceil(1000))
                    < flag;
                if feasible {
                    self.decided = Some(Decided::AcceptDraw {
                        step,
                        at: decision_at,
                        flag,
                    });
                    return Ok(Some(decision_at));
                }
                tracing::info!(session = %self.terms.id, step, "no time to accept; moving instead");
                self.answer(ctx, &view, step, content, false).await
            }
            Decision::Resign => {
                self.decided = Some(Decided::Resign {
                    step,
                    at: decision_at,
                });
                Ok(Some(decision_at))
            }
            Decision::MoveOfferingDraw => self.answer(ctx, &view, step, content, true).await,
            Decision::Move => self.answer(ctx, &view, step, content, false).await,
        }
    }

    /// Holds the answer until its pace, or publishes it when the pace has
    /// come: the publisher would otherwise wait with the loop blind.
    async fn answer(
        &mut self,
        ctx: &Arc<GameContext>,
        view: &SessionView,
        step: u32,
        content: String,
        draw: bool,
    ) -> Result<Option<u64>, GameEnd> {
        let pace = self.pace(ctx, view);
        let now = ctx.publisher.now();
        if now.saturating_add(1) < pace {
            self.turn = Turn::Answered {
                step,
                tip: view.tip.clone(),
                content,
                draw,
                pace,
            };
            return Ok(Some(pace.saturating_sub(1)));
        }
        self.publish_ply(ctx, view, step, content, draw).await
    }

    /// Whether the chain changed since `view`, assembled with `conclusions`
    /// Conclusions.
    fn chain_changed(&self, ctx: &GameContext, view: &SessionView, conclusions: usize) -> bool {
        let assembled = self.assembled(ctx);
        if assembled.conclusions.len() != conclusions {
            return true;
        }
        let refreshed = {
            let mut oracle = lock(&ctx.oracle);
            chain::session_view(
                &mut *oracle,
                &self.terms,
                self.start,
                &assembled,
                ctx.publisher.now(),
            )
        };
        match refreshed {
            Ok(r) => r.chain_len != view.chain_len || r.tip != view.tip || r.anchor != view.anchor,
            Err(_) => true,
        }
    }

    /// The search of a turn: the engine's answer as a content, its
    /// evaluation, and whether an engine was expected — or `None` when the
    /// turn was withdrawn or the game stopped.
    async fn search(
        &mut self,
        ctx: &Arc<GameContext>,
        inputs: &mut mpsc::Receiver<GameInput>,
        view: &SessionView,
        legal: &[String],
        hard_stop: Instant,
    ) -> Option<(Option<String>, Option<Evaluation>, bool)> {
        let Some(engine_config) = ctx.engine_config() else {
            return Some((None, None, false));
        };
        self.ensure_engine(ctx, Some(hard_stop)).await;
        let mut engine = match std::mem::replace(&mut self.engine, EngineSlot::Down) {
            EngineSlot::Running(engine) => engine,
            other => {
                self.engine = other;
                return Some((None, None, true));
            }
        };

        let Ok(tip) = notation::position(&view.tip) else {
            self.engine = EngineSlot::Running(engine);
            return Some((None, None, true));
        };
        let legal_set: BTreeSet<&String> = legal.iter().collect();
        let judge = |pmn: &str| -> Option<String> {
            let content = notation::to_content(&tip, pmn).ok()?;
            legal_set.contains(&content).then_some(content)
        };
        let roots = ctx
            .probe
            .as_ref()
            .filter(|p| p.announcement.features.roots)
            .map(|_| {
                legal
                    .iter()
                    .filter_map(|content| notation::to_pmn(&tip, content).ok())
                    .collect::<Vec<_>>()
            });
        let now_ms = ctx.now_ms();
        let charged_ms = now_ms.saturating_sub(view.anchor.saturating_mul(1000));
        let clock = view.clocks.and_then(|clocks| {
            let (own, opp) = match self.seat {
                Seat::First => (clocks.first, clocks.second),
                Seat::Second => (clocks.second, clocks.first),
            };
            sei::clock::search_clock(
                &self.terms.time_control,
                own,
                opp,
                charged_ms,
                ctx.overhead_ms(),
            )
        });
        let Some(clock) = clock else {
            self.engine = EngineSlot::Running(engine);
            return Some((None, None, true));
        };
        let request = SearchRequest {
            position: self.terms.position.clone(),
            moves: view.moves.clone(),
            clock: Some(clock),
            limits: None,
            roots,
            strength: engine_config.strength,
            fresh: !self.searched,
        };
        self.searched = true;

        // The search, with the session's events absorbed as they come: a
        // change of the chain interrupts it.
        let conclusions = self.assembled(ctx).conclusions.len();
        let interrupt = Notify::new();
        let mut withdrawn = false;
        let mut closed = false;
        let turn = {
            let search = sei::search(
                &mut engine,
                &request,
                judge,
                hard_stop,
                sei::turn::STOP_GRACE,
                &interrupt,
            );
            tokio::pin!(search);
            loop {
                tokio::select! {
                    turn = &mut search => break turn,
                    input = inputs.recv(), if !closed => match input {
                        Some(GameInput::Event(event)) => {
                            self.absorb(event);
                            if !withdrawn && self.chain_changed(ctx, view, conclusions) {
                                withdrawn = true;
                                interrupt.notify_one();
                            }
                        }
                        Some(GameInput::Reread) => {
                            // A reconnection: the search goes on; the read
                            // comes before the next pass, and the chain
                            // may prove changed then.
                            self.reread_owed = true;
                        }
                        Some(GameInput::Stop) | None => {
                            closed = input.is_none();
                            self.stopped = true;
                            interrupt.notify_one();
                        }
                    },
                }
            }
        };
        let evaluation = turn
            .answer
            .as_ref()
            .filter(|a| !a.provisional)
            .map(|a| Evaluation {
                score: a.score,
                advice: a.advice,
            });
        match turn.verdict {
            EngineVerdict::InOrder => self.engine = EngineSlot::Running(engine),
            EngineVerdict::Refused(ref error) if turn.verdict.disagrees_on_rules() => {
                tracing::error!(session = %self.terms.id, %error, "the engine disagrees on the rules; fallbacks from now on");
                self.engine = EngineSlot::Disagreeing(engine);
            }
            EngineVerdict::NoMove => {
                tracing::error!(session = %self.terms.id, "the engine holds the position terminal; fallbacks from now on");
                self.engine = EngineSlot::Disagreeing(engine);
            }
            EngineVerdict::Refused(ref error) => {
                tracing::error!(session = %self.terms.id, %error, "the engine refused the search");
                engine.close().await;
                self.engine = EngineSlot::Down;
                self.relaunches = self.relaunches.saturating_add(1);
            }
            EngineVerdict::Failed(ref failure) => {
                if (withdrawn || self.stopped) && engine.is_alive() {
                    // An interrupted search ends as the hard stop does; an
                    // engine in order is kept.
                    self.engine = EngineSlot::Running(engine);
                } else {
                    tracing::error!(session = %self.terms.id, %failure, "engine failure");
                    self.engine = EngineSlot::Down;
                    self.relaunches = self.relaunches.saturating_add(1);
                }
            }
        }
        if withdrawn || self.stopped {
            return None;
        }
        Some((turn.answer.map(|a| a.content), evaluation, true))
    }

    /// The pace floor `min(anchor + min_move_secs, H)`, H rounded up to the
    /// second so that the floor is never earlier than H.
    fn pace(&self, ctx: &GameContext, view: &SessionView) -> u64 {
        let flag = view.anchor.saturating_add(view.affordable);
        let hard_stop_secs = flag
            .saturating_mul(1000)
            .saturating_sub(ctx.config.play().margin_ms)
            .div_ceil(1000);
        view.anchor
            .saturating_add(ctx.config.play().min_move_secs)
            .min(hard_stop_secs)
    }

    /// The Ply draft for `step`, within its window.
    fn ply_draft(
        &self,
        ctx: &GameContext,
        view: &SessionView,
        step: u32,
        content: String,
        draw: bool,
    ) -> drafts::Ply {
        let flag = view.anchor.saturating_add(view.affordable);
        let pace = self.pace(ctx, view);
        drafts::Ply {
            session: self.terms.id,
            opponent: self.opponent,
            step,
            content,
            draw,
            not_before: pace,
            not_after: flag,
        }
    }

    /// Publishes the Ply for `step`, and records the commitment.
    async fn publish_ply(
        &mut self,
        ctx: &Arc<GameContext>,
        view: &SessionView,
        step: u32,
        content: String,
        draw: bool,
    ) -> Result<Option<u64>, GameEnd> {
        let draft = self.ply_draft(ctx, view, step, content.clone(), draw);
        match ctx.publisher.publish(draft).await {
            Outcome::Accepted(event) => {
                self.turn = Turn::Committed {
                    step,
                    accepted: true,
                    content,
                    draw,
                    event: Box::new(event.clone()),
                };
                self.absorb(event);
            }
            Outcome::Unknown(event) => {
                self.turn = Turn::Committed {
                    step,
                    accepted: false,
                    content,
                    draw,
                    event: Box::new(event),
                };
            }
            Outcome::Rejected(rejection) => {
                tracing::error!(session = %self.terms.id, step, %rejection, "the Ply was rejected; the clock decides");
                self.turn = Turn::Failed { step };
            }
            Outcome::Withheld(why) => {
                tracing::warn!(session = %self.terms.id, step, %why, "the Ply was withheld");
                self.turn = Turn::Failed { step };
            }
            Outcome::Failed(reason) => {
                tracing::error!(session = %self.terms.id, step, %reason, "the Ply could not be built");
                self.turn = Turn::Failed { step };
            }
            Outcome::Closed => return Err(GameEnd::Stopped),
        }
        Ok(Some(ctx.publisher.now().saturating_add(1)))
    }

    /// An unacknowledged Ply: resolved on the relay, then — while the window
    /// allows — sent again with the same content (`Collapses`).
    async fn resolve_ply(
        &mut self,
        ctx: &Arc<GameContext>,
        view: &SessionView,
        step: u32,
        content: String,
        draw: bool,
        event: &Event,
    ) -> Result<Option<u64>, GameEnd> {
        let now = ctx.publisher.now();
        let resolve_at = event
            .created_at
            .as_secs()
            .saturating_add(ctx.past_tolerance())
            .saturating_add(1);
        if now < resolve_at {
            return Ok(Some(resolve_at));
        }
        match ctx.publisher.resolve(event.id).await {
            Resolution::Found(found) => {
                self.turn = Turn::Committed {
                    step,
                    accepted: true,
                    content,
                    draw,
                    event: Box::new(found.clone()),
                };
                self.absorb(found);
                Ok(Some(now))
            }
            Resolution::Unknown => Ok(Some(now.saturating_add(5))),
            Resolution::ConfirmedAbsent => {
                // An earlier stamp of the same content? A fresh read of our
                // Plies for the step says.
                if self.reread(ctx).await && self.own_ply_on_relay(ctx, step) {
                    return Ok(Some(now));
                }
                let flag = view.anchor.saturating_add(view.affordable);
                if ctx.now_ms().saturating_add(ctx.config.play().margin_ms)
                    >= flag.saturating_mul(1000)
                {
                    self.turn = Turn::Failed { step };
                    return Ok(Some(
                        flag.saturating_add(ctx.past_tolerance()).saturating_add(1),
                    ));
                }
                tracing::warn!(session = %self.terms.id, step, "the Ply never landed; sending it again");
                self.publish_ply(ctx, view, step, content, draw).await
            }
        }
    }

    fn own_ply_on_relay(&self, ctx: &GameContext, step: u32) -> bool {
        let me = ctx.me.to_hex();
        self.assembled(ctx).plies.iter().any(|ply| {
            ply.get("signer").and_then(Value::as_str) == Some(me.as_str())
                && ply.get("step").and_then(Value::as_u64) == Some(u64::from(step))
        })
    }

    /// The instant came for a decided acceptance or resignation: a fresh
    /// read, then the Conclusion — or a move, when the act is no longer the
    /// verdict.
    async fn act_on_decision(
        &mut self,
        ctx: &Arc<GameContext>,
        view: &SessionView,
        request: &Value,
        decided: Decided,
        now: u64,
    ) -> Result<Option<u64>, GameEnd> {
        let (step, at, status, window) = match decided {
            Decided::AcceptDraw { step, at, flag } => (
                step,
                at,
                "agreement",
                drafts::Window {
                    not_before: Some(at),
                    not_after: Some(flag),
                },
            ),
            Decided::Resign { step, at } => (step, at, "resignation", drafts::Window::default()),
        };
        if now < at {
            return Ok(Some(at));
        }
        if step != view.step || self.conclusion_attempts >= MAX_CONCLUSION_ATTEMPTS {
            // The step passed, or the relay would not take a Conclusion of
            // ours: the turn moves.
            self.decided = None;
            self.streaks.reset();
            return Ok(Some(now));
        }
        // The fresh read: without it, the act waits.
        if !self.reread(ctx).await {
            return Ok(Some(now.saturating_add(1)));
        }
        let request = if self.assembled(ctx).plies.len()
            == request
                .get("plies")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        {
            request.clone()
        } else {
            self.request(ctx)
        };
        let verdict = {
            let mut oracle = lock(&ctx.oracle);
            module::verdict_at(
                &mut *oracle,
                &request,
                self.seat.name(),
                now.saturating_add(1),
            )
        };
        let wanted = match verdict {
            Ok(VerdictAt::Verdict(v))
                if v.status == status || (status == "resignation" && v.status == "timeout") =>
            {
                v
            }
            Ok(_) => {
                tracing::info!(session = %self.terms.id, step, "the act is no longer the verdict; moving instead");
                self.decided = None;
                self.streaks.reset();
                return Ok(Some(now));
            }
            Err(e) => return Err(GameEnd::Unverified(e.to_string())),
        };
        let attempts = self.conclusion_attempts;
        let wake = self
            .publish_conclusion(ctx, request, wanted, window)
            .await?;
        if self.conclusion_attempts > attempts {
            // Rejected, or unbuildable: the act stands, at the pause's end.
            let at = wake.unwrap_or(now);
            self.decided = Some(match decided {
                Decided::AcceptDraw { step, flag, .. } => Decided::AcceptDraw { step, at, flag },
                Decided::Resign { step, .. } => Decided::Resign { step, at },
            });
        } else {
            self.decided = None;
        }
        Ok(wake)
    }

    /// The session past a flag, or ended by a rule: the Conclusion the bot
    /// can claim, and when to look again.
    async fn conclude_if_due(
        &mut self,
        ctx: &Arc<GameContext>,
        view: &SessionView,
        request: &Value,
        now: u64,
    ) -> Result<u64, GameEnd> {
        let flag = view.anchor.saturating_add(view.affordable);
        let mut request = request.clone();
        if !view.terminal {
            // A timeout: the opponent's, from their flag plus a second; our
            // own, from our flag plus L plus a second — on a fresh read: a
            // Ply the relay holds, theirs unseen or ours unacknowledged,
            // may yet be selected.
            let ours = view.on_move == self.seat;
            let claim_at = if ours {
                flag.saturating_add(ctx.past_tolerance()).saturating_add(1)
            } else {
                flag.saturating_add(1)
            };
            if now < claim_at {
                return Ok(claim_at);
            }
            // A claim rests on a fresh, proven read: the opponent's Ply
            // may be on the relay unseen.
            if !self.reread(ctx).await {
                return Ok(now.saturating_add(5));
            }
            request = self.request(ctx);
        }
        if self.conclusion_attempts >= MAX_CONCLUSION_ATTEMPTS {
            tracing::warn!(session = %self.terms.id, "no more Conclusion attempts; waiting for the opponent's");
            return Ok(now.saturating_add(60));
        }
        let verdict = {
            let mut oracle = lock(&ctx.oracle);
            module::verdict_at(&mut *oracle, &request, self.seat.name(), now)
        };
        match verdict {
            // A rule ending, or a timeout. Never an `agreement` the bot did
            // not decide (the opponent's offer standing at our own flag),
            // never the residual resignation against the invoker.
            Ok(VerdictAt::Verdict(v)) if view.terminal || v.status == "timeout" => Ok(self
                .publish_conclusion(ctx, request, v, drafts::Window::default())
                .await?
                .unwrap_or(now)),
            // Not ours to claim; or, after the read, a Ply of ours the next
            // pass selects.
            Ok(_) => Ok(if self.own_ply_on_relay(ctx, view.step) {
                now.saturating_add(1)
            } else {
                now.saturating_add(30)
            }),
            Err(e) => Err(GameEnd::Unverified(e.to_string())),
        }
    }

    async fn publish_conclusion(
        &mut self,
        ctx: &Arc<GameContext>,
        request: Value,
        verdict: Verdict,
        window: drafts::Window,
    ) -> Result<Option<u64>, GameEnd> {
        let oracle = Arc::clone(&ctx.oracle);
        let seat = self.seat.name();
        let expected = verdict.clone();
        let still = move |stamp: u64| -> bool {
            let mut oracle = lock(&oracle);
            matches!(
                module::verdict_at(&mut *oracle, &request, seat, stamp),
                Ok(VerdictAt::Verdict(v)) if v == expected
            )
        };
        let draft = drafts::Conclusion {
            session: self.terms.id,
            first: self.terms.first,
            second: self.terms.second,
            verdict,
            window,
            still: Box::new(still),
        };
        let now = ctx.publisher.now();
        match ctx.publisher.publish(draft).await {
            Outcome::Accepted(event) => {
                self.conclusion_attempts = 0;
                self.absorb(event);
                Ok(Some(now))
            }
            Outcome::Unknown(event) => {
                self.pending_conclusion = Some(event);
                Ok(Some(
                    now.saturating_add(ctx.past_tolerance()).saturating_add(1),
                ))
            }
            Outcome::Withheld(why) => {
                tracing::info!(session = %self.terms.id, %why, "the Conclusion was withheld");
                Ok(Some(now.saturating_add(2)))
            }
            Outcome::Rejected(rejection) => {
                self.conclusion_attempts = self.conclusion_attempts.saturating_add(1);
                tracing::error!(session = %self.terms.id, %rejection, attempt = self.conclusion_attempts, "the Conclusion was rejected");
                Ok(Some(now.saturating_add(
                    30u64.saturating_mul(u64::from(self.conclusion_attempts)),
                )))
            }
            Outcome::Failed(reason) => {
                self.conclusion_attempts = self.conclusion_attempts.saturating_add(1);
                tracing::error!(session = %self.terms.id, %reason, attempt = self.conclusion_attempts, "the Conclusion could not be built");
                Ok(Some(now.saturating_add(
                    30u64.saturating_mul(u64::from(self.conclusion_attempts)),
                )))
            }
            Outcome::Closed => Err(GameEnd::Stopped),
        }
    }

    /// Reads the session's events from the relay again, proven by its
    /// EOSE; whether the relay answered.
    async fn reread(&mut self, ctx: &GameContext) -> bool {
        let filter = Filter::new()
            .kinds([
                Kind::Custom(session::KIND_PLY),
                Kind::Custom(session::KIND_CONCLUSION),
            ])
            .event(self.terms.id);
        let relay = &ctx.publisher.settings().relay;
        match query::query(&ctx.client, relay, vec![filter], REREAD_TIMEOUT).await {
            Some(events) => {
                for event in events {
                    self.absorb(event);
                }
                true
            }
            None => false,
        }
    }
}

fn lock(oracle: &SharedOracle) -> std::sync::MutexGuard<'_, Box<dyn Oracle + Send>> {
    oracle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
