// SPDX-License-Identifier: Apache-2.0
//! The bench the end-to-end tests share: a bot over the in-process relay,
//! its configuration from a short spec, signed events as a person's client
//! or the library would write them, and the polling helpers.

#![allow(
    dead_code,
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

pub const RULES: &str = "7777777777777777777777777777777777777777777777777777777777777777";
pub const DESIGNATED: &str = "wss://relay.example.com";

pub fn temp_dir(tag: &str) -> PathBuf {
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
pub const RESIGNING: &str = r#"
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

pub fn resigning_engine(dir: &std::path::Path) -> String {
    let script = dir.join("engine.py");
    std::fs::write(&script, RESIGNING).unwrap();
    format!(
        "[engine]\ncommand = \"python3\"\nargs = [\"{}\"]\ncwd = \"{}\"\noptions = {{ seed = 1 }}\nmax_relaunches_per_game = 1\nlaunch_ms = 5000\n[play.resign]\nmax_win = 20\nmin_loss = 900\nstreak = 1\n",
        script.display(),
        std::env::temp_dir().display()
    )
}

pub struct Spec<'a> {
    pub name: &'a str,
    pub challenges: String,
    pub extra: String,
    /// The blitz cap.
    pub blitz: u32,
    /// The relay's rate, per minute.
    pub rate: u32,
    /// The quarantine (a test's: usually none).
    pub quarantine: Duration,
}

impl<'a> Spec<'a> {
    pub fn new(name: &'a str, challenges: String, extra: String) -> Self {
        Self {
            name,
            challenges,
            extra,
            blitz: 1,
            rate: 120,
            quarantine: Duration::ZERO,
        }
    }
}

pub fn config(data_dir: &std::path::Path, spec: &Spec<'_>) -> Config {
    Config::from_toml(&format!(
        r#"
schema = 1
[connection]
relay = "{DESIGNATED}"
rules = "{RULES}"
data_dir = "{}"
rate_per_minute = {}
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
blitz = {}
"#,
        data_dir.display(),
        spec.rate,
        data_dir.display(),
        spec.name,
        spec.challenges,
        spec.extra,
        spec.blitz,
    ))
    .unwrap()
}

pub struct Running {
    pub stop: Option<oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<()>,
}

impl Running {
    /// `kill -9`: the task aborted, nothing told to stop. (The publisher's
    /// sends in flight finish on their own, as a socket's bytes do; the
    /// lease follows the last of them.)
    pub fn kill(self) {
        self.task.abort();
        // Never resolve the stop: the runtime must not stop gracefully.
        std::mem::forget(self.stop);
    }

    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the bot stops")
            .unwrap();
    }
}

pub async fn prepared(relay: &MiniRelay, keys: &Keys, spec: &Spec<'_>) -> Prepared {
    let data_dir = temp_dir(spec.name);
    let config = Arc::new(config(&data_dir, spec));
    let identity = Identity::from_keys(keys.clone());
    // The lease: after a kill, the previous instance's releases it once
    // its tasks are gone — within a moment.
    let mut lease = Lease::take(&data_dir, identity.public_key());
    for _ in 0..100 {
        if lease.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        lease = Lease::take(&data_dir, identity.public_key());
    }
    let lease = lease.unwrap();
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
        quarantine: Some(spec.quarantine),
    }
}

/// Starts a bot and runs it until stopped.
pub async fn run_bot(
    relay: &MiniRelay,
    keys: &Keys,
    spec: &Spec<'_>,
) -> Result<Running, StartError> {
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

pub fn everyone() -> String {
    "[challenges]\npolicy = \"everyone\"\nblocks = []\n".to_owned()
}

pub fn challenging(target: &PublicKey) -> String {
    format!(
        "[challenges]\npolicy = \"everyone\"\nblocks = []\n[challenges.outgoing]\ntargets = [\"{}\"]\ntime_control = [[60, 2]]\nvariant = \"chess\"\nevery_secs = 600\naccept_secs = 30\nmax_per_day = 5\n",
        target.to_hex()
    )
}

pub async fn wait_for<F: Fn(&[serde_json::Value]) -> bool>(
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

pub fn of_kind<'a>(
    stored: &'a [serde_json::Value],
    kind: u64,
    pubkey: &PublicKey,
) -> Vec<&'a serde_json::Value> {
    stored
        .iter()
        .filter(|e| e["kind"] == kind && e["pubkey"] == pubkey.to_hex())
        .collect()
}

pub fn challenge(
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
pub fn session_on(
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

pub async fn inject(relay: &MiniRelay, event: &Event) {
    relay
        .inject(serde_json::from_str(&event.as_json()).unwrap())
        .await;
}
