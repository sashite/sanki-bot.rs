// SPDX-License-Identifier: Apache-2.0
//! `sanki-bot [--config FILE] [--engine ARGV]` — one bot in one process
//! (ADR-0045 §7 *Running a bot*): the configuration, the identity, the
//! lease, then the run until `SIGTERM`.
//!
//! Without `--config`, the built-in bot runs (`sanki-bot --defaults` prints
//! it); a file overrides it key by key. `--engine` names the SEI engine —
//! a command and its arguments, separated by spaces — in place of the
//! file's `[engine]`; without either, the bot plays at random. A key file
//! that does not exist is created at the first start (`0600`), and the
//! npub logged: keep a copy of the file. `RUST_LOG` filters the log
//! (`info` by default).

use std::path::PathBuf;
use std::process::ExitCode;

use sashite_sanki_bot::bot::Bot;
use sashite_sanki_bot::config::Config;
use sashite_sanki_bot::identity::{Identity, IdentityError};

/// The built-in bot, as a commented file: what `--defaults` prints.
const DEFAULTS: &str = include_str!("../../sanki-bot.example.toml");

const USAGE: &str = "usage: sanki-bot [--config FILE] [--engine 'COMMAND ARGS…']
       sanki-bot --defaults    print the built-in configuration
       sanki-bot --version";

/// What the command line asks.
enum Invocation {
    Run {
        config: Option<PathBuf>,
        engine: Option<Vec<String>>,
    },
    Defaults,
    Version,
    Help,
}

fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut config = None;
    let mut engine = None;
    let mut alone = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let value = rest.next().ok_or("--config needs a file")?;
                if config.replace(PathBuf::from(value)).is_some() {
                    return Err("--config given twice".to_owned());
                }
            }
            "--engine" | "-e" => {
                let value = rest.next().ok_or("--engine needs a command")?;
                let argv: Vec<String> = value.split_whitespace().map(str::to_owned).collect();
                if argv.is_empty() {
                    return Err("--engine needs a command".to_owned());
                }
                if engine.replace(argv).is_some() {
                    return Err("--engine given twice".to_owned());
                }
            }
            "--defaults" => alone = Some(Invocation::Defaults),
            "--version" | "-V" => alone = Some(Invocation::Version),
            "--help" | "-h" => alone = Some(Invocation::Help),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    match alone {
        Some(_) if config.is_some() || engine.is_some() => {
            Err("--defaults, --version and --help stand alone".to_owned())
        }
        Some(invocation) => Ok(invocation),
        None => Ok(Invocation::Run { config, engine }),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (config_path, engine) = match parse(&args) {
        Ok(Invocation::Run { config, engine }) => (config, engine),
        Ok(Invocation::Defaults) => {
            print!("{DEFAULTS}");
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Version) => {
            println!("sanki-bot {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(why) => {
            eprintln!("sanki-bot: {why}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match Config::load(config_path.as_deref(), engine.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("sanki-bot: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = config.prepare() {
        eprintln!("sanki-bot: creating the directories: {e}");
        return ExitCode::from(1);
    }
    // The key: read, or created at the first start — the file's existence
    // decided by the creation itself (two starts at once: one creates, the
    // other reads).
    let identity = match Identity::generate(config.identity_file()) {
        Ok(identity) => {
            tracing::warn!(
                npub = %identity.npub(),
                file = %config.identity_file().display(),
                "a new identity: keep a copy of the key file"
            );
            Ok(identity)
        }
        Err(IdentityError::Exists(_)) => Identity::from_file(config.identity_file()),
        Err(e) => Err(e),
    };
    let identity = match identity {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!("sanki-bot: identity: {e}");
            return ExitCode::from(1);
        }
    };
    tracing::info!(npub = %identity.npub(), name = config.profile().name, "sanki-bot");
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
