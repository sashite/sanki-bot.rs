// SPDX-License-Identifier: Apache-2.0
//! The start (ADR-0045 §7 *Start*), from what [`super::Bot`] prepared
//! without the network — the configuration, the identity, the lease, the
//! engine probe, the relay's document, the rule system — through its
//! network steps, in order: the relay's document checked and the publisher
//! opened with its bounds; the clock check; the quarantine; the
//! subscriptions; adoption; the rebuild; the standing events; the echo
//! detector's watch.

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use sashite_sanki_client::clock;
use sashite_sanki_client::module::Describe;
use sashite_sanki_client::publisher::{Lease, Publisher, Settings};
use sashite_sanki_client::relay::{RelayInfo, SESSION_KINDS};

use super::adoption::{self, Adoption};
use super::echo::Watch;
use super::rebuild::{self, Rebuilt, Scope};
use super::standing::{self, Reconciled};
use super::StartError;
use crate::config::Config;
use crate::game::{GameContext, SharedOracle, SKEW_ALLOWANCE_SECS};
use crate::identity::Identity;
use crate::sei::Probe;

/// What a bot prepared before any network step.
pub struct Prepared {
    /// The configuration.
    pub config: Arc<Config>,
    /// The identity; moved into the publisher.
    pub identity: Identity,
    /// The lease on the key.
    pub lease: Lease,
    /// The engine's probe, when an engine is configured.
    pub probe: Option<Probe>,
    /// The relay client, connected to `relay`.
    pub client: Client,
    /// The relay the client is connected to and the publisher speaks to:
    /// the configured one — a test's in-process relay, whose `ws://` URL
    /// no event may designate, while the configuration keeps designating
    /// the `wss://` one the terms are compared with.
    pub relay: RelayUrl,
    /// The relay's document.
    pub relay_info: RelayInfo,
    /// The rule system's module.
    pub oracle: SharedOracle,
    /// What the module describes.
    pub describe: Describe,
    /// The quarantine, when not the relay's
    /// (`created_at_upper_limit + created_at_lower_limit + 2` seconds):
    /// a test's.
    pub quarantine: Option<Duration>,
}

/// What the start leaves the runtime.
pub struct Started {
    /// What every game shares.
    pub ctx: Arc<GameContext>,
    /// The relay's word on the bot.
    pub rebuilt: Rebuilt,
    /// The echo detector's watch.
    pub watch: Watch,
    /// What the standing events' reconciliation published.
    pub standing: Reconciled,
    /// The relay's notifications, subscribed before the bot's
    /// subscriptions were opened: nothing the relay answers is missed.
    pub notifications:
        std::pin::Pin<Box<dyn futures_util::Stream<Item = ClientNotification> + Send>>,
}

/// The quarantine the relay's document prescribes.
#[must_use]
pub fn quarantine_of(info: &RelayInfo) -> Duration {
    let secs = info
        .future_tolerance()
        .saturating_add(info.past_tolerance().unwrap_or(1))
        .saturating_add(2);
    Duration::from_secs(secs)
}

/// The start's network steps.
///
/// # Errors
///
/// The first step that fails; see [`StartError`].
pub async fn start(prepared: Prepared) -> Result<Started, StartError> {
    let Prepared {
        config,
        identity,
        lease,
        probe,
        client,
        relay,
        relay_info,
        oracle,
        describe,
        quarantine,
    } = prepared;
    let me = identity.public_key();
    let fallback_key = identity.fallback_key().clone();
    let connection = config.connection();

    // 3. The relay's document, the latency and mining bounds.
    relay_info
        .check_self_timed()
        .map_err(StartError::RelayInfo)?;
    let settings = Settings::from_relay_info(
        relay.clone(),
        &relay_info,
        connection.rate_per_minute,
        connection.data_dir.clone(),
    );
    let publisher = Publisher::open_leased(client.clone(), identity, settings, lease)
        .await
        .map_err(StartError::Publisher)?;
    let publisher = Arc::new(publisher);

    // 4. The rule system's clock check.
    {
        let mut oracle = oracle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clock::check(&mut *oracle).map_err(|e| StartError::ClockCheck(e.to_string()))?;
    }
    warn_on_acceptance(&config, publisher.settings().past_tolerance);

    // 5. The quarantine: nothing published or queried.
    let quarantine = quarantine.unwrap_or_else(|| quarantine_of(&relay_info));
    tracing::info!(secs = quarantine.as_secs(), "quarantine");
    tokio::time::sleep(quarantine).await;

    // The subscriptions, from the quarantine's start: what lands during
    // the rebuild is seen twice, never missed. The notification channel
    // first: what the relay replays before it exists is lost to it.
    let notifications = client.notifications();
    let since = Timestamp::from(
        publisher
            .now()
            .saturating_sub(quarantine.as_secs())
            .saturating_sub(SKEW_ALLOWANCE_SECS),
    );
    let kinds = SESSION_KINDS.map(Kind::Custom);
    let to_me = Filter::new().kinds(kinds).pubkey(me).since(since);
    let by_me = Filter::new().kinds(kinds).author(me).since(since);
    client
        .subscribe(vec![to_me, by_me])
        .await
        .map_err(|e| StartError::Subscribe(e.to_string()))?;

    // 6. Adoption.
    match adoption::check(&client, &relay, me).await {
        Adoption::Adopted => {}
        Adoption::Refused(refusal) => return Err(StartError::NotABotKey(refusal)),
        Adoption::Unknown(kind) => return Err(StartError::UnknownRead(kind)),
    }

    // 7. The rebuild.
    let rebuilt = {
        let scope = Scope {
            me,
            config: &config,
            relay: &relay,
            max_step: describe.max_step,
            now: publisher.now(),
            past_tolerance: publisher.settings().past_tolerance,
        };
        rebuild::rebuild(&client, &scope, &oracle)
            .await
            .map_err(StartError::Rebuild)?
    };
    tracing::info!(
        open = rebuilt.open.len(),
        closed = rebuilt.closed,
        unverified = rebuilt.unverified,
        pending = rebuilt.pending.len(),
        incoming = rebuilt.incoming.len(),
        "rebuilt"
    );

    // 8. The standing events.
    let standing = standing::reconcile(&client, &publisher, &config, me)
        .await
        .map_err(StartError::Standing)?;

    // 9. The echo detector's watch.
    let watch = Watch::new(me, publisher.now(), publisher.settings().future_tolerance);

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
    Ok(Started {
        ctx,
        rebuilt,
        watch,
        standing,
        notifications,
    })
}

/// ADR-0045 §7: in a time control whose per-move allowance is below
/// `L + 1 s + skew + margin_ms` plus a query, `accept_draw` can never fire.
/// The shortest allowance the bot admits is `min_move_secs`.
fn warn_on_acceptance(config: &Config, past_tolerance: u64) {
    let play = config.play();
    if play.accept_draw.is_none() {
        return;
    }
    let needed = past_tolerance
        .saturating_add(1)
        .saturating_add(SKEW_ALLOWANCE_SECS)
        .saturating_add(play.margin_ms.div_ceil(1000))
        .saturating_add(crate::game::REREAD_TIMEOUT.as_secs());
    if play.min_move_secs < needed {
        tracing::warn!(
            min_move_secs = play.min_move_secs,
            needed,
            "accept_draw can never fire in a game at the shortest playable per-move share"
        );
    }
}
