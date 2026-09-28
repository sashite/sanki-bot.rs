// SPDX-License-Identifier: Apache-2.0
//! The end-to-end cases of ADR-0045 §9 the other benches leave out: three
//! concurrent games at the capacity limit under the relay's rate limit; an
//! engine that dies at every turn; a clock skew under the relay's strict
//! window, small and large, both ways; a `kill -9` with a Ply in transit,
//! followed by an immediate restart.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use nostr_sdk::prelude::*;
use sashite_sanki_client::module::{self, native::Native};
use sashite_sanki_client::session::{self, Seat, SessionTerms};
use sashite_sanki_client::testing::{MiniRelay, Window};

/// An opponent that answers at once: it watches the session's Plies on the
/// relay, replays the chain as the kernel selects it (for each half-move,
/// the earliest legal Ply of the seat on move at the step the seat is at),
/// and injects the first legal move whenever the next half-move is its
/// seat's. Stops when told, or when the game ends.
fn instant_opponent(
    relay: Arc<MiniRelay>,
    keys: Keys,
    terms: SessionTerms,
    start: u64,
    seat: Seat,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let session_id = terms.id.to_hex();
        let mine = keys.public_key().to_hex();
        let mut sent_steps: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let stored = relay.stored().await;
            if stored.iter().any(|e| {
                e["kind"] == 3425
                    && e["tags"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t[0] == "e" && t[1] == session_id)
            }) {
                return;
            }
            let mut plies: Vec<&serde_json::Value> = stored
                .iter()
                .filter(|e| {
                    e["kind"] == 3423
                        && e["tags"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|t| t[0] == "e" && t[1] == session_id)
                })
                .collect();
            plies.sort_by_key(|e| {
                (
                    e["created_at"].as_u64().unwrap(),
                    e["id"].as_str().unwrap().to_owned(),
                )
            });
            let mut position = terms.position.clone();
            let mut half_moves = 0u32;
            // Never before t₀: the founder's stamp may be ahead of the
            // relay's clock, within its tolerance.
            let mut last_stamp = start;
            let mut used = vec![false; plies.len()];
            loop {
                let expected = if half_moves.is_multiple_of(2) {
                    terms.first.to_hex()
                } else {
                    terms.second.to_hex()
                };
                let step = (half_moves / 2 + 1).to_string();
                let mut applied = None;
                for (index, ply) in plies.iter().enumerate() {
                    if used[index] || ply["pubkey"] != expected || step_of(ply) != step {
                        continue;
                    }
                    let content = ply["content"].as_str().unwrap();
                    if let Ok(next) = module::apply(&mut Native, &position, content) {
                        applied = Some((index, next.position, ply["created_at"].as_u64().unwrap()));
                        break;
                    }
                }
                let Some((index, next, stamp)) = applied else {
                    break;
                };
                used[index] = true;
                position = next;
                half_moves += 1;
                last_stamp = stamp;
            }
            let on_move = if half_moves.is_multiple_of(2) {
                Seat::First
            } else {
                Seat::Second
            };
            let my_plies = plies.iter().filter(|e| e["pubkey"] == mine).count() as u32;
            if on_move != seat || my_plies != sent_steps {
                continue;
            }
            let Ok(legal) = module::legal_moves(&mut Native, &position) else {
                continue;
            };
            let Some(reply) = legal.first() else {
                return;
            };
            // Informed: at or after the last Ply's stamp (a stamp equal to
            // it is anterior to nothing).
            let stamp = (relay.now().await + 1).max(last_stamp);
            sent_steps += 1;
            let ply = EventBuilder::new(Kind::Custom(3423), reply.clone())
                .tags(vec![
                    Tag::parse(["e", &session_id, "", "game_session"]).unwrap(),
                    Tag::parse([
                        "p",
                        &terms.opponent_of(&keys.public_key()).unwrap().to_hex(),
                        "",
                        "opponent",
                    ])
                    .unwrap(),
                    Tag::parse(["step", &sent_steps.to_string()]).unwrap(),
                    Tag::parse(["nonce", "0", "0"]).unwrap(),
                ])
                .custom_created_at(Timestamp::from(stamp))
                .finalize(&keys)
                .unwrap();
            inject(&relay, &ply).await;
        }
    })
}

fn step_of(ply: &serde_json::Value) -> String {
    ply["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t[0] == "step")
        .map(|t| t[1].as_str().unwrap().to_owned())
        .unwrap_or_default()
}

/// The bot's Plies on `session`, by step.
fn bot_plies(stored: &[serde_json::Value], me: &PublicKey, session: &str) -> Vec<u64> {
    let mut steps: Vec<u64> = of_kind(stored, 3423, me)
        .into_iter()
        .filter(|e| {
            e["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "e" && t[1] == session)
        })
        .map(|e| step_of(e).parse().unwrap())
        .collect();
    steps.sort_unstable();
    steps
}

/// The session's Plies' stamps, by `(pubkey, step)`.
fn stamps_by_step(
    stored: &[serde_json::Value],
    session: &str,
) -> std::collections::HashMap<(String, u64), u64> {
    stored
        .iter()
        .filter(|e| {
            e["kind"] == 3423
                && e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == "e" && t[1] == session)
        })
        .map(|e| {
            (
                (
                    e["pubkey"].as_str().unwrap().to_owned(),
                    step_of(e).parse().unwrap(),
                ),
                e["created_at"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// A person's challenge founded by the bot: the session and its terms, and
/// the seat the person got.
async fn founded(
    relay: &MiniRelay,
    me: &PublicKey,
    person: &Keys,
    accept_secs: u64,
) -> (Event, SessionTerms, Seat) {
    let challenge = challenge(person, *me, relay.now().await, accept_secs, false);
    inject(relay, &challenge).await;
    let id = challenge.id.to_hex();
    let stored = wait_for(relay, 15, |s| {
        of_kind(s, 3422, me).into_iter().any(|e| {
            e["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "e" && t[1] == id)
        })
    })
    .await;
    let session = of_kind(&stored, 3422, me)
        .into_iter()
        .find(|e| {
            e["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "e" && t[1] == id)
        })
        .unwrap();
    let session = Event::from_json(session.to_string()).unwrap();
    let terms = session::terms(&session, &challenge).unwrap();
    let seat = terms.seat_of(&person.public_key()).unwrap();
    (session, terms, seat)
}

/// The rejections the relay answered the bot.
async fn bot_rejections(relay: &MiniRelay, me: &PublicKey) -> Vec<String> {
    relay
        .rejected()
        .await
        .into_iter()
        .filter(|(pubkey, _, _)| *pubkey == me.to_hex())
        .map(|(_, _, reason)| reason)
        .collect()
}

#[tokio::test]
async fn three_games_at_the_capacity_limit_are_paced_and_meet_no_rate_limit() {
    let relay = Arc::new(MiniRelay::start().await);
    // The reference relay's rate limit, at the configured rate: with three
    // blitz games the capacity inequality is tight (3 × 36 = 108 = 120 −
    // 12). Instant opponents would have the bot answer several times a
    // second; the pace floor is what keeps it under the limit.
    relay.set_rate_limit(Some((120, 60))).await;
    let me = Keys::generate();
    let mut spec = Spec::new("cap", everyone(), String::new());
    spec.blitz = 3;
    let bot = run_bot(&relay, &me, &spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let stop = Arc::new(AtomicBool::new(false));
    let mut opponents = Vec::new();
    let mut sessions = Vec::new();
    for _ in 0..3 {
        let person = Keys::generate();
        let (session, terms, seat) = founded(&relay, &me.public_key(), &person, 120).await;
        sessions.push((
            session.id.to_hex(),
            session.created_at.as_secs(),
            terms.first == me.public_key(),
            person.public_key().to_hex(),
        ));
        opponents.push(instant_opponent(
            Arc::clone(&relay),
            person,
            terms,
            session.created_at.as_secs(),
            seat,
            Arc::clone(&stop),
        ));
    }
    // A fourth person: no slot.
    let fourth = challenge(
        &Keys::generate(),
        me.public_key(),
        relay.now().await,
        120,
        false,
    );
    inject(&relay, &fourth).await;

    // Twelve Plies of the bot's in each game: half a minute at the pace.
    let stored = wait_for(&relay, 60, |s| {
        sessions
            .iter()
            .all(|(id, _, _, _)| bot_plies(s, &me.public_key(), id).len() >= 12)
    })
    .await;
    assert_eq!(
        of_kind(&stored, 3422, &me.public_key()).len(),
        3,
        "no fourth founding"
    );
    // No rejection at all: never rate-limited.
    assert_eq!(
        bot_rejections(&relay, &me.public_key()).await,
        Vec::<String>::new()
    );
    for (id, start, bot_first, opponent) in &sessions {
        // One content per step.
        let steps = bot_plies(&stored, &me.public_key(), id);
        let mut unique = steps.clone();
        unique.dedup();
        assert_eq!(steps, unique, "two Plies for one step");
        // The pace floor: every Ply of the bot's at least `min_move_secs`
        // after its anchor — the opponent's Ply of the half-move before,
        // or t₀ for the first player's first.
        let stamps = stamps_by_step(&stored, id);
        for step in &steps {
            let anchor = if *bot_first {
                if *step == 1 {
                    *start
                } else {
                    stamps[&(opponent.clone(), step - 1)]
                }
            } else {
                stamps[&(opponent.clone(), *step)]
            };
            let at = stamps[&(me.public_key().to_hex(), *step)];
            assert!(
                at >= anchor + 2,
                "paced: step {step} at {at}, anchor {anchor}"
            );
        }
    }
    stop.store(true, Ordering::Relaxed);
    for o in opponents {
        let _ = o.await;
    }
    bot.stop().await;
}

/// An engine that answers its first search then dies: `e2-e4` — legal on
/// the bot's first turn as first player, illegal otherwise — then exits.
/// Every launch appends a line to the file its argument names.
const DYING: &str = r#"
import sys, json, os
with open(sys.argv[1], "a") as f:
    f.write("launch\n")
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
HELLO = {"version": 1, "versions": [1], "engine": {"name": "dying"},
         "rules": {"sashite.sanki.kernel/1": {}}, "features": {},
         "options": {"seed": {"type": "int", "default": 0, "min": 0, "max": 1000}}}
for line in sys.stdin:
    r = json.loads(line); op = r["op"]; i = r["id"]
    if op == "hello": out({"re": i, "ev": "done", **HELLO})
    elif op in ("ping", "configure", "cancel"): out({"re": i, "ev": "done"})
    elif op == "search":
        out({"re": i, "ev": "done", "best": "e2-e4"})
        os._exit(0)
"#;

fn dying_engine(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    let script = dir.join("dying.py");
    let launches = dir.join("launches.log");
    std::fs::write(&script, DYING).unwrap();
    (
        format!(
            "[engine]\ncommand = \"python3\"\nargs = [\"{}\", \"{}\"]\ncwd = \"{}\"\noptions = {{ seed = 1 }}\nmax_relaunches_per_game = 1\nlaunch_ms = 5000\n",
            script.display(),
            launches.display(),
            std::env::temp_dir().display()
        ),
        launches,
    )
}

#[tokio::test]
async fn an_engine_dying_at_every_turn_is_relaunched_once_then_fallbacks_play() {
    let relay = Arc::new(MiniRelay::start().await);
    let me = Keys::generate();
    let dir = temp_dir("dying");
    let (engine, launches) = dying_engine(&dir);
    let mut spec = Spec::new("dying", everyone(), engine);
    spec.blitz = 2;
    let bot = run_bot(&relay, &me, &spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let stop = Arc::new(AtomicBool::new(false));
    let mut opponents = Vec::new();
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let person = Keys::generate();
        let (session, terms, seat) = founded(&relay, &me.public_key(), &person, 120).await;
        sessions.push(session.id.to_hex());
        opponents.push(instant_opponent(
            Arc::clone(&relay),
            person,
            terms,
            session.created_at.as_secs(),
            seat,
            Arc::clone(&stop),
        ));
    }
    // Both games go on: five Plies of the bot's each, every step once.
    let stored = wait_for(&relay, 40, |s| {
        sessions
            .iter()
            .all(|id| bot_plies(s, &me.public_key(), id).len() >= 5)
    })
    .await;
    for id in &sessions {
        let steps = bot_plies(&stored, &me.public_key(), id);
        assert_eq!(steps, (1..=steps.len() as u64).collect::<Vec<_>>(), "{id}");
    }
    // The engine was launched for the probe, then once per game at its
    // opening and once more (`max_relaunches_per_game = 1`) — never again:
    // 1 + 2 × 2.
    let launched = std::fs::read_to_string(&launches).unwrap().lines().count();
    assert_eq!(launched, 5, "launches");
    stop.store(true, Ordering::Relaxed);
    for o in opponents {
        let _ = o.await;
    }
    bot.stop().await;
}

/// The bot plays under a relay clock `skew` seconds off the host's, with
/// the relay's strict window on; `rejections` is how many timing
/// rejections the bot must meet — its first timed event when the skew is
/// beyond the window, none otherwise — before its stamps are right.
async fn plays_under_skew(name: &str, skew: i64, rejections: usize) {
    let relay = Arc::new(MiniRelay::start().await);
    relay.set_window(Some(Window::REFERENCE)).await;
    relay.set_skew(skew).await;
    let me = Keys::generate();
    let spec = Spec::new(name, everyone(), String::new());
    let bot = run_bot(&relay, &me, &spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let person = Keys::generate();
    let (session, terms, seat) = founded(&relay, &me.public_key(), &person, 120).await;
    let stop = Arc::new(AtomicBool::new(false));
    let opponent = instant_opponent(
        Arc::clone(&relay),
        person,
        terms,
        session.created_at.as_secs(),
        seat,
        Arc::clone(&stop),
    );
    let id = session.id.to_hex();
    let stored = wait_for(&relay, 30, |s| {
        bot_plies(s, &me.public_key(), &id).len() >= 3
    })
    .await;
    // Every step once, none lost to a rejection.
    assert_eq!(bot_plies(&stored, &me.public_key(), &id), vec![1, 2, 3]);
    // The relay's clock learnt from the first rejection's wording: exactly
    // that many, then every stamp within the window.
    let met = bot_rejections(&relay, &me.public_key()).await;
    assert_eq!(met.len(), rejections, "{met:?}");
    stop.store(true, Ordering::Relaxed);
    let _ = opponent.await;
    bot.stop().await;
}

#[tokio::test]
async fn the_bot_plays_with_the_relay_three_seconds_ahead() {
    // The first stamp is two seconds in the relay's past: one rejection.
    plays_under_skew("ahead3", 3, 1).await;
}

#[tokio::test]
async fn the_bot_plays_with_the_relay_three_seconds_behind() {
    // The stamps are four seconds in the relay's future: within tolerance.
    plays_under_skew("behind3", -3, 0).await;
}

#[tokio::test]
async fn the_bot_plays_with_the_relay_ten_seconds_ahead() {
    plays_under_skew("ahead10", 10, 1).await;
}

#[tokio::test]
async fn the_bot_plays_with_the_relay_ten_seconds_behind() {
    plays_under_skew("behind10", -10, 1).await;
}

#[tokio::test]
async fn a_kill_with_a_ply_in_transit_and_an_immediate_restart_repeat_nothing() {
    let relay = Arc::new(MiniRelay::start().await);
    let me = Keys::generate();
    let target = Keys::generate();
    let person = Keys::generate();
    let mut spec = Spec::new("phoenix", challenging(&target.public_key()), String::new());
    // Three slots: the open game, the pending challenge to the target, and
    // the founding that proves the second instance is not halted.
    spec.blitz = 3;
    spec.rate = 130;
    // The quarantine, longer than the relay's transit: what makes the
    // second instance see the Ply the first sent as it died.
    spec.quarantine = Duration::from_secs(4);

    // The first instance: a challenge sent, a person's challenge founded,
    // a Ply played.
    let first = run_bot(&relay, &me, &spec).await.unwrap();
    let _ = wait_for(&relay, 15, |s| {
        !of_kind(s, 3420, &me.public_key()).is_empty()
    })
    .await;
    let (session, terms, seat) = founded(&relay, &me.public_key(), &person, 120).await;
    let stop = Arc::new(AtomicBool::new(false));
    let opponent = instant_opponent(
        Arc::clone(&relay),
        person,
        terms,
        session.created_at.as_secs(),
        seat,
        Arc::clone(&stop),
    );
    let id = session.id.to_hex();
    let _ = wait_for(&relay, 20, |s| {
        !bot_plies(s, &me.public_key(), &id).is_empty()
    })
    .await;

    // The next Ply of the bot's stays in transit for three seconds; the
    // bot is killed as soon as it has sent it, before the relay stored it.
    relay.set_store_delay(Some(Duration::from_secs(3))).await;
    let sent_by_me = |received: Vec<sashite_sanki_client::testing::ReceivedEvent>| {
        received
            .into_iter()
            .filter(|r| r.kind == 3423 && r.pubkey == me.public_key().to_hex())
            .count()
    };
    let sent_before = sent_by_me(relay.received().await);
    for _ in 0..200 {
        if sent_by_me(relay.received().await) > sent_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    first.kill();
    relay.set_store_delay(None).await;
    let landed = bot_plies(&relay.stored().await, &me.public_key(), &id).len();
    assert_eq!(landed, sent_before, "the Ply is in transit, not stored");
    let second = run_bot(&relay, &me, &spec).await.unwrap();

    // The game goes on from the relay: the Ply in transit landed during
    // the quarantine, and two more of the bot's follow.
    let stored = wait_for(&relay, 30, |s| {
        bot_plies(s, &me.public_key(), &id).len() >= landed + 3
    })
    .await;
    // Nothing repeated: one challenge to the target, one founding, one Ply
    // per step, every step once.
    assert_eq!(of_kind(&stored, 3420, &me.public_key()).len(), 1);
    assert_eq!(of_kind(&stored, 3422, &me.public_key()).len(), 1);
    let steps = bot_plies(&stored, &me.public_key(), &id);
    assert_eq!(steps, (1..=steps.len() as u64).collect::<Vec<_>>());
    // Not halted: another person's challenge is founded.
    let another = Keys::generate();
    let _ = founded(&relay, &me.public_key(), &another, 120).await;
    stop.store(true, Ordering::Relaxed);
    let _ = opponent.await;
    second.stop().await;
}
