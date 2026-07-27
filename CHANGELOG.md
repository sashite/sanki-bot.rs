# Changelog

All notable changes to this service are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.0] — 2026-07-27

### Fixed

- **Asymmetric open challenges are courted when the persona opts in.** A pool
  entry imposing the opponent's variant asymmetrically (the premium form —
  e.g. a human playing ōgi who imposes xiongqi) was refused outright
  ("never emitted nor courted"), even by a persona with
  `accept_imposed_variant = true` — the knob only governed the directed path.
  The pool path now honours it: the bot courts the entry with the imposed
  variant (persona weight still required), and its own entry fixes `self`
  alone, leaving the opponent unconstrained — both to satisfy the pairing
  (the imposer's `self` differs from ours) and to stay in the free tier.
  The fleet still never EMITS asymmetric entries spontaneously.
- **A human's Rematch click is always answered.** Two compounding gates could
  silence the reply path: the persona's per-game 75% willingness die also
  applied to INCOMING offers, and the one-offer-per-game guard blocked any
  fresh mirror once the bot's own proactive offer (a 300 s window) had
  expired — the plan's assumed limitation. Reciprocating an offerer from
  outside the fleet is now unconditional (the die keeps gating only
  volunteered offers and bot-vs-bot mirrors), the offer window grows to
  `REMATCH_WINDOW_SECS = 900`, and the guard is deadline-aware (keyed to our
  offer's own `accept_until`): an opponent offering after our offer lapsed
  earns a fresh mirror instead of silence. The startup recovery keeps the
  latest deadline per game.

### Changed

- **Rules kernels brought to the castling release** (2026-07-27):
  `sashite-sanki-engine` 0.6 → **0.7**, `sashite-sanki-arbiter` 0.9 →
  **0.10**, `sashite-sanki-player` 0.2 → **0.3** — castling in ōgi and
  xiongqi. The personas play (and answer) the new castlings; the founding
  positions they join carry the `-R` corner markers.

## [0.2.0] — 2026-07-22

### Changed

- **Dependencies brought to the movecap release** (2026-07-22):
  `sashite-sanki-engine` 0.5 → **0.6**, `sashite-sanki-arbiter` 0.8 → **0.9**,
  `sashite-sanki-player` 0.1 → **0.2**.
- **Predictive invocation now ratifies the `movecap` draw.** The engine's new
  absolute 300-move (600-half-move) cap terminates as `Status::MoveCap`; the
  invocation policy (`actor.rs`) folds it into the "rule-system ending → ratify"
  arm, so a persona that plays a game to the cap invokes the arbiter to ratify
  the draw, exactly as it does for the 50-move `movelimit`, threefold
  repetition, and insufficiency.

## [0.1.0] — 2026-07-20

### Added

- **Initial release** (ADR-0014, v1 scope): one supervisor binary (`players`)
  running N persona actors, each with its own keypair, relay client and
  subscriptions.
  - Standing events: kind-0 profile with the NIP-24 `bot: true` flag,
    kind-30420 challenge policy.
  - Courtship: reactive pool entries (kind 6418 — mirror variants only,
    byte-identical cadences, same arbiter, self-timed, `following` filter
    checked fail-closed, `rating` filter skipped while unrated, public mute
    lists honoured both ways) and Direct Challenge acceptance (6420 → 6421,
    supplying exactly the open pieces: mirror-rule variant, uniform seat).
  - Session play: the live view computed through `sashite-sanki-arbiter`'s
    own `natural_state` (a synthetic self-timed probe request timed "now"),
    move choice by `sashite-sanki-player` under the clock budget
    (`max_affordable` pinned against the engine's `clock::tick`), persona
    think-time pacing, presence windows with per-day jitter, correspondence
    scheduling with the clock override.
  - Arbiter invocation (6424) strictly by prediction: the bot publishes only
    when `verdict::adjudicate` on its probe says the verdict is one it wants
    (ratification, win on time as the winner, draw acceptance by
    temperament, resignation on sustained hopeless assessments).
  - Self-timed `created_at` discipline (relay-clock estimate, stale/future
    rejection retry with signed skew) and NIP-13 mining on player kinds.
  - Fleet ledger: bot-vs-bot budget, pool-occupancy bookkeeping.
  - Stars (§6.8): a rare, budgeted NIP-25 reaction on notable finished
    sessions.
  - Stateless restart from relay replay; graceful shutdown.

### v1 limitations (ADR-0014 §13)

- Self-timed only (an attested founding is abandoned); single relay; no
  premoves; reactive only on the directed path (no outbound 6420); no
  spontaneous pool entries yet (reactive courtship keeps the pool served);
  in-memory only.
