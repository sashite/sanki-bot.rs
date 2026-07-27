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
//! The test seats the bot itself (signing the founding and the Game Session
//! with the arbiter key the fleet TOML names) and plays the opponent. The
//! assertions, on the relay's frame log:
//!
//!   1. the bot answers its slot (a Ply for step 1 arrives);
//!   2. no re-send before the grace (the second frame is ≥ ~15 s after the
//!      first — never the pre-fix immediate re-search);
//!   3. the re-send carries the IDENTICAL content;
//!   4. across the whole run, ONE distinct content per slot — never two.
//!
//! Runtime ≈ 60 s (ticks + grace), single-threaded on the wire — run it as
//! `cargo test --test e2e`.

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

const GAME_SESSION_KIND: u16 = 6422;
const PAIRING_KIND: u16 = 6419;
const PLY_KIND: u16 = 6423;
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

fn fleet_toml(relay_url: &str, matchmaker: &PublicKey, arbiter: &PublicKey) -> String {
    format!(
        r#"[fleet]
relay_url  = "{relay_url}"
matchmaker = "{matchmaker}"
arbiter    = "{arbiter}"
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

  [bot.schedule]
  timezone = "UTC"
  windows  = [{{ days = "mon-sun", from = "00:00", to = "23:59" }}]
  jitter_minutes = 0
  presence_probability = 1.0

  [bot.tempo]
  think = {{ median_s = 0.5, sigma = 0.1 }}
  correspondence_think = {{ median_s = 0.5, sigma = 0.1 }}
"#,
        matchmaker = matchmaker.to_hex(),
        arbiter = arbiter.to_hex(),
    )
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

#[tokio::test(flavor = "multi_thread")]
async fn republishes_the_same_content_after_grace_and_never_two_contents_per_slot() {
    let relay = MiniRelay::start().await;

    let arbiter = Keys::generate();
    let opponent = Keys::generate();
    let bot = Keys::generate();
    let matchmaker = Keys::generate(); // configured but never involved here
    let bot_hex = bot.public_key().to_hex();

    // The fleet file lives outside every repository — here, the temp dir.
    let config_path =
        std::env::temp_dir().join(format!("sanki-e2e-fleet-{}.toml", std::process::id()));
    std::fs::write(
        &config_path,
        fleet_toml(&relay.url, &matchmaker.public_key(), &arbiter.public_key()),
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
    let _bot_process = BotProcess(child);

    // Readiness: the bot's startup publishes its kind-0 profile — the first
    // frame the relay sees from its key.
    let ready_bot = bot_hex.clone();
    wait_until(
        "the bot's kind-0 profile (startup)",
        Duration::from_secs(30),
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

    // The fault is armed BEFORE the game exists: the bot's very first Ply is
    // acknowledged and delivered to no one — the incident's exact gap.
    relay.swallow_plies_from(Some(bot_hex.clone())).await;

    // ── Seat the bot (the test signs as the fleet's arbiter) ────────────────
    // Founding: a self-timed Pairing (no timestamper) carrying the cadence.
    let founding = EventBuilder::new(Kind::Custom(PAIRING_KIND), "")
        .tags([
            tag(&["p", &opponent.public_key().to_hex(), "", "player"]),
            tag(&["p", &bot_hex, "", "player"]),
            tag(&["p", &arbiter.public_key().to_hex(), "", "arbiter"]),
            tag(&["time_control", "300", "3"]),
        ])
        .sign_with_keys(&arbiter)
        .expect("sign founding");
    let session = EventBuilder::new(Kind::Custom(GAME_SESSION_KIND), START_FEEN)
        .tags([
            tag(&["e", &founding.id.to_hex(), "", "pairing"]),
            tag(&["game", "sanki"]),
            tag(&["p", &opponent.public_key().to_hex(), "", "player"]),
            tag(&["p", &bot_hex, "", "player"]),
            tag(&["seat", &opponent.public_key().to_hex(), "first"]),
            tag(&["seat", &bot_hex, "second"]),
            tag(&["variant", &opponent.public_key().to_hex(), "chess"]),
            tag(&["variant", &bot_hex, "chess"]),
        ])
        .sign_with_keys(&arbiter)
        .expect("sign session");
    let session_id = session.id.to_hex();
    relay
        .inject(serde_json::to_value(&founding).expect("founding json"))
        .await;
    relay
        .inject(serde_json::to_value(&session).expect("session json"))
        .await;

    // The opponent (seat `first`) opens: the bot (seat `second`) is on move.
    let opening = EventBuilder::new(Kind::Custom(PLY_KIND), r#"["e2","e4",null]"#)
        .tags([
            tag(&["e", &session_id, "", "game_session"]),
            tag(&["p", &bot_hex, "", "opponent"]),
            tag(&["step", "1"]),
            tag(&["nonce", "0", "0"]),
        ])
        .sign_with_keys(&opponent)
        .expect("sign opening ply");
    relay
        .inject(serde_json::to_value(&opening).expect("opening json"))
        .await;

    // ── 1. The bot answers its slot (frame #1, swallowed on arrival) ───────
    let bot_plies = |frames: &[mini_relay::ReceivedEvent]| -> Vec<mini_relay::ReceivedEvent> {
        frames
            .iter()
            .filter(|frame| {
                frame.kind == u64::from(PLY_KIND)
                    && frame.pubkey == bot_hex
                    && frame.step.as_deref() == Some("1")
            })
            .cloned()
            .collect()
    };
    wait_until("the bot's first Ply", Duration::from_secs(30), || {
        let relay = &relay;
        async move { !bot_plies(&relay.received().await).is_empty() }
    })
    .await;

    // ── 2 + 3. The grace re-send: same content, never before the grace ─────
    wait_until(
        "the grace re-send of the same Ply",
        Duration::from_secs(45),
        || {
            let relay = &relay;
            async move { bot_plies(&relay.received().await).len() >= 2 }
        },
    )
    .await;
    let plies = bot_plies(&relay.received().await);
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
    let contents: BTreeSet<String> = bot_plies(&relay.received().await)
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
