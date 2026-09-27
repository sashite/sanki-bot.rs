// SPDX-License-Identifier: Apache-2.0
//! The bot (ADR-0045 §7 *Running a bot*):
//!
//! ```text
//! let config = Config::from_file(path)?;
//! let identity = Identity::from_file(config.identity_file())?;
//! Bot::new(config, identity)?        // the lease: a second instance fails here
//!     .run().await                   // the probe, then the start, then the runtime; Ok on SIGTERM
//! ```
//!
//! | Module | ADR-0045 | What it does |
//! |---|---|---|
//! | [`reads`] | §8 | a query that proves something: `Found`, `ConfirmedAbsent`, `Unknown`; paged |
//! | [`adoption`] | §2 | the three reads that refuse a person's key |
//! | [`standing`] | §7 | the standing events, published only where they differ |
//! | [`echo`] | §2 | the echo detector: another instance of the library, or a person |
//! | [`slots`] | §5 | the games open and the reservations held, per family |
//! | [`rebuild`] | §7 | the bot's sessions and challenges, from the relay |
//! | [`start`] | §7 | the start's network steps, in order |
//! | [`challenges`] | §5 | the network checks and the founding; sending; the query on a pending challenge |
//! | [`runtime`] | §7 | the loop: events, games, challenge work, the tick, the stop |

pub mod adoption;
pub mod challenges;
pub mod echo;
pub mod reads;
pub mod rebuild;
pub mod runtime;
pub mod slots;
pub mod standing;
pub mod start;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr_sdk::prelude::*;
use sashite_sanki_client::module::Oracle;
use sashite_sanki_client::publisher::{Lease, OpenError};
use sashite_sanki_client::relay::{RelayInfo, RelayInfoError};
use sashite_sanki_client::rules::{self, LoadError};

use crate::config::{Config, ConfigError};
use crate::game::SharedOracle;
use crate::identity::Identity;
use crate::sei::{self, Host, Needs, Probe, ProbeError};

/// How long the relay gets to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The relay client's notification channel: the events of a rebuild (up
/// to six hundred half-moves per open session, the closed sessions'
/// Conclusions) and of a live replay, without a lag.
pub const NOTIFICATION_CHANNEL: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(1 << 17)
{
    Some(size) => size,
    None => std::num::NonZeroUsize::MIN,
};

/// Why the bot did not start.
#[derive(Debug)]
pub enum StartError {
    /// The configuration names the bot's own key in a list.
    Config(ConfigError),
    /// The key is in use on this host.
    Lease(OpenError),
    /// The engine failed its probe.
    Probe(ProbeError),
    /// The relay's document could not be read, or the relay is not a
    /// self-timed one.
    RelayInfo(RelayInfoError),
    /// The relay could not be reached.
    Connect(String),
    /// The publisher could not open: the latency or mining bounds.
    Publisher(OpenError),
    /// The rule system could not be loaded.
    Rules(LoadError),
    /// The module's clock primitive disagrees with the client's arithmetic.
    ClockCheck(String),
    /// The subscriptions could not be opened.
    Subscribe(String),
    /// The key is a person's (ADR-0045 §2).
    NotABotKey(adoption::Refusal),
    /// A read at start proved nothing.
    UnknownRead(u16),
    /// The rebuild failed.
    Rebuild(rebuild::RebuildError),
    /// The standing events could not be reconciled.
    Standing(standing::StandingError),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "configuration: {e}"),
            Self::Lease(e) => write!(f, "lease: {e}"),
            Self::Probe(e) => write!(f, "engine probe: {e}"),
            Self::RelayInfo(e) => write!(f, "relay: {e}"),
            Self::Connect(e) => write!(f, "connecting: {e}"),
            Self::Publisher(e) => write!(f, "publisher: {e}"),
            Self::Rules(e) => write!(f, "rule system: {e}"),
            Self::ClockCheck(e) => write!(f, "rule system: {e}"),
            Self::Subscribe(e) => write!(f, "subscribing: {e}"),
            Self::NotABotKey(refusal) => write!(f, "not a bot's key: {refusal}"),
            Self::UnknownRead(kind) => {
                write!(f, "the relay did not answer the read of kind {kind}")
            }
            Self::Rebuild(e) => write!(f, "rebuild: {e}"),
            Self::Standing(e) => write!(f, "standing events: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// A bot: its configuration, its identity and the lease on its key.
pub struct Bot {
    config: Arc<Config>,
    identity: Identity,
    lease: Lease,
}

impl Bot {
    /// Takes the lease on the key, before any other work: a second
    /// instance on the host fails here.
    ///
    /// # Errors
    ///
    /// [`StartError::Config`] when the configuration names the bot's own
    /// key; [`StartError::Lease`] when the key is in use.
    pub fn new(config: Config, identity: Identity) -> Result<Self, StartError> {
        let me = identity.public_key();
        config.excludes(&me).map_err(StartError::Config)?;
        let lease = Lease::take(&config.connection().data_dir, me).map_err(StartError::Lease)?;
        Ok(Self {
            config: Arc::new(config),
            identity,
            lease,
        })
    }

    /// The engine probe (ADR-0045 §3), before any network I/O.
    ///
    /// # Errors
    ///
    /// [`StartError::Probe`].
    pub async fn probe(&self) -> Result<Option<Probe>, StartError> {
        let Some(engine) = self.config.engine() else {
            return Ok(None);
        };
        let needs = Needs {
            pairings: self.config.play().pairings(),
            options: engine.options.clone(),
            strength: engine.strength,
            launch_ms: engine.launch_ms,
        };
        let probe = sei::probe(&engine.launch, &Host::default(), &needs)
            .await
            .map_err(StartError::Probe)?;
        tracing::info!(
            engine = ?probe.announcement.name,
            rtt_ms = probe.engine_rtt.as_millis(),
            "engine probed"
        );
        Ok(Some(probe))
    }

    /// Runs the bot: the probe, the start, then the runtime until
    /// `SIGTERM` (or `SIGINT`), which returns `Ok`.
    ///
    /// # Errors
    ///
    /// [`StartError`], before the runtime runs.
    pub async fn run(self) -> Result<(), StartError> {
        let probe = self.probe().await?;
        let connection = self.config.connection();

        // The relay's document.
        let http = reqwest::Client::new();
        let relay_info = RelayInfo::fetch(&http, &connection.relay.to_string())
            .await
            .map_err(StartError::RelayInfo)?;

        // The relay. The notification channel holds what the start's
        // queries answer — every event the rebuild reads passes through
        // it — without a lag that would drop live events.
        let client = Client::builder()
            .notification_channel_size(NOTIFICATION_CHANNEL)
            .build();
        client
            .add_relay(connection.relay.clone())
            .await
            .map_err(|e| StartError::Connect(e.to_string()))?;
        client.connect().and_wait(CONNECT_TIMEOUT).await;
        let connected = matches!(
            client.relay(connection.relay.clone()).await,
            Ok(Some(relay)) if relay.status().is_connected()
        );
        if !connected {
            return Err(StartError::Connect(format!(
                "{} did not connect within {} s",
                connection.relay,
                CONNECT_TIMEOUT.as_secs()
            )));
        }

        // The rule system: the event, the module, `describe`.
        let cache = connection.data_dir.join("rules");
        let loaded = rules::load(&client, &http, &cache, connection.rules)
            .await
            .map_err(StartError::Rules)?;
        let describe = loaded.describe.clone();
        let oracle: SharedOracle = Arc::new(Mutex::new(
            Box::new(loaded.runtime) as Box<dyn Oracle + Send>
        ));

        let prepared = start::Prepared {
            config: Arc::clone(&self.config),
            identity: self.identity,
            lease: self.lease,
            probe,
            client,
            relay: connection.relay.clone(),
            relay_info,
            oracle,
            describe,
            quarantine: None,
        };
        let started = start::start(prepared).await?;
        tracing::info!("started");
        let runtime = runtime::Runtime::new(&started, true);
        runtime.run(started, terminated()).await;
        tracing::info!("stopped");
        Ok(())
    }
}

/// Resolves on `SIGTERM` or `SIGINT`.
async fn terminated() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let interrupt = tokio::signal::ctrl_c();
    match term.as_mut() {
        Some(term) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt => {}
            }
        }
        None => {
            let _ = interrupt.await;
        }
    }
}
