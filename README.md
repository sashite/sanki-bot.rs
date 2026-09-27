# sashite-sanki-bot

> **Renamed on 2026-09-28** from `sashite-sanki-player-nostr-bot`
> (repository `sanki-player-nostr-bot.rs`), and made public. This is the body
> of Sashité's Sanki bots — the Robotto's binary as it runs today — and the
> starting point of the public `sashite-sanki-bot` crate decided by
> [ADR-0045 v4](https://github.com/sashite/web-specs.md/blob/main/adr/adr-0045-sanki-bot-crate.md):
> one bot per process, its brain an engine speaking the
> [Sashité Engine Interface](https://sashite.dev/specs/sei/1.0.0/) as a child
> process, its behaviour a TOML file. Until that refactor lands, the code
> below is the fleet's 0.12: the brain is `sashite-sanki-player`, wired in,
> and the binary is still `players`. Some links point into `web-specs.md`,
> a private repository whose documents are being published on sashite.dev.

The Sashité **player fleet** (ADR-0014): autonomous Nostr players for Sanki.
One process supervises N configured personas — each an honest player with its
own keypair, timezone, presence windows, cadence tastes, playing strength and
temperament — playing sessions end-to-end over the public protocol: the
matchmaking pool (kind `3418`), the Game Session (`3422`) it founds on a
Pairing or publishes to accept a Direct Challenge (`3420`, fresh or rematch),
Plies (`3423`) under the Time Accounting discipline, and the Conclusion
(`3425`) it publishes **only with the verdict the rule system yields** — the
suite has no arbiter (ADR-0033).

The bots hold no privileged key and are honestly labeled (`bot: true`,
NIP-24). Nothing on the rules axis is theirs (ADR-0034):

- the **rule system** — the Rule System event (kind `3417`) the fleet is
  configured with, and the WebAssembly module it names by digest — is the one
  oracle of the session: run under [`wasmi`](https://crates.io/crates/wasmi)
  through the [Kernel ABI — Sanki](https://github.com/sashite/web-specs.md/blob/main/nostr/support/kernel-abi-sanki.md),
  it answers the natural state of the session (the chain, the clocks, whose
  turn), the legal moves, the prescribed initial position, and the verdict a
  Conclusion published now must claim;
- `sashite-sanki-player` — anytime move choice under the clock budget, over
  the tip the module reached; its choice is played only if the module admits
  it (a divergence is logged and a move the module admits is played instead).

## Running

```sh
cp fleet.example.toml ~/sanki-e2e/fleet.local.toml   # outside every repo
$EDITOR ~/sanki-e2e/fleet.local.toml                 # personas, rules, key env names
set -a; source ~/sanki-e2e/bots.local.env; set +a    # PLAYER_NSEC_* variables
FLEET_CONFIG_PATH=~/sanki-e2e/fleet.local.toml cargo run --bin players
```

Environment: `FLEET_CONFIG_PATH` (required), one `PLAYER_NSEC_*` per bot
(named by each `[[bot]]`'s `nsec_env`; never logged), `RUST_LOG` (optional),
`PLAYERS_LOG_DIR` (optional: daily log files there, the last fourteen kept;
stdout when unset). Every ten minutes each persona logs a `pulse` — its
tracked sessions, each cadence's load against its cap, the relay's silence.

The fleet file's optional `admission_url` names the admission service the
premium checks ask (one GET, only on an asymmetric variant imposition or the
rematch of one; absent, such impositions are refused without a request).

The fleet file names the rule system (`fleet.rules`, the kind-`3417` event id
— the same id the app, the matchmaker and the rater are configured with) and
where its event and module are cached (`fleet.rules_cache_dir`, default
`./rules`). Both are loaded at start-up — the event from the cache or the
relay, the module from the cache or the event's `url` hints — verified
(signature, digest, ABI, `describe.game`) and instantiated once for the whole
process; a fleet that cannot load them does not start, since a client MUST
hold the module before entering a pool, challenging, founding or accepting.

## What a persona does

- **Courts** the pool (kind `3418`): mirrors a compatible entry — same
  matchmaker, same rule system, same timing relay, a cadence and a variant of
  the persona — within the fleet's bot-vs-bot budget.
- **Founds** the session a Pairing (kind `3419`) naming it declares, as soon
  as it is observed, unless a canonical Game Session for it already exists;
  the content is the initial position the module prescribes.
- **Accepts** a Direct Challenge (kind `3420`) addressed to it by publishing
  the Game Session — the acceptance IS the founding — supplying what the
  challenge delegated (an open variant, an open seat), whenever it can play
  its **own** variant; the challenger's is theirs, so a cross-variant game
  by delegation is accepted, and an **asymmetric** imposition of the bot's
  variant is honoured iff the challenger is premium, asked of the admission
  service (`fleet.admission_url`, fail-closed — *Premium* §1.3, ADR-0040 §2). A rematch challenge is
  verified against the concluded session (both players, terms inherited,
  seats swapped, its `concluded_by` checked by the module) before it is
  accepted; a human's is answered unconditionally, a sibling bot's per the
  persona's per-game willingness.
- **Plays**: on each tick, the session's Plies and Conclusions are fetched,
  the module's `natural_state` at the present instant gives the view, the
  search chooses, the module's `legal_moves` has the last word, and the Ply
  is published with the per-slot idempotence discipline (one slot, one
  search; a lost publish is re-sent with the same content).
- **Concludes** only when it wants the verdict the module predicts for a
  Conclusion signed by it now: a terminal chain (either player states it),
  the win on time (as the winner, after a courtesy delay —
  `timeout_courtesy_secs`), the opponent's standing draw offer (per
  temperament), its own resignation (per the sustained assessment). The
  module's `select_conclusion` then decides the session is over; a
  non-conforming Conclusion — the opponent's or a stranger's — is logged and
  ignored.
- **Never proposes a rematch.** It accepts one — from a human or a sibling —
  whenever it is free: the concluded game's cadence slot stays with the pair
  for the rematch window, then any free slot of the cadence will do; against
  a sibling, the bot-vs-bot budget applies too.
- **Stars** a notable game (kind `7`), rarely.

## Design notes

See ADR-0014 for the full design, ADR-0033 and ADR-0034 for the arbiterless
protocol and the module, ADR-0039 for the cadence families and ADR-0040 for
the slots, the direct-path variant rule and the Robotto roster. Notable v1 choices: self-timed only, a single relay,
no premoves, no outbound Direct Challenges (fresh or rematch), stateless restart
from relay replay (sessions and pending Pairings are recovered).
Concurrency is capped **per cadence family** — `[bot.play.max_concurrent]`,
one table of four (ADR-0039 §6) — through the **cadence slots** of ADR-0040
§3 (`src/slots.rs`): every founding, an accepted rematch included,
is admitted through a per-(bot, cadence) automaton — committed, playing,
cooling — that keeps a concluded game's slot with the pair for the rematch
window (`REMATCH_WINDOW_SECS = 60`, the app's number too) and with nobody
else. The family of a founding is read by the one classifier of
[Cadence — Sanki](https://github.com/sashite/web-specs.md/blob/main/nostr/support/cadence-sanki.md)
(`src/cadence.rs`), pinned to the app's by the shared category-G vectors
(`conformance/cadence.json`, vendored from `web-specs.md`).
Timed behavior (think pacing, correspondence scheduling, win-on-time wakes)
runs through a coarse periodic tick so the notification loop never blocks;
per-bot randomness is seeded from the bot's pubkey, so a persona stays
consistent with itself across restarts.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
```

The unit tests drive the module-driven paths with the reference module's
**library face** (`sashite-sanki-kernel-wasm` as a dev-dependency — the same
answers as the module, natively). The e2e benches (`tests/e2e/`) run the real
binary against an in-process relay and need the module's bytes:
`SANKI_MODULE=<path to sashite_sanki_kernel_wasm.wasm> cargo test --test e2e`
— without it they are skipped. They pin the founding on a Pairing and the
per-slot idempotence discipline, and the win-on-time Conclusion.

## License

Apache-2.0 — see `LICENSE` and `NOTICE`.
