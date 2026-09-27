// SPDX-License-Identifier: Apache-2.0
//! The start's reads over the in-process relay: adoption (ADR-0045 §2),
//! the standing events (§7) and the rebuild (§7, step 7).

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
use sashite_sanki_bot::bot::adoption::{self, Adoption, Refusal};
use sashite_sanki_bot::bot::rebuild::{self, Scope};
use sashite_sanki_bot::bot::standing;
use sashite_sanki_bot::config::Config;
use sashite_sanki_bot::game::SharedOracle;
use sashite_sanki_bot::identity::Identity;
use sashite_sanki_client::drafts::{self, Publishable};
use sashite_sanki_client::module::{native::Native, Oracle};
use sashite_sanki_client::publisher::{Publisher, Settings, CLIENT_TAG};
use sashite_sanki_client::readers::Founding;
use sashite_sanki_client::session::fixtures::{World, CHESS_CHESS};
use sashite_sanki_client::testing::MiniRelay;

const RULES: &str = "7777777777777777777777777777777777777777777777777777777777777777";

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sanki-bot-start-{}-{}",
        std::process::id(),
        Timestamp::now().as_secs()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The relay the configuration designates: the in-process relay speaks
/// `ws://`, which no conforming event may name, so the events name this
/// one and the client connects to the other.
const DESIGNATED: &str = "wss://relay.example.com";

fn config(data_dir: &std::path::Path, relay: &str, challenges: &str) -> Config {
    Config::from_toml(&format!(
        r#"
schema = 1
[connection]
relay = "{relay}"
rules = "{RULES}"
data_dir = "{}"
rate_per_minute = 120
[identity]
file = "{}/key.nsec"
[profile]
name = "kitsune"
about = "a bot"
picture = "https://example.com/k.png"
nip05 = "kitsune@example.com"
{challenges}
[play]
variants = ["chess"]
preferred = "chess"
opponents = ["chess"]
min_move_secs = 2
margin_ms = 300
[play.max_concurrent]
blitz = 2
"#,
        data_dir.display(),
        data_dir.display(),
    ))
    .unwrap()
}

const EVERYONE: &str = "[challenges]\npolicy = \"everyone\"\nblocks = []\n";

struct Bench {
    relay: MiniRelay,
    relay_url: RelayUrl,
    world: World,
    client: Client,
    publisher: Arc<Publisher<Identity>>,
    config: Config,
    me: PublicKey,
}

async fn bench(challenges: &str) -> Bench {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let relay = MiniRelay::start().await;
    let data_dir = temp_dir();
    let mut world = World::new();
    world.time_control = vec!["180".to_owned(), "2".to_owned()];
    let identity = Identity::from_keys(world.first.clone());
    let me = identity.public_key();
    let client = Client::builder().build();
    client.add_relay(&relay.url).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    let relay_url = RelayUrl::parse(&relay.url).unwrap();
    let settings = Settings {
        relay: relay_url.clone(),
        rate_per_minute: 120,
        data_dir: data_dir.clone(),
        pow: 0,
        past_tolerance: 1,
        future_tolerance: 5,
    };
    let publisher = Arc::new(
        Publisher::open(client.clone(), identity, settings)
            .await
            .unwrap(),
    );
    let config = config(&data_dir, DESIGNATED, challenges);
    Bench {
        relay,
        relay_url,
        world,
        client,
        publisher,
        config,
        me,
    }
}

/// Signs an event of `kind` by `keys` at `at`, with `tags` and `content`.
fn signed(keys: &Keys, kind: u16, at: u64, tags: Vec<Tag>, content: &str) -> Event {
    EventBuilder::new(Kind::Custom(kind), content)
        .tags(tags)
        .custom_created_at(Timestamp::from(at))
        .finalize(keys)
        .unwrap()
}

async fn inject(bench: &Bench, event: &Event) {
    bench
        .relay
        .inject(serde_json::from_str(&event.as_json()).unwrap())
        .await;
}

fn client_tag() -> Tag {
    Tag::custom("client", [CLIENT_TAG.to_owned()])
}

// ---- adoption ----

#[tokio::test]
async fn a_persons_key_is_refused_a_bots_adopted() {
    let bench = bench(EVERYONE).await;
    let now = bench.publisher.now();
    let check = || adoption::check(&bench.client, &bench.relay_url, bench.me);
    assert_eq!(check().await, Adoption::Adopted, "an empty relay");

    // A profile without `bot: true`.
    let person = signed(
        &bench.world.first,
        0,
        now - 10,
        vec![],
        r#"{"name":"cyril"}"#,
    );
    inject(&bench, &person).await;
    assert_eq!(check().await, Adoption::Refused(Refusal::ProfileNotABot));
    // A newer profile with it: adopted again (the newest wins).
    let bot = signed(
        &bench.world.first,
        0,
        now - 5,
        vec![client_tag()],
        r#"{"name":"kitsune","bot":true}"#,
    );
    inject(&bench, &bot).await;
    assert_eq!(check().await, Adoption::Adopted);

    // A contact list the library did not write.
    let contacts = signed(&bench.world.first, 3, now - 5, vec![], "");
    inject(&bench, &contacts).await;
    assert_eq!(check().await, Adoption::Refused(Refusal::ContactsNotOurs));
    let ours = signed(&bench.world.first, 3, now - 4, vec![client_tag()], "");
    inject(&bench, &ours).await;
    assert_eq!(check().await, Adoption::Adopted);

    // A mute list the library did not write.
    let mutes = signed(&bench.world.first, 10000, now - 5, vec![], "");
    inject(&bench, &mutes).await;
    assert_eq!(check().await, Adoption::Refused(Refusal::MuteListNotOurs));

    // A silent relay proves nothing.
    bench.relay.set_silent(true).await;
    assert_eq!(check().await, Adoption::Unknown(0));
}

// ---- standing events ----

fn stored_kinds(stored: &[serde_json::Value], pubkey: &PublicKey) -> Vec<u64> {
    stored
        .iter()
        .filter(|e| e["pubkey"] == pubkey.to_hex())
        .map(|e| e["kind"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn standing_events_are_published_only_where_they_differ() {
    let bench = bench(EVERYONE).await;
    let reconcile =
        || standing::reconcile(&bench.client, &bench.publisher, &bench.config, bench.me);

    // An empty relay: the profile and the policy; no contact list under
    // `everyone`, no mute list for an empty `blocks`.
    let first = reconcile().await.unwrap();
    assert_eq!(first.published, vec![0, 30420]);
    let stored = bench.relay.stored().await;
    assert_eq!(stored_kinds(&stored, &bench.me), vec![0, 30420]);
    let profile = stored.iter().find(|e| e["kind"] == 0).unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(profile["content"].as_str().unwrap()).unwrap();
    assert_eq!(metadata["bot"], true);
    assert_eq!(metadata["nip05"], "kitsune@example.com");
    assert!(profile["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t[0] == "client" && t[1] == CLIENT_TAG));
    let policy = stored.iter().find(|e| e["kind"] == 30420).unwrap();
    assert!(policy["tags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t[0] == "mode" && t[1] == "everyone"));

    // The same again: nothing.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = reconcile().await.unwrap();
    assert!(second.published.is_empty(), "{second:?}");

    // A profile edited by hand, newer (ahead of the bot's own stamp),
    // without the tag: replaced.
    let now = bench.publisher.now();
    let edited = signed(
        &bench.world.first,
        0,
        now + 2,
        vec![],
        r#"{"name":"kitsune","about":"a bot","picture":"https://example.com/k.png","bot":true,"nip05":"kitsune@example.com","website":"https://x"}"#,
    );
    inject(&bench, &edited).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let third = reconcile().await.unwrap();
    assert_eq!(third.published, vec![0]);

    // A non-empty mute list on the relay, an empty `blocks`: the empty list
    // replaces it.
    let muted = Keys::generate().public_key();
    let mutes = signed(
        &bench.world.first,
        10000,
        bench.publisher.now() + 2,
        vec![client_tag(), Tag::public_key(muted)],
        "",
    );
    inject(&bench, &mutes).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let fourth = reconcile().await.unwrap();
    assert_eq!(fourth.published, vec![10000]);

    // A copy stamped beyond the relay's future tolerance (a hand edit from
    // a wrong clock): not replaceable, not waited for.
    let far = signed(
        &bench.world.first,
        0,
        bench.publisher.now() + 100,
        vec![],
        r#"{"name":"someone"}"#,
    );
    inject(&bench, &far).await;
    let fifth = reconcile().await.unwrap();
    assert!(fifth.published.is_empty(), "{fifth:?}");
    assert_eq!(fifth.unreplaceable, vec![0]);

    // A silent relay fails the start.
    bench.relay.set_silent(true).await;
    assert_eq!(
        reconcile().await,
        Err(standing::StandingError::UnknownRead(0))
    );
}

#[tokio::test]
async fn following_publishes_the_contact_list_and_blocks_the_mute_list() {
    let follow = Keys::generate().public_key();
    let block = Keys::generate().public_key();
    let challenges = format!(
        "[challenges]\npolicy = \"following\"\nfollows = [\"{}\"]\nblocks = [\"{}\"]\n",
        follow.to_hex(),
        block.to_hex()
    );
    let bench = bench(&challenges).await;
    let first = standing::reconcile(&bench.client, &bench.publisher, &bench.config, bench.me)
        .await
        .unwrap();
    assert_eq!(first.published, vec![0, 30420, 3, 10000]);
    let stored = bench.relay.stored().await;
    let contacts = stored.iter().find(|e| e["kind"] == 3).unwrap();
    let ps: Vec<_> = contacts["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t[0] == "p")
        .map(|t| t[1].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ps, vec![follow.to_hex()]);
    let mutes = stored.iter().find(|e| e["kind"] == 10000).unwrap();
    let muted: Vec<_> = mutes["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t[0] == "p")
        .map(|t| t[1].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(muted, vec![block.to_hex()]);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = standing::reconcile(&bench.client, &bench.publisher, &bench.config, bench.me)
        .await
        .unwrap();
    assert!(second.published.is_empty(), "{second:?}");
}

// ---- rebuild ----

/// A Direct Challenge by `challenger` to `target`, stamped `at`.
fn challenge(challenger: &Keys, target: PublicKey, at: u64, accept_secs: u64) -> Event {
    let draft = drafts::DirectChallenge {
        me: challenger.public_key(),
        target,
        game: "sanki".to_owned(),
        rules: EventId::from_hex(RULES).unwrap(),
        timing_relay: DESIGNATED.to_owned(),
        time_control: vec![[Some(180), Some(2), None]],
        variant: "chess".to_owned(),
        accept_secs: NonZeroU64::new(accept_secs).unwrap(),
        not_after: None,
        content: String::new(),
    };
    let parts = draft.at(at, DESIGNATED).unwrap();
    let mut tags = parts.tags;
    tags.push(Tag::parse(["nonce", "0", "0"]).unwrap());
    signed(challenger, 3420, at, tags, &parts.content)
}

/// The Game Session `founder` founds on `challenge`, at `at`.
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
    signed(founder, 3422, at, parts.tags, &parts.content)
}

#[tokio::test]
async fn the_rebuild_sorts_the_bots_sessions_and_challenges() {
    let bench = bench(EVERYONE).await;
    let now = bench.publisher.now();
    let me = bench.me;
    let target = Keys::generate();

    // 1. A session the bot founded on a Pairing, with two Plies: open.
    let pairing = bench.world.pairing();
    let open = bench.world.session(&pairing, now - 100);
    let ply1 = bench.world.ply(
        &open.id,
        &bench.world.first,
        1,
        r#"["e2","e4",null]"#,
        now - 90,
    );
    let ply2 = bench.world.ply(
        &open.id,
        &bench.world.second,
        1,
        r#"["e7","e5",null]"#,
        now - 80,
    );
    for e in [&pairing, &open, &ply1, &ply2] {
        inject(&bench, e).await;
    }

    // 2. A session founded on a challenge to the bot, concluded by the
    // opponent's resignation: closed.
    let to_me = challenge(&bench.world.second, me, now - 300, 60);
    let closed = session_on(
        &to_me,
        &bench.world.first,
        me,
        bench.world.second.public_key(),
        now - 290,
    );
    let resignation = bench.world.conclusion(
        &closed.id,
        &bench.world.second,
        "resignation",
        100,
        0,
        now - 280,
    );
    for e in [&to_me, &closed, &resignation] {
        inject(&bench, e).await;
    }

    // 3. A challenge of the bot's, answered by the target: open, founded by
    // the target (the bot second).
    let answered = challenge(&bench.world.first, target.public_key(), now - 200, 120);
    let by_target = session_on(&answered, &target, target.public_key(), me, now - 190);
    // 4. A challenge of the bot's, unanswered, pending.
    let pending = challenge(&bench.world.first, target.public_key(), now - 30, 120);
    // 5. A challenge of the bot's, lapsed beyond the tolerance.
    let lapsed = challenge(
        &bench.world.first,
        Keys::generate().public_key(),
        now - 130,
        60,
    );
    for e in [&answered, &by_target, &pending, &lapsed] {
        inject(&bench, e).await;
    }

    // 6. A challenge to the bot within `accept_until`: incoming; one past
    // it: not.
    let fresh = challenge(&target, me, now - 5, 120);
    let stale = challenge(&target, me, now - 500, 60);
    for e in [&fresh, &stale] {
        inject(&bench, e).await;
    }

    // 7. A second session of the bot's on the open Pairing, later (a
    // person founding again with the key): the earliest is the one.
    let again = bench.world.session(&pairing, now - 50);
    inject(&bench, &again).await;
    // 8. A challenge of the bot's answered by a stranger, and one answered
    // by the target after `accept_until`: the sessions are ignored, the
    // challenges still pending.
    let stranger = Keys::generate();
    let mis_answered = challenge(&bench.world.first, target.public_key(), now - 20, 120);
    let by_stranger = session_on(
        &mis_answered,
        &stranger,
        stranger.public_key(),
        me,
        now - 10,
    );
    let late_answered = challenge(
        &bench.world.first,
        Keys::generate().public_key(),
        now - 25,
        20,
    );
    let late_target = Keys::generate();
    let late = session_on(
        &late_answered,
        &late_target,
        late_target.public_key(),
        me,
        now - 3,
    );
    for e in [&mis_answered, &by_stranger, &late_answered, &late] {
        inject(&bench, e).await;
    }
    // 9. A session of the bot's under other rules: not ours.
    let mut other_rules = World::new();
    other_rules.first = bench.world.first.clone();
    other_rules.rules = EventId::from_hex(&"8".repeat(64)).unwrap();
    let other_pairing = other_rules.pairing();
    let other_session = other_rules.session(&other_pairing, now - 60);
    for e in [&other_pairing, &other_session] {
        inject(&bench, e).await;
    }

    let scope = Scope {
        me,
        config: &bench.config,
        relay: &bench.relay_url,
        max_step: 300,
        now,
        past_tolerance: 1,
    };
    let oracle: SharedOracle = Arc::new(Mutex::new(Box::new(Native) as Box<dyn Oracle + Send>));
    let rebuilt = rebuild::rebuild(&bench.client, &scope, &oracle)
        .await
        .unwrap();

    let mut open_ids: Vec<EventId> = rebuilt.open.iter().map(|s| s.session.id).collect();
    open_ids.sort();
    let mut wanted = vec![open.id, by_target.id];
    wanted.sort();
    assert_eq!(
        open_ids, wanted,
        "not `again`, not the stranger's, not the late one, not other rules"
    );
    assert_eq!(rebuilt.closed, 1);
    assert_eq!(rebuilt.unverified, 0);
    let ours = rebuilt
        .open
        .iter()
        .find(|s| s.session.id == open.id)
        .unwrap();
    assert_eq!(ours.events.len(), 2);
    assert_eq!(ours.start, now - 100);
    assert_eq!(ours.cadence, sashite_sanki_client::cadence::Cadence::Blitz);
    let theirs = rebuilt
        .open
        .iter()
        .find(|s| s.session.id == by_target.id)
        .unwrap();
    assert_eq!(theirs.terms.second, me);
    assert_eq!(theirs.founding.id, answered.id);

    let mut pending_ids: Vec<EventId> = rebuilt.pending.iter().map(|c| c.event.id).collect();
    pending_ids.sort();
    let mut wanted = vec![pending.id, mis_answered.id];
    wanted.sort();
    assert_eq!(
        pending_ids, wanted,
        "`late_answered` lapsed beyond L + skew"
    );
    assert_eq!(rebuilt.sent_last_day, 5);
    assert_eq!(rebuilt.last_sent[&target.public_key()], now - 20);

    let incoming: Vec<EventId> = rebuilt.incoming.iter().map(|(e, _)| e.id).collect();
    assert_eq!(incoming, vec![fresh.id]);

    // A silent relay fails the rebuild.
    bench.relay.set_silent(true).await;
    assert!(rebuild::rebuild(&bench.client, &scope, &oracle)
        .await
        .is_err());
}
