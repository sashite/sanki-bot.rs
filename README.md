# sashite-sanki-player-nostr-bot

The Sashité **player fleet** (ADR-0014): autonomous Nostr players for Sanki.
One process supervises N configured personas — each an honest player with its
own keypair, timezone, presence windows, cadence tastes, playing strength and
temperament — playing sessions end-to-end over the public protocol: the
matchmaking pool (kind `6418`), Direct Challenges (`6420`/`6421`), Plies
(`6423`) under the Time Accounting discipline, and Adjudication Requests
(`6424`) published **only when the predicted verdict is wanted**.

The bots hold no privileged key and are honestly labeled (`bot: true`,
NIP-24). Game and protocol semantics are reused, never reimplemented:

- `sashite-sanki-engine` — legality, application, terminal status;
- `sashite-sanki-arbiter` — the canonical chain, the clocks, and verdict
  prediction: a synthetic self-timed probe request timed "now" turns
  `natural_state` / `adjudicate` into a live oracle of exactly what the
  arbiter would rule;
- `sashite-sanki-player` — anytime move choice under the clock budget.

## Running

```sh
cp fleet.example.toml ~/sanki-e2e/fleet.local.toml   # outside every repo
$EDITOR ~/sanki-e2e/fleet.local.toml                 # personas + key env names
set -a; source ~/sanki-e2e/bots.local.env; set +a    # PLAYER_NSEC_* variables
FLEET_CONFIG_PATH=~/sanki-e2e/fleet.local.toml cargo run --bin players
```

Environment: `FLEET_CONFIG_PATH` (required), one `PLAYER_NSEC_*` per bot
(named by each `[[bot]]`'s `nsec_env`; never logged), `RUST_LOG` (optional).

## Design notes

See ADR-0014 for the full design. Notable v1 choices: self-timed only, a
single relay, no premoves, no outbound Direct Challenges, stateless restart
from relay replay. Timed behavior (think pacing, correspondence scheduling,
win-on-time wakes) runs through a coarse periodic tick so the notification
loop never blocks; per-bot randomness is seeded from the bot's pubkey, so a
persona stays consistent with itself across restarts.

## License

Apache-2.0 — see `LICENSE` and `NOTICE`.
