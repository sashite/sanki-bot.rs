// SPDX-License-Identifier: Apache-2.0
//! The opening and the probe (ADR-0045 §3 *Opening*, *The probe, at start*).
//!
//! **Opening.** After the launch, `hello` with `versions: [1]` and the host's
//! name; once its `done` is read into an [`Announcement`] and carries the
//! common version, `configure` with the configured options and a `ping`;
//! all three acknowledged within `launch_ms` of the launch, or the engine
//! has failed. A `hello` without a common version ends the opening there:
//! the announcement is returned for the probe to name the gap, and nothing
//! else is sent to an engine the host cannot speak to (SEI §7, rule 1).
//!
//! **The probe.** Before anything is published, the bot launches the engine
//! once, opens it, and checks that it announces what the configuration will
//! ask of it: every pairing the bot can play, every configured option in
//! its domain, `strength` only with the feature and within its domain. It
//! measures the `ping` round trip — `engine_rtt`, the largest of twenty —
//! and closes the engine. A gap is [`ProbeError::Unsupported`], naming it:
//! a bot never accepts a game its engine cannot play.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::announce::Announcement;
use super::process::{Engine, Launch};
use super::wire::Kind;
use super::{EngineFailure, Host};

/// How many pings the probe times.
const PROBE_PINGS: usize = 20;

/// How long each probe ping may take before the engine is held hung.
const PROBE_PING_GRACE: Duration = Duration::from_secs(1);

/// Opens a launched engine: `hello`, then — with a common version —
/// `configure` and `ping`, all acknowledged by `deadline` (the launch plus
/// `launch_ms`). Without a common version, the announcement is returned as
/// it is, `version` unset, and nothing else is sent.
///
/// # Errors
///
/// [`EngineFailure::Opening`] when an acknowledgment is late or refused,
/// or the failure the engine fell into.
pub async fn open(
    engine: &mut Engine,
    host: &Host,
    options: &BTreeMap<String, Value>,
    deadline: Instant,
) -> Result<Announcement, EngineFailure> {
    let mut hello = Map::new();
    hello.insert("versions".to_owned(), json!([1]));
    hello.insert(
        "host".to_owned(),
        json!({ "name": host.name, "version": host.version }),
    );
    let hello_id = engine.send("hello", hello).await?;
    let fields = acknowledged(engine, hello_id, "hello", deadline).await?;
    let announcement = Announcement::read(&fields);
    if announcement.version != Some(1) {
        return Ok(announcement);
    }

    let mut configure = Map::new();
    configure.insert(
        "options".to_owned(),
        Value::Object(
            options
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
    );
    let configure_id = engine.send("configure", configure).await?;
    acknowledged(engine, configure_id, "configure", deadline).await?;
    let ping_id = engine.send("ping", Map::new()).await?;
    acknowledged(engine, ping_id, "ping", deadline).await?;
    Ok(announcement)
}

/// Waits for the `done` of request `id`; its fields.
async fn acknowledged(
    engine: &mut Engine,
    id: u64,
    what: &str,
    deadline: Instant,
) -> Result<Map<String, Value>, EngineFailure> {
    loop {
        let Some(event) = engine.recv(deadline).await? else {
            return Err(engine.fail(EngineFailure::Opening(format!(
                "{what} not acknowledged within the launch budget"
            ))));
        };
        if event.re != Some(id) {
            continue;
        }
        match event.kind {
            Kind::Done(fields) => return Ok(fields),
            Kind::Error(error) => {
                return Err(engine.fail(EngineFailure::Opening(format!("{what} refused: {error}"))));
            }
            Kind::Info(_) | Kind::Unknown => {}
        }
    }
}

/// What the probe learnt.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// The engine as it announces itself.
    pub announcement: Announcement,
    /// The `ping` round trip: the largest of twenty.
    pub engine_rtt: Duration,
}

/// Why the probe refused the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The engine failed during the probe.
    Engine(EngineFailure),
    /// The engine does not announce what the configuration needs; each gap
    /// named.
    Unsupported(Vec<String>),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine(failure) => write!(f, "the engine failed during the probe: {failure}"),
            Self::Unsupported(gaps) => {
                write!(
                    f,
                    "the engine cannot play what the configuration asks: {}",
                    gaps.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for ProbeError {}

/// What the probe checks the engine against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Needs {
    /// Every pairing the bot can play (`variants` × `opponents`, both seats).
    pub pairings: BTreeSet<String>,
    /// The options `configure` will send.
    pub options: BTreeMap<String, Value>,
    /// The configured strength, when set.
    pub strength: Option<i64>,
    /// The launch budget.
    pub launch_ms: u64,
}

/// Runs the probe.
///
/// # Errors
///
/// See [`ProbeError`].
pub async fn probe(launch: &Launch, host: &Host, needs: &Needs) -> Result<Probe, ProbeError> {
    let mut engine = Engine::launch(launch).map_err(ProbeError::Engine)?;
    let deadline = engine
        .started()
        .checked_add(Duration::from_millis(needs.launch_ms))
        .unwrap_or_else(Instant::now);
    let announcement = open(&mut engine, host, &needs.options, deadline)
        .await
        .map_err(ProbeError::Engine)?;
    let gaps = announcement.gaps(&needs.pairings, &needs.options, needs.strength);
    if !gaps.is_empty() {
        engine.close().await;
        return Err(ProbeError::Unsupported(gaps));
    }

    let mut worst = Duration::ZERO;
    for _ in 0..PROBE_PINGS {
        let sent = Instant::now();
        let id = engine
            .send("ping", Map::new())
            .await
            .map_err(ProbeError::Engine)?;
        let until = sent.checked_add(PROBE_PING_GRACE).unwrap_or(sent);
        loop {
            match engine.recv(until).await {
                Ok(Some(event)) if event.re == Some(id) && event.is_terminal() => break,
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(ProbeError::Engine(engine.fail(EngineFailure::Unresponsive)));
                }
                Err(failure) => return Err(ProbeError::Engine(failure)),
            }
        }
        worst = worst.max(sent.elapsed());
    }
    engine.close().await;
    Ok(Probe {
        announcement,
        engine_rtt: worst,
    })
}
