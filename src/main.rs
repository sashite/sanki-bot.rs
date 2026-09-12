//! Sashité player-bot fleet (ADR-0014).
//!
//! One process, N identities: a supervisor loads the TOML fleet
//! configuration (path from `FLEET_CONFIG_PATH`, kept outside every
//! repository; keys by environment indirection), loads the **rule system**
//! the fleet plays under (the Rule System event named by `fleet.rules`, and
//! the module it names — verified, cached, instantiated under `wasmi`),
//! builds the shared ledger (bot-vs-bot budget, pool occupancy), and runs
//! one bot actor per persona — each with its own keypair, relay client and
//! subscriptions. The bots hold no privileged key and exercise exactly the
//! public protocol: entering the matchmaking pool (kind 3418), founding the
//! session a Pairing declares and accepting Direct Challenges by founding
//! the Game Session (3422), exchanging Plies (3423) under the Time
//! Accounting discipline, concluding (3425) only with the verdict the rule
//! system yields, and proposing rematches (3420).
//!
//! Rule semantics are never reimplemented (ADR-0034): the session's module
//! is the one oracle of state, legality and verdict; `sashite-sanki-player`
//! chooses the moves, within what the module admits.
//!
//! Configuration (environment):
//! - `FLEET_CONFIG_PATH` (required): path to the fleet TOML (see
//!   `fleet.example.toml`).
//! - one `PLAYER_NSEC_*` variable per bot, named by each `[[bot]]`'s
//!   `nsec_env` (never logged; the derived npub only).
//! - `RUST_LOG` (optional): log filter; defaults to `info`.

mod actor;
mod admission;
mod cadence;
mod chain;
mod clockmath;
mod conclusion;
mod config;
mod courtship;
mod fleet;
mod founding;
mod module;
mod persona;
mod prng;
mod publish;
mod rematch;
mod rules;
mod session;
mod slots;
mod tags;

use std::collections::BTreeSet;
use std::env;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path =
        env::var("FLEET_CONFIG_PATH").context("FLEET_CONFIG_PATH must point to the fleet TOML")?;
    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("cannot read fleet config {config_path}"))?;
    let fleet_config = config::parse(&contents)?;

    let matchmaker = PublicKey::parse(&fleet_config.fleet.matchmaker)
        .context("fleet.matchmaker is not a valid pubkey")?;
    let rules = EventId::from_hex(&fleet_config.fleet.rules)
        .context("fleet.rules is not a Rule System event id")?;

    // Resolve every key first (fail fast on a missing variable), and build
    // the fleet member set for the ledger.
    let mut roster: Vec<(config::BotConfig, Keys)> = Vec::new();
    let mut members = BTreeSet::new();
    for bot in fleet_config.bots.clone() {
        let nsec = env::var(&bot.nsec_env)
            .with_context(|| format!("bot {}: {} is not set", bot.name, bot.nsec_env))?;
        let keys = Keys::parse(&nsec)
            .with_context(|| format!("bot {}: {} is not a valid key", bot.name, bot.nsec_env))?;
        members.insert(keys.public_key());
        roster.push((bot, keys));
    }
    let ledger = Arc::new(fleet::Ledger::new(
        members,
        fleet_config.fleet.max_concurrent_bot_vs_bot,
    ));

    // The rule system, before anything is published: a client MUST hold and
    // have instantiated the module before entering a pool, challenging,
    // founding or accepting (kind 3417 §Retrieval and verification). One
    // instance serves every persona of the process.
    let loaded = load_rule_system(
        &fleet_config.fleet.relay_url,
        Path::new(&fleet_config.fleet.rules_cache_dir),
        rules,
    )
    .await?;
    if loaded.describe.game != fleet_config.fleet.game {
        anyhow::bail!(
            "the rule system is for game {:?}, the fleet plays {:?}",
            loaded.describe.game,
            fleet_config.fleet.game
        );
    }
    let describe = loaded.describe.clone();
    tracing::info!(
        %rules,
        digest = loaded.event.digest(),
        publisher = %loaded.event.publisher(),
        pairings = describe.positions.len(),
        max_step = describe.max_step,
        "rule system loaded"
    );
    let module = Arc::new(Mutex::new(loaded));

    tracing::info!(
        bots = roster.len(),
        relay = %fleet_config.fleet.relay_url,
        %matchmaker,
        "starting the Sashité player fleet"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut handles = Vec::new();
    for (bot, keys) in roster {
        let npub = keys.public_key();
        // The per-cadence caps, in canonical order (ADR-0039 §6) — the one
        // line an operator reads to know how many games a persona may hold.
        let caps: Vec<String> = cadence::Cadence::ALL
            .iter()
            .map(|c| format!("{}={}", c.token(), bot.play.max_concurrent.cap(*c)))
            .collect();
        tracing::info!(bot = %bot.name, %npub, caps = caps.join(" "), "spawning persona");
        // The bot seed derives from the pubkey: stable across restarts,
        // distinct across bots (personas stay consistent with themselves).
        let bot_seed = seed_from_pubkey(&npub);
        let ctx = actor::BotContext {
            name: bot.name.clone(),
            keys,
            config: bot,
            fleet: fleet_config.fleet.clone(),
            matchmaker,
            rules,
            module: Arc::clone(&module),
            describe: describe.clone(),
            ledger: Arc::clone(&ledger),
            bot_seed,
            shutdown: shutdown_rx.clone(),
        };
        handles.push(tokio::spawn(async move {
            if let Err(error) = actor::run(ctx).await {
                tracing::error!(error = %error, "bot actor stopped with an error");
            }
        }));
    }

    shutdown_signal().await;
    tracing::info!("shutdown signal received");
    let _ = shutdown_tx.send(true);
    for handle in handles {
        let _ = handle.await;
    }
    tracing::info!("fleet down; bye");
    Ok(())
}

/// Loads the Rule System event `rules` and its module through a throwaway
/// relay connection (the event from the relay if not cached, the module
/// from the event's `url` hints if not cached).
async fn load_rule_system(
    relay_url: &str,
    cache: &Path,
    rules: EventId,
) -> Result<rules::LoadedRuleSystem> {
    let client = Client::builder().build();
    client
        .add_relay(relay_url)
        .await
        .with_context(|| format!("failed to add relay {relay_url}"))?;
    client.connect().await;
    let http = reqwest::Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("building the HTTP client")?;
    let loaded = rules::load(&client, &http, cache, rules)
        .await
        .with_context(|| format!("could not load the rule system {rules}"))?;
    client.disconnect().await;
    Ok(loaded)
}

/// A stable per-bot seed from its pubkey bytes.
fn seed_from_pubkey(pubkey: &PublicKey) -> u64 {
    let bytes = pubkey.to_bytes();
    let mut seed = 0_u64;
    for chunk in bytes.chunks(8) {
        let mut word = [0_u8; 8];
        for (slot, byte) in word.iter_mut().zip(chunk.iter()) {
            *slot = *byte;
        }
        seed ^= u64::from_le_bytes(word);
    }
    seed
}

/// Resolves on Ctrl-C (any platform) or SIGTERM (Unix — service managers,
/// `docker stop`).
// Installing a signal handler can only fail for unrecoverable process-level
// reasons; panicking at startup is the correct response.
#[allow(clippy::expect_used)]
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
