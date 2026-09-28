// SPDX-License-Identifier: Apache-2.0
//! A recorded game replayed: the person's Plies injected as they were
//! (contents, tags, a promotion by capture), the bot's answered by a
//! scripted engine — the game of 2026-09-28 09:19 UTC on
//! `relay.sanki.app`, in which the bot stopped answering at the person's
//! eleventh Ply (the wake race of 0.2.0's changelog).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use nostr_sdk::prelude::*;
use sashite_sanki_client::session::{self, Seat};
use sashite_sanki_client::testing::{MiniRelay, Window};

const APP: [&str; 11] = [
    r#"["e2","e4",null]"#,
    r#"["d2","d4",null]"#,
    r#"["b1","c3",null]"#,
    r#"["g1","f3",null]"#,
    r#"["f1","d3",null]"#,
    r#"["e4","f5",null]"#,
    r#"["f5","g6",null]"#,
    r#"["g6","g7",null]"#,
    r#"["g7","h8","queen"]"#,
    r#"["h8","g8",null]"#,
    r#"["f3","e5",null]"#,
];

const SCRIPT: &str = r#"
import sys, json
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
HELLO = {"version": 1, "versions": [1], "engine": {"name": "recorded"},
         "rules": {"sashite.sanki.kernel/1": {}}, "features": {},
         "options": {}}
BOT = ["f7-f6", "b7-b6", "b8-c6", "g7-g6", "f6-f5", "e7-e6", "f8-a3", "c6-e5", "a3-f8", "e8-e7"]
for line in sys.stdin:
    r = json.loads(line); op = r["op"]; i = r["id"]
    if op == "hello": out({"re": i, "ev": "done", **HELLO})
    elif op in ("ping", "configure", "cancel"): out({"re": i, "ev": "done"})
    elif op == "search":
        n = len(r.get("moves", []))
        k = n // 2
        if k < len(BOT): out({"re": i, "ev": "done", "best": BOT[k]})
        else: out({"re": i, "ev": "done", "best": None})
"#;

#[tokio::test]
async fn the_recorded_game_is_answered_to_the_end() {
    let relay = Arc::new(MiniRelay::start().await);
    relay.set_window(Some(Window::REFERENCE)).await;
    let dir = std::env::temp_dir().join(format!("sanki-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("engine.py");
    std::fs::write(&script, SCRIPT).unwrap();
    let engine = format!(
        "[engine]\ncommand = \"python3\"\nargs = [\"{}\"]\ncwd = \"{}\"\nmax_relaunches_per_game = 1\nlaunch_ms = 5000\n",
        script.display(),
        std::env::temp_dir().display()
    );
    let me = Keys::generate();
    let spec = Spec::new("replay", everyone(), engine);
    let bot = run_bot(&relay, &me, &spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A person whose challenge seats them first: the bot's draw for an
    // open seat is its own (HMAC of the challenge's id), predicted here
    // from the same key — a challenge is signed until the draw says so.
    let identity = sashite_sanki_bot::identity::Identity::from_keys(me.clone());
    let person = Keys::generate();
    let challenge = loop {
        let challenge = challenge(&person, me.public_key(), relay.now().await, 120, false);
        if !identity.open_seat_first(&challenge.id) {
            break challenge;
        }
    };
    inject(&relay, &challenge).await;
    let cid = challenge.id.to_hex();
    let stored = wait_for(&relay, 15, |s| {
        of_kind(s, 3422, &me.public_key()).into_iter().any(|e| {
            e["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "e" && t[1] == cid)
        })
    })
    .await;
    let session = of_kind(&stored, 3422, &me.public_key())
        .into_iter()
        .find(|e| {
            e["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t[0] == "e" && t[1] == cid)
        })
        .unwrap();
    let session = Event::from_json(session.to_string()).unwrap();
    let terms = session::terms(&session, &challenge).unwrap();
    assert_eq!(terms.seat_of(&person.public_key()), Some(Seat::First));
    let id = session.id.to_hex();
    let bot_plies = |s: &[serde_json::Value]| {
        of_kind(s, 3423, &me.public_key())
            .into_iter()
            .filter(|e| {
                e["tags"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t[0] == "e" && t[1] == id)
            })
            .count()
    };
    for (index, content) in APP.iter().enumerate() {
        // The person moves three seconds after the bot's answer.
        let _ = wait_for(&relay, 30, |s| bot_plies(s) >= index).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let ply = EventBuilder::new(Kind::Custom(3423), *content)
            .tags(vec![
                Tag::parse(["e", &id, "", "game_session"]).unwrap(),
                Tag::parse(["p", &me.public_key().to_hex(), "", "opponent"]).unwrap(),
                Tag::parse(["step", &(index + 1).to_string()]).unwrap(),
                Tag::parse(["nonce", "0", "0"]).unwrap(),
            ])
            .custom_created_at(Timestamp::from(relay.now().await + 1))
            .finalize(&person)
            .unwrap();
        inject(&relay, &ply).await;
    }
    // The eleventh answer.
    let stored = wait_for(&relay, 40, |s| bot_plies(s) >= 11).await;
    assert_eq!(bot_plies(&stored), 11);
    bot.stop().await;
}
