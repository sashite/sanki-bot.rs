// SPDX-License-Identifier: Apache-2.0
//! `sanki-bot CONFIG.toml` — one bot in one process (ADR-0045 §7 *Running
//! a bot*): the configuration, the identity, the lease, then the run until
//! `SIGTERM`.
//!
//! `RUST_LOG` filters the log (`info` by default). An identity file that
//! does not exist is refused; `sanki-bot --generate-identity CONFIG.toml`
//! writes one (`0600`) and prints the npub, without running.

use std::path::Path;
use std::process::ExitCode;

use sashite_sanki_bot::bot::Bot;
use sashite_sanki_bot::config::Config;
use sashite_sanki_bot::identity::Identity;

fn usage() -> ExitCode {
    eprintln!("usage: sanki-bot [--generate-identity] CONFIG.toml");
    ExitCode::from(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (generate, config_path) = match args.as_slice() {
        [path] => (false, path.clone()),
        [flag, path] if flag == "--generate-identity" => (true, path.clone()),
        _ => return usage(),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match Config::from_file(Path::new(&config_path)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("sanki-bot: configuration: {e}");
            return ExitCode::from(1);
        }
    };
    if generate {
        return match Identity::generate(config.identity_file()) {
            Ok(identity) => {
                println!("{}", identity.npub());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("sanki-bot: identity: {e}");
                ExitCode::from(1)
            }
        };
    }
    let identity = match Identity::from_file(config.identity_file()) {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("sanki-bot: identity: {e}");
            return ExitCode::from(1);
        }
    };
    tracing::info!(npub = %identity.npub(), "sanki-bot");
    let bot = match Bot::new(config, identity) {
        Ok(bot) => bot,
        Err(e) => {
            eprintln!("sanki-bot: {e}");
            return ExitCode::from(1);
        }
    };
    match bot.run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sanki-bot: {e}");
            ExitCode::from(1)
        }
    }
}
