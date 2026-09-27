# Changelog

All notable changes to this service are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **The v4 bot begins as a library** beside the fleet's binary (ADR-0045
  Plan, step 4): `src/lib.rs` grows module by module until the runtime
  exists and the binary switches to it; the fleet's modules are untouched
  until then. Two modules land first:
  - **`sei`** — the SEI host (ADR-0045 §3; SEI 1.0.0 §5, §8, §11):
    `process` launches the engine as SEI §5 prescribes (the configured
    argument vector, no shell, an empty environment but for the configured
    variables, three piped descriptors, its own process group), reads its
    output continuously, drains its error stream under bounds, and ends the
    session by closing its input then killing the group after two seconds;
    `wire` reads events tolerantly and writes requests strictly; `announce`
    reads the `done` of `hello` and names the gaps between it and what the
    configuration needs; `probe` opens an engine (`hello`, `configure`,
    `ping` within `launch_ms`) and runs the probe at start (the gaps,
    `engine_rtt` as the largest of twenty pings); `clock` maps the session's
    clocks onto SEI's `clock` at the emission — bank and quota periods,
    rollovers, `deadline` exact from `max_affordable`; `turn` runs one
    search: every Move the engine names is judged by the caller
    (`parse_canonical`, then membership in the module's `legal_moves`), the
    safety-net `info` is the provisional answer, the `done`'s `best` is the
    answer, an attached error is a refusal that says whether the two
    disagree on the rules, the hard stop sends `cancel` and waits the
    bounded-stop grace, `ping` runs every second. Twenty-three tests over real
    processes: the random engine (probe, then a game with `roots`), and
    scripted engines that never answer `hello`, answer `ping` but never
    `done`, emit an illegal or a non-canonical `best`, exit mid-search,
    flood their output, violate the envelope, refuse the search, hang, fail
    fatally, or answer only within the grace after `cancel`.
  - **`fallback`** — the move the bot plays with no answer:
    `HMAC-SHA256(k_fallback, session ‖ step)` over the module's legal
    moves, unpredictable to others and identical after a crash; one vector
    pinned.
- **`config`** (ADR-0045 §4) — one TOML file with `schema = 1`, parsed into
  a private `RawConfig` (unknown keys refused) and converted into a `Config`
  whose types forbid the incoherent cases: the policy's shape, the variant
  lists, the lists' bounds and intersections, the engine's command resolved
  to an executable and its `cwd` kept away from `data_dir` and the key, the
  score policies only with an engine, the outgoing time control well-formed
  and playable by the per-move share rule (§5) with a capped family, every
  bounded value, the caps and the **capacity inequality**. The example of
  §4 ships as `sanki-bot.example.toml`, read by a test. What only the
  identity can tell — the bot's own key in none of the lists — is
  `Config::excludes`.
- **`identity`** (ADR-0045 §2) — `Identity::from_file` (a private file,
  `nsec1…` or hex), `Identity::generate` (`0600`, never over an existing
  file); neither `Clone` nor `Serialize`, `Debug` shows the npub, the
  derived secrets are zeroed on drop; it signs (`SignEvent`) and gives the
  key to no one. The derived secrets by HKDF-SHA256 from the secret key:
  the fallback key, the open seat (`HMAC(k_seat, challenge_id) & 1`), the
  jitter of an outgoing challenge. The lease on the host is the client's
  `Publisher`'s (`sashite_sanki_client::publisher::Lease`), taken at
  `Publisher::open`; adoption and the echo detector need the relay and come
  with the runtime.
- **`admit`** (ADR-0045 §5) — the admission of a direct challenge, the
  rules in the ADR's order (halted; game, rules, timing; `accept_until` at
  least ten seconds away; a rematch refused; the variants and the seats —
  a fixed `opponent_variant` refused unless the challenger also fixes its
  own, the mirror rule; a cadence, a family played, a time control
  playable by the per-move share; a free slot, the hold on the challenger
  freed when its kind and family match; blocks; the policy — `rating` left
  to the network's check), and what the acceptance needs: the seats, the
  variants, the cadence, `accept_until`.
- **`policy`** (ADR-0045 §7) — the draw offer, the acceptance and the
  resignation, each on the current turn's evaluation only (the first
  variation's `wdl` and `mate`, the advice when announced); the streaks,
  reset by a fallback, a withdrawn turn or a restart; never a resignation
  while an offer stands (an acceptance with the section, a move without).
- **`outgoing`** (ADR-0045 §5 *Sending one*) — when a target is due, the
  first due target in the configured order under the global gates (a free
  slot, the day's quota, a minute since the last), the firing instant
  jittered within the minute.
- **`game`** (ADR-0045 §7) — the runtime of one open session: re-derived
  from the relay through the module on every event and at every timer;
  one engine process of its own, launched as the session opens and
  relaunched on the opponent's time or under a turn's hard stop; the turn
  as a state machine (`Idle`, `Answered` waiting for its pace — withdrawn
  when the tip changes under it —, `Committed` acknowledged or `Unknown`,
  `Failed`), the search interrupted by `cancel` when the chain changes
  under it, the answer or the fallback, the pace floor
  `min(anchor + min_move_secs, ⌈H⌉)`, the Ply's window `[pace, F]`; an
  `Unknown` Ply resolved by id then by a fresh read of the session and
  sent again with the same content while the window allows; the timers
  (the opponent's flag plus a second, the bot's own flag plus `L` plus a
  second, a paced Ply's stamp, a held event still ahead of the relay's
  clock, a decided act's instant); the Conclusions (a rule ending, a
  timeout — the bot's own only after a fresh read, an `agreement` the bot
  did not decide never; `Moot` at any stamp where the verdict no longer
  holds; an `Unknown` Conclusion resolved before anything else and never
  believed; at most three attempts, with a growing pause, before the
  bot leaves the claim to the opponent); the acceptance and the
  resignation decided by `policy`, concluded no earlier than
  `anchor + L + 1 s + 2 s` after a fresh read and only if the module then
  yields the verdict the act means; a module failure ends the game's acts
  (`Unverified`), `Stop` ends the engine and publishes nothing more.
  Seven tests over the in-process relay: a game played at random and
  concluded on the opponent's flag, the same with the random engine, the
  opponent's resignation closing the game, `Stop`, a scripted engine's
  lost evaluation resigning on a streak, a standing offer accepted when
  the draw is judged likely, a co-writer's Ply withdrawing the answered
  turn.
- `sei::search` takes an `interrupt` (`tokio::sync::Notify`): the caller
  ends the search early, as the hard stop does (`cancel`, then the grace).
- Dependencies: `sashite-sanki-client` (the protocol), `nix` (`killpg`, a
  safe wrapper), `hmac`, `hkdf`, `zeroize`; `tokio` gains `process` and
  `io-util`.

### Changed

- The fleet's `config` module moved to `src/fleet_config.rs` (declared
  with `#[path]` in `main.rs`, so nothing else in the binary changes),
  making room for the library's `config`.

- **Renamed** to `sashite-sanki-bot` (repository `sanki-bot.rs`), and made
  public, as the starting point of the crate ADR-0045 v4 describes: the
  engine as an SEI child process, one bot per process, a TOML configuration.
  The binary keeps its name `players` until that refactor; nothing else
  changes for the fleet in this release.

### Notes

- **The engine's priority.** ADR-0045 §3 launches the engine at a priority
  below the bot's. Lowering it from the bot takes `setpriority`, an `unsafe`
  call this crate forbids (`nix` 0.30 wraps none); the priority is left to
  the deployment, as SEI §5 leaves the process's confinement to it.

## [0.12.0] — 2026-09-21

The bots answer rematches; they no longer ask for them — so two members of
a fleet may meet on a direct challenge without the game turning into a
series.

### Removed
- The bot's own rematch proposal: no kind-`3420` rematch challenge is ever
  published (`maybe_offer_rematch`, the `offered_rematches` set and its
  recovery at start, and the `rematch` module — `wants_rematch`,
  `rematch_challenge_tags` — are gone).
- The per-game willingness dice on an incoming SIBLING rematch challenge.

### Changed
- An incoming rematch challenge, from a human or a sibling, is accepted
  whenever the bot is free: through the cooling hold the concluded game
  left, or any free slot of the cadence once it has run out; against a
  sibling the bot-vs-bot budget applies as to any founding. Its checks
  against the concluded session (`verify_rematch`) are unchanged.

## [0.11.0] — 2026-09-19

The personas get a NIP-05 (ADR-0023 §4): `julee@`, `ogichan@`,
`zhuying@sanki.app`, answered by the app's `/.well-known/nostr.json`.

### Added
- `[bot.profile] name` (the handle, NIP-05's local part; the display name
  serves when absent), `nip05` (the whole address the key claims) and
  `website` (the game's origin). The kind-0 `content` is built by ONE
  function (`actor::metadata_json`, unit-tested) for the game relay and the
  profile relays alike, and now carries the whole profile — `name`,
  `display_name`, `about`, `bot`, and the optional `picture`, `nip05`,
  `website` — so a field edited by hand in a client survives the next start
  only if the fleet file has it: the file is the profile's one owner
  (ADR-0040 §7), and it says everything the profile says.

### Changed
- The kind-0 `name` was the display name; it is the handle now when one is
  set, and `display_name` is published beside it (clients read both).

## [0.10.0] — 2026-09-18

The personas become Nostr citizens (ADR-0042): their profile, with a
picture, where the clients are.

### Added
- `[fleet] profile_relays` — public relays the persona's kind-0 (the same
  JSON the game relay receives, `bot: true` included) and a NIP-65 relay
  list (kind 10002, naming the game relay and these) are also published
  on, at every start, through a throwaway client that carries none of the
  game's subscriptions (`publish::publish_profile_abroad`). Not
  self-timed: those relays keep no `created_at` window. Best effort,
  logged at `info` (all accepted), `warn` (some refused: a rate limit, a
  web-of-trust gate) or `warn` (unreachable); never fatal. Empty or
  absent: the game relay only, as before. Validated as relay URLs at
  load.
- `[bot.profile] picture` is now set in the production fleet file: the
  three avatars, on the content-addressed store (`blobs.sanki.app`).

## [0.9.0] — 2026-09-12

What the first day in production asked for: a log that rotates itself, a
pulse to read, no double bookkeeping after a restart, and the bench that
would have caught 0.8.2's bug.

### Added

- **`PLAYERS_LOG_DIR`**: when set, the fleet writes daily log files there
  (`players.YYYY-MM-DD.log`, the last fourteen kept, no ANSI) through a
  non-blocking writer, instead of stdout. launchd redirects stdout to a
  file nothing rotates; the bot now rotates its own. Unset (development,
  the e2e bench), stdout as before. What escapes tracing — a fatal start-up
  error, a panic — still goes to stderr.
- **A pulse**: one `INFO` line every ten minutes per persona — tracked
  sessions, each family's load against its cap
  (`byoyomi=0/1 blitz=1/1 …`), and how long the relay has been silent —
  the line an operator reads to know the bot lives.
- **e2e: `accepts_a_direct_challenge_then_plays_it`** — the opponent
  challenges the bot directly, the bot founds the Game Session and then
  answers the opening. Fails on 0.8.1 at "the bot's answer", passes on
  0.8.2: the regression bench the directed path never had.

### Fixed

- A session concluded in this process is not tracked — and concluded —
  a second time when a replay (a recovered Pairing, a challenge inside the
  subscription's lookback) delivers its founding again.
- The e2e persona's window ends at `24:00` rather than `23:59`.

## [0.8.2] — 2026-09-12

### Fixed

- **An accepted Direct Challenge was never played.** The acceptance is the
  Game Session the bot publishes itself, from a task of its own, and the
  code then waited for the subscription to deliver that session back so it
  could be tracked — but the client never notifies an event it sent (it
  already holds it when the relay's echo arrives), so the session was never
  tracked and the bot never moved, on the direct path and on a rematch it
  accepted. Found on the first rematch in production. The task now hands the
  founded session back to the actor's loop through an internal channel,
  which tracks and serves it as it would one from the relay. The pool path,
  which tracks explicitly after founding, was never affected.

## [0.8.1] — 2026-09-12

Two things the first production run showed in the log.

### Fixed

- **The persona's name on every log line.** The `bot{…}` span was entered
  with a guard held across `await` points, so it leaked onto whichever task
  ran next on the same thread — a move by `robotto-ogi` logged as
  `robotto-chess`'s, a `tracking session` with no name at all — and the
  publish tasks (`tokio::spawn`) carried no span. The actor's future is now
  `instrument`ed by the supervisor and the spawned tasks inherit
  `Span::current()`: a line wears its own persona's name, always.
- **The pool feed no longer replays its whole history at start-up.** The
  subscription carries a `since` of ten minutes — every live entry is
  within it, and none of the hundreds of expired ones a restart used to
  read and refuse one by one.

## [0.8.0] — 2026-09-12

The three Robotto ([ADR-0040](https://github.com/sashite/web-specs.md/blob/main/adr/adr-0040-three-robotto.md)):
what a mono-variant persona answers on each founding path, one game per
cadence with the rematch kept possible, and the pieces a fleet on a personal
machine needs.

### Changed

- **The cadence slots (`src/slots.rs`).** A family's cap is a number of
  holds, and a hold is an automaton — `Committed(until)` (the bot's own pool
  entry, or its acceptance not yet founded) → `Playing(g)` → `Cooling(g,
  t_end + W)` → free — that **every** founding is admitted through: the
  mirror entry in the pool, a fresh or rematch Direct Challenge, and the
  bot's **own rematch proposal**, which used to bypass the cap. A concluded
  game's slot stays with the pair for the rematch window and admits nothing
  but a rematch of that game; a rematch challenge observed or published in
  the window's last second is honoured for its whole life. A Pairing landing
  in an entry's last second extends the commitment to its founding deadline.
  `Committed` and `Cooling` expire by themselves; nothing leaks. Replaces
  the per-family session count and the per-cadence pool lock of 0.7.0
  (both are now states of the same automaton).
- **`REMATCH_WINDOW_SECS` 900 → 60**, the one published rematch window of
  every Sashité client (the app's `rematch-window.ts` publishes the same
  sixty seconds). The self-subscription replay lookback no longer derives
  from it (45 minutes, its own constant).
- **The direct path plays cross-variant, and honours a premium imposition
  (ADR-0040 §2).** A Direct Challenge is accepted whenever the bot can play
  its own variant — left open, the persona supplies it; imposed, it must be
  one the persona plays — and the challenger's variant is theirs: a
  cross-variant game by delegation is accepted as before. An **asymmetric**
  imposition (the challenger plays something else, or left their own open)
  is no longer refused flat: it is honoured **iff the challenger is
  premium**, asked of the admission service — `GET {admission_url}/premium/
  {pubkey}` (`src/admission.rs`), fail-closed, five-second bound — as
  *Premium* §1.3 prescribes for a Sashité client. The §1.3 re-check at
  rematch is implemented on both sides: a rematch of a session whose
  configuration descends from an asymmetric imposition (the chain walked
  back to its first founding, on either path) is accepted or proposed only
  while the original imposer is premium. `accept_imposed_variant` is now
  the **pool** knob only.
- **New `[fleet] admission_url`** (optional): the admission service's
  origin. Absent, every asymmetric imposition is refused without a request
  — for a deployment with no admission service (the e2e bench); a
  production fleet sets it from day one, since premium status does not
  exist apart from that service.
- **A fully open pool entry (no variant term) is courted**, mirrored with
  the persona's own draw, by **one** member of the fleet: the ledger hands
  it to the first that claims it (`Ledger::claim_open_entry`), so a human
  who said "anything" meets one bot rather than three racing for them.
- **A window may end at `24:00`** (`to` only): the end of the local day,
  closing the one-minute hole a `to = "23:59"` schedule left at 23:59.

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
