//! Sashité player-bot fleet (ADR-0014).
//!
//! One process, N identities: a supervisor loads the TOML fleet
//! configuration (path from `FLEET_CONFIG_PATH`, kept outside every
//! repository; keys by environment indirection), builds the shared ledger
//! (bot-vs-bot budget, pool occupancy), and runs one bot actor per persona —
//! each with its own keypair, relay client and subscriptions. The bots hold
//! no privileged key and exercise exactly the public protocol: entering the
//! matchmaking pool (kind 3418), answering Direct Challenges (3420/3421),
//! exchanging Plies (3423) under the Time Accounting discipline, and
//! invoking the arbiter (3424) only when the predicted verdict is wanted.
//!
//! Game and protocol semantics are REUSED, never reimplemented:
//! `sashite-sanki-engine` (legality, application), `sashite-sanki-arbiter`
//! (canonical chain, clocks, verdict prediction — the bot believes exactly
//! what the arbiter will rule), `sashite-sanki-player` (move choice).
//!
//! Configuration (environment):
//! - `FLEET_CONFIG_PATH` (required): path to the fleet TOML (see
//!   `fleet.example.toml`).
//! - one `PLAYER_NSEC_*` variable per bot, named by each `[[bot]]`'s
//!   `nsec_env` (never logged; the derived npub only).
//! - `RUST_LOG` (optional): log filter; defaults to `info`.

mod actor;
mod chain;
mod clockmath;
mod config;
mod courtship;
mod fleet;
mod mapping;
mod persona;
mod prng;
mod publish;
mod rematch;
mod tags;

use std::collections::BTreeSet;
use std::env;
use std::sync::Arc;

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
    let arbiter = PublicKey::parse(&fleet_config.fleet.arbiter)
        .context("fleet.arbiter is not a valid pubkey")?;

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

    tracing::info!(
        bots = roster.len(),
        relay = %fleet_config.fleet.relay_url,
        %matchmaker,
        %arbiter,
        "starting the Sashité player fleet"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut handles = Vec::new();
    for (bot, keys) in roster {
        let npub = keys.public_key();
        tracing::info!(bot = %bot.name, %npub, "spawning persona");
        // The bot seed derives from the pubkey: stable across restarts,
        // distinct across bots (personas stay consistent with themselves).
        let bot_seed = seed_from_pubkey(&npub);
        let ctx = actor::BotContext {
            name: bot.name.clone(),
            keys,
            config: bot,
            fleet: fleet_config.fleet.clone(),
            matchmaker,
            arbiter,
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
