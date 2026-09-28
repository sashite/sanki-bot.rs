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

mod common;

use std::time::Duration;

use common::*;
use nostr_sdk::prelude::*;
use sashite_sanki_bot::bot::StartError;
use sashite_sanki_client::publisher::CLIENT_TAG;
use sashite_sanki_client::testing::MiniRelay;

#[tokio::test]
async fn two_bots_challenge_each_other_and_play() {
    let relay = MiniRelay::start().await;
    let a = Keys::generate();
    let b = Keys::generate();
    let engine_dir = temp_dir("engine");
    // A challenges B, and resigns on its first turn; B answers everyone.
    let spec_a = Spec::new(
        "alpha",
        challenging(&b.public_key()),
        resigning_engine(&engine_dir),
    );
    let spec_b = Spec::new("beta", everyone(), String::new());
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

    let spec = Spec::new("gamma", challenging(&target.public_key()), String::new());
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
    let spec = Spec::new("delta", everyone(), String::new());
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
    let spec = Spec::new("epsilon", everyone(), String::new());
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
    let spec = Spec::new("zeta", everyone(), String::new());
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
