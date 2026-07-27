//! Minimal in-process NIP-01 relay for the e2e bench — the bot connects to it
//! like to any relay, while the test holds the OTHER side of the wire: it can
//! inject already-signed events straight into the store (acting as arbiter and
//! opponent without a second client), observe EVERY frame the bot publishes
//! (`received` — even the swallowed ones), and inject the one fault this bench
//! exists for: `swallow_plies_from` acknowledges a bot Ply with `OK true` but
//! neither stores nor delivers it — the "relay accepted it, nobody ever saw
//! it" gap behind the double-publish incident (§6.5).
//!
//! Filter support is the subset the bot uses: `ids`, `authors`, `kinds`,
//! `#e` / `#p` (any single-letter tag), `since`, `until`. `limit` is ignored
//! (the bench stores dozens of events, never thousands).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex;

/// One EVENT frame as the relay received it (stored or swallowed alike).
#[derive(Debug, Clone)]
pub struct ReceivedEvent {
    pub pubkey: String,
    pub kind: u64,
    pub content: String,
    /// The first `step` tag value, when present (Ply slot ordinal).
    pub step: Option<String>,
    /// Wall-clock receipt instant — the grace-delay assertions read this.
    pub at: Instant,
}

struct Subscription {
    sub_id: String,
    filters: Vec<Value>,
    outbound: UnboundedSender<String>,
}

#[derive(Default)]
pub struct RelayState {
    stored: Mutex<Vec<Value>>,
    received: Mutex<Vec<ReceivedEvent>>,
    swallow_plies_from: Mutex<Option<String>>,
    subscriptions: Mutex<Vec<Subscription>>,
}

pub struct MiniRelay {
    pub url: String,
    pub state: Arc<RelayState>,
}

const PLY_KIND: u64 = 6423;

impl MiniRelay {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(RelayState::default());
        let accept_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle_connection(stream, Arc::clone(&accept_state)));
            }
        });
        Self {
            url: format!("ws://127.0.0.1:{port}"),
            state,
        }
    }

    /// Acknowledge-but-drop every Ply (kind 6423) from `pubkey_hex` — or stop.
    pub async fn swallow_plies_from(&self, pubkey_hex: Option<String>) {
        *self.state.swallow_plies_from.lock().await = pubkey_hex;
    }

    /// Inject an already-signed event as though a client had published it
    /// (stored + broadcast) — how the test acts as arbiter and opponent.
    pub async fn inject(&self, event: Value) {
        store_and_broadcast(&self.state, event).await;
    }

    /// Snapshot of every EVENT frame received so far (swallowed included).
    pub async fn received(&self) -> Vec<ReceivedEvent> {
        self.state.received.lock().await.clone()
    }

    /// Snapshot of the stored events (what a fetching client can see).
    pub async fn stored(&self) -> Vec<Value> {
        self.state.stored.lock().await.clone()
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<RelayState>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = ws.split();
    // One writer task per connection; REQ handlers and broadcasts feed it.
    let (outbound, mut outbox) = unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = outbox.recv().await {
            if sink
                .send(tokio_tungstenite::tungstenite::Message::text(text))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(Ok(message)) = source.next().await {
        let Ok(text) = message.into_text() else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(items) = frame.as_array() else {
            continue;
        };
        match items.first().and_then(Value::as_str) {
            Some("EVENT") => {
                if let Some(event) = items.get(1) {
                    handle_publish(&state, event.clone(), &outbound).await;
                }
            }
            Some("REQ") => {
                let Some(sub_id) = items.get(1).and_then(Value::as_str) else {
                    continue;
                };
                let filters: Vec<Value> = items.iter().skip(2).cloned().collect();
                // Replay the store, then EOSE, then keep the subscription live.
                for event in state.stored.lock().await.iter() {
                    if filters.iter().any(|filter| matches(event, filter)) {
                        let _ = outbound.send(json!(["EVENT", sub_id, event]).to_string());
                    }
                }
                let _ = outbound.send(json!(["EOSE", sub_id]).to_string());
                state.subscriptions.lock().await.push(Subscription {
                    sub_id: sub_id.to_owned(),
                    filters,
                    outbound: outbound.clone(),
                });
            }
            Some("CLOSE") => {
                if let Some(sub_id) = items.get(1).and_then(Value::as_str) {
                    state.subscriptions.lock().await.retain(|sub| {
                        !(sub.sub_id == sub_id && sub.outbound.same_channel(&outbound))
                    });
                }
            }
            _ => {}
        }
    }
}

async fn handle_publish(state: &Arc<RelayState>, event: Value, outbound: &UnboundedSender<String>) {
    let id = event
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let pubkey = event
        .get("pubkey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let kind = event.get("kind").and_then(Value::as_u64).unwrap_or(0);
    let content = event
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let step = event
        .get("tags")
        .and_then(Value::as_array)
        .and_then(|tags| {
            tags.iter().find_map(|tag| {
                let tag = tag.as_array()?;
                if tag.first()?.as_str()? == "step" {
                    Some(tag.get(1)?.as_str()?.to_owned())
                } else {
                    None
                }
            })
        });
    state.received.lock().await.push(ReceivedEvent {
        pubkey: pubkey.clone(),
        kind,
        content,
        step,
        at: Instant::now(),
    });

    // The fault: acknowledge, deliver to no one. The publisher believes the
    // relay took it; every reader (the publisher's own fetches included)
    // never sees it.
    let swallowed = kind == PLY_KIND
        && state
            .swallow_plies_from
            .lock()
            .await
            .as_deref()
            .is_some_and(|target| target == pubkey);
    let _ = outbound.send(json!(["OK", id, true, ""]).to_string());
    if !swallowed {
        store_and_broadcast(state, event).await;
    }
}

async fn store_and_broadcast(state: &Arc<RelayState>, event: Value) {
    state.stored.lock().await.push(event.clone());
    let subscriptions = state.subscriptions.lock().await;
    for sub in subscriptions.iter() {
        if sub.filters.iter().any(|filter| matches(&event, filter)) {
            let _ = sub
                .outbound
                .send(json!(["EVENT", sub.sub_id, event]).to_string());
        }
    }
}

/// NIP-01 filter matching — the subset the bot's REQs use.
fn matches(event: &Value, filter: &Value) -> bool {
    let Some(fields) = filter.as_object() else {
        return false;
    };
    for (key, value) in fields {
        let hit = match key.as_str() {
            "ids" => prefix_match(event.get("id"), value),
            "authors" => prefix_match(event.get("pubkey"), value),
            "kinds" => value.as_array().is_some_and(|kinds| {
                kinds
                    .iter()
                    .any(|k| k == event.get("kind").unwrap_or(&Value::Null))
            }),
            "since" => {
                event.get("created_at").and_then(Value::as_u64).unwrap_or(0)
                    >= value.as_u64().unwrap_or(0)
            }
            "until" => {
                event
                    .get("created_at")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX)
                    <= value.as_u64().unwrap_or(u64::MAX)
            }
            "limit" => true, // bounded store; the bench never needs windowing
            tag_key if tag_key.starts_with('#') && tag_key.len() == 2 => {
                let name = &tag_key[1..];
                let wanted = value.as_array().cloned().unwrap_or_default();
                event
                    .get("tags")
                    .and_then(Value::as_array)
                    .is_some_and(|tags| {
                        tags.iter().any(|tag| {
                            tag.as_array().is_some_and(|tag| {
                                tag.first().and_then(Value::as_str) == Some(name)
                                    && tag.get(1).is_some_and(|v| wanted.contains(v))
                            })
                        })
                    })
            }
            _ => true, // unknown key: permissive (the bench favours delivery)
        };
        if !hit {
            return false;
        }
    }
    true
}

fn prefix_match(actual: Option<&Value>, wanted: &Value) -> bool {
    let Some(actual) = actual.and_then(Value::as_str) else {
        return false;
    };
    wanted.as_array().is_some_and(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .any(|prefix| actual.starts_with(prefix))
    })
}
