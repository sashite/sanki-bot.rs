# Changelog

All notable changes to this service are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.7.0] — 2026-09-12

One cadence ([ADR-0039](https://github.com/sashite/web-specs.md/blob/main/adr/adr-0039-one-cadence.md)):
the four families — `byoyomi`, `blitz`, `rapid`, `correspondence` — are
classified once, in the suite's documents ([Cadence — Sanki](https://github.com/sashite/web-specs.md/blob/main/nostr/support/cadence-sanki.md)),
and the fleet derives its reading from there instead of keeping one of its
own.

### Changed

- **BREAKING (config) — `[bot.play.max_concurrent]` replaces `max_live` and
  `max_correspondence`.** One table, one cap per cadence family
  (`byoyomi`, `blitz`, `rapid`, `correspondence`; defaults `1`, `1`, `1`,
  `4`; an omitted family takes its default, `0` disables one). The old keys
  are **rejected** at start-up, not silently honoured. A persona's
  `time_controls` must each classify (a `time_control` the classifier calls
  malformed — a `duration` of `0` outside the per-move form, a leading zero
  — fails the fleet file).
- **The cadence classifier is the spec's** (`src/cadence.rs`): the ordered
  rules of *Cadence — Sanki* on a founding's **first** period — a day in the
  bank or in the increment is correspondence; `duration 0` is byōyomi; five
  minutes or less is blitz; the rest is rapid — and a malformed or absent
  first period has **no cadence**. The fleet's own
  `duration >= 7200 || increment >= 3600` is gone. Two behaviours move in
  consequence: a `["7200"]` challenge is a **live** rapid game (live think
  distribution, accepted only while present), and a `["300", "3600"]` one
  is blitz. Both admission gates — the pool's and the direct challenge's —
  count sessions per family; a challenge whose first period has no cadence
  is refused, `accept_any_time_control` or not.
- **The pool's one-entry lock is per cadence** (ADR-0039 §7): a persona that
  has just mirrored a blitz entry no longer ignores every other entry for
  three minutes — only blitz ones. With caps per family the four slots can
  now actually fill.
- The category-G conformance vectors (`conformance/cadence.json`, vendored
  from `web-specs.md`) run in the unit suite, the same file the app's
  `cadence.spec.ts` runs. Each persona's caps are logged at spawn.

## [0.6.0] — 2026-09-05

The fleet plays the **arbiterless** protocol under an **executable rule
system** ([ADR-0033](https://github.com/sashite/web-specs.md/blob/main/adr/adr-0033-arbiterless-sessions.md),
[ADR-0034](https://github.com/sashite/web-specs.md/blob/main/adr/adr-0034-reference-build.md)).
Deploy together with the revised relay policy, matchmaker and rater, and
the first Rule System event published.

### Changed

- **BREAKING (wire) — no arbiter.** The Accepted Challenge (`3421`), the
  Adjudication Request (`3424`) and the Rematch Offer (`3430`) are retired.
  A bot **accepts** a Direct Challenge by publishing the Game Session
  (`3422`) itself — the acceptance is the founding — and **founds** the
  session a Pairing (`3419`) naming it declares as soon as it observes it
  (either player may; the canonical one wins the slot). It **concludes** by
  publishing a Conclusion (`3425`) claiming exactly the verdict the rule
  system yields at that instant with itself as the invoker — a terminal
  chain, the win on time (after a courtesy delay), the opponent's standing
  draw offer, its own resignation — and treats a session as over when the
  module selects a canonical Conclusion; a non-conforming one is logged and
  ignored. A **rematch** is a Direct Challenge citing the concluded session
  and its Conclusion (`rematch_of` / `concluded_by`), seats swapped, terms
  inherited; an incoming one is verified against the concluded session —
  its `concluded_by` through the module — before being accepted.
- **BREAKING (config) — `fleet.rules` replaces `fleet.arbiter`:** the id of
  the Rule System event (kind `3417`) the fleet plays under, loaded at
  start-up with the module it names (`fleet.rules_cache_dir`, default
  `./rules`). A pool entry, a Pairing, a challenge under another rule system
  is ignored. New per-bot `timeout_courtesy_secs` (default `5`).
- **The module is the rules oracle.** `sashite-sanki-engine` and
  `sashite-sanki-arbiter` leave the dependency tree: the session view (chain,
  clocks, turn, terminal state), the history bookkeeping the search needs
  (replayed through `apply`), the legal moves, the prescribed initial
  position and every verdict come from the module under `wasmi`, through the
  Kernel ABI. `sashite-sanki-player` still chooses the moves; its choice is
  checked against the module's `legal_moves` before it is played. The clock
  budget arithmetic is pinned against the module's `clock` primitive.
- **Timing designation:** exactly one `timing_relay`, ours (the revised
  founding kinds carry exactly one); an entry or challenge designating any
  other relay, or a timestamper, is not played.
- **Proof of work** is added only on the kinds that prescribe a `nonce` tag
  (foundings, Plies, Conclusions) — a Game Session, a profile, a reaction
  carry none.
- The notification stream is opened before the subscriptions, so nothing the
  relay replays on REQ is lost; pending Pairings are recovered at start-up
  along with sessions and rematch challenges (30 days back). A game that
  concluded longer ago than the rematch window is neither proposed a rematch
  of nor starred when re-observed.
- **Courtship no longer blocks the actor's loop:** the persona's reaction
  delay and the publish it ends with (a pool entry, an acceptance) run in a
  task of their own, so a live game on a fast cadence is served meanwhile.
  The concurrency caps now gate pool courtship too, and the bot keeps one
  live pool entry at a time.
- Every timing comparison — the module's cutoff, the deadline, a window —
  uses the relay's clock as estimated by the publish path, never the raw
  host clock.
- One tracked session per slot: a canonical Game Session arriving after a
  sibling (both players of a Pairing founding; both rematch challenges
  accepted) evicts the stale one. A founding is trusted only once verified
  (signature, the matchmaker's Pairing, the challenge's nonce, the acceptor
  as signer); a replayed challenge already accepted is not accepted twice; a
  rematch challenge of a session whose rematch is already founded is moot.

### Added

- The e2e benches need the module's bytes (`SANKI_MODULE`) and cover the
  founding on a Pairing, the per-slot idempotence discipline, and the
  win-on-time Conclusion — the whole loop from the Rule System event on the
  relay to the rematch proposal.

### Previously unreleased (since 0.5.0, folded into this release)

The entries below were written before the arbiter left; where they speak of
acceptances (`3421`), Rematch Offers (`3430`) and the arbiter, this release
supersedes them.

### Changed

- **BREAKING (wire) — in-band timing designation, and kind `3410` for
  attestations (2026-08-11 spec revisions).** A self-timed challenge now
  carries one or more `["timing_relay", "<wss://…>"]` tags (XOR a
  `timestamper` `p` tag — Canonical Timing NIP). The bot's own Open Challenges
  designate its configured relay; it only accepts a Direct Challenge whose
  designated set names that relay, and its acceptance MIRRORS the challenge's
  set verbatim (kind `3421` constraint 4). Rematch offers mirror the concluded
  session's designation. Attestations are kind `3410` (was `1041`, M-14).
  Deploy together with the arbiter and the rest of the stack.

- **BREAKING (wire) — acceptances now carry the `direct_challenge` marker** on
  their founding `e` reference (kind `3421` §Reference tag). Deploy together
  with the arbiter bot, which no longer ratifies an unmarked acceptance.

### Changed

- **BREAKING — the suite's kind numbers moved out of NIP-90's reserved range.**
  The Game Sessions this bot plays are now kind `3422`, its Plies `3423`, the
  Direct Challenges it answers `3420` with acceptances at `3421`, and the
  Rematch Offers it exchanges `3430`. NIP-90 reserves `5000-7000` in one block
  and pairs a job request with its result at a fixed offset of a thousand, so a
  Ply at `6423` *was* the result of job request `5423` to anything that knows
  NIP-90 (`web-specs.md` README §Kind numbers).

  Nothing else changed, and nothing needed to: the numbers live in this crate,
  not in its libraries — `sashite-sanki-engine`, `-arbiter` and `-player` carry
  them only in doc comments, with no kind constant between them. The 68 unit
  tests pass unchanged, and so does the per-slot idempotence e2e, which runs a
  real session over a mini-relay and is therefore the first thing here to have
  exercised the new numbers on a wire.

  **Deploy with the arbiter and the matchmaker.** A kind number is the suite's
  version identifier: a player on `3xxx` and an arbiter on `6xxx` are two
  protocols, and no session forms between them.

- **`nostr-sdk` 0.44 → 0.45.** Clears
  [RUSTSEC-2026-0243](https://rustsec.org/advisories/RUSTSEC-2026-0243): 0.44
  pulls the standalone `nostr-relay-pool`, no longer maintained since its
  functionality moved into `nostr-sdk` itself. `cargo deny check` is green
  again and the crate has left the lock file.

  `ClientBuilder::signer` is gone, which costs this bot nothing — it already
  signed every event itself before sending it (§Client obligations in
  self-timed mode), so only the builder line changed.
  `subscribe(filter, None)` became `subscribe(filter)`,
  `fetch_events(filter, timeout)` became `fetch_events(filter).timeout(t)`,
  `RelayPoolNotification` became `ClientNotification`, and `notifications()`
  yields a `Stream` instead of a broadcast `Receiver`. On the `nostr` side,
  `TagKind` is gone — a tag name is a string now, which is what `variant`,
  `time_control`, `step`, `seat` and the rest always were — and
  `sign_with_keys` became `finalize`.

  `EventBuilder::pow` is the one that touches conduct rather than spelling: it
  became `UnsignedEvent::mine`, so mining now happens on the **unsigned** event,
  between building and signing. `publish_self_timed` carries the difficulty down
  to the signing step instead of applying it to the builder. The order is the
  honest one — the nonce is part of what the id commits to — and the
  `created_at` retry loop is unaffected: a stale rejection still re-stamps,
  re-mines and re-signs from scratch. The difficulty-0 path, which hand-writes
  `["nonce", "0", "0"]` because a conforming client requires the tag whatever
  the relay's policy, is untouched.

  One thing is lost, and it is upstream's doing: the 0.45 notification stream
  silently drops the broadcast lag error, so a bot that falls behind now misses
  events without a warning where the loop used to emit one. The `TICK_SECS`
  service pass is the safety net that was already there — it re-services every
  session on a timer, so a missed Ply is noticed late rather than never — but
  the relay pool is private in 0.45, and the signal is out of reach.

  The 68 unit tests pass unchanged, and so does the per-slot idempotence e2e:
  the real binary against a mini-relay, publishing mined Plies over a wire,
  which is what actually exercises the new signing and mining path.

## [0.5.0] — 2026-08-01

### Changed

- **All three rules crates brought to their reviewed releases:**
  `sashite-sanki-engine` 0.8 → **0.9**, `sashite-sanki-arbiter` 0.11 →
  **0.12**, `sashite-sanki-player` 0.4 → **0.5**, carrying with them
  `sashite-feen` 0.1 → **0.2**, `sashite-qi` 0.1 → **0.2**, and
  `sashite-sin` / `sashite-pin` / `sashite-epin` 1.0 → **1.1**. No code change
  here; both test binaries (unit suite and the end-to-end relay test) pass
  unchanged.

  **No move selection changes.** The player's tactics, root tie-break and
  property suites all pass identically across the bump, so this fleet plays the
  same moves it played before.

  What it gains is upstream correctness that had not reached it: every one of
  those five notation crates was still pinned at its pre-review version in this
  bot's lockfile, and for a binary the lockfile *is* the deployment. The two
  that matter here are FEEN's encoder, which could return a string its own
  parser rejects, and the engine's `Position::new`, which accepted boards that
  are not 8×8. Neither was reachable from this bot — positions arrive through
  `Position::parse` — but `chain.rs` keys its repetition map by
  `Position::to_feen()`, and that key now comes from an encoder that cannot
  produce something unreadable.

## [0.4.0] — 2026-07-31

### Changed

- **All three rules crates brought to their current releases:**
  `sashite-sanki-engine` 0.7 → **0.8**, `sashite-sanki-arbiter` 0.10 →
  **0.11**, `sashite-sanki-player` 0.3 → **0.4**. No code change here; the
  suite (68 unit + the end-to-end relay test) passes unchanged. What the fleet
  actually gains:
  - *engine 0.8* — a checkmate is no longer misreported as `Ongoing` when a
    cross-variant capture leaves an inert, opposite-cased token in the
    capturer's hand tray. A persona reading that position saw a game still
    running where it had in fact been mated, or had mated.
  - *player 0.4* — three decision bugs fixed since 0.3.0: the root tie-break
    could return a move that does not mate (a losing move tying a real mate's
    fail-soft bound and winning the seeded draw); the dead-position gate was
    inverted for mixed pairings and missed the unbounded same-coloured-bishops
    rule, so dead positions scored as material edges; and an extreme
    `contempt` could inflate a draw past `MATE` itself, letting a persona
    prefer a repetition over an available checkmate.
  - *arbiter 0.11* — the adjudication the bot reads back is now computed on the
    corrected engine, so a session it played cross-variant is ruled the way it
    was actually played.

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
