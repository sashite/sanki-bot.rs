# sashite-sanki-bot

A **Sanki bot** for [Sashité](https://sashite.com/): one identity, one TOML
configuration, one engine — a child process speaking the
[Sashité Engine Interface](https://sashite.dev/specs/sei/1.0.0/) (SEI) —
per game, in one process. Without an engine it plays at random: the bot of
a course's first day, before the students write theirs. Decided by
[ADR-0045 v4](https://github.com/sashite/web-specs.md/blob/main/adr/adr-0045-sanki-bot-crate.md).

## The map

The bot stack, from the specification down to the binaries. The naming rule
of the organisation: crate **`sashite-<x>`** ↔ repository **`<x>.<lang>`**.

| Concept | Specification | Repository | Crate / artefact | Role |
|---|---|---|---|---|
| The engine protocol | [SEI 1.0.0](https://sashite.dev/specs/sei/1.0.0/) | `sashite.dev` | — | how a host and an engine talk: JSON Lines, FEEN, PMN, SIN |
| What SEI means for Sanki | *SEI Rules Document — Sanki* | `web-specs.md` (`rules/sei-sanki.md`) | identifier `sashite.sanki.kernel/1` | canonical FEEN and PMN, styles, pairings, modules |
| The rules | *Kernel — Sanki*, *Kernel ABI — Sanki* | [`sanki-engine.rs`](https://github.com/sashite/sanki-engine.rs), `sanki-kernel-wasm.rs` | [`sashite-sanki-engine`](https://crates.io/crates/sashite-sanki-engine), [`sashite-sanki-kernel-wasm`](https://crates.io/crates/sashite-sanki-kernel-wasm) (the module) | the only oracle; the PMN ↔ content converters |
| The protocol client | ADR-0045 §1, the NIPs of [`sashite/nostr`](https://github.com/sashite/nostr) | [`sanki-client.rs`](https://github.com/sashite/sanki-client.rs) | [`sashite-sanki-client`](https://crates.io/crates/sashite-sanki-client) | the verbs: rule system, relay information, readers, drafts, publisher |
| **The bot** | ADR-0045 §2–§7 | **`sanki-bot.rs`** (this repository) | **`sashite-sanki-bot`**, binary `sanki-bot` | one identity, one configuration, one SEI engine per game |
| The engine to fork | SEI, the rules document | [`sanki-sei-random-engine.rs`](https://github.com/sashite/sanki-sei-random-engine.rs) | [`sashite-sanki-sei-random-engine`](https://crates.io/crates/sashite-sanki-sei-random-engine) | a complete SEI engine that plays at random; the students' starting point |
| The reference brain | ADR-0015 | [`sanki-player.rs`](https://github.com/sashite/sanki-player.rs) | [`sashite-sanki-player`](https://crates.io/crates/sashite-sanki-player) | search: iterative deepening, alpha-beta, anytime |
| The engine that thinks | SEI, the rules document | [`sanki-sei-player.rs`](https://github.com/sashite/sanki-sei-player.rs) | [`sashite-sanki-sei-player`](https://crates.io/crates/sashite-sanki-sei-player) | the brain under SEI's clock; the sparring partner |

*Vocabulary.* **Engine** is SEI's word: a process that answers `search` with
a Move. The rules are `sanki-engine` — a name that predates SEI and is kept;
the `sei-` infix of the example engine's name is what tells the two apart.

## What the bot does

Over the public protocol, with no privileged key, honestly labelled
(`bot: true`, NIP-24), under **one rule system** — the Rule System event
(kind `3417`) it is configured with, whose WebAssembly module is the only
oracle of state, legality and verdict (ADR-0034):

- **Answers Direct Challenges** (kind `3420`) under its **Challenge Policy**
  (kind `30420`: `everyone`, `following`, `rating`, `nobody`) and its mute
  list, one cap per cadence family, the local checks in order and the
  network checks fail-closed; the acceptance *is* the founding of the Game
  Session (kind `3422`).
- **Sends Direct Challenges** to configured targets, so that two bots meet
  without a person — one a minute, at a jittered instant, at most one
  pending per target.
- **Plays** each session re-derived from the relay through the module on
  every event and timer: the engine's `search` under the session's clock,
  the hard stop before the flag, the fallback move when the engine gives no
  answer, one content per step, never a stamp backdated.
- **Concludes** only with the verdict the module yields — a rule ending, a
  timeout — and **offers, accepts and resigns** on the engine's scores,
  each policy enabled by the configuration, an acceptance or a resignation
  concluded only once no earlier candidate can re-select the chain.
- **Keeps its standing events** — profile, policy, contact list under
  `following`, mute list — equal to the configuration, published only where
  the relay's copy differs.
- **Refuses a person's key** (adoption), fails with `KeyInUse` when another
  instance holds the key on the host, and **halts** when another instance
  of the library writes with it from elsewhere.
- **Restarts stateless**: its open sessions, its pending challenges and the
  challenges to it are rebuilt from the relay.

Out of scope, each for a later ADR on top of this one: the matchmaking pool,
rematches, personas and social acts, the profile abroad, premium variant
imposition, several bots per process, pondering.

## Running

```sh
cargo install sashite-sanki-bot sashite-sanki-sei-player
sanki-bot --engine sanki-sei-player                # a bot that thinks, until SIGTERM
sanki-bot                                          # the built-in bot alone: random play
```

That is a bot: on Sashité's relay, open to everyone, playing the three
variants in blitz and rapid — a key created at the first start, under
`~/Library/Application Support/sanki-bot` on macOS (`~/.local/share/sanki-bot`
elsewhere), its npub in the log. Keep a copy of the key file. Two
parameters make it yours:

```sh
sanki-bot --defaults > ~/sanki/kitsune.toml        # the built-in bot, every key at its default
$EDITOR ~/sanki/kitsune.toml                       # the name, the picture, the engine and its depth, the caps
sanki-bot --config ~/sanki/kitsune.toml
```

**[GUIDE.md](GUIDE.md)** takes you from nothing to a bot in the background
and explains every key of the file.

`--config` overrides the built-in bot key by key (an unknown key is
refused); `--engine` names the SEI engine — a command and its arguments,
separated by spaces (a path with a space goes in the file's `[engine]`),
a path or a name in `PATH` — in place of the file's `[engine]`. Another
bot is another file: its own name, its own key and data (the paths under
`[connection]` and `[identity]`, and `engine.cwd` if the engine writes
files), its own process.

`RUST_LOG` filters the log (`info` by default). The key never leaves its
file: no environment variable, since the engine — a child process — would
inherit it. The start refuses a relay that is not self-timed (its NIP-11
`created_at_lower_limit` missing or above 5 s), a host whose latency or
mining would stamp stale, a person's key, and a read the relay does not
answer — nothing is written on an unknown state.

**In the background, on macOS.** `sanki-bot.example.plist` is a LaunchAgent:
started at login, restarted when it exits. Copy it, replace `kitsune` and
`/Users/you`, `mkdir -p ~/Library/Logs/sanki-bot`, then
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.sashite.sanki-bot.kitsune.plist`.
One file per bot.

**The rate.** A key plays under the relay's per-key limit (30 events a
minute on Sashité's free tier): at five seconds a move that is two games
at a time (`[play.max_concurrent]`); the bot refuses a configuration
that asks for more than its `rate_per_minute` allows — the capacity
inequality of ADR-0045 §4.

## The configuration

One file, every key explained in [GUIDE.md](GUIDE.md), read into a
`Config` whose types forbid the incoherent cases
(ADR-0045 §4): the lists' bounds and intersections, the engine's command
resolved to an executable and its `cwd` kept away from the data directory
and the key, the score policies only with an engine, the outgoing time
control playable by the per-move share rule, and the **capacity
inequality** — the caps against the relay's rate limit. Every key has a
default, the built-in bot's; `sanki-bot.example.toml` is that bot, every
key commented — what `sanki-bot --defaults` prints.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
SANKI_SEI_RANDOM_ENGINE="$(which sanki-sei-random-engine)" cargo test
cargo deny check
```

The tests drive the module-driven paths with the reference module's native
face and an in-process relay, both from `sashite-sanki-client`'s `testing`
feature: the SEI host against real processes (the random engine when
`SANKI_SEI_RANDOM_ENGINE` names one, scripted engines otherwise), whole
games, the start's reads, and bots end to end — two of them challenging
each other and playing.

## License

Apache-2.0 — see `LICENSE` and `NOTICE`.
