# Changelog

All notable changes to this service are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
