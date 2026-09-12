//! E2E — the per-slot idempotence discipline over a real wire (§6.5;
//! reliability lot 10).
//!
//! The incident this pins: the bot published a Ply, the relay acknowledged
//! it, and the bot's own session fetches never showed it back (asynchronous
//! indexing, a silent gap). The pre-fix re-entry SEARCHED AGAIN — a fresh
//! seed, so usually a different move — and published a SECOND content for
//! the same slot (observed: c2c4+b2b4, c2c4+e2e4, c7c5+d7d5), leaving every
//! consumer's selection to race the two. The fix: one slot, one search;
//! after `REPUBLISH_GRACE_SECS` (15 s) the bot re-sends the SAME content —
//! identical contents collapse to one candidate in the selection rule.
//!
//! This bench runs the REAL binary (`CARGO_BIN_EXE_players`) against an
//! in-process NIP-01 relay whose one fault is exactly the incident's:
//! `swallow_plies_from` acknowledges the bot's Ply and delivers it to no one.
//! The test publishes the Rule System event (kind 3417) the fleet plays
//! under — naming the reference module whose bytes `SANKI_MODULE` points at,
//! dropped in the fleet's rules cache — pairs the bot (signing the Pairing
//! with the matchmaker key the fleet TOML names) and plays the opponent. The
//! bot itself founds the Game Session on the Pairing (ADR-0033: no arbiter).
//! The assertions, on the relay's frame log:
//!
//!   0. the bot founds the session: a Game Session referencing the Pairing,
//!      with the initial position the rule system prescribes;
//!   1. the bot answers its slot (a Ply for step 1 arrives);
//!   2. no re-send before the grace (the second frame is ≥ ~15 s after the
//!      first — never the pre-fix immediate re-search);
//!   3. the re-send carries the IDENTICAL content;
//!   4. across the whole run, ONE distinct content per slot — never two.
//!
//! A second bench, on the same wire, pins the **conclusion** path (ADR-0033:
//! the player concludes, with the verdict the rule system yields): at a
//! 10 s/move cadence the opponent opens, the bot answers, the opponent
//! never moves again — and the bot, after its courtesy delay, publishes a
//! Conclusion claiming the win on time, and exactly one.
//!
//! A third bench pins the **directed path** (ADR-0014 §6.3; ADR-0040 §2):
//! the opponent challenges the bot directly (kind 3420), the bot accepts by
//! founding the Game Session itself — and then PLAYS it. The incident this
//! one pins: the acceptance was published from a task of its own and the
//! bot waited for the relay to hand the session back through its
//! subscription, which never happens for an event the client itself sent;
//! the session was never tracked and the bot never moved (0.8.2).
//!
//! Runtime ≈ 60 s each (ticks + grace), single-threaded on the wire — run
//! them as `SANKI_MODULE=<path to the module> cargo test --test e2e`; without
//! the module the benches are skipped.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod mini_relay;

use std::collections::BTreeSet;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use mini_relay::MiniRelay;
use nostr_sdk::prelude::*;
use serde_json::Value;

const RULE_SYSTEM_KIND: u16 = 3417;
const GAME_SESSION_KIND: u16 = 3422;
const PAIRING_KIND: u16 = 3419;
const DIRECT_CHALLENGE_KIND: u16 = 3420;
const PLY_KIND: u16 = 3423;
const CONCLUSION_KIND: u16 = 3425;
const ABI: &str = "sashite.sanki.kernel-abi/1";
const REPUBLISH_GRACE_SECS: u64 = 15; // mirror of actor.rs — the contract under test

/// Initial chess/chess position (the app fixtures' FEEN, byte-identical).
const START_FEEN: &str = "-rnbqk^bn-r/+p+p+p+p+p+p+p+p/8/8/8/8/+P+P+P+P+P+P+P+P/-RNBQK^BN-R / W/w";

/// Kills the bot process even when an assertion unwinds the test.
struct BotProcess(Child);
impl Drop for BotProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fleet_toml(relay_url: &str, matchmaker: &PublicKey, rules: &EventId, cache: &str) -> String {
    format!(
        r#"[fleet]
relay_url  = "{relay_url}"
matchmaker = "{matchmaker}"
rules      = "{rules}"
rules_cache_dir = "{cache}"
game       = "sanki"
pow_difficulty = 0

[[bot]]
name     = "e2e"
nsec_env = "PLAYER_NSEC_E2E"

  [bot.profile]
  display_name = "E2E"
  about        = "e2e bench persona (bot)"

  [bot.play]
  variants      = {{ chess = 1.0 }}
  time_controls = [{{ spec = [["300", "3"]], weight = 1.0 }}]
  strength      = {{ time_ms = 200, depth = 2 }}
  draw_offer    = "never"
  timeout_courtesy_secs = 3

  [bot.schedule]
  timezone = "UTC"
  windows  = [{{ days = "mon-sun", from = "00:00", to = "24:00" }}]
  jitter_minutes = 0
  presence_probability = 1.0

  [bot.tempo]
  think = {{ median_s = 0.5, sigma = 0.1 }}
  correspondence_think = {{ median_s = 0.5, sigma = 0.1 }}
"#,
        matchmaker = matchmaker.to_hex(),
        rules = rules.to_hex(),
    )
}

/// SHA-256 as 64 lowercase hex characters.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn tag(parts: &[&str]) -> Tag {
    Tag::parse(parts.iter().copied()).expect("static tag")
}

async fn wait_until<F, Fut>(what: &str, timeout: Duration, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if probe().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The shared setup: the relay, the rule system on it (its module in the
/// fleet's cache), the running bot, and the keys the test plays with.
struct Bench {
    relay: MiniRelay,
    opponent: Keys,
    bot_hex: String,
    matchmaker: Keys,
    rules_hex: String,
    cache: std::path::PathBuf,
    _process: BotProcess,
}

impl Bench {
    /// `None` when `SANKI_MODULE` is unset (the bench is skipped).
    async fn start() -> Option<Self> {
        let Ok(module_path) = std::env::var("SANKI_MODULE") else {
            eprintln!("SANKI_MODULE unset; skipping the e2e bench");
            return None;
        };
        let module_bytes = std::fs::read(&module_path).expect("read the module");
        let digest = sha256_hex(&module_bytes);

        let relay = MiniRelay::start().await;

        let publisher = Keys::generate(); // signs the Rule System event
        let opponent = Keys::generate();
        let bot = Keys::generate();
        let matchmaker = Keys::generate();
        let bot_hex = bot.public_key().to_hex();

        // The rule system: its event on the relay, its module in the fleet's
        // cache (no `url` hint here — a module dropped by hand is used as is).
        let rule_system = EventBuilder::new(Kind::Custom(RULE_SYSTEM_KIND), "e2e")
            .tags([
                tag(&["game", "sanki"]),
                tag(&["x", &digest]),
                tag(&["abi", ABI]),
                tag(&["nonce", "0", "0"]),
            ])
            .finalize(&publisher)
            .expect("sign rule system");
        let rules_id = rule_system.id;
        relay
            .inject(serde_json::to_value(&rule_system).expect("rule system json"))
            .await;
        let unique = format!("{}-{}", std::process::id(), rules_id.to_hex());
        let cache = std::env::temp_dir().join(format!("sanki-e2e-rules-{unique}"));
        std::fs::create_dir_all(&cache).expect("create the rules cache");
        std::fs::write(cache.join(format!("{digest}.wasm")), &module_bytes)
            .expect("drop the module");

        // The fleet file lives outside every repository — here, the temp dir.
        let config_path = std::env::temp_dir().join(format!("sanki-e2e-fleet-{unique}.toml"));
        std::fs::write(
            &config_path,
            fleet_toml(
                &relay.url,
                &matchmaker.public_key(),
                &rules_id,
                &cache.display().to_string(),
            ),
        )
        .expect("write fleet toml");

        let child = Command::new(env!("CARGO_BIN_EXE_players"))
            .env("FLEET_CONFIG_PATH", &config_path)
            .env("PLAYER_NSEC_E2E", bot.secret_key().to_secret_hex())
            .env("RUST_LOG", "info")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the players binary");
        let process = BotProcess(child);

        // Readiness: the bot's startup publishes its kind-0 profile — the
        // first frame the relay sees from its key (after the rule system
        // loaded).
        let ready_bot = bot_hex.clone();
        wait_until(
            "the bot's kind-0 profile (startup)",
            Duration::from_secs(60),
            || {
                let relay = &relay;
                let ready_bot = ready_bot.clone();
                async move {
                    relay
                        .received()
                        .await
                        .iter()
                        .any(|frame| frame.kind == 0 && frame.pubkey == ready_bot)
                }
            },
        )
        .await;

        Some(Self {
            relay,
            opponent,
            bot_hex,
            matchmaker,
            rules_hex: rules_id.to_hex(),
            cache,
            _process: process,
        })
    }

    /// Pair the bot (the test signs as the fleet's matchmaker): a self-timed
    /// Pairing carrying the cadence, the rule system, the seats (the opponent
    /// first, the bot second) and both variants. Returns the Pairing's id.
    async fn pair(&self, time_control: &[&str]) -> String {
        let now = Timestamp::now().as_secs();
        let mut tc = vec!["time_control"];
        tc.extend_from_slice(time_control);
        let founding = EventBuilder::new(Kind::Custom(PAIRING_KIND), "")
            .tags([
                tag(&["p", &self.opponent.public_key().to_hex(), "", "player"]),
                tag(&["p", &self.bot_hex, "", "player"]),
                tag(&["e", &self.rules_hex, "", "rules"]),
                tag(&["timing_relay", &self.relay.url]),
                tag(&["game", "sanki"]),
                tag(&["variant", &self.opponent.public_key().to_hex(), "chess"]),
                tag(&["variant", &self.bot_hex, "chess"]),
                tag(&["seat", &self.opponent.public_key().to_hex(), "first"]),
                tag(&["seat", &self.bot_hex, "second"]),
                tag(&tc),
                tag(&["found_until", &(now + 300).to_string()]),
                tag(&["nonce", "0", "0"]),
            ])
            .finalize(&self.matchmaker)
            .expect("sign pairing");
        let id = founding.id.to_hex();
        self.relay
            .inject(serde_json::to_value(&founding).expect("pairing json"))
            .await;
        id
    }

    /// Challenge the bot directly (the opponent signs a kind 3420): the
    /// mirror form, both on chess, the opponent `first` so the bot is on move
    /// second, a two-minute acceptance window. Returns the challenge's id.
    async fn challenge(&self, time_control: &[&str]) -> String {
        let now = Timestamp::now().as_secs();
        let mut tc = vec!["time_control"];
        tc.extend_from_slice(time_control);
        let challenge = EventBuilder::new(Kind::Custom(DIRECT_CHALLENGE_KIND), "")
            .tags([
                tag(&["p", &self.bot_hex, "", "opponent"]),
                tag(&["e", &self.rules_hex, "", "rules"]),
                tag(&["timing_relay", &self.relay.url]),
                tag(&["game", "sanki"]),
                tag(&["variant", &self.opponent.public_key().to_hex(), "chess"]),
                tag(&["variant", &self.bot_hex, "chess"]),
                tag(&["seat", "first"]),
                tag(&tc),
                tag(&["accept_until", &(now + 120).to_string()]),
                tag(&["nonce", "0", "0"]),
            ])
            .finalize(&self.opponent)
            .expect("sign direct challenge");
        let id = challenge.id.to_hex();
        self.relay
            .inject(serde_json::to_value(&challenge).expect("challenge json"))
            .await;
        id
    }

    /// Wait for the bot's Game Session on `founding_id` — a Pairing
    /// (`marker` = `pairing`) or a Direct Challenge (`direct_challenge`) —
    /// and check it: founded on it, the rules mirrored, the prescribed
    /// position. Returns the session's id and its `created_at` — t₀.
    async fn founded(&self, founding_id: &str, marker: &str) -> (String, u64) {
        let bot_hex = self.bot_hex.clone();
        wait_until("the bot's Game Session", Duration::from_secs(30), || {
            let relay = &self.relay;
            let bot_hex = bot_hex.clone();
            async move {
                relay.received().await.iter().any(|frame| {
                    frame.kind == u64::from(GAME_SESSION_KIND) && frame.pubkey == bot_hex
                })
            }
        })
        .await;
        let session = self
            .relay
            .stored()
            .await
            .into_iter()
            .find(|event| {
                event.get("kind").and_then(Value::as_u64) == Some(u64::from(GAME_SESSION_KIND))
                    && event.get("pubkey").and_then(Value::as_str) == Some(self.bot_hex.as_str())
            })
            .expect("the Game Session is stored");
        assert_eq!(
            session.get("content").and_then(Value::as_str),
            Some(START_FEEN),
            "the Game Session carries the position the rule system prescribes"
        );
        let session_tags = session.get("tags").and_then(Value::as_array).expect("tags");
        let has = |name: &str, value: &str, marker: Option<&str>| {
            session_tags.iter().any(|t| {
                t.get(0).and_then(Value::as_str) == Some(name)
                    && t.get(1).and_then(Value::as_str) == Some(value)
                    && marker.is_none_or(|m| t.get(3).and_then(Value::as_str) == Some(m))
            })
        };
        assert!(
            has("e", founding_id, Some(marker)),
            "founded on the {marker}"
        );
        assert!(
            has("e", &self.rules_hex, Some("rules")),
            "mirrors the rules reference"
        );
        assert!(has("seat", &self.bot_hex, None) && has("variant", &self.bot_hex, None));
        let id = session
            .get("id")
            .and_then(Value::as_str)
            .expect("session id")
            .to_owned();
        let created_at = session
            .get("created_at")
            .and_then(Value::as_u64)
            .expect("session created_at");
        (id, created_at)
    }

    /// The opponent (seat `first`) opens: the bot (seat `second`) is on move.
    /// The Ply is timed just after t₀ — the bot stamps its events with a
    /// forward buffer over the relay's clock, so a Ply stamped "now" could
    /// precede the session it answers and be invalid (kind 3423 §Time
    /// accounting).
    async fn open(&self, session_id: &str, start: u64) {
        let opening = EventBuilder::new(Kind::Custom(PLY_KIND), r#"["e2","e4",null]"#)
            .tags([
                tag(&["e", session_id, "", "game_session"]),
                tag(&["p", &self.bot_hex, "", "opponent"]),
                tag(&["step", "1"]),
                tag(&["nonce", "0", "0"]),
            ])
            .custom_created_at(Timestamp::from_secs(
                start.max(Timestamp::now().as_secs()) + 1,
            ))
            .finalize(&self.opponent)
            .expect("sign opening ply");
        self.relay
            .inject(serde_json::to_value(&opening).expect("opening json"))
            .await;
    }

    /// The bot's Plies for its step 1, in arrival order.
    async fn bot_plies(&self) -> Vec<mini_relay::ReceivedEvent> {
        self.relay
            .received()
            .await
            .iter()
            .filter(|frame| {
                frame.kind == u64::from(PLY_KIND)
                    && frame.pubkey == self.bot_hex
                    && frame.step.as_deref() == Some("1")
            })
            .cloned()
            .collect()
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn founds_on_the_pairing_then_republishes_the_same_content_after_grace() {
    let Some(bench) = Bench::start().await else {
        return;
    };
    let relay = &bench.relay;
    let bot_hex = bench.bot_hex.clone();

    // The fault is armed BEFORE the game exists: the bot's very first Ply is
    // acknowledged and delivered to no one — the incident's exact gap.
    relay.swallow_plies_from(Some(bot_hex.clone())).await;

    // ── 0. Paired, the bot founds the session on the Pairing ───────────────
    let pairing_id = bench.pair(&["300", "3"]).await;
    let (session_id, start) = bench.founded(&pairing_id, "pairing").await;
    bench.open(&session_id, start).await;

    // ── 1. The bot answers its slot (frame #1, swallowed on arrival) ───────
    wait_until("the bot's first Ply", Duration::from_secs(30), || {
        let bench = &bench;
        async move { !bench.bot_plies().await.is_empty() }
    })
    .await;

    // ── 2 + 3. The grace re-send: same content, never before the grace ─────
    wait_until(
        "the grace re-send of the same Ply",
        Duration::from_secs(45),
        || {
            let bench = &bench;
            async move { bench.bot_plies().await.len() >= 2 }
        },
    )
    .await;
    let plies = bench.bot_plies().await;
    let first = &plies[0];
    let second = &plies[1];
    let gap = second.at.duration_since(first.at);
    assert!(
        gap >= Duration::from_secs(REPUBLISH_GRACE_SECS - 1),
        "the re-send came {gap:?} after the first publish — before the {REPUBLISH_GRACE_SECS}s grace: \
         that is the pre-fix re-search, not the idempotent re-send"
    );
    assert_eq!(
        second.content, first.content,
        "the re-send changed the content — the double-publish incident (one slot, two moves)"
    );

    // ── The gap heals: stop swallowing, let a re-send land for real ────────
    relay.swallow_plies_from(None).await;
    let stored_ply_from = |events: &[Value], author: &str| {
        events.iter().any(|event| {
            event.get("kind").and_then(Value::as_u64) == Some(u64::from(PLY_KIND))
                && event.get("pubkey").and_then(Value::as_str) == Some(author)
        })
    };
    wait_until(
        "the healed re-send reaching the store",
        Duration::from_secs(45),
        || {
            let relay = &relay;
            let bot_hex = bot_hex.clone();
            async move { stored_ply_from(&relay.stored().await, &bot_hex) }
        },
    )
    .await;

    // ── 4. The whole run: ONE distinct content for the slot — never two ────
    let contents: BTreeSet<String> = bench
        .bot_plies()
        .await
        .into_iter()
        .map(|frame| frame.content)
        .collect();
    assert_eq!(
        contents.len(),
        1,
        "the bot published {} distinct contents for one slot: {:?}",
        contents.len(),
        contents
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concludes_with_the_win_on_time_the_rule_system_yields_once() {
    let Some(bench) = Bench::start().await else {
        return;
    };
    // 10 s per move: the opponent opens, the bot answers, the opponent's
    // clock then runs out.
    let pairing_id = bench.pair(&["0", "10", "1"]).await;
    let (session_id, start) = bench.founded(&pairing_id, "pairing").await;
    bench.open(&session_id, start).await;
    wait_until("the bot's answer", Duration::from_secs(30), || {
        let bench = &bench;
        async move { !bench.bot_plies().await.is_empty() }
    })
    .await;
    let answered_at = std::time::Instant::now();

    // The bot's Conclusions of the session.
    let conclusions = |frames: &[mini_relay::ReceivedEvent]| -> Vec<mini_relay::ReceivedEvent> {
        frames
            .iter()
            .filter(|frame| {
                frame.kind == u64::from(CONCLUSION_KIND) && frame.pubkey == bench.bot_hex
            })
            .cloned()
            .collect()
    };
    wait_until(
        "the bot's Conclusion (win on time)",
        Duration::from_secs(60),
        || {
            let bench = &bench;
            async move { !conclusions(&bench.relay.received().await).is_empty() }
        },
    )
    .await;
    let claimed_after = answered_at.elapsed();
    assert!(
        claimed_after >= Duration::from_secs(10 + 3),
        "claimed {claimed_after:?} after answering — before the opponent's 10 s ran out \
         plus the 3 s courtesy"
    );
    let conclusion = conclusions(&bench.relay.received().await)[0].clone();
    assert_eq!(
        conclusion.content, "timeout",
        "the status the rule system yields"
    );
    let stored = bench
        .relay
        .stored()
        .await
        .into_iter()
        .find(|event| {
            event.get("kind").and_then(Value::as_u64) == Some(u64::from(CONCLUSION_KIND))
                && event.get("pubkey").and_then(Value::as_str) == Some(bench.bot_hex.as_str())
        })
        .expect("the Conclusion is stored");
    let tags = stored.get("tags").and_then(Value::as_array).expect("tags");
    let result_of = |pubkey: &str| {
        tags.iter().find_map(|t| {
            (t.get(0).and_then(Value::as_str) == Some("result")
                && t.get(1).and_then(Value::as_str) == Some(pubkey))
            .then(|| t.get(2).and_then(Value::as_str).unwrap_or("").to_owned())
        })
    };
    assert_eq!(result_of(&bench.bot_hex).as_deref(), Some("100"));
    assert_eq!(
        result_of(&bench.opponent.public_key().to_hex()).as_deref(),
        Some("0")
    );
    assert!(tags.iter().any(|t| {
        t.get(0).and_then(Value::as_str) == Some("e")
            && t.get(1).and_then(Value::as_str) == Some(session_id.as_str())
            && t.get(3).and_then(Value::as_str) == Some("game_session")
    }));

    // Exactly one Conclusion: once its echo is back, the session is
    // concluded for the bot and nothing more is published for it.
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert_eq!(
        conclusions(&bench.relay.received().await).len(),
        1,
        "one Conclusion, never two"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_a_direct_challenge_then_plays_it() {
    let Some(bench) = Bench::start().await else {
        return;
    };
    // ── The opponent challenges the bot directly; the acceptance IS the
    //    Game Session the bot publishes (kind 3422 §Signing party) ──────────
    let challenge_id = bench.challenge(&["300", "3"]).await;
    let (session_id, start) = bench.founded(&challenge_id, "direct_challenge").await;

    // ── And the bot then serves what it founded: the opponent opens, the
    //    bot answers. (0.8.2: the session it published itself never came
    //    back through the subscription, so it was never tracked, and this
    //    wait timed out.) ────────────────────────────────────────────────────
    bench.open(&session_id, start).await;
    wait_until(
        "the bot's answer in the session it accepted",
        Duration::from_secs(30),
        || {
            let bench = &bench;
            async move { !bench.bot_plies().await.is_empty() }
        },
    )
    .await;
    // One acceptance, one session: the bot founded exactly once.
    let sessions = bench
        .relay
        .received()
        .await
        .iter()
        .filter(|frame| frame.kind == u64::from(GAME_SESSION_KIND) && frame.pubkey == bench.bot_hex)
        .count();
    assert_eq!(
        sessions, 1,
        "the bot published {sessions} Game Sessions for one challenge"
    );
}
