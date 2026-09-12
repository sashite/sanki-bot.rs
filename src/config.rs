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

use crate::cadence::Cadence;

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
    /// The Rule System event (kind 3417) the fleet plays under — its event
    /// id, 64 hex characters (ADR-0034). A bot enters the pool, challenges,
    /// founds and accepts under this rule system only, and loads its module
    /// at start-up.
    pub rules: String,
    /// Where the Rule System event and its module are cached (`<id>.json`,
    /// `<digest>.wasm`); a module dropped here by hand is used without
    /// fetching. Defaults to `./rules`.
    #[serde(default = "default_rules_cache_dir")]
    pub rules_cache_dir: String,
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
    /// The admission service (ADR-0008), asked — `GET {admission_url}/premium/
    /// {pubkey}`, fail-closed, bounded — whether the challenger of an
    /// **asymmetric** variant imposition is premium (*Premium* §1.3, ADR-0040
    /// §2). Absent: every such imposition is refused without a request — for
    /// a deployment that has no admission service at all (the e2e bench); a
    /// production fleet sets it from day one.
    #[serde(default)]
    pub admission_url: Option<String>,
}

fn default_game() -> String {
    "sanki".to_owned()
}
fn default_rules_cache_dir() -> String {
    "./rules".to_owned()
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
    /// Accept a Direct Challenge at **any** cadence, not only those in
    /// `time_controls`. Opt-in; default `false` keeps the persona's cadence
    /// gate. `time_controls` is unaffected — it still governs which cadences
    /// the bot **offers** in the matchmaking pool (and stays non-empty).
    #[serde(default)]
    pub accept_any_time_control: bool,
    /// Accept a Direct Challenge that **imposes** a specific variant on the bot,
    /// even asymmetrically — the challenger plays one variant and assigns the bot a
    /// DIFFERENT one (an explicit cross-variant game). Opt-in; default `false` keeps
    /// the persona policy of refusing an asymmetric imposition. The imposed variant
    /// must still be one the persona plays (`variants` weight > 0).
    #[serde(default)]
    pub accept_imposed_variant: bool,
    /// Concurrent games per cadence family (per bot) — `[bot.play.max_concurrent]`
    /// (ADR-0014 §8 as amended by ADR-0039 §6). An omitted family takes its
    /// default; `0` disables a family outright.
    #[serde(default)]
    pub max_concurrent: MaxConcurrent,
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
    /// The courtesy delay, in seconds, before claiming a win on time: the
    /// bot waits this long after it first predicts the opponent's flag
    /// before publishing the Conclusion (§6.6) — a human whose clock just
    /// fell is not flagged to the second. Default `5`.
    #[serde(default = "default_timeout_courtesy")]
    pub timeout_courtesy_secs: u64,
}

const fn default_cap_live() -> u32 {
    1
}
const fn default_cap_correspondence() -> u32 {
    4
}

/// The per-cadence concurrency caps (ADR-0039 §6): one slot per live family
/// and four correspondence games by default. The old `max_live` /
/// `max_correspondence` keys are rejected by `deny_unknown_fields`, not
/// silently honoured.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaxConcurrent {
    /// Concurrent byōyomi games.
    #[serde(default = "default_cap_live")]
    pub byoyomi: u32,
    /// Concurrent blitz games.
    #[serde(default = "default_cap_live")]
    pub blitz: u32,
    /// Concurrent rapid games.
    #[serde(default = "default_cap_live")]
    pub rapid: u32,
    /// Concurrent correspondence games.
    #[serde(default = "default_cap_correspondence")]
    pub correspondence: u32,
}

impl Default for MaxConcurrent {
    fn default() -> Self {
        Self {
            byoyomi: default_cap_live(),
            blitz: default_cap_live(),
            rapid: default_cap_live(),
            correspondence: default_cap_correspondence(),
        }
    }
}

impl MaxConcurrent {
    /// The cap of one family.
    #[must_use]
    pub const fn cap(&self, cadence: Cadence) -> u32 {
        match cadence {
            Cadence::Byoyomi => self.byoyomi,
            Cadence::Blitz => self.blitz,
            Cadence::Rapid => self.rapid,
            Cadence::Correspondence => self.correspondence,
        }
    }
}
const fn default_resign_threshold() -> i32 {
    -700
}
fn default_policy() -> String {
    "everyone".to_owned()
}
const fn default_timeout_courtesy() -> u64 {
    5
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
    if nostr_sdk::prelude::EventId::from_hex(&config.fleet.rules).is_err() {
        bail!("fleet.rules is not a Rule System event id (64 hex characters)");
    }
    if let Some(url) = &config.fleet.admission_url {
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.ends_with('/') {
            bail!("fleet.admission_url must be an http(s) origin without a trailing slash");
        }
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
            // The first period must classify (kind 3420 §Match-terms tags,
            // *Cadence — Sanki* §Well-formedness): a persona never offers, nor
            // caps under, a cadence that does not exist.
            if Cadence::of_rows(&preference.spec).is_none() {
                bail!(
                    "bot {}: a time_control the cadence classifier rejects: {:?}",
                    bot.name,
                    preference.spec
                );
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
            crate::persona::parse_hhmm_end(&window.to)
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
rules      = "7777777777777777777777777777777777777777777777777777777777777777"
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
  strength            = { time_ms = 2000, depth = 8 }
  resign_threshold    = -700
  draw_offer          = "balanced"
  challenge_policy    = "everyone"

    [bot.play.max_concurrent]
    byoyomi        = 1
    blitz          = 1
    rapid          = 1
    correspondence = 4

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
        assert_eq!(config.fleet.rules_cache_dir, "./rules");
        assert_eq!(config.bots.len(), 1);
        assert_eq!(config.bots[0].play.timeout_courtesy_secs, 5);
        let bot = &config.bots[0];
        assert_eq!(bot.name, "kaoru");
        assert_eq!(bot.play.time_controls[0].spec, vec![vec!["0", "10", "1"]]);
        assert_eq!(bot.play.draw_offer, DrawOffer::Balanced);
        assert!((bot.schedule.presence_probability - 0.85).abs() < 1e-9);
        assert_eq!(bot.play.max_concurrent.cap(Cadence::Rapid), 1);
        assert_eq!(bot.play.max_concurrent.cap(Cadence::Correspondence), 4);
    }

    #[test]
    fn caps_default_per_family_and_reject_the_old_keys() {
        let contents = EXAMPLE.replace(r#"distribution = "lognormal", "#, "");
        // The whole table omitted: the defaults.
        let without = contents.replace(
            "    [bot.play.max_concurrent]\n    byoyomi        = 1\n    blitz          = 1\n    rapid          = 1\n    correspondence = 4\n",
            "",
        );
        assert!(without.contains("[bot.schedule]"));
        assert!(!without.contains("[bot.play.max_concurrent]"));
        let config = parse(&without).unwrap();
        let caps = config.bots[0].play.max_concurrent;
        assert_eq!(
            (caps.byoyomi, caps.blitz, caps.rapid, caps.correspondence),
            (1, 1, 1, 4)
        );
        // One family omitted takes its default; zero disables it.
        let partial = contents
            .replace("    rapid          = 1\n", "")
            .replace("    blitz          = 1\n", "    blitz          = 0\n");
        let config = parse(&partial).unwrap();
        let caps = config.bots[0].play.max_concurrent;
        assert_eq!((caps.blitz, caps.rapid), (0, 1));
        // ADR-0039 §6: the removed keys fail loudly.
        let old = contents.replace(
            "  strength            = { time_ms = 2000, depth = 8 }",
            "  max_live = 1\n  strength            = { time_ms = 2000, depth = 8 }",
        );
        assert!(parse(&old).is_err());
        // A persona cadence the classifier rejects (duration 0 outside the
        // per-move form) fails at start-up.
        let malformed = contents.replace(r#"[["0", "10", "1"]]"#, r#"[["0", "10"]]"#);
        assert!(parse(&malformed).is_err());
    }

    #[test]
    fn rejects_bad_fleets() {
        let contents = EXAMPLE.replace(r#"distribution = "lognormal", "#, "");
        assert!(parse(&contents.replace("ogi = 0.8", "shogi = 0.8")).is_err());
        assert!(parse(&contents.replace("-700", "700")).is_err());
        assert!(parse(&contents.replace("Asia/Tokyo", "Mars/Olympus")).is_err());
        assert!(parse(&contents.replace("mon-sun", "lundi")).is_err());
        assert!(parse(&contents.replace("20:30", "25:99")).is_err());
        // `24:00` is legal as an END only (ADR-0040 §5).
        assert!(parse(&contents.replace("23:30", "24:00")).is_ok());
        assert!(parse(&contents.replace("20:30", "24:00")).is_err());
        assert!(parse(&contents.replace(&"7".repeat(64), "not-an-id")).is_err());
    }

    #[test]
    fn admission_url_is_optional_and_shaped() {
        let contents = EXAMPLE.replace(r#"distribution = "lognormal", "#, "");
        assert_eq!(parse(&contents).unwrap().fleet.admission_url, None);
        let with = contents.replace(
            "pow_difficulty = 10\n",
            "pow_difficulty = 10\nadmission_url = \"https://admission.sanki.app\"\n",
        );
        assert_eq!(
            parse(&with).unwrap().fleet.admission_url.as_deref(),
            Some("https://admission.sanki.app")
        );
        for bad in ["admission.sanki.app", "https://admission.sanki.app/"] {
            let bad = contents.replace(
                "pow_difficulty = 10\n",
                &format!("pow_difficulty = 10\nadmission_url = \"{bad}\"\n"),
            );
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }
}
