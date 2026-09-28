// SPDX-License-Identifier: Apache-2.0
//! One game, end to end over the in-process relay: the bot plays its turns
//! (its fallback, at random — or its engine when `SANKI_SEI_RANDOM_ENGINE`
//! names one), the test plays the opponent, and the game closes on the
//! Conclusion the bot claims when the opponent's clock falls.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr_sdk::prelude::*;
use sashite_sanki_bot::config::Config;
use sashite_sanki_bot::game::{Game, GameContext, GameEnd, GameInput, SharedOracle};
use sashite_sanki_bot::identity::Identity;
use sashite_sanki_client::module::{self, native::Native, Oracle};
use sashite_sanki_client::notation;
use sashite_sanki_client::publisher::{Publisher, Settings};
use sashite_sanki_client::readers;
use sashite_sanki_client::session::{self, fixtures::World};
use sashite_sanki_client::testing::MiniRelay;
use tokio::sync::mpsc;

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sanki-bot-game-{}-{}",
        std::process::id(),
        Timestamp::now().as_secs()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An engine to launch: the command and its arguments.
#[derive(Clone)]
struct EngineSpec {
    command: String,
    args: Vec<String>,
}

fn config(data_dir: &std::path::Path, engine: Option<&EngineSpec>, extra_play: &str) -> Config {
    let engine_section = engine.map_or(String::new(), |spec| {
        format!(
            "[engine]\ncommand = \"{}\"\nargs = {}\ncwd = \"{}\"\noptions = {{ seed = 3 }}\nmax_relaunches_per_game = 1\nlaunch_ms = 5000\n",
            spec.command,
            serde_json::to_string(&spec.args).unwrap(),
            std::env::temp_dir().display()
        )
    });
    Config::from_toml(&format!(
        r#"
schema = 1
[connection]
relay = "wss://relay.example.com"
rules = "{}"
data_dir = "{}"
rate_per_minute = 120
[identity]
file = "{}/key.nsec"
[profile]
name = "kitsune"
about = ""
picture = "https://example.com/k.png"
{engine_section}
[challenges]
policy = "everyone"
blocks = []
[play]
variants = ["chess"]
preferred = "chess"
opponents = ["chess"]
min_move_secs = 2
margin_ms = 300
[play.max_concurrent]
blitz = 1
{extra_play}
"#,
        "7".repeat(64),
        data_dir.display(),
        data_dir.display(),
    ))
    .unwrap()
}

struct Bench {
    relay: MiniRelay,
    world: World,
    ctx: Arc<GameContext>,
}

/// The bank, in seconds, of the games' time control (`bank + 1`).
const BANK: u64 = 12;

/// The reference module, slow to read a session: its answer comes once
/// the clock has crossed two second boundaries — a module in an
/// interpreter, on a long chain, in a debug build, as the games of
/// 2026-09-28 met at their eleventh and fourteenth Plies.
struct Slow;

impl Oracle for Slow {
    fn answer(&mut self, request: &[u8]) -> Option<Vec<u8>> {
        if request.windows(15).any(|w| w == b"\"natural_state\"") {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            let until = Duration::from_secs(now.as_secs() + 2) + Duration::from_millis(50);
            std::thread::sleep(until - now);
        }
        Native.answer(request)
    }
}

async fn bench(engine: Option<&EngineSpec>, extra_play: &str, bank: u64) -> Bench {
    bench_with(engine, extra_play, bank, Box::new(Native)).await
}

async fn bench_with(
    engine: Option<&EngineSpec>,
    extra_play: &str,
    bank: u64,
    oracle: Box<dyn Oracle + Send>,
) -> Bench {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let relay = MiniRelay::start().await;
    let data_dir = temp_dir();
    let config = Arc::new(config(&data_dir, engine, extra_play));
    let mut world = World::new();
    world.time_control = vec![bank.to_string(), "1".to_owned()];
    let identity = Identity::from_keys(world.first.clone());
    let me = identity.public_key();
    let fallback_key = identity.fallback_key().clone();
    let client = Client::builder().build();
    client.add_relay(&relay.url).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    let settings = Settings {
        relay: RelayUrl::parse(&relay.url).unwrap(),
        rate_per_minute: 120,
        data_dir,
        pow: 0,
        past_tolerance: 1,
        future_tolerance: 5,
    };
    let publisher = Arc::new(
        Publisher::open(client.clone(), identity, settings)
            .await
            .unwrap(),
    );
    let oracle: SharedOracle = Arc::new(Mutex::new(oracle));
    let describe = module::describe(&mut Native).unwrap();
    let probe = match engine {
        Some(spec) => {
            let launch = sashite_sanki_bot::sei::Launch {
                command: PathBuf::from(&spec.command),
                args: spec.args.clone(),
                cwd: std::env::temp_dir(),
                env: Vec::new(),
            };
            let mut needs = sashite_sanki_bot::sei::Needs {
                pairings: config.play().pairings(),
                options: config.engine().unwrap().options.clone(),
                strength: None,
                launch_ms: 5000,
            };
            needs
                .options
                .insert("seed".to_owned(), serde_json::json!(3));
            Some(
                sashite_sanki_bot::sei::probe(
                    &launch,
                    &sashite_sanki_bot::sei::Host::default(),
                    &needs,
                )
                .await
                .unwrap(),
            )
        }
        None => None,
    };
    let ctx = Arc::new(GameContext {
        config,
        me,
        fallback_key,
        publisher,
        client,
        oracle,
        describe,
        probe,
    });
    Bench { relay, world, ctx }
}

/// Injects a signed event into the relay and hands it to the game.
async fn inject(bench: &Bench, inputs: &mpsc::Sender<GameInput>, event: Event) {
    bench
        .relay
        .inject(serde_json::from_str(&event.as_json()).unwrap())
        .await;
    inputs.send(GameInput::Event(event)).await.unwrap();
}

async fn play(engine: Option<&EngineSpec>) {
    let bench = bench(engine, "", BANK).await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms.clone(), now, Vec::new());
    let (tx, rx) = mpsc::channel(64);
    let ctx = Arc::clone(&bench.ctx);
    let running = tokio::spawn(game.run(ctx, rx, false));

    // The bot is `first`: its Ply lands, paced at anchor + 2 s.
    let first_ply = wait_for_ply(&bench, 1, &bench.world.first.public_key()).await;
    assert!(readers::has_client_tag(&first_ply, "sanki-bot"));
    assert!(first_ply.created_at.as_secs() >= now + 2);
    // Its content is a legal move of the initial position.
    let legal = module::legal_moves(&mut Native, &terms.position).unwrap();
    assert!(legal.contains(&first_ply.content));

    // The opponent answers, twice; the bot answers each time.
    let mut position = terms.position.clone();
    position = module::apply(&mut Native, &position, &first_ply.content)
        .unwrap()
        .position;
    let mut last_ours = first_ply.created_at.as_secs();
    for step in 1..=2u32 {
        let reply = module::legal_moves(&mut Native, &position).unwrap()[0].clone();
        // Stamped one second ahead, as a relay client's Ply arrives: the
        // game must wake when it becomes visible, not sleep to the flag.
        let stamp = bench.ctx.publisher.now() + 1;
        let ply = bench
            .world
            .ply(&terms.id, &bench.world.second, step, &reply, stamp);
        inject(&bench, &tx, ply).await;
        position = module::apply(&mut Native, &position, &reply)
            .unwrap()
            .position;
        let ours = wait_for_ply(&bench, step + 1, &bench.world.first.public_key()).await;
        let legal = module::legal_moves(&mut Native, &position).unwrap();
        assert!(legal.contains(&ours.content), "step {}", step + 1);
        last_ours = ours.created_at.as_secs();
        position = module::apply(&mut Native, &position, &ours.content)
            .unwrap()
            .position;
    }

    // The opponent falls silent: 12 s after the bot's last Ply their clock
    // falls, and the bot claims the timeout a second later.
    let end = tokio::time::timeout(Duration::from_secs(40), running)
        .await
        .expect("the game closes")
        .unwrap();
    match end {
        GameEnd::Closed(verdict) => {
            assert_eq!(verdict.status, "timeout");
            assert_eq!(verdict.result.first, 100, "{verdict:?}");
        }
        other => panic!("{other:?}"),
    }
    let conclusions: Vec<_> = bench
        .relay
        .stored()
        .await
        .into_iter()
        .filter(|e| e["kind"] == 3425)
        .collect();
    assert_eq!(conclusions.len(), 1);
    assert_eq!(conclusions[0]["content"], "timeout");
    // Claimed from the opponent's flag plus a second: their bank (at least
    // `BANK`, the increments not spent) from the bot's last Ply, plus one.
    let claimed = conclusions[0]["created_at"].as_u64().unwrap();
    assert!(claimed > last_ours + BANK, "{claimed} vs {last_ours}");
}

async fn wait_for_ply(bench: &Bench, step: u32, signer: &PublicKey) -> Event {
    for _ in 0..300 {
        let stored = bench.relay.stored().await;
        if let Some(value) = stored.iter().find(|e| {
            e["kind"] == 3423
                && e["pubkey"] == signer.to_hex()
                && e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == "step" && t[1].as_str() == Some(step.to_string().as_str()))
        }) {
            return Event::from_json(value.to_string()).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no Ply at step {step}");
}

#[tokio::test]
async fn a_game_without_an_engine_is_played_at_random_and_concluded() {
    play(None).await;
}

#[tokio::test]
async fn a_slow_module_leaves_no_ply_behind() {
    // Reading the session takes the module past two second boundaries —
    // a reconciliation straddles them — and the opponent's Ply — stamped a second ahead,
    // as a relay client's is — falls between the instant the view was
    // judged at and the instant the wake would have been. The bot must
    // still answer it, at its pace, not at the flag.
    let bench = bench_with(None, "", 60, Box::new(Slow)).await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms.clone(), now, Vec::new());
    let (tx, rx) = mpsc::channel(64);
    let ctx = Arc::clone(&bench.ctx);
    let running = tokio::spawn(game.run(ctx, rx, false));
    let first_ply = wait_for_ply(&bench, 1, &bench.world.first.public_key()).await;
    let mut position = module::apply(&mut Native, &terms.position, &first_ply.content)
        .unwrap()
        .position;
    for step in 1..=3u32 {
        // The game idle, its reconciliations after its own Ply over: the
        // opponent's Ply is what wakes it.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let reply = module::legal_moves(&mut Native, &position).unwrap()[0].clone();
        let stamp = bench.ctx.publisher.now() + 1;
        let ply = bench
            .world
            .ply(&terms.id, &bench.world.second, step, &reply, stamp);
        inject(&bench, &tx, ply).await;
        position = module::apply(&mut Native, &position, &reply)
            .unwrap()
            .position;
        let ours = wait_for_ply(&bench, step + 1, &bench.world.first.public_key()).await;
        // Answered within the pace and a few reconciliations — never at
        // the flag, sixty seconds on.
        assert!(
            ours.created_at.as_secs() <= stamp + 25,
            "step {}: answered at {} to a Ply of {}",
            step + 1,
            ours.created_at.as_secs(),
            stamp
        );
        position = module::apply(&mut Native, &position, &ours.content)
            .unwrap()
            .position;
    }
    tx.send(GameInput::Stop).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), running).await;
}

#[tokio::test]
async fn a_game_with_the_random_engine() {
    let Some(path) = std::env::var_os("SANKI_SEI_RANDOM_ENGINE") else {
        eprintln!("SANKI_SEI_RANDOM_ENGINE not set: skipped");
        return;
    };
    play(Some(&EngineSpec {
        command: path.to_string_lossy().into_owned(),
        args: Vec::new(),
    }))
    .await;
}

#[tokio::test]
async fn a_canonical_conclusion_closes_the_game_whoever_signed_it() {
    let bench = bench(None, "", BANK).await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms.clone(), now, Vec::new());
    let (tx, rx) = mpsc::channel(64);
    let running = tokio::spawn(game.run(Arc::clone(&bench.ctx), rx, false));
    let first_ply = wait_for_ply(&bench, 1, &bench.world.first.public_key()).await;
    // Stamped after the bot's Ply: a resignation after a move, not the
    // residual one against the invoker.
    let stamp = first_ply.created_at.as_secs() + 1;
    let resignation =
        bench
            .world
            .conclusion(&terms.id, &bench.world.second, "resignation", 100, 0, stamp);
    inject(&bench, &tx, resignation).await;
    let end = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(end, GameEnd::Closed(ref v) if v.status == "resignation"),
        "{end:?}"
    );
}

#[tokio::test]
async fn stop_ends_the_game() {
    let bench = bench(None, "", BANK).await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms, now, Vec::new());
    let (tx, rx) = mpsc::channel(64);
    let running = tokio::spawn(game.run(Arc::clone(&bench.ctx), rx, false));
    tx.send(GameInput::Stop).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end, GameEnd::Stopped);
}

// ---- the policies, with a scripted engine ----

/// A few lines of Python answering SEI: legal `best`s for the bot's first
/// two turns of a chess game (`e2-e4`, then `d2-d4`, legal whatever Black
/// replied), and the evaluation `mode` asks for.
const SCRIPT: &str = r#"
import sys, json
mode = sys.argv[1]
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
HELLO = {"version": 1, "versions": [1], "engine": {"name": "scripted"},
         "rules": {"sashite.sanki.kernel/1": {}}, "features": {"advice": {}},
         "options": {"seed": {"type": "int", "default": 0, "min": 0, "max": 1000}}}
for line in sys.stdin:
    r = json.loads(line); op = r["op"]; i = r["id"]
    if op == "hello": out({"re": i, "ev": "done", **HELLO})
    elif op in ("ping", "configure", "cancel"): out({"re": i, "ev": "done"})
    elif op == "search":
        best = "e2-e4" if len(r.get("moves", [])) == 0 else "d2-d4"
        if mode == "resign":
            out({"re": i, "ev": "done", "best": best, "advice": "resign",
                 "variations": [{"pv": [best], "score": {"cp": -900, "wdl": [10, 90, 900]}}]})
        else:
            out({"re": i, "ev": "done", "best": best, "advice": "draw",
                 "variations": [{"pv": [best], "score": {"cp": 0, "wdl": [100, 800, 100]}}]})
"#;

fn scripted(mode: &str) -> EngineSpec {
    let dir = std::env::temp_dir().join(format!("sanki-bot-game-sei-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("engine.py");
    std::fs::write(&script, SCRIPT).unwrap();
    EngineSpec {
        command: "python3".to_owned(),
        args: vec![script.to_string_lossy().into_owned(), mode.to_owned()],
    }
}

#[tokio::test]
async fn a_lost_evaluation_on_a_streak_resigns_instead_of_moving() {
    let engine = scripted("resign");
    let bench = bench(
        Some(&engine),
        "[play.resign]\nmax_win = 20\nmin_loss = 900\nstreak = 1\n",
        BANK,
    )
    .await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms, now, Vec::new());
    let (_tx, rx) = mpsc::channel(64);
    let running = tokio::spawn(game.run(Arc::clone(&bench.ctx), rx, false));
    let end = tokio::time::timeout(Duration::from_secs(20), running)
        .await
        .expect("the game closes")
        .unwrap();
    match end {
        GameEnd::Closed(verdict) => {
            assert_eq!(verdict.status, "resignation");
            assert_eq!(verdict.result.second, 100, "{verdict:?}");
        }
        other => panic!("{other:?}"),
    }
    // No Ply was played: the bot resigned on its first turn.
    let stored = bench.relay.stored().await;
    assert!(stored.iter().all(|e| e["kind"] != 3423));
    let conclusion = stored.iter().find(|e| e["kind"] == 3425).unwrap();
    assert_eq!(
        conclusion["pubkey"],
        bench.world.first.public_key().to_hex()
    );
    assert!(
        conclusion["created_at"].as_u64().unwrap() >= now + 4,
        "past tolerance, 1 s, skew"
    );
}

#[tokio::test]
async fn a_standing_offer_is_accepted_when_the_draw_is_judged_likely() {
    let engine = scripted("draw");
    // A bank wide enough for the acceptance to be feasible whatever the
    // engine's start-up took: its instant, the read and the margin before
    // the flag.
    let bench = bench(Some(&engine), "[play.accept_draw]\nmin_draw = 600\n", 30).await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms.clone(), now, Vec::new());
    let (tx, rx) = mpsc::channel(64);
    let running = tokio::spawn(game.run(Arc::clone(&bench.ctx), rx, false));

    // Without a standing offer the bot moves, whatever its evaluation.
    let first_ply = wait_for_ply(&bench, 1, &bench.world.first.public_key()).await;
    let e2e4 =
        notation::to_content(&notation::position(&terms.position).unwrap(), "e2-e4").unwrap();
    assert_eq!(first_ply.content, e2e4);

    // The opponent replies with the `draw` flag.
    let position = module::apply(&mut Native, &terms.position, &e2e4)
        .unwrap()
        .position;
    let reply = module::legal_moves(&mut Native, &position).unwrap()[0].clone();
    let stamp = bench.ctx.publisher.now() + 1;
    let offer = EventBuilder::new(Kind::Custom(3423), reply)
        .tags(vec![
            World::e(&terms.id, "game_session"),
            World::p(&bench.world.first.public_key(), "opponent"),
            Tag::parse(["step", "1"]).unwrap(),
            Tag::parse(["draw"]).unwrap(),
            Tag::parse(["nonce", "0", "0"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(stamp))
        .finalize(&bench.world.second)
        .unwrap();
    let offer_stamp = offer.created_at.as_secs();
    inject(&bench, &tx, offer).await;

    let end = tokio::time::timeout(Duration::from_secs(25), running)
        .await
        .expect("the game closes")
        .unwrap();
    match end {
        GameEnd::Closed(verdict) => {
            assert_eq!(verdict.status, "agreement");
            assert_eq!(verdict.result.first, 50, "{verdict:?}");
        }
        other => panic!("{other:?}"),
    }
    let stored = bench.relay.stored().await;
    // Two Plies only: the bot accepted at its step 2 instead of moving.
    assert_eq!(stored.iter().filter(|e| e["kind"] == 3423).count(), 2);
    let conclusion = stored.iter().find(|e| e["kind"] == 3425).unwrap();
    assert_eq!(conclusion["content"], "agreement");
    assert_eq!(
        conclusion["pubkey"],
        bench.world.first.public_key().to_hex()
    );
    // Concluded no earlier than the offer's anchor + L + 1 s + the skew
    // allowance (ADR-0045 §7).
    let concluded = conclusion["created_at"].as_u64().unwrap();
    assert!(
        concluded >= offer_stamp + 1 + 1 + 2,
        "{concluded} vs {offer_stamp}"
    );
}

#[tokio::test]
async fn a_co_writers_ply_withdraws_the_answered_turn() {
    let bench = bench(None, "", BANK).await;
    let now = bench.ctx.publisher.now();
    let pairing = bench.world.pairing();
    let session = bench.world.session(&pairing, now);
    let terms = session::terms(&session, &pairing).unwrap();
    let game = Game::new(&bench.ctx, terms.clone(), now, Vec::new());
    let (tx, rx) = mpsc::channel(64);
    let running = tokio::spawn(game.run(Arc::clone(&bench.ctx), rx, false));

    // While the bot's answer waits for its pace (anchor + 2 s), a co-writer
    // acting with the same key publishes a Ply for the step, visible at
    // once: the turn is withdrawn, and the bot publishes nothing for it.
    let legal = module::legal_moves(&mut Native, &terms.position).unwrap();
    let theirs = legal[legal.len() - 1].clone();
    let ply = bench
        .world
        .ply(&terms.id, &bench.world.first, 1, &theirs, now);
    inject(&bench, &tx, ply).await;
    tokio::time::sleep(Duration::from_secs(4)).await;

    let stored = bench.relay.stored().await;
    let ours: Vec<_> = stored
        .iter()
        .filter(|e| e["kind"] == 3423 && e["pubkey"] == bench.world.first.public_key().to_hex())
        .collect();
    assert_eq!(ours.len(), 1, "{ours:?}");
    assert_eq!(ours[0]["content"], theirs);

    tx.send(GameInput::Stop).await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end, GameEnd::Stopped);
}
