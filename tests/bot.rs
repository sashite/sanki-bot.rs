// SPDX-License-Identifier: Apache-2.0
//! Bots end to end over the in-process relay: two bots challenging each
//! other by configuration and playing (ADR-0045 §9), a restart resuming a
//! session founded by a target and a pending challenge, a person's key
//! refused, another instance of the library halting the bot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr_sdk::prelude::*;
use sashite_sanki_bot::bot::runtime::Runtime;
use sashite_sanki_bot::bot::start::{self, Prepared};
use sashite_sanki_bot::bot::StartError;
use sashite_sanki_bot::config::Config;
use sashite_sanki_bot::game::SharedOracle;
use sashite_sanki_bot::identity::Identity;
use sashite_sanki_client::drafts::{self, Publishable};
use sashite_sanki_client::module::{self, native::Native, Oracle};
use sashite_sanki_client::publisher::{Lease, CLIENT_TAG};
use sashite_sanki_client::readers::Founding;
use sashite_sanki_client::session::fixtures::CHESS_CHESS;
use sashite_sanki_client::testing::MiniRelay;
use tokio::sync::oneshot;

const RULES: &str = "7777777777777777777777777777777777777777777777777777777777777777";
const DESIGNATED: &str = "wss://relay.example.com";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sanki-bot-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A few lines of Python answering SEI with a legal first move for either
/// colour and the advice to resign: the bot resigns on its first turn.
const RESIGNING: &str = r#"
import sys, json
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
HELLO = {"version": 1, "versions": [1], "engine": {"name": "resigning"},
         "rules": {"sashite.sanki.kernel/1": {}}, "features": {"advice": {}},
         "options": {"seed": {"type": "int", "default": 0, "min": 0, "max": 1000}}}
for line in sys.stdin:
    r = json.loads(line); op = r["op"]; i = r["id"]
    if op == "hello": out({"re": i, "ev": "done", **HELLO})
    elif op in ("ping", "configure", "cancel"): out({"re": i, "ev": "done"})
    elif op == "search":
        n = len(r.get("moves", []))
        best = ["e2-e4", "e7-e5", "d2-d4", "d7-d5"][n] if n < 4 else "a2-a3"
        out({"re": i, "ev": "done", "best": best, "advice": "resign",
             "variations": [{"pv": [best], "score": {"cp": -900, "wdl": [10, 90, 900]}}]})
"#;

fn resigning_engine(dir: &std::path::Path) -> String {
    let script = dir.join("engine.py");
    std::fs::write(&script, RESIGNING).unwrap();
    format!(
        "[engine]\ncommand = \"python3\"\nargs = [\"{}\"]\ncwd = \"{}\"\noptions = {{ seed = 1 }}\nmax_relaunches_per_game = 1\nlaunch_ms = 5000\n[play.resign]\nmax_win = 20\nmin_loss = 900\nstreak = 1\n",
        script.display(),
        std::env::temp_dir().display()
    )
}

struct Spec<'a> {
    name: &'a str,
    challenges: String,
    extra: String,
}

fn config(data_dir: &std::path::Path, spec: &Spec<'_>) -> Config {
    Config::from_toml(&format!(
        r#"
schema = 1
[connection]
relay = "{DESIGNATED}"
rules = "{RULES}"
data_dir = "{}"
rate_per_minute = 120
[identity]
file = "{}/key.nsec"
[profile]
name = "{}"
about = "a bot"
picture = "https://example.com/k.png"
{}
{}
[play]
variants = ["chess"]
preferred = "chess"
opponents = ["chess"]
min_move_secs = 2
margin_ms = 300
[play.max_concurrent]
blitz = 1
"#,
        data_dir.display(),
        data_dir.display(),
        spec.name,
        spec.challenges,
        spec.extra,
    ))
    .unwrap()
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the bot stops")
            .unwrap();
    }
}

async fn prepared(relay: &MiniRelay, keys: &Keys, spec: &Spec<'_>) -> Prepared {
    let data_dir = temp_dir(spec.name);
    let config = Arc::new(config(&data_dir, spec));
    let identity = Identity::from_keys(keys.clone());
    let lease = Lease::take(&data_dir, identity.public_key()).unwrap();
    let client = Client::builder()
        .notification_channel_size(sashite_sanki_bot::bot::NOTIFICATION_CHANNEL)
        .build();
    client.add_relay(&relay.url).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    let oracle: SharedOracle = Arc::new(Mutex::new(Box::new(Native) as Box<dyn Oracle + Send>));
    let describe = module::describe(&mut Native).unwrap();
    let probe = match config.engine() {
        Some(engine) => {
            let needs = sashite_sanki_bot::sei::Needs {
                pairings: config.play().pairings(),
                options: engine.options.clone(),
                strength: None,
                launch_ms: engine.launch_ms,
            };
            Some(
                sashite_sanki_bot::sei::probe(
                    &engine.launch,
                    &sashite_sanki_bot::sei::Host::default(),
                    &needs,
                )
                .await
                .unwrap(),
            )
        }
        None => None,
    };
    // The connection's relay is the designated one; the client connects
    // to the in-process relay, whose document the start reads.
    let mut relay_info = relay.info().await;
    relay_info.limitation.created_at_lower_limit = Some(1);
    relay_info.limitation.created_at_upper_limit = Some(5);
    Prepared {
        config,
        identity,
        lease,
        probe,
        client,
        relay: RelayUrl::parse(&relay.url).unwrap(),
        relay_info,
        oracle,
        describe,
        quarantine: Some(Duration::ZERO),
    }
}

/// Starts a bot and runs it until stopped.
async fn run_bot(relay: &MiniRelay, keys: &Keys, spec: &Spec<'_>) -> Result<Running, StartError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let prepared = prepared(relay, keys, spec).await;
    let started = start::start(prepared).await?;
    let runtime = Runtime::new(&started, false);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        runtime
            .run(started, async move {
                let _ = stop_rx.await;
            })
            .await;
    });
    Ok(Running {
        stop: Some(stop_tx),
        task,
    })
}

fn everyone() -> String {
    "[challenges]\npolicy = \"everyone\"\nblocks = []\n".to_owned()
}

fn challenging(target: &PublicKey) -> String {
    format!(
        "[challenges]\npolicy = \"everyone\"\nblocks = []\n[challenges.outgoing]\ntargets = [\"{}\"]\ntime_control = [[60, 2]]\nvariant = \"chess\"\nevery_secs = 600\naccept_secs = 30\nmax_per_day = 5\n",
        target.to_hex()
    )
}

async fn wait_for<F: Fn(&[serde_json::Value]) -> bool>(
    relay: &MiniRelay,
    secs: u64,
    f: F,
) -> Vec<serde_json::Value> {
    for _ in 0..secs * 10 {
        let stored = relay.stored().await;
        if f(&stored) {
            return stored;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not observed within {secs} s: {:?}", relay.stored().await);
}

fn of_kind<'a>(
    stored: &'a [serde_json::Value],
    kind: u64,
    pubkey: &PublicKey,
) -> Vec<&'a serde_json::Value> {
    stored
        .iter()
        .filter(|e| e["kind"] == kind && e["pubkey"] == pubkey.to_hex())
        .collect()
}

#[tokio::test]
async fn two_bots_challenge_each_other_and_play() {
    let relay = MiniRelay::start().await;
    let a = Keys::generate();
    let b = Keys::generate();
    let engine_dir = temp_dir("engine");
    // A challenges B, and resigns on its first turn; B answers everyone.
    let spec_a = Spec {
        name: "alpha",
        challenges: challenging(&b.public_key()),
        extra: resigning_engine(&engine_dir),
    };
    let spec_b = Spec {
        name: "beta",
        challenges: everyone(),
        extra: String::new(),
    };
    let bot_b = run_bot(&relay, &b, &spec_b).await.unwrap();
    let bot_a = run_bot(&relay, &a, &spec_a).await.unwrap();

    // The challenge, the founding by B, the game, A's resignation.
    let stored = wait_for(&relay, 40, |s| {
        s.iter()
            .any(|e| e["kind"] == 3425 && e["content"] == "resignation")
    })
    .await;
    let challenges = of_kind(&stored, 3420, &a.public_key());
    assert_eq!(challenges.len(), 1, "one challenge, to B");
    assert!(challenges[0]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t[0] == "p" && t[1] == b.public_key().to_hex()));
    let sessions = of_kind(&stored, 3422, &b.public_key());
    assert_eq!(sessions.len(), 1, "B founds, once");
    assert!(sessions[0]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t[0] == "e" && t[1] == challenges[0]["id"] && t[3] == "direct_challenge"));
    let conclusions: Vec<_> = stored.iter().filter(|e| e["kind"] == 3425).collect();
    assert_eq!(conclusions.len(), 1);
    assert_eq!(conclusions[0]["pubkey"], a.public_key().to_hex());
    // Every event of the library carries its tag.
    for e in stored
        .iter()
        .filter(|e| [3420, 3422, 3423, 3425].contains(&e["kind"].as_u64().unwrap()))
    {
        assert!(e["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t[0] == "client" && t[1] == CLIENT_TAG));
    }
    // The standing events of both.
    for key in [&a, &b] {
        assert_eq!(of_kind(&stored, 0, &key.public_key()).len(), 1);
        assert_eq!(of_kind(&stored, 30420, &key.public_key()).len(), 1);
    }
    // No second challenge within `every_secs`, no second founding.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let stored = relay.stored().await;
    assert_eq!(of_kind(&stored, 3420, &a.public_key()).len(), 1);
    assert_eq!(of_kind(&stored, 3422, &b.public_key()).len(), 1);

    bot_a.stop().await;
    bot_b.stop().await;
}

/// A Direct Challenge by `challenger` to `target`, stamped `at`, as the
/// library would write it (`tagged`), or as a person's client would.
fn challenge(
    challenger: &Keys,
    target: PublicKey,
    at: u64,
    accept_secs: u64,
    tagged: bool,
) -> Event {
    let draft = drafts::DirectChallenge {
        me: challenger.public_key(),
        target,
        game: "sanki".to_owned(),
        rules: EventId::from_hex(RULES).unwrap(),
        timing_relay: DESIGNATED.to_owned(),
        time_control: vec![[Some(60), Some(2), None]],
        variant: "chess".to_owned(),
        accept_secs: NonZeroU64::new(accept_secs).unwrap(),
        not_after: None,
        content: String::new(),
    };
    let parts = draft.at(at, DESIGNATED).unwrap();
    let mut tags = parts.tags;
    tags.push(Tag::parse(["nonce", "0", "0"]).unwrap());
    if tagged {
        tags.push(Tag::custom("client", [CLIENT_TAG.to_owned()]));
    }
    EventBuilder::new(Kind::Custom(3420), parts.content)
        .tags(tags)
        .custom_created_at(Timestamp::from(at))
        .finalize(challenger)
        .unwrap()
}

/// The Game Session `founder` founds on `challenge` at `at`, with the
/// seats given.
fn session_on(
    challenge: &Event,
    founder: &Keys,
    first: PublicKey,
    second: PublicKey,
    at: u64,
) -> Event {
    let draft = drafts::GameSession {
        plan: drafts::SessionPlan {
            founding: Founding::DirectChallenge(challenge.id),
            game: "sanki".to_owned(),
            rules: EventId::from_hex(RULES).unwrap(),
            timing_relay: DESIGNATED.to_owned(),
            first,
            second,
            first_variant: "chess".to_owned(),
            second_variant: "chess".to_owned(),
            position: CHESS_CHESS.to_owned(),
        },
        not_before: challenge.created_at.as_secs(),
        not_after: at + 1,
    };
    let parts = draft.at(at, DESIGNATED).unwrap();
    EventBuilder::new(Kind::Custom(3422), parts.content)
        .tags(parts.tags)
        .custom_created_at(Timestamp::from(at))
        .finalize(founder)
        .unwrap()
}

async fn inject(relay: &MiniRelay, event: &Event) {
    relay
        .inject(serde_json::from_str(&event.as_json()).unwrap())
        .await;
}

#[tokio::test]
async fn a_restart_resumes_a_session_a_target_founded_and_a_pending_challenge() {
    let relay = MiniRelay::start().await;
    let me = Keys::generate();
    let target = Keys::generate();
    let other = Keys::generate();
    let now = relay.now().await;

    // Before the start: a challenge of ours the target answered — the
    // target seated first, and its first Ply played: the bot is on move.
    let answered = challenge(&me, target.public_key(), now - 20, 30, true);
    let session = session_on(
        &answered,
        &target,
        target.public_key(),
        me.public_key(),
        now - 15,
    );
    // The target's first Ply, then it is our turn.
    let ply = EventBuilder::new(Kind::Custom(3423), r#"["e2","e4",null]"#)
        .tags(vec![
            Tag::parse(["e", &session.id.to_hex(), "", "game_session"]).unwrap(),
            Tag::parse(["p", &me.public_key().to_hex(), "", "opponent"]).unwrap(),
            Tag::parse(["step", "1"]).unwrap(),
            Tag::parse(["nonce", "0", "0"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(now - 10))
        .finalize(&target)
        .unwrap();
    // And a challenge of ours still pending, to another target.
    let pending = challenge(&me, other.public_key(), now - 5, 30, true);
    for e in [&answered, &session, &ply, &pending] {
        inject(&relay, e).await;
    }

    let spec = Spec {
        name: "gamma",
        challenges: challenging(&target.public_key()),
        extra: String::new(),
    };
    let bot = run_bot(&relay, &me, &spec).await.unwrap();

    // The session is resumed: our Ply at step 1 lands.
    let stored = wait_for(&relay, 20, |s| {
        s.iter().any(|e| {
            e["kind"] == 3423
                && e["pubkey"] == me.public_key().to_hex()
                && e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == "e" && t[1] == session.id.to_hex())
        })
    })
    .await;
    // No new challenge: a session with `target` is open, and the last
    // challenge to it is younger than `every_secs`; `other` is not a
    // target, and its challenge is pending.
    assert_eq!(of_kind(&stored, 3420, &me.public_key()).len(), 2);

    // `other` answers late but within accept_until, seating the bot
    // first: the session is followed — one over the blitz cap of 1 — and
    // the bot moves.
    let late = session_on(
        &pending,
        &other,
        me.public_key(),
        other.public_key(),
        relay.now().await,
    );
    inject(&relay, &late).await;
    let _ = wait_for(&relay, 20, |s| {
        s.iter().any(|e| {
            e["kind"] == 3423
                && e["pubkey"] == me.public_key().to_hex()
                && e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == "e" && t[1] == late.id.to_hex())
        })
    })
    .await;
    bot.stop().await;
}

#[tokio::test]
async fn a_persons_key_is_refused_at_start() {
    let relay = MiniRelay::start().await;
    let me = Keys::generate();
    let profile = EventBuilder::new(Kind::Metadata, r#"{"name":"cyril"}"#)
        .custom_created_at(Timestamp::from(relay.now().await - 10))
        .finalize(&me)
        .unwrap();
    inject(&relay, &profile).await;
    let spec = Spec {
        name: "delta",
        challenges: everyone(),
        extra: String::new(),
    };
    match run_bot(&relay, &me, &spec).await {
        Err(StartError::NotABotKey(refusal)) => {
            assert_eq!(
                refusal,
                sashite_sanki_bot::bot::adoption::Refusal::ProfileNotABot
            );
        }
        Ok(running) => {
            running.stop().await;
            panic!("a person's key was adopted");
        }
        Err(other) => panic!("{other}"),
    }
}

#[tokio::test]
async fn another_instance_of_the_library_halts_the_bot() {
    let relay = MiniRelay::start().await;
    let me = Keys::generate();
    let challenger = Keys::generate();
    let person = Keys::generate();
    let spec = Spec {
        name: "epsilon",
        challenges: everyone(),
        extra: String::new(),
    };
    let bot = run_bot(&relay, &me, &spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A game open before the halt: a person's challenge, founded by the
    // bot, the bot seated by the open seat rule; whoever is first moves.
    let now = relay.now().await;
    let first_challenge = challenge(&person, me.public_key(), now, 120, false);
    inject(&relay, &first_challenge).await;
    let stored = wait_for(&relay, 15, |s| {
        !of_kind(s, 3422, &me.public_key()).is_empty()
    })
    .await;
    let session = of_kind(&stored, 3422, &me.public_key())[0].clone();
    let session_id = session["id"].as_str().unwrap().to_owned();
    let bot_first = session["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t[0] == "seat" && t[1] == me.public_key().to_hex() && t[2] == "first");
    let plies_on = |s: &[serde_json::Value]| {
        of_kind(s, 3423, &me.public_key())
            .into_iter()
            .filter(|e| {
                e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == "e" && t[1] == session_id)
            })
            .count()
    };
    if bot_first {
        let _ = wait_for(&relay, 15, |s| plies_on(s) == 1).await;
    }

    // Another instance of the library: a challenge by the bot's key with
    // its tag, which this instance did not send, stamped past the watch.
    let now = relay.now().await;
    let echo = challenge(&me, challenger.public_key(), now + 6, 60, true);
    inject(&relay, &echo).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Halted: a challenge to the bot is not admitted (no founding within
    // far more than a founding takes), and the open game publishes
    // nothing more — the person moves, the bot does not answer.
    let incoming = challenge(&challenger, me.public_key(), now + 6, 60, false);
    inject(&relay, &incoming).await;
    let before = plies_on(&relay.stored().await);
    let content = if bot_first {
        // The bot's first Ply is on the relay; the person's reply must be
        // legal after it — any pawn push of the second player is.
        r#"["e7","e5",null]"#
    } else {
        r#"["e2","e4",null]"#
    };
    let reply = EventBuilder::new(Kind::Custom(3423), content)
        .tags(vec![
            Tag::parse(["e", &session_id, "", "game_session"]).unwrap(),
            Tag::parse(["p", &me.public_key().to_hex(), "", "opponent"]).unwrap(),
            Tag::parse(["step", "1"]).unwrap(),
            Tag::parse(["nonce", "0", "0"]).unwrap(),
        ])
        .custom_created_at(Timestamp::from(relay.now().await + 1))
        .finalize(&person)
        .unwrap();
    inject(&relay, &reply).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let stored = relay.stored().await;
    assert_eq!(
        of_kind(&stored, 3422, &me.public_key()).len(),
        1,
        "halted: no founding"
    );
    assert_eq!(plies_on(&stored), before, "halted: no Ply");
    bot.stop().await;
}

#[tokio::test]
async fn a_challenge_to_the_bot_is_founded_and_a_person_founding_is_followed() {
    let relay = MiniRelay::start().await;
    let me = Keys::generate();
    let challenger = Keys::generate();
    let other = Keys::generate();
    let spec = Spec {
        name: "zeta",
        challenges: everyone(),
        extra: String::new(),
    };
    let bot = run_bot(&relay, &me, &spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A challenge from a person's client (no library tag): founded.
    let now = relay.now().await;
    let incoming = challenge(&challenger, me.public_key(), now, 60, false);
    inject(&relay, &incoming).await;
    let stored = wait_for(&relay, 15, |s| {
        !of_kind(s, 3422, &me.public_key()).is_empty()
    })
    .await;
    let session = of_kind(&stored, 3422, &me.public_key())[0].clone();
    assert!(session["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t[0] == "e" && t[1] == incoming.id.to_hex()));
    // The same challenge delivered again: not founded twice.
    inject(&relay, &incoming).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let stored = relay.stored().await;
    assert_eq!(of_kind(&stored, 3422, &me.public_key()).len(), 1);

    // A person acting with the bot's key founds, from the app (no
    // library tag), a session on a challenge from `other`, seating the
    // bot first: the bot follows it — one over the blitz cap of 1 — and
    // moves.
    let others = challenge(&other, me.public_key(), relay.now().await, 60, false);
    inject(&relay, &others).await;
    // The bot itself admits `others`? No slot: the cap is 1 and the first
    // game is open. So it is the person who founds.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        of_kind(&relay.stored().await, 3422, &me.public_key()).len(),
        1,
        "no slot"
    );
    let by_person = session_on(
        &others,
        &me,
        me.public_key(),
        other.public_key(),
        relay.now().await,
    );
    inject(&relay, &by_person).await;
    let _ = wait_for(&relay, 15, |s| {
        of_kind(s, 3423, &me.public_key()).into_iter().any(|e| {
            e["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "e" && t[1] == by_person.id.to_hex())
        })
    })
    .await;
    bot.stop().await;
}
