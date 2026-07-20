//! Fleet configuration — the TOML persona file (ADR-0014 §4).
//!
//! The file lives **outside every repository** (path from
//! `FLEET_CONFIG_PATH`), like `bots.local.env`; keys are referenced by
//! **environment-variable name** (`nsec_env`) and resolved at startup — the
//! configuration itself never holds a secret. Parsing and validation are
//! pure and unit-tested; the supervisor resolves the environment.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// The whole fleet file: one global section plus one `[[bot]]` per identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// Global (fleet-wide) settings.
    pub fleet: FleetSection,
    /// The roster.
    #[serde(default, rename = "bot")]
    pub bots: Vec<BotConfig>,
}

/// The `[fleet]` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetSection {
    /// Relay to play on (single relay in v1 — ADR-0014 §13).
    pub relay_url: String,
    /// The configured matchmaker (hex or npub).
    pub matchmaker: String,
    /// The configured arbiter — a bot only plays under this arbiter.
    pub arbiter: String,
    /// The game family (`sanki`).
    #[serde(default = "default_game")]
    pub game: String,
    /// Fleet-wide cap on concurrent bot-vs-bot sessions (§8).
    #[serde(default = "default_bot_vs_bot")]
    pub max_concurrent_bot_vs_bot: u32,
    /// NIP-13 difficulty mined on published player events (the relay's
    /// advertised minimum; a config value in v1).
    #[serde(default = "default_pow")]
    pub pow_difficulty: u8,
}

fn default_game() -> String {
    "sanki".to_owned()
}
const fn default_bot_vs_bot() -> u32 {
    3
}
const fn default_pow() -> u8 {
    0
}

/// One `[[bot]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    /// Internal id (also the tracing span name).
    pub name: String,
    /// Environment variable holding the bot's nsec (never the key itself).
    pub nsec_env: String,
    /// kind-0 profile material (`bot: true` is always added — NIP-24).
    pub profile: ProfileConfig,
    /// Play preferences and strength.
    pub play: PlayConfig,
    /// Presence schedule.
    pub schedule: ScheduleConfig,
    /// Think-time habits.
    pub tempo: TempoConfig,
}

/// kind-0 profile fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Display name shown by Nostr clients.
    pub display_name: String,
    /// Free-text description (conventionally mentioning the bot nature).
    #[serde(default)]
    pub about: String,
    /// Avatar URL, if any.
    #[serde(default)]
    pub picture: Option<String>,
}

/// Play preferences: variants, cadences, concurrency, strength, temperament.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayConfig {
    /// Weighted self-variant preferences (`ogi = 0.8, chess = 0.15, …`).
    pub variants: BTreeMap<String, f64>,
    /// Weighted time-control preferences, each stored as its raw
    /// `time_control` tag rows — byte-identical to what the bot's Open
    /// Challenges carry (the matchmaker pairs identical configurations).
    pub time_controls: Vec<WeightedTimeControl>,
    /// Concurrent live games (per bot).
    #[serde(default = "default_max_live")]
    pub max_live: u32,
    /// Concurrent correspondence games (per bot).
    #[serde(default = "default_max_correspondence")]
    pub max_correspondence: u32,
    /// Search parameters for `sashite-sanki-player`.
    pub strength: StrengthConfig,
    /// Sustained centi-eval below which the bot resigns (§6.6). Negative.
    #[serde(default = "default_resign_threshold")]
    pub resign_threshold: i32,
    /// Draw-offer temperament.
    #[serde(default)]
    pub draw_offer: DrawOffer,
    /// kind-30420 mode published for `sanki`.
    #[serde(default = "default_policy")]
    pub challenge_policy: String,
}

const fn default_max_live() -> u32 {
    1
}
const fn default_max_correspondence() -> u32 {
    4
}
const fn default_resign_threshold() -> i32 {
    -700
}
fn default_policy() -> String {
    "everyone".to_owned()
}

/// A weighted time-control preference.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightedTimeControl {
    /// The `time_control` tag rows, values as strings (tag format).
    pub spec: Vec<Vec<String>>,
    /// Draw weight among the persona's preferences (spontaneous entries).
    #[allow(dead_code)]
    pub weight: f64,
}

/// Search-strength parameters (persona-graded honest search — never blunders).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrengthConfig {
    /// Wall-clock search allowance per move, in milliseconds.
    pub time_ms: u64,
    /// Iterative-deepening ceiling.
    pub depth: u8,
}

/// Draw-offer temperament (§6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DrawOffer {
    /// Never offers.
    Never,
    /// Offers in balanced, drawish stretches.
    #[default]
    Balanced,
    /// Offers readily when not clearly better.
    Eager,
}

/// Presence schedule: timezone windows with daily jitter and a show-up
/// probability (§6.7).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// IANA timezone name (e.g. `Asia/Tokyo`).
    pub timezone: String,
    /// Presence windows.
    pub windows: Vec<WindowConfig>,
    /// Window edges are shifted by up to ± this many minutes, per day.
    #[serde(default = "default_jitter")]
    pub jitter_minutes: u32,
    /// Probability the bot shows up at all on a given day.
    #[serde(default = "default_presence")]
    pub presence_probability: f64,
}

const fn default_jitter() -> u32 {
    0
}
const fn default_presence() -> f64 {
    1.0
}

/// One presence window (`days = "mon-sun"`, `from = "20:30"`, `to = "23:30"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowConfig {
    /// A day range (`mon-fri`) or single day (`sat`), lowercase.
    pub days: String,
    /// Window start, `HH:MM` local.
    pub from: String,
    /// Window end, `HH:MM` local (same-day; crossing midnight is expressed
    /// as two windows).
    pub to: String,
}

/// Think-time habits: log-normal reflection, per pace (§4).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TempoConfig {
    /// Live reflection.
    pub think: ThinkConfig,
    /// Correspondence reflection (hours, not seconds).
    pub correspondence_think: ThinkConfig,
}

/// A log-normal think-time distribution.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkConfig {
    /// Median reflection, in seconds.
    pub median_s: f64,
    /// Log-space sigma.
    pub sigma: f64,
}

/// Parse and validate a fleet file's contents.
pub fn parse(contents: &str) -> Result<FleetConfig> {
    let config: FleetConfig = toml::from_str(contents).context("malformed fleet TOML")?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &FleetConfig) -> Result<()> {
    if config.bots.is_empty() {
        bail!("the fleet has no [[bot]] table");
    }
    let mut names = std::collections::BTreeSet::new();
    for bot in &config.bots {
        if !names.insert(bot.name.as_str()) {
            bail!("duplicate bot name: {}", bot.name);
        }
        if bot.play.variants.is_empty() {
            bail!("bot {}: no variant preference", bot.name);
        }
        for variant in bot.play.variants.keys() {
            if !matches!(variant.as_str(), "chess" | "ogi" | "xiongqi") {
                bail!("bot {}: unknown variant {variant}", bot.name);
            }
        }
        if bot.play.time_controls.is_empty() {
            bail!("bot {}: no time-control preference", bot.name);
        }
        for preference in &bot.play.time_controls {
            if preference.spec.is_empty() {
                bail!("bot {}: an empty time_control spec", bot.name);
            }
            for row in &preference.spec {
                if row.is_empty() || row.len() > 3 {
                    bail!("bot {}: a malformed time_control row", bot.name);
                }
            }
        }
        if bot.play.resign_threshold >= 0 {
            bail!("bot {}: resign_threshold must be negative", bot.name);
        }
        if !(0.0..=1.0).contains(&bot.schedule.presence_probability) {
            bail!("bot {}: presence_probability out of [0, 1]", bot.name);
        }
        crate::persona::parse_timezone(&bot.schedule.timezone).ok_or_else(|| {
            anyhow!(
                "bot {}: unknown timezone {}",
                bot.name,
                bot.schedule.timezone
            )
        })?;
        for window in &bot.schedule.windows {
            crate::persona::parse_days(&window.days)
                .ok_or_else(|| anyhow!("bot {}: bad days {}", bot.name, window.days))?;
            crate::persona::parse_hhmm(&window.from)
                .ok_or_else(|| anyhow!("bot {}: bad time {}", bot.name, window.from))?;
            crate::persona::parse_hhmm(&window.to)
                .ok_or_else(|| anyhow!("bot {}: bad time {}", bot.name, window.to))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;

    const EXAMPLE: &str = r#"
[fleet]
relay_url  = "wss://relay.sanki.app/"
matchmaker = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
arbiter    = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
game       = "sanki"
max_concurrent_bot_vs_bot = 3
pow_difficulty = 10

[[bot]]
name     = "kaoru"
nsec_env = "PLAYER_NSEC_KAORU"

  [bot.profile]
  display_name = "Kaoru"
  about        = "Ōgi enthusiast. Osaka evenings. (bot)"

  [bot.play]
  variants        = { ogi = 0.8, chess = 0.15, xiongqi = 0.05 }
  time_controls   = [
    { spec = [["0", "10", "1"]],     weight = 0.7 },
    { spec = [["300", "3"]],         weight = 0.3 },
  ]
  max_live            = 1
  max_correspondence  = 4
  strength            = { time_ms = 2000, depth = 8 }
  resign_threshold    = -700
  draw_offer          = "balanced"
  challenge_policy    = "everyone"

  [bot.schedule]
  timezone = "Asia/Tokyo"
  windows  = [{ days = "mon-sun", from = "20:30", to = "23:30" }]
  jitter_minutes = 25
  presence_probability = 0.85

  [bot.tempo]
  think = { distribution = "lognormal", median_s = 3.0, sigma = 0.6 }
  correspondence_think = { median_s = 14400, sigma = 0.9 }
"#;

    #[test]
    fn parses_the_adr_example() {
        // The ADR sample carries a `distribution` key; the config drops it —
        // adjust here to the accepted shape.
        let contents = EXAMPLE.replace(r#"distribution = "lognormal", "#, "");
        let config = parse(&contents).unwrap();
        assert_eq!(config.fleet.game, "sanki");
        assert_eq!(config.bots.len(), 1);
        let bot = &config.bots[0];
        assert_eq!(bot.name, "kaoru");
        assert_eq!(bot.play.time_controls[0].spec, vec![vec!["0", "10", "1"]]);
        assert_eq!(bot.play.draw_offer, DrawOffer::Balanced);
        assert!((bot.schedule.presence_probability - 0.85).abs() < 1e-9);
    }

    #[test]
    fn rejects_bad_fleets() {
        let contents = EXAMPLE.replace(r#"distribution = "lognormal", "#, "");
        assert!(parse(&contents.replace("ogi = 0.8", "shogi = 0.8")).is_err());
        assert!(parse(&contents.replace("-700", "700")).is_err());
        assert!(parse(&contents.replace("Asia/Tokyo", "Mars/Olympus")).is_err());
        assert!(parse(&contents.replace("mon-sun", "lundi")).is_err());
        assert!(parse(&contents.replace("20:30", "25:99")).is_err());
    }
}
