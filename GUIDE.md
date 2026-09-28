# Running your own Sanki bot

A bot that plays on [Sashité](https://sashite.com/) is one program on your
own computer: `sanki-bot`, with an identity it creates for itself, a
configuration file that says who it is and what it plays, and — if you
want it to think — an **engine**, a second program it talks to over the
[Sashité Engine Interface](https://sashite.dev/specs/sei/1.0.0/) (SEI).
Nothing runs on Sashité's servers for you; your bot is a player like any
other, reading and writing the same public Nostr events as the app, under
its own key, honestly labelled as a bot.

This guide takes you from nothing to a bot running in the background, then
explains every key of the configuration file. Commands are for macOS and
Linux; the bot has no Windows build.

## 1. What you need

- **Rust**, to build the programs: `curl https://sh.rustup.rs -sSf | sh`
  (see [rustup.rs](https://rustup.rs/)), then a new terminal.
- **The bot** and **an engine**, from crates.io:

  ```sh
  cargo install sashite-sanki-bot          # the binary: sanki-bot
  cargo install sashite-sanki-sei-player   # an engine that thinks: sanki-sei-player
  ```

  Both land in `~/.cargo/bin`, which rustup adds to your `PATH`. There is
  also `sashite-sanki-sei-random-engine` (`sanki-sei-random-engine`), an
  engine that plays a random legal move — the one to fork if you write your
  own — and no engine at all is fine too: the bot then plays at random by
  itself.

## 2. A bot in one command

```sh
sanki-bot --engine sanki-sei-player
```

That is a bot. At its first start it creates a key, logs its **npub** and
where the key file is, connects to Sashité's relay, publishes its profile
and its challenge policy, and waits for challenges. The log looks like:

```
WARN a new identity: keep a copy of the key file npub=npub1… file=/Users/you/Library/Application Support/sanki-bot/identity/key.nsec
INFO sanki-bot npub=npub1… name=sanki-bot
INFO engine probed engine=Some("sanki-sei-player") rtt_ms=3
INFO quarantine secs=…
INFO rebuilt open=0 closed=0 unverified=0 pending=0 incoming=0
INFO standing event published kind=0
INFO standing event published kind=30420
INFO started
```

Open the app, find the bot by its npub (or search its name, `sanki-bot`),
and challenge it in blitz or rapid. You will see:

```
INFO challenge admitted; founding challenge=… challenger=… cadence=Blitz
INFO game open session=… cadence=Blitz opponent=… fresh=true
…
INFO game closed session=… status=checkmate freed=Some((Blitz, …))
```

Stop it with Ctrl-C (or `SIGTERM`): the bot stops its games cleanly and
exits; its open games are re-read from the relay at the next start.

**Keep a copy of the key file.** The key *is* the bot: its name, its
rating, its history. Lose the file and the bot is gone; leak it and anyone
can play as your bot. Nothing else is stored: the bot restarts stateless,
rebuilding what it needs from the relay.

## 3. Making it yours: the configuration file

The bot you just ran is the **built-in bot**: every setting at its default.
A configuration file overrides those defaults **key by key** — you write
only what you change. Start from the full, commented default:

```sh
mkdir -p ~/sanki
sanki-bot --defaults > ~/sanki/kitsune.toml
$EDITOR ~/sanki/kitsune.toml
sanki-bot --config ~/sanki/kitsune.toml
```

Three things to change first:

```toml
[profile]
name  = "kitsune"                          # the handle people search
about = "A fox that plays ōgi. Runs on a laptop in Lyon."
# picture = "https://example.com/kitsune.png"

[engine]
command = "sanki-sei-player"               # the engine, by name (in PATH) or by absolute path
options = { depth = 5 }                    # its options — here, how deep it thinks
```

With `[engine]` in the file you no longer pass `--engine`: the command-line
flag *replaces* the whole `[engine]` table (it exists for the one-liner of
§2), so a file that sets `options` must name the engine itself.

Another bot is another file — and another key: set `identity.file` to a
path of its own (see §5), and run it as its own process. One process, one
key, one bot; the data directory can be shared.

**A profile edit needs a restart.** The bot compares its profile, policy
and lists to the relay's copies at every start and republishes what
differs. Edit the file, restart the bot, done. A field you edit by hand in
a Nostr client is overwritten at the next start: the file is the owner.

## 4. In the background (macOS)

A LaunchAgent starts the bot at login and restarts it if it exits.
`sanki-bot.example.plist` in this repository is one; copy it, replace
`kitsune` by your bot's name and `/Users/you` by your home (launchd
expands no `~`), check the path of the binary (`which sanki-bot`), and
give `engine.command` in the file an absolute path too — launchd's `PATH`
is minimal. Then:

```sh
mkdir -p ~/Library/Logs/sanki-bot
cp sanki-bot.example.plist ~/Library/LaunchAgents/com.sashite.sanki-bot.kitsune.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.sashite.sanki-bot.kitsune.plist
tail -f ~/Library/Logs/sanki-bot/kitsune.log
```

Restart after a change of the configuration:

```sh
launchctl kickstart -k gui/$(id -u)/com.sashite.sanki-bot.kitsune
```

Stop for good:

```sh
launchctl bootout gui/$(id -u)/com.sashite.sanki-bot.kitsune
```

The bot is the job's own process (no wrapper), so launchd's `SIGTERM`
reaches it. A Mac that goes to sleep with a game open loses that game on
time: set *Prevent automatic sleeping* in System Settings › Energy (or
Battery › Options), or accept it. On Linux, the same with a systemd user
unit (`ExecStart=%h/.cargo/bin/sanki-bot --config %h/sanki/kitsune.toml`,
`Restart=always`).

## 5. Every key of the file

The file is TOML. Every key has a default — the built-in bot's; an unknown
key is refused, so a typo never passes silently. Paths must be absolute.
When the bot refuses a file it names the key: `sanki-bot: configuration:
\`play.margin_ms\`: margin_ms + 1,000 must be below min_move_secs × 1,000`.

The **data root** is `~/Library/Application Support/sanki-bot` on macOS,
`$XDG_DATA_HOME/sanki-bot` (else `~/.local/share/sanki-bot`) elsewhere. The
three default paths live under it; the bot creates the directories it
writes to.

### `schema`

`schema = 1` — the version of this file's format. Only `1` is read.

### `[connection]`

| Key | Default | Meaning |
|---|---|---|
| `relay` | `"wss://relay.sanki.app"` | The only relay the bot talks to, and the clock every game is timed by. It must be *self-timed* (its NIP-11 document states a `created_at_lower_limit` of 5 s or less); the bot refuses to start otherwise. Another relay is another Sashité — the app's events name the relay they were made for. |
| `rules` | the Rule System of this release | The id (64 hex) of the kind-`3417` Rule System event the bot plays under: the WebAssembly module it loads is the only judge of legality and outcome. Change it only when Sashité publishes a new one; the app's founding events name it. |
| `data_dir` | `<data root>/data` | Where the bot caches the rules and keeps its lease file (`<pubkey>.lock`: two instances with the same key on the same host refuse each other). Absolute. |
| `rate_per_minute` | `30` | The relay's per-key limit (1–1,000), which NIP-11 does not advertise. Sashité's free tier is 30 events a minute; the bot keeps a tenth in reserve and refuses a configuration that could exceed the rest (see `[play.max_concurrent]`). Only raise it if your key really has a higher allowance. |

### `[identity]`

| Key | Default | Meaning |
|---|---|---|
| `file` | `<data root>/identity/key.nsec` | The key file: `nsec1…` or 64 hex, one line. Created at the first start with mode `0600` if absent. A file readable by the group or others is refused. The key is never put in the environment (the engine, a child process, would inherit it). |

The bot refuses a key that already belongs to a person: a key whose kind-0
profile on the relay is not marked `bot: true`, or whose contact list or
mute list was written by anything but this bot library (its `client` tag
tells). To turn a fresh key you generated elsewhere into a bot's, just
give it to the bot before using it anywhere. A relay that does not answer
these reads fails the start (`the relay did not answer the read of kind
…`): nothing is decided on an unknown state.

### `[profile]`

The kind-0 profile (NIP-24), republished at every start when it differs
from the relay's copy; `bot: true` is always written.

| Key | Default | Meaning |
|---|---|---|
| `name` | `"sanki-bot"` | The handle: 1 to 64 characters, what people search and what the log prints. |
| `about` | `"A Sanki bot (sashite-sanki-bot)."` | Free text, up to 1,000 characters. Say what it plays, how strong it is, where it runs, who to contact. Newlines are kept: `"""…"""` for several lines. |
| `display_name` | *(none)* | A longer or prettier name, 1 to 64 characters, optional. |
| `picture` | *(none)* | An `https://` URL of the avatar, optional. |
| `nip05` | *(none)* | A NIP-05 address (`name@domain`) you can answer for, optional. |
| `website` | *(none)* | An `https://` URL, optional. |

### `[engine]`

Absent, the bot plays at random — its **fallback**, the move it also plays
when the engine gives no answer in time. Present, the bot launches the
engine once per game as a child process, sends it every position with the
whole history and the clock, validates each answer under the rules, and
plays it. `--engine 'COMMAND ARGS…'` on the command line replaces this
whole table.

| Key | Default | Meaning |
|---|---|---|
| `command` | *(required)* | The engine's program: an absolute path, a relative path (resolved from the working directory), or a bare name looked up in `PATH`. Must be an existing executable file. |
| `args` | `[]` | Its arguments, one string each. |
| `cwd` | `<data root>/engine` | The directory the engine runs in — the only place it may write. Absolute, created if absent; it may be neither under `data_dir` nor in the key file's directory. |
| `env` | `{}` | The **only** environment variables the engine receives — not even `PATH`: the bot's own environment is not inherited. A wrapper script needs absolute paths inside, or `env = { PATH = "/usr/bin:/bin" }`. |
| `options` | `{}` | SEI options, sent by `configure` after `hello`: booleans, integers or strings, each one the engine announces, within its domain — the bot refuses to start otherwise (`sanki-sei-player`: `depth` 1–12, `seed`; `sanki-sei-random-engine`: `seed`). |
| `strength` | *(none)* | An Elo target, only with an engine that announces the SEI `strength` feature, within the bounds it announces. |
| `max_relaunches_per_game` | `2` | How many times a crashed or unresponsive engine is relaunched within one game (0–10) before the bot finishes the game on fallback moves. |
| `launch_ms` | `5000` | The engine must answer `hello`, `configure` and a `ping` within this (1,000–60,000 ms). |

The engine is **probed at the bot's start**: launched, asked `hello`,
configured, pinged; the bot refuses to start when the engine does not
speak SEI 1, does not implement `sashite.sanki.kernel/1` for every pairing
the bot may play, or rejects an option. The engine runs at the bot's
priority; on a laptop that also does other things, `nice` is yours to add
in a wrapper script named as the `command` (with absolute paths inside:
see `env`).

### `[challenges]`

Who may challenge the bot. Published as its Challenge Policy (kind
`30420`), so that the app shows the right button, and **enforced**: a
challenge the policy refuses is ignored.

| Key | Default | Meaning |
|---|---|---|
| `policy` | `"everyone"` | `everyone`; `following` (only the keys in `follows`); `rating` (only players whose rating, attested by `rating_authority`, is within `max_delta` of the bot's own — both must be rated, so a fresh bot under `rating` plays nothing until it is); `nobody` (the bot only plays the games it starts itself). |
| `follows` | — | With `following`: the allowed keys (hex, 1 to 1,000, distinct, none in `blocks`). Also published as the bot's contact list (kind `3`). |
| `max_delta` | — | With `rating`: 1 to 1,000 Elo points. |
| `rating_authority` | — | With `rating`: the hex key of the rater whose attestations (kind `3426`) count — Sashité's, on its relay. |
| `blocks` | `[]` | Keys never played, whatever the policy; published as the bot's mute list (kind `10000`). |

A key of another policy (`follows` under `everyone`, say) is refused: a
setting nobody decided on is a mistake, not a default.

#### `[challenges.outgoing]`

Absent — the default — the bot never challenges anyone. Present, it sends
Direct Challenges (kind `3420`) to the `targets`, so that two bots meet
without a person: yours and a friend's, or a student's and the class's.

| Key | Default | Meaning |
|---|---|---|
| `targets` | *(required)* | Hex keys (1 to 1,000), distinct, none in `blocks`. |
| `time_control` | *(required)* | The periods, as the app writes them: `[[180, 2]]` is 3 min + 2 s; `[[0, 10, 1]]` is 10 s a move (byōyomi); `[[600]]` is 10 min; `[[900, 10], [300, 5]]` two periods. Each period is `[seconds]`, `[seconds, increment]` or `[seconds, increment, moves]`. It must offer at least `play.min_move_secs` a move (`duration / 40 + increment` for a bank, `duration / min(moves, 40) + increment` for a quota), and its family must have a cap. |
| `variant` | *(required)* | The variant both players get (`chess`, `ogi`, `xiongqi`); it must be in `play.variants` and in `play.opponents`. |
| `every_secs` | *(required)* | The least time between two challenges to the same target (≥ 60 s; the example says 600). |
| `accept_secs` | *(required)* | How long a challenge stays open (30–3,600 s; the example says 120). |
| `max_per_day` | *(required)* | Over all targets, on a rolling 24 hours (1–1,000; the example says 24). |

The table is all or nothing: present, every key is required. At most one
challenge is pending per target — and none while a game with that target
is open or a challenge from it is pending — at most one is sent a minute,
at a jittered instant so that two bots challenging each other do not
collide. A pending challenge holds a slot of its family.

### `[play]`

| Key | Default | Meaning |
|---|---|---|
| `variants` | `["chess", "ogi", "xiongqi"]` | What the bot itself plays. A challenger who fixes the bot's variant must pick one of these. |
| `preferred` | `"chess"` | What the bot plays when the challenger leaves its variant open. Must be in `variants`. |
| `opponents` | `["chess", "ogi", "xiongqi"]` | What it accepts across the board. `["ogi"]` for a bot that only ever plays ōgi against ōgi (with `variants = ["ogi"]`). |
| `min_move_secs` | `5` | The bot's own pace — it answers no faster than this unless the flag is nearer, so that a human can follow — and the least time per move a game must offer for the bot to accept it (2–3,600). Lower it for a bot that should play byōyomi at 3 s; the capacity rule below then admits fewer games. |
| `margin_ms` | `300` | Kept between the engine's hard stop and the flag (100–5,000; with `margin_ms + 1,000 < min_move_secs × 1,000`). Part of the `overhead` the engine is told; raise it on a slow machine or a slow connection. |

#### `[play.max_concurrent]`

How many games the bot plays at once, **per cadence family**, read on the
first period: `byoyomi` (a period of 0 seconds plus a per-move allowance),
`blitz` (up to five minutes), `rapid` (over five minutes, under a day),
`correspondence` (a day or more, as duration or as increment). A challenge
in a family at its cap is refused. Every cap at 0 is allowed only with
`policy = "nobody"` and no `[challenges.outgoing]`.

This is the one table that is not merged key by key: given, it says what
is played, and a family left out is 0. The default is `blitz = 1`,
`rapid = 1`.

The **capacity rule**: the bot may emit at most `rate_per_minute` events a
minute less a tenth in reserve, and one game at `min_move_secs` a move may
emit `⌈60 / (min_move_secs − margin_ms / 1000)⌉` Plies a minute — 13 at
the defaults. Σ caps × 13 (+ 1 with `[challenges.outgoing]`) must stay
within 27: **two games at a time** on the free tier, whatever the families.
The bot refuses a configuration that asks for more, naming the numbers.

#### `[play.resign]`, `[play.offer_draw]`, `[play.accept_draw]`

Absent — the default — the bot never resigns, never offers a draw, never
accepts one; it plays every game to its end on the board or on the clock.
Each table needs an `[engine]`: the decisions are taken on the engine's
own evaluation of the current turn — its `wdl` (win/draw/loss per mille)
and `mate` scores, and its `advice` when it gives one.

| Table | Keys | Meaning |
|---|---|---|
| `[play.resign]` | `max_win = 20`, `min_loss = 900`, `streak = 3` | Resign when, for `streak` consecutive own turns, the win chance is at most `max_win` ‰ and the loss chance at least `min_loss` ‰ — or the engine sees itself mated, or advises resigning (`max_win < min_loss`, `streak` 1–10). |
| `[play.offer_draw]` | `min_draw = 800`, `streak = 3`, `after_ply = 40` | Offer a draw when the draw chance is at least `min_draw` ‰ for `streak` own turns — or the engine advises a draw — from ply `after_ply` (0–600) on. |
| `[play.accept_draw]` | `min_draw = 600` | Accept an offer when the draw chance is at least `min_draw` ‰, or the engine advises a draw. |

The values shown are the example's, not defaults: each table is all or
nothing. While an offer stands the bot never resigns: with
`[play.accept_draw]` it accepts, without it moves. A resignation or an
acceptance is concluded only once the relay's copy of the game confirms
it is still the right verdict.

## 6. Two engines, and yours

**`sanki-sei-player`** thinks: the reference search of Sashité's
`sashite-sanki-player` (iterative deepening, alpha-beta, a transposition
table, quiescence) under SEI's clock. Its strength is its `depth` option
(default 4; up to 12): under a clock the depth is the ceiling and the time
the search plans on is the game's — a twenty-fifth of what is left plus
the increment, three quarters of it — so a deeper bot is a slower one, and
the clock decides what the depth cannot. Its `seed` (default 0: drawn
afresh each search) breaks the ties among equal moves.

**`sanki-sei-random-engine`** plays a legal move at random, instantly. It
is the engine to fork: everything an SEI engine must do is in it and
tested, and the one thing it does badly is one function.

**Yours** is any program that speaks SEI on its standard streams — in Rust,
Python, anything. The bot validates every move it returns under the rules
and plays a fallback when it answers late, so a broken engine loses games,
never breaks the protocol. Read the specification, fork the random engine,
and give the bot your program's path in `engine.command`.

## 7. When something is wrong

- **`lease: another instance holds this key on this host (….lock)`** — a
  bot with this key is already running (a LaunchAgent, another terminal).
  One process per key.
- **`not a bot's key: …`** — the key belongs to a person (its profile is
  not marked `bot: true`). Use a fresh key.
- **`configuration: \`engine.command\`: … is not an existing executable
  file`** — the engine is not in `PATH` (under launchd, `PATH` is
  minimal: use absolute paths).
- **`engine probe: …`** — the engine did not pass the probe: an option it
  does not announce, a pairing it does not play, no answer within
  `launch_ms`.
- **The bot ignores my challenge** — run with `RUST_LOG=debug` and read
  the `refusal=…` line: the family is at its cap or not played
  (`FamilyNotPlayed(Byoyomi)`), the game offers less than
  `min_move_secs` a move, the variant is not in `opponents`, the
  challenge fixes the bot's variant without fixing yours to the same one,
  the policy refuses you. Under `rating`, `not founded why=outside the
  rating window` at `info`.
- **`the relay did not answer the read; again in a second`** — the
  connection dropped; the bot reconnects on its own and re-reads its games.
  Nothing is written on an unknown state.
- **The bot lost on time while my computer slept** — a sleeping laptop
  does not play. Keep it awake, or play correspondence only.

`RUST_LOG=info` is the default; `RUST_LOG=debug` shows every decision;
`RUST_LOG=info,sashite_sanki_bot=debug` the bot's decisions with the
client's and the binary's own lines kept.
