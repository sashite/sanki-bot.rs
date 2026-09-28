// SPDX-License-Identifier: Apache-2.0
//! The configuration (ADR-0045 §4): one TOML file with `schema = 1`, parsed
//! into a private `RawConfig`, then converted into a [`Config`] whose types
//! forbid the incoherent cases. An error is a [`ConfigError`] naming the
//! key. The file holds no secret.
//!
//! **Every key has a default** (ADR-0045 v4.2): the file overrides the
//! built-in bot — the one `sanki-bot` runs without a file — key by key, and
//! an unknown key is refused. The built-in bot: Sashité's relay and its
//! current Rule System ([`DEFAULT_RULES`]), the key and the data under
//! [`data_root`], named `sanki-bot`, open to everyone, the three variants,
//! one game per cadence family at five seconds a move, no engine (random
//! play) — what [`Config::defaults`] yields, and `sanki-bot --defaults`
//! prints. The absent sections whose absence is a behaviour stay so:
//! `engine` (random play), `outgoing`, `resign`, `offer_draw`,
//! `accept_draw` (never).
//!
//! What only the engine can tell — an option announced and in its domain,
//! `strength` announced and within its bounds — is the probe's
//! (`sei::probe`), not the types'. What only the identity can tell — the
//! bot's own key in none of the lists — is [`Config::excludes`]', called
//! once the identity is read.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use nostr_sdk::prelude::*;
use sashite_sanki_client::cadence::Cadence;
use sashite_sanki_client::clock::TimeControl;
use serde::Deserialize;

use crate::sei::Launch;

/// The one schema this crate reads.
const SCHEMA: u64 = 1;

/// The built-in relay: Sashité's.
pub const DEFAULT_RELAY: &str = "wss://relay.sanki.app";

/// The built-in Rule System: the kind-`3417` event Sashité's app founds
/// under at this release. A bot under another rule system than the one a
/// challenge names refuses it (`OtherRules`): after a revision of the
/// module, configure `connection.rules`, or update the crate.
pub const DEFAULT_RULES: &str = "000006e9da2118b7fdcdb87e13f0e30b1120676957b0b5ca9932e7b4756f9322";

/// The built-in per-key rate: the relay's free tier.
pub const DEFAULT_RATE_PER_MINUTE: u32 = 30;

/// Where the built-in bot keeps its key, its data and its engine's working
/// directory: `~/Library/Application Support/sanki-bot` on macOS,
/// `$XDG_DATA_HOME/sanki-bot` (else `~/.local/share/sanki-bot`) elsewhere.
///
/// # Errors
///
/// `HOME` (and `XDG_DATA_HOME`) unset.
pub fn data_root() -> Result<PathBuf, ConfigError> {
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| {
                ConfigError::new(
                    "",
                    "HOME is not set: set connection.data_dir, identity.file and engine.cwd",
                )
            })
    };
    if cfg!(target_os = "macos") {
        return Ok(home()?.join("Library/Application Support/sanki-bot"));
    }
    match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(xdg) if xdg.is_absolute() => Ok(xdg.join("sanki-bot")),
        _ => Ok(home()?.join(".local/share/sanki-bot")),
    }
}

/// Why a configuration is refused: the key, and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The offending key, dotted (`play.min_move_secs`), or `""` for the
    /// file itself.
    pub key: String,
    /// What is wrong with it.
    pub reason: String,
}

impl ConfigError {
    fn new(key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key.is_empty() {
            write!(f, "configuration: {}", self.reason)
        } else {
            write!(f, "configuration: `{}`: {}", self.key, self.reason)
        }
    }
}

impl std::error::Error for ConfigError {}

/// A Sanki variant, as the configuration and the founding events name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Variant {
    /// Chess (style `W`).
    Chess,
    /// Ōgi (style `J`).
    Ogi,
    /// Xiongqi (style `C`).
    Xiongqi,
}

impl Variant {
    /// The variant `name` names, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "chess" => Some(Self::Chess),
            "ogi" => Some(Self::Ogi),
            "xiongqi" => Some(Self::Xiongqi),
            _ => None,
        }
    }

    /// The name the events carry.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chess => "chess",
            Self::Ogi => "ogi",
            Self::Xiongqi => "xiongqi",
        }
    }

    /// The SIN style letter, uppercase.
    #[must_use]
    pub const fn style(self) -> char {
        match self {
            Self::Chess => 'W',
            Self::Ogi => 'J',
            Self::Xiongqi => 'C',
        }
    }
}

/// The SEI pairing of two variants: the first seat's style, then the
/// second's.
#[must_use]
pub fn pairing(first: Variant, second: Variant) -> String {
    format!("{}{}", first.style(), second.style())
}

/// A non-empty list without duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    /// The elements.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// The first element.
    #[must_use]
    pub fn first(&self) -> &T {
        // Non-empty by construction; the fallback never runs.
        self.0.first().unwrap_or_else(|| unreachable!())
    }

    /// The number of elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Never true.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T> NonEmpty<T> {
    /// A list for tests, unchecked.
    #[cfg(test)]
    pub(crate) fn test(items: Vec<T>) -> Self {
        Self(items)
    }
}

impl<T: PartialEq> NonEmpty<T> {
    /// Whether `item` is listed.
    #[must_use]
    pub fn contains(&self, item: &T) -> bool {
        self.0.contains(item)
    }
}

/// `[connection]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The only relay, and every session's timing relay; normalised.
    pub relay: RelayUrl,
    /// The Rule System event (kind `3417`).
    pub rules: EventId,
    /// The rules cache and the lease file; absolute.
    pub data_dir: PathBuf,
    /// The relay's per-key limit, events per minute.
    pub rate_per_minute: u32,
}

/// `[profile]` (kind `0`; `bot: true` is always written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// The handle; 1 to 64 characters.
    pub name: String,
    /// The name shown; 1 to 64 characters, when set.
    pub display_name: Option<String>,
    /// 0 to 1,000 characters.
    pub about: String,
    /// An `https` URL, when set.
    pub picture: Option<String>,
    /// `local@domain`, when set.
    pub nip05: Option<String>,
    /// An `https` URL, when set.
    pub website: Option<String>,
}

/// `[engine]`.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// How to launch it: the command resolved to an existing executable.
    pub launch: Launch,
    /// The SEI options `configure` sends; scalars.
    pub options: BTreeMap<String, serde_json::Value>,
    /// `search.strength.elo`, when set.
    pub strength: Option<i64>,
    /// 0 to 10.
    pub max_relaunches_per_game: u8,
    /// 1,000 to 60,000.
    pub launch_ms: u64,
}

/// `challenges.policy` (kind `30420`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Everyone may challenge.
    Everyone,
    /// No one may.
    Nobody,
    /// Only the listed keys (kind `3` is published equal to the list).
    Following(NonEmpty<PublicKey>),
    /// Only players rated by `authority` within `max_delta`.
    Rating {
        /// 1 to 1,000.
        max_delta: u32,
        /// The rating authority (kind `3426`).
        authority: PublicKey,
    },
}

impl Policy {
    /// The kind-`30420` token.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Everyone => "everyone",
            Self::Nobody => "nobody",
            Self::Following(_) => "following",
            Self::Rating { .. } => "rating",
        }
    }
}

/// `[challenges.outgoing]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// Distinct, none in `blocks`, at most 1,000.
    pub targets: NonEmpty<PublicKey>,
    /// The periods, well-formed and playable.
    pub time_control: Vec<[Option<u64>; 3]>,
    /// The time control's family.
    pub cadence: Cadence,
    /// Fixed for both players (the mirror form).
    pub variant: Variant,
    /// At least 60.
    pub every_secs: u64,
    /// 30 to 3,600.
    pub accept_secs: u64,
    /// 1 to 1,000.
    pub max_per_day: u32,
}

/// `[challenges]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenges {
    /// The policy.
    pub policy: Policy,
    /// The mute list (kind `10000`), applied before the policy.
    pub blocks: Vec<PublicKey>,
    /// Outgoing challenges, when configured.
    pub outgoing: Option<Outgoing>,
}

/// `[play.resign]`: the bot resigns when, for `streak` consecutive own
/// turns, `wdl.win ≤ max_win` and `wdl.loss ≥ min_loss`, or `mate < 0`,
/// or the advice is `resign`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resign {
    /// Per mille.
    pub max_win: u16,
    /// Per mille; above `max_win`.
    pub min_loss: u16,
    /// 1 to 10.
    pub streak: u8,
}

/// `[play.offer_draw]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfferDraw {
    /// Per mille.
    pub min_draw: u16,
    /// 1 to 10.
    pub streak: u8,
    /// 0 to 600.
    pub after_ply: u32,
}

/// `[play.accept_draw]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptDraw {
    /// Per mille.
    pub min_draw: u16,
}

/// `[play]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Play {
    /// The variants the bot plays.
    pub variants: NonEmpty<Variant>,
    /// Played when the challenger leaves the bot's variant open.
    pub preferred: Variant,
    /// The variants the bot plays against.
    pub opponents: NonEmpty<Variant>,
    /// 2 to 3,600.
    pub min_move_secs: u64,
    /// 100 to 5,000.
    pub margin_ms: u64,
    /// The cap of each family; absent families are `0`.
    pub max_concurrent: BTreeMap<Cadence, u32>,
    /// When set, the bot resigns.
    pub resign: Option<Resign>,
    /// When set, the bot offers draws.
    pub offer_draw: Option<OfferDraw>,
    /// When set, the bot accepts draws.
    pub accept_draw: Option<AcceptDraw>,
}

impl Play {
    /// The cap of `cadence`.
    #[must_use]
    pub fn cap(&self, cadence: Cadence) -> u32 {
        self.max_concurrent.get(&cadence).copied().unwrap_or(0)
    }

    /// The sum of the caps.
    #[must_use]
    pub fn total_cap(&self) -> u32 {
        self.max_concurrent
            .values()
            .fold(0u32, |sum, cap| sum.saturating_add(*cap))
    }

    /// Every pairing the bot can play: `variants` × `opponents`, in both
    /// seats.
    #[must_use]
    pub fn pairings(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for own in self.variants.as_slice() {
            for other in self.opponents.as_slice() {
                out.insert(pairing(*own, *other));
                out.insert(pairing(*other, *own));
            }
        }
        out
    }
}

/// The configuration, coherent by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    connection: Connection,
    identity_file: PathBuf,
    profile: Profile,
    engine: Option<EngineConfig>,
    challenges: Challenges,
    play: Play,
}

impl Config {
    /// Reads and checks the file at `path`.
    ///
    /// # Errors
    ///
    /// The first key that is wrong, as a [`ConfigError`].
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::new("", format!("{}: {e}", path.display())))?;
        Self::from_toml(&text)
    }

    /// Reads and checks a TOML text: the built-in bot, overridden key by
    /// key by what the text says.
    ///
    /// # Errors
    ///
    /// The first key that is wrong, as a [`ConfigError`].
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Self::try_from(RawConfig::parse(text)?)
    }

    /// The built-in bot: what an empty file yields.
    ///
    /// # Errors
    ///
    /// The paths cannot be defaulted ([`data_root`]).
    pub fn defaults() -> Result<Self, ConfigError> {
        Self::from_toml("")
    }

    /// What the command line asks: the file (or the built-in bot), with
    /// `[engine]` replaced by `argv` when one is given — the command, a
    /// path or a name in `PATH`, and its arguments — under the section's
    /// defaults (the working directory under [`data_root`], an empty
    /// environment, no option, two relaunches, five seconds to launch).
    ///
    /// # Errors
    ///
    /// The first key that is wrong; an empty `argv`, or a command that is
    /// not an executable file.
    pub fn load(file: Option<&Path>, argv: Option<&[String]>) -> Result<Self, ConfigError> {
        let text = match file {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| ConfigError::new("", format!("{}: {e}", path.display())))?,
            None => String::new(),
        };
        let mut raw = RawConfig::parse(&text)?;
        if let Some(argv) = argv {
            let (command, args) = argv
                .split_first()
                .ok_or_else(|| ConfigError::new("engine.command", "empty"))?;
            raw.engine = Some(RawEngine {
                command: command.clone(),
                args: args.to_vec(),
                ..RawEngine::default()
            });
        }
        Self::try_from(raw)
    }

    /// Creates the directories the bot writes to — the data directory, the
    /// key's, the engine's working directory — where they do not exist.
    ///
    /// # Errors
    ///
    /// The directories cannot be created.
    pub fn prepare(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.connection.data_dir)?;
        if let Some(dir) = self.identity_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(engine) = &self.engine {
            std::fs::create_dir_all(&engine.launch.cwd)?;
        }
        Ok(())
    }

    /// `[connection]`.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// `identity.file`.
    #[must_use]
    pub fn identity_file(&self) -> &Path {
        &self.identity_file
    }

    /// `[profile]`.
    #[must_use]
    pub const fn profile(&self) -> &Profile {
        &self.profile
    }

    /// `[engine]`, when configured; without it, every turn is the fallback.
    #[must_use]
    pub const fn engine(&self) -> Option<&EngineConfig> {
        self.engine.as_ref()
    }

    /// `[challenges]`.
    #[must_use]
    pub const fn challenges(&self) -> &Challenges {
        &self.challenges
    }

    /// `[play]`.
    #[must_use]
    pub const fn play(&self) -> &Play {
        &self.play
    }

    /// Checks that the bot's own key is in none of the lists (§4 *Lists*),
    /// which only the identity can tell.
    ///
    /// # Errors
    ///
    /// The list that holds it.
    pub fn excludes(&self, me: &PublicKey) -> Result<(), ConfigError> {
        let own = |key: &str| ConfigError::new(key, "lists the bot's own key");
        if self.challenges.blocks.contains(me) {
            return Err(own("challenges.blocks"));
        }
        if let Policy::Following(follows) = &self.challenges.policy {
            if follows.contains(me) {
                return Err(own("challenges.follows"));
            }
        }
        if let Some(outgoing) = &self.challenges.outgoing {
            if outgoing.targets.contains(me) {
                return Err(own("challenges.outgoing.targets"));
            }
        }
        Ok(())
    }
}

/// The smallest per-move share a time control offers, in seconds
/// (ADR-0045 §5, rule 7): over all its periods, `d / 40 + i` for a bank
/// period and `d / min(p, 40) + i` for a quota period of `p` plies. The
/// time control is **playable** for a bot when the share is at least its
/// `min_move_secs`.
#[must_use]
pub fn per_move_share(tc: &TimeControl) -> u64 {
    tc.iter()
        .map(|[duration, increment, plies]| {
            let duration = duration.unwrap_or(0);
            let increment = increment.unwrap_or(0);
            let divisor = plies.map_or(40, |p| p.clamp(1, 40));
            duration
                .checked_div(divisor)
                .unwrap_or(0)
                .saturating_add(increment)
        })
        .min()
        .unwrap_or(0)
}

/// The Plies a minute a game may emit at most, for the capacity inequality
/// (§4): `⌈60 / (min_move_secs − margin_ms / 1,000)⌉`.
#[must_use]
pub fn plies_per_minute(min_move_secs: u64, margin_ms: u64) -> u32 {
    let floor_ms = min_move_secs
        .saturating_mul(1000)
        .saturating_sub(margin_ms)
        .max(1);
    let per_minute = 60_000u64.div_ceil(floor_ms);
    u32::try_from(per_minute).unwrap_or(u32::MAX)
}

// ---- the raw file ----

// Every key defaults to the built-in bot's (the `Default` impls below);
// a path left empty is resolved under `data_root()` at conversion.

impl RawConfig {
    fn parse(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::new("", e.message().to_owned()))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawConfig {
    schema: Option<u64>,
    connection: RawConnection,
    identity: RawIdentity,
    profile: RawProfile,
    engine: Option<RawEngine>,
    challenges: RawChallenges,
    play: RawPlay,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawConnection {
    relay: String,
    rules: String,
    data_dir: PathBuf,
    rate_per_minute: u32,
}

impl Default for RawConnection {
    fn default() -> Self {
        Self {
            relay: DEFAULT_RELAY.to_owned(),
            rules: DEFAULT_RULES.to_owned(),
            data_dir: PathBuf::new(),
            rate_per_minute: DEFAULT_RATE_PER_MINUTE,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawIdentity {
    file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawProfile {
    name: String,
    display_name: Option<String>,
    about: String,
    picture: Option<String>,
    nip05: Option<String>,
    website: Option<String>,
}

impl Default for RawProfile {
    fn default() -> Self {
        Self {
            name: "sanki-bot".to_owned(),
            display_name: None,
            about: "A Sanki bot (sashite-sanki-bot).".to_owned(),
            picture: None,
            nip05: None,
            website: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawEngine {
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    options: BTreeMap<String, toml::Value>,
    strength: Option<i64>,
    max_relaunches_per_game: u8,
    launch_ms: u64,
}

impl Default for RawEngine {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            cwd: PathBuf::new(),
            env: BTreeMap::new(),
            options: BTreeMap::new(),
            strength: None,
            max_relaunches_per_game: 2,
            launch_ms: 5000,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawChallenges {
    policy: String,
    follows: Option<Vec<String>>,
    max_delta: Option<u32>,
    rating_authority: Option<String>,
    blocks: Vec<String>,
    outgoing: Option<RawOutgoing>,
}

impl Default for RawChallenges {
    fn default() -> Self {
        Self {
            policy: "everyone".to_owned(),
            follows: None,
            max_delta: None,
            rating_authority: None,
            blocks: Vec::new(),
            outgoing: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutgoing {
    targets: Vec<String>,
    time_control: Vec<Vec<u64>>,
    variant: String,
    every_secs: u64,
    accept_secs: u64,
    max_per_day: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawPlay {
    variants: Vec<String>,
    preferred: String,
    opponents: Vec<String>,
    min_move_secs: u64,
    margin_ms: u64,
    max_concurrent: RawCaps,
    resign: Option<RawResign>,
    offer_draw: Option<RawOfferDraw>,
    accept_draw: Option<RawAcceptDraw>,
}

impl Default for RawPlay {
    fn default() -> Self {
        let all = || ["chess", "ogi", "xiongqi"].map(str::to_owned).to_vec();
        Self {
            variants: all(),
            preferred: "chess".to_owned(),
            opponents: all(),
            min_move_secs: 5,
            margin_ms: 300,
            max_concurrent: RawCaps::default(),
            resign: None,
            offer_draw: None,
            accept_draw: None,
        }
    }
}

// The one table that is not overridden key by key: given, it says what is
// played — a family left out is 0. Absent, the built-in bot's.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCaps {
    byoyomi: Option<u32>,
    blitz: Option<u32>,
    rapid: Option<u32>,
    correspondence: Option<u32>,
}

impl Default for RawCaps {
    fn default() -> Self {
        Self {
            byoyomi: None,
            blitz: Some(1),
            rapid: Some(1),
            correspondence: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResign {
    max_win: u16,
    min_loss: u16,
    streak: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOfferDraw {
    min_draw: u16,
    streak: u8,
    after_ply: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAcceptDraw {
    min_draw: u16,
}

// ---- the conversion ----

fn within<T: PartialOrd + fmt::Display + Copy>(
    key: &str,
    value: T,
    min: T,
    max: T,
) -> Result<T, ConfigError> {
    if value < min || value > max {
        return Err(ConfigError::new(
            key,
            format!("{value} is outside {min}..={max}"),
        ));
    }
    Ok(value)
}

fn pubkey(key: &str, text: &str) -> Result<PublicKey, ConfigError> {
    PublicKey::parse(text)
        .map_err(|_| ConfigError::new(key, format!("{text:?} is not a public key")))
}

fn pubkeys(key: &str, list: &[String]) -> Result<Vec<PublicKey>, ConfigError> {
    if list.len() > 1000 {
        return Err(ConfigError::new(key, "more than 1,000 entries"));
    }
    let mut out = Vec::with_capacity(list.len());
    let mut seen = BTreeSet::new();
    for (index, text) in list.iter().enumerate() {
        let parsed = pubkey(&format!("{key}[{index}]"), text)?;
        if !seen.insert(parsed) {
            return Err(ConfigError::new(format!("{key}[{index}]"), "listed twice"));
        }
        out.push(parsed);
    }
    Ok(out)
}

fn variants(key: &str, list: &[String]) -> Result<NonEmpty<Variant>, ConfigError> {
    if list.is_empty() {
        return Err(ConfigError::new(key, "must not be empty"));
    }
    let mut out = Vec::with_capacity(list.len());
    for (index, name) in list.iter().enumerate() {
        let variant = Variant::parse(name).ok_or_else(|| {
            ConfigError::new(
                format!("{key}[{index}]"),
                format!("{name:?} is not a variant (chess, ogi, xiongqi)"),
            )
        })?;
        if out.contains(&variant) {
            return Err(ConfigError::new(format!("{key}[{index}]"), "listed twice"));
        }
        out.push(variant);
    }
    Ok(NonEmpty(out))
}

fn absolute(key: &str, path: &Path) -> Result<PathBuf, ConfigError> {
    if !path.is_absolute() {
        return Err(ConfigError::new(
            key,
            format!("{} is not absolute", path.display()),
        ));
    }
    Ok(path.to_owned())
}

/// Resolves `command`: a path (a relative one from the current directory,
/// made absolute), or a name looked up in the bot's own `PATH` at start;
/// either way an existing, executable file.
fn resolve_command(command: &str) -> Result<PathBuf, ConfigError> {
    let key = "engine.command";
    let candidates: Vec<PathBuf> = if Path::new(command).is_absolute() {
        vec![PathBuf::from(command)]
    } else if command.contains('/') {
        vec![std::fs::canonicalize(command)
            .map_err(|e| ConfigError::new(key, format!("{command:?}: {e}")))?]
    } else {
        std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .filter(|dir| dir.is_absolute())
                    .map(|dir| dir.join(command))
                    .collect()
            })
            .unwrap_or_default()
    };
    for candidate in &candidates {
        if let Ok(metadata) = std::fs::metadata(candidate) {
            use std::os::unix::fs::PermissionsExt;
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return Ok(candidate.clone());
            }
        }
    }
    Err(ConfigError::new(
        key,
        format!("{command:?} is not an existing executable file"),
    ))
}

/// A kind-`3420` period from its numbers: `[duration]`,
/// `[duration, increment]` or `[duration, increment, plies]`.
fn period(key: &str, row: &[u64]) -> Result<[Option<u64>; 3], ConfigError> {
    match *row {
        [duration] if duration > 0 => Ok([Some(duration), None, None]),
        [duration, increment] if duration > 0 => Ok([Some(duration), Some(increment), None]),
        [duration, increment, plies] if plies > 0 => {
            Ok([Some(duration), Some(increment), Some(plies)])
        }
        [_, _, 0] => Err(ConfigError::new(key, "plies must be positive")),
        [0] | [0, _] => Err(ConfigError::new(
            key,
            "a duration of 0 needs a per-move form [0, increment, plies]",
        )),
        _ => Err(ConfigError::new(
            key,
            "a period is [duration], [duration, increment] or [duration, increment, plies]",
        )),
    }
}

/// `[engine]`, checked against the data directory and the key's.
fn engine(
    raw_engine: RawEngine,
    data_dir: &Path,
    identity_file: &Path,
) -> Result<EngineConfig, ConfigError> {
    if raw_engine.command.is_empty() {
        return Err(ConfigError::new("engine.command", "empty"));
    }
    let command = resolve_command(&raw_engine.command)?;
    let cwd = if raw_engine.cwd.as_os_str().is_empty() {
        data_root()?.join("engine")
    } else {
        absolute("engine.cwd", &raw_engine.cwd)?
    };
    if cwd.starts_with(data_dir) {
        return Err(ConfigError::new(
            "engine.cwd",
            format!("{} is under connection.data_dir", cwd.display()),
        ));
    }
    if let Some(key_dir) = identity_file.parent() {
        if cwd == key_dir || cwd.starts_with(key_dir) && key_dir != Path::new("/") {
            return Err(ConfigError::new(
                "engine.cwd",
                format!("{} is in the directory of identity.file", cwd.display()),
            ));
        }
    }
    let mut env = Vec::with_capacity(raw_engine.env.len());
    for (name, value) in &raw_engine.env {
        if name.is_empty() || name.contains('=') || name.chars().any(char::is_control) {
            return Err(ConfigError::new(
                format!("engine.env.{name}"),
                "not a variable name",
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(ConfigError::new(
                format!("engine.env.{name}"),
                "control characters",
            ));
        }
        env.push((name.clone(), value.clone()));
    }
    let mut options = BTreeMap::new();
    for (name, value) in &raw_engine.options {
        let scalar = match value {
            toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
            toml::Value::Integer(i) => serde_json::Value::from(*i),
            toml::Value::String(s) => serde_json::Value::from(s.as_str()),
            _ => {
                return Err(ConfigError::new(
                    format!("engine.options.{name}"),
                    "a bool, an integer or a string",
                ))
            }
        };
        options.insert(name.clone(), scalar);
    }
    Ok(EngineConfig {
        launch: Launch {
            command,
            args: raw_engine.args,
            cwd,
            env,
        },
        options,
        strength: raw_engine.strength,
        max_relaunches_per_game: within(
            "engine.max_relaunches_per_game",
            raw_engine.max_relaunches_per_game,
            0,
            10,
        )?,
        launch_ms: within("engine.launch_ms", raw_engine.launch_ms, 1000, 60_000)?,
    })
}

impl TryFrom<RawConfig> for Config {
    type Error = ConfigError;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        if let Some(schema) = raw.schema.filter(|s| *s != SCHEMA) {
            return Err(ConfigError::new(
                "schema",
                format!("{schema} is not the schema this bot reads ({SCHEMA})"),
            ));
        }

        // [connection]
        let relay = RelayUrl::parse(&raw.connection.relay)
            .map_err(|e| ConfigError::new("connection.relay", e.to_string()))?;
        let rules = EventId::from_hex(&raw.connection.rules)
            .map_err(|_| ConfigError::new("connection.rules", "not a 64-hex event id"))?;
        let data_dir = if raw.connection.data_dir.as_os_str().is_empty() {
            data_root()?.join("data")
        } else {
            absolute("connection.data_dir", &raw.connection.data_dir)?
        };
        let rate_per_minute = within(
            "connection.rate_per_minute",
            raw.connection.rate_per_minute,
            1,
            1000,
        )?;
        let connection = Connection {
            relay,
            rules,
            data_dir,
            rate_per_minute,
        };

        // [identity]
        let identity_file = if raw.identity.file.as_os_str().is_empty() {
            data_root()?.join("identity/key.nsec")
        } else {
            absolute("identity.file", &raw.identity.file)?
        };

        // [profile]
        let name_len = raw.profile.name.chars().count();
        if !(1..=64).contains(&name_len) {
            return Err(ConfigError::new("profile.name", "1 to 64 characters"));
        }
        if raw.profile.about.chars().count() > 1000 {
            return Err(ConfigError::new(
                "profile.about",
                "at most 1,000 characters",
            ));
        }
        if let Some(display_name) = &raw.profile.display_name {
            if !(1..=64).contains(&display_name.chars().count()) {
                return Err(ConfigError::new(
                    "profile.display_name",
                    "1 to 64 characters",
                ));
            }
        }
        for (key, url) in [
            ("profile.picture", &raw.profile.picture),
            ("profile.website", &raw.profile.website),
        ] {
            if let Some(url) = url {
                if !url.starts_with("https://") || url.len() < 9 {
                    return Err(ConfigError::new(key, "an https URL"));
                }
            }
        }
        if let Some(nip05) = &raw.profile.nip05 {
            let well_formed = nip05.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty()
                    && !domain.is_empty()
                    && domain.contains('.')
                    && nip05.chars().all(|c| !c.is_whitespace())
            });
            if !well_formed {
                return Err(ConfigError::new("profile.nip05", "local@domain"));
            }
        }
        let profile = Profile {
            name: raw.profile.name,
            display_name: raw.profile.display_name,
            about: raw.profile.about,
            picture: raw.profile.picture,
            nip05: raw.profile.nip05,
            website: raw.profile.website,
        };

        // [engine]
        let engine = match raw.engine {
            None => None,
            Some(raw_engine) => Some(engine(raw_engine, &connection.data_dir, &identity_file)?),
        };

        // [play]
        let play_variants = variants("play.variants", &raw.play.variants)?;
        let preferred = Variant::parse(&raw.play.preferred).ok_or_else(|| {
            ConfigError::new("play.preferred", "not a variant (chess, ogi, xiongqi)")
        })?;
        if !play_variants.contains(&preferred) {
            return Err(ConfigError::new("play.preferred", "not in play.variants"));
        }
        let opponents = variants("play.opponents", &raw.play.opponents)?;
        let min_move_secs = within("play.min_move_secs", raw.play.min_move_secs, 2, 3600)?;
        let margin_ms = within("play.margin_ms", raw.play.margin_ms, 100, 5000)?;
        if margin_ms.saturating_add(1000) >= min_move_secs.saturating_mul(1000) {
            return Err(ConfigError::new(
                "play.margin_ms",
                "margin_ms + 1,000 must be below min_move_secs × 1,000",
            ));
        }
        let mut max_concurrent = BTreeMap::new();
        for (cadence, cap) in [
            (Cadence::Byoyomi, raw.play.max_concurrent.byoyomi),
            (Cadence::Blitz, raw.play.max_concurrent.blitz),
            (Cadence::Rapid, raw.play.max_concurrent.rapid),
            (
                Cadence::Correspondence,
                raw.play.max_concurrent.correspondence,
            ),
        ] {
            max_concurrent.insert(cadence, cap.unwrap_or(0));
        }
        let per_mille = |key: &str, value: u16| within(key, value, 0, 1000);
        let resign = match raw.play.resign {
            None => None,
            Some(r) => {
                let max_win = per_mille("play.resign.max_win", r.max_win)?;
                let min_loss = per_mille("play.resign.min_loss", r.min_loss)?;
                if max_win >= min_loss {
                    return Err(ConfigError::new(
                        "play.resign",
                        "max_win must be below min_loss",
                    ));
                }
                Some(Resign {
                    max_win,
                    min_loss,
                    streak: within("play.resign.streak", r.streak, 1, 10)?,
                })
            }
        };
        let offer_draw = match raw.play.offer_draw {
            None => None,
            Some(o) => Some(OfferDraw {
                min_draw: per_mille("play.offer_draw.min_draw", o.min_draw)?,
                streak: within("play.offer_draw.streak", o.streak, 1, 10)?,
                after_ply: within("play.offer_draw.after_ply", o.after_ply, 0, 600)?,
            }),
        };
        let accept_draw = match raw.play.accept_draw {
            None => None,
            Some(a) => Some(AcceptDraw {
                min_draw: per_mille("play.accept_draw.min_draw", a.min_draw)?,
            }),
        };
        if engine.is_none() {
            for (key, set) in [
                ("play.resign", resign.is_some()),
                ("play.offer_draw", offer_draw.is_some()),
                ("play.accept_draw", accept_draw.is_some()),
            ] {
                if set {
                    return Err(ConfigError::new(
                        key,
                        "needs an [engine]: without one there is no score to decide on",
                    ));
                }
            }
        }
        let play = Play {
            variants: play_variants,
            preferred,
            opponents,
            min_move_secs,
            margin_ms,
            max_concurrent,
            resign,
            offer_draw,
            accept_draw,
        };

        // [challenges]
        let blocks = pubkeys("challenges.blocks", &raw.challenges.blocks)?;
        let policy = match raw.challenges.policy.as_str() {
            "everyone" => Policy::Everyone,
            "nobody" => Policy::Nobody,
            "following" => {
                let follows = raw.challenges.follows.as_deref().ok_or_else(|| {
                    ConfigError::new("challenges.follows", "required by policy = following")
                })?;
                let follows = pubkeys("challenges.follows", follows)?;
                if follows.is_empty() {
                    return Err(ConfigError::new("challenges.follows", "must not be empty"));
                }
                if let Some(both) = follows.iter().find(|k| blocks.contains(k)) {
                    return Err(ConfigError::new(
                        "challenges.follows",
                        format!("{} is also in challenges.blocks", both.to_hex()),
                    ));
                }
                Policy::Following(NonEmpty(follows))
            }
            "rating" => {
                let max_delta = raw.challenges.max_delta.ok_or_else(|| {
                    ConfigError::new("challenges.max_delta", "required by policy = rating")
                })?;
                let authority = raw.challenges.rating_authority.as_deref().ok_or_else(|| {
                    ConfigError::new("challenges.rating_authority", "required by policy = rating")
                })?;
                Policy::Rating {
                    max_delta: within("challenges.max_delta", max_delta, 1, 1000)?,
                    authority: pubkey("challenges.rating_authority", authority)?,
                }
            }
            other => {
                return Err(ConfigError::new(
                    "challenges.policy",
                    format!("{other:?} is not everyone, following, rating or nobody"),
                ))
            }
        };
        // A key of another policy is a decision no one made.
        for (key, present) in [
            (
                "challenges.follows",
                raw.challenges.follows.is_some() && !matches!(policy, Policy::Following(_)),
            ),
            (
                "challenges.max_delta",
                raw.challenges.max_delta.is_some() && !matches!(policy, Policy::Rating { .. }),
            ),
            (
                "challenges.rating_authority",
                raw.challenges.rating_authority.is_some()
                    && !matches!(policy, Policy::Rating { .. }),
            ),
        ] {
            if present {
                return Err(ConfigError::new(
                    key,
                    format!("set, but the policy is {}", policy.token()),
                ));
            }
        }
        let outgoing = match raw.challenges.outgoing {
            None => None,
            Some(o) => {
                let targets = pubkeys("challenges.outgoing.targets", &o.targets)?;
                if targets.is_empty() {
                    return Err(ConfigError::new(
                        "challenges.outgoing.targets",
                        "must not be empty",
                    ));
                }
                if let Some(both) = targets.iter().find(|k| blocks.contains(k)) {
                    return Err(ConfigError::new(
                        "challenges.outgoing.targets",
                        format!("{} is also in challenges.blocks", both.to_hex()),
                    ));
                }
                if o.time_control.is_empty() {
                    return Err(ConfigError::new(
                        "challenges.outgoing.time_control",
                        "at least one period",
                    ));
                }
                let mut time_control = Vec::with_capacity(o.time_control.len());
                for (index, row) in o.time_control.iter().enumerate() {
                    time_control.push(period(
                        &format!("challenges.outgoing.time_control[{index}]"),
                        row,
                    )?);
                }
                let share = per_move_share(&time_control);
                if share < play.min_move_secs {
                    return Err(ConfigError::new(
                        "challenges.outgoing.time_control",
                        format!(
                            "offers {share} s a move, below play.min_move_secs ({})",
                            play.min_move_secs
                        ),
                    ));
                }
                let rows: Vec<Vec<String>> = time_control
                    .iter()
                    .map(|p| p.iter().flatten().map(u64::to_string).collect())
                    .collect();
                let cadence = Cadence::of_rows(&rows).ok_or_else(|| {
                    ConfigError::new("challenges.outgoing.time_control", "no cadence")
                })?;
                if play.cap(cadence) == 0 {
                    return Err(ConfigError::new(
                        "challenges.outgoing.time_control",
                        format!(
                            "its family, {}, has no cap in play.max_concurrent",
                            cadence.token()
                        ),
                    ));
                }
                let variant = Variant::parse(&o.variant).ok_or_else(|| {
                    ConfigError::new(
                        "challenges.outgoing.variant",
                        "not a variant (chess, ogi, xiongqi)",
                    )
                })?;
                if !play.variants.contains(&variant) || !play.opponents.contains(&variant) {
                    return Err(ConfigError::new(
                        "challenges.outgoing.variant",
                        "must be in both play.variants and play.opponents",
                    ));
                }
                Some(Outgoing {
                    targets: NonEmpty(targets),
                    time_control,
                    cadence,
                    variant,
                    every_secs: within(
                        "challenges.outgoing.every_secs",
                        o.every_secs,
                        60,
                        u64::MAX,
                    )?,
                    accept_secs: within(
                        "challenges.outgoing.accept_secs",
                        o.accept_secs,
                        30,
                        3600,
                    )?,
                    max_per_day: within("challenges.outgoing.max_per_day", o.max_per_day, 1, 1000)?,
                })
            }
        };
        let challenges = Challenges {
            policy,
            blocks,
            outgoing,
        };

        // Caps and capacity.
        let total = play.total_cap();
        if total == 0 && (challenges.policy != Policy::Nobody || challenges.outgoing.is_some()) {
            return Err(ConfigError::new(
                "play.max_concurrent",
                "every cap is 0: the policy must be nobody and there must be no [challenges.outgoing]",
            ));
        }
        let plies = plies_per_minute(play.min_move_secs, play.margin_ms);
        let demand = u64::from(total)
            .saturating_mul(u64::from(plies))
            .saturating_add(u64::from(challenges.outgoing.is_some()));
        let rate = u64::from(connection.rate_per_minute);
        let reserve = rate.div_ceil(10);
        let supply = rate.saturating_sub(reserve);
        if demand > supply {
            return Err(ConfigError::new(
                "play.max_concurrent",
                format!(
                    "capacity: {total} games × {plies} Plies a minute{} = {demand}, above {supply} (rate_per_minute {rate} less the reserve {reserve})",
                    if challenges.outgoing.is_some() { " + 1 challenge" } else { "" }
                ),
            ));
        }

        Ok(Self {
            connection,
            identity_file,
            profile,
            engine,
            challenges,
            play,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;

    const KEY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const KEY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// The example of ADR-0045 §4, without an engine and without outgoing
    /// challenges (they need files and keys the test sets itself).
    fn example() -> String {
        format!(
            r#"
schema = 1

[connection]
relay           = "wss://relay.sanki.app"
rules           = "{}"
data_dir        = "/var/lib/sanki-bot"
rate_per_minute = 30

[identity]
file = "/var/lib/sanki-bot/key.nsec"

[profile]
name    = "kitsune"
about   = "a bot"
picture = "https://example.com/kitsune.png"

[challenges]
policy = "everyone"
blocks = []

[play]
variants      = ["chess"]
preferred     = "chess"
opponents     = ["chess", "ogi", "xiongqi"]
min_move_secs = 5
margin_ms     = 300

  [play.max_concurrent]
  byoyomi        = 0
  blitz          = 1
  rapid          = 1
  correspondence = 0
"#,
            "7".repeat(64)
        )
    }

    fn with(extra: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!("{}\n{extra}", example()))
    }

    fn replaced(from: &str, to: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&example().replace(from, to))
    }

    #[test]
    fn the_example_reads() {
        let config = Config::from_toml(&example()).unwrap();
        assert_eq!(config.connection().rate_per_minute, 30);
        assert_eq!(
            config.connection().relay.to_string(),
            "wss://relay.sanki.app"
        );
        assert_eq!(config.play().cap(Cadence::Blitz), 1);
        assert_eq!(config.play().total_cap(), 2);
        assert_eq!(config.play().pairings().len(), 5); // WW, WJ, JW, WC, CW
        assert!(config.engine().is_none());
        assert_eq!(config.challenges().policy, Policy::Everyone);
        assert_eq!(plies_per_minute(5, 300), 13);
    }

    #[test]
    fn unknown_keys_are_refused_and_missing_keys_default() {
        let err = replaced(
            "min_move_secs = 5\n",
            "min_move_secs = 5\nthinking = true\n",
        )
        .unwrap_err();
        assert!(err.reason.contains("thinking"), "{err}");
        // A key left out takes its default.
        let config = replaced("min_move_secs = 5\n", "").unwrap();
        assert_eq!(config.play().min_move_secs, 5);
        let err = replaced("schema = 1", "schema = 2").unwrap_err();
        assert_eq!(err.key, "schema");
    }

    #[test]
    fn the_built_in_bot_is_the_example_and_an_empty_file() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/sanki-bot.example.toml"
        ))
        .unwrap();
        let example = Config::from_toml(&text).unwrap();
        let defaults = Config::defaults().unwrap();
        assert_eq!(example, defaults);
        assert_eq!(Config::from_toml("").unwrap(), defaults);
        assert_eq!(defaults.connection().relay.to_string(), DEFAULT_RELAY);
        assert_eq!(defaults.connection().rules.to_hex(), DEFAULT_RULES);
        assert_eq!(defaults.profile().name, "sanki-bot");
        assert_eq!(defaults.profile().picture, None);
        assert!(defaults.engine().is_none());
        assert_eq!(defaults.play().total_cap(), 2);
        // The paths, under the data root.
        let root = data_root().unwrap();
        assert_eq!(defaults.connection().data_dir, root.join("data"));
        assert_eq!(defaults.identity_file(), root.join("identity/key.nsec"));
        // A section given overrides only what it says — but the caps table
        // says what is played: a family left out of it is 0.
        let config = Config::from_toml("[profile]\nname = \"kitsune\"\n").unwrap();
        assert_eq!(config.profile().name, "kitsune");
        assert_eq!(config.profile().about, defaults.profile().about);
        let config = Config::from_toml("[play.max_concurrent]\nblitz = 2\n").unwrap();
        assert_eq!(config.play().cap(Cadence::Blitz), 2);
        assert_eq!(config.play().cap(Cadence::Byoyomi), 0);
    }

    #[test]
    fn the_engine_from_the_command_line() {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
        let argv = vec![sh.clone(), "-c".to_owned(), "cat".to_owned()];
        let config = Config::load(None, Some(&argv)).unwrap();
        let engine = config.engine().unwrap();
        assert_eq!(engine.launch.command, PathBuf::from(&sh));
        assert_eq!(engine.launch.args, vec!["-c", "cat"]);
        assert_eq!(engine.launch.cwd, data_root().unwrap().join("engine"));
        assert_eq!(engine.max_relaunches_per_game, 2);
        assert_eq!(engine.launch_ms, 5000);
        // A name in PATH.
        let config = Config::load(None, Some(&["sh".to_owned()])).unwrap();
        assert!(config.engine().unwrap().launch.command.is_absolute());
        let err = Config::load(None, Some(&["no-such-engine-anywhere".to_owned()])).unwrap_err();
        assert_eq!(err.key, "engine.command");
        assert_eq!(
            Config::load(None, Some(&[])).unwrap_err().key,
            "engine.command"
        );
        // A file's score policies, with the engine from the command line.
        let dir = std::env::temp_dir().join(format!("sanki-bot-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bot.toml");
        std::fs::write(
            &file,
            "[play.resign]\nmax_win = 20\nmin_loss = 900\nstreak = 3\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(Some(&file), None).unwrap_err().key,
            "play.resign"
        );
        let config = Config::load(Some(&file), Some(&argv)).unwrap();
        assert!(config.play().resign.is_some());
        assert_eq!(config.engine().unwrap().launch.args, vec!["-c", "cat"]);
        // A file's own [engine] is replaced whole.
        std::fs::write(&file, "[engine]\ncommand = \"nowhere\"\nlaunch_ms = 9000\n").unwrap();
        let config = Config::load(Some(&file), Some(&argv)).unwrap();
        assert_eq!(config.engine().unwrap().launch_ms, 5000);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_profile_fields() {
        let config = Config::from_toml(
            "[profile]\nname = \"kitsune\"\ndisplay_name = \"Kitsune\"\npicture = \"https://example.com/k.png\"\nnip05 = \"kitsune@sanki.app\"\nwebsite = \"https://chess.page/\"\n",
        )
        .unwrap();
        assert_eq!(config.profile().display_name.as_deref(), Some("Kitsune"));
        assert_eq!(
            config.profile().website.as_deref(),
            Some("https://chess.page/")
        );
        for (text, key) in [
            (
                "[profile]\nwebsite = \"http://chess.page/\"\n",
                "profile.website",
            ),
            ("[profile]\npicture = \"ftp://x\"\n", "profile.picture"),
            ("[profile]\ndisplay_name = \"\"\n", "profile.display_name"),
            ("[profile]\nnip05 = \"kitsune\"\n", "profile.nip05"),
        ] {
            assert_eq!(Config::from_toml(text).unwrap_err().key, key, "{text}");
        }
    }

    #[test]
    fn values_have_their_domains() {
        assert_eq!(
            replaced("rate_per_minute = 30", "rate_per_minute = 0")
                .unwrap_err()
                .key,
            "connection.rate_per_minute"
        );
        assert_eq!(
            replaced("min_move_secs = 5", "min_move_secs = 1")
                .unwrap_err()
                .key,
            "play.min_move_secs"
        );
        assert_eq!(
            replaced("margin_ms     = 300", "margin_ms     = 4500")
                .unwrap_err()
                .key,
            "play.margin_ms"
        );
        assert_eq!(
            replaced("preferred     = \"chess\"", "preferred     = \"ogi\"")
                .unwrap_err()
                .key,
            "play.preferred"
        );
        assert_eq!(
            replaced(
                "data_dir        = \"/var/lib/sanki-bot\"",
                "data_dir        = \"lib\""
            )
            .unwrap_err()
            .key,
            "connection.data_dir"
        );
        assert_eq!(
            replaced(
                "picture = \"https://example.com/kitsune.png\"",
                "picture = \"http://x/y\""
            )
            .unwrap_err()
            .key,
            "profile.picture"
        );
        assert_eq!(
            replaced(
                "relay           = \"wss://relay.sanki.app\"",
                "relay           = \"relay\""
            )
            .unwrap_err()
            .key,
            "connection.relay"
        );
        assert_eq!(
            replaced(
                "opponents     = [\"chess\", \"ogi\", \"xiongqi\"]",
                "opponents     = [\"chess\", \"chess\"]"
            )
            .unwrap_err()
            .key,
            "play.opponents[1]"
        );
    }

    #[test]
    fn policies_and_lists() {
        let following = replaced(
            "policy = \"everyone\"",
            &format!("policy = \"following\"\nfollows = [\"{KEY_A}\"]"),
        )
        .unwrap();
        assert!(matches!(
            following.challenges().policy,
            Policy::Following(_)
        ));
        assert_eq!(
            replaced("policy = \"everyone\"", "policy = \"following\"")
                .unwrap_err()
                .key,
            "challenges.follows"
        );
        assert_eq!(
            replaced(
                "policy = \"everyone\"",
                "policy = \"following\"\nfollows = []"
            )
            .unwrap_err()
            .key,
            "challenges.follows"
        );
        assert_eq!(
            replaced(
                "policy = \"everyone\"\nblocks = []",
                &format!("policy = \"following\"\nfollows = [\"{KEY_A}\"]\nblocks = [\"{KEY_A}\"]")
            )
            .unwrap_err()
            .key,
            "challenges.follows"
        );
        // A key of another policy.
        assert_eq!(
            replaced(
                "policy = \"everyone\"",
                "policy = \"everyone\"\nmax_delta = 3"
            )
            .unwrap_err()
            .key,
            "challenges.max_delta"
        );
        let rating = replaced(
            "policy = \"everyone\"",
            &format!("policy = \"rating\"\nmax_delta = 200\nrating_authority = \"{KEY_B}\""),
        )
        .unwrap();
        assert!(matches!(
            rating.challenges().policy,
            Policy::Rating { max_delta: 200, .. }
        ));
        assert_eq!(
            replaced("policy = \"everyone\"", "policy = \"friends\"")
                .unwrap_err()
                .key,
            "challenges.policy"
        );
        // Duplicates.
        assert_eq!(
            replaced(
                "blocks = []",
                &format!("blocks = [\"{KEY_A}\", \"{KEY_A}\"]")
            )
            .unwrap_err()
            .key,
            "challenges.blocks[1]"
        );
        // The bot's own key.
        let config = replaced("blocks = []", &format!("blocks = [\"{KEY_A}\"]")).unwrap();
        let me = PublicKey::from_hex(KEY_A).unwrap();
        assert_eq!(config.excludes(&me).unwrap_err().key, "challenges.blocks");
        assert!(config
            .excludes(&PublicKey::from_hex(KEY_B).unwrap())
            .is_ok());
    }

    #[test]
    fn caps_and_capacity() {
        // Every cap at 0 needs nobody and no outgoing.
        let err = replaced(
            "blitz          = 1\n  rapid          = 1",
            "blitz          = 0\n  rapid          = 0",
        )
        .unwrap_err();
        assert_eq!(err.key, "play.max_concurrent");
        let quiet = Config::from_toml(
            &example()
                .replace(
                    "blitz          = 1\n  rapid          = 1",
                    "blitz          = 0\n  rapid          = 0",
                )
                .replace("policy = \"everyone\"", "policy = \"nobody\""),
        )
        .unwrap();
        assert_eq!(quiet.play().total_cap(), 0);
        // Three families at 3 s: 60 Plies a minute, above 27.
        let err = Config::from_toml(
            &example()
                .replace("min_move_secs = 5", "min_move_secs = 3")
                .replace("byoyomi        = 0", "byoyomi        = 1"),
        )
        .unwrap_err();
        assert_eq!(err.key, "play.max_concurrent");
        assert!(err.reason.contains("capacity"), "{err}");
        // Two games at 5 s with a 300 ms margin: 26, within 27.
        assert!(Config::from_toml(&example()).is_ok());
        // A sandbox relay with a higher rate plays more.
        assert!(Config::from_toml(
            &example()
                .replace("min_move_secs = 5", "min_move_secs = 3")
                .replace("byoyomi        = 0", "byoyomi        = 1")
                .replace("rate_per_minute = 30", "rate_per_minute = 120")
        )
        .is_ok());
    }

    #[test]
    fn score_policies_need_an_engine() {
        let err = with("[play.resign]\nmax_win = 20\nmin_loss = 900\nstreak = 3").unwrap_err();
        assert_eq!(err.key, "play.resign");
        assert!(err.reason.contains("engine"));
    }

    #[test]
    fn the_engine_section() {
        let dir = std::env::temp_dir().join(format!("sanki-bot-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sh = which_sh();
        let engine = format!(
            "[engine]\ncommand = \"{sh}\"\nargs = [\"-c\", \"cat\"]\ncwd = \"{}\"\nenv = {{ RUST_LOG = \"info\" }}\noptions = {{ threads = 2, book = true, style = \"solid\" }}\nmax_relaunches_per_game = 2\nlaunch_ms = 5000\n",
            dir.display()
        );
        let config = with(&engine).unwrap();
        let e = config.engine().unwrap();
        assert_eq!(e.launch.command, PathBuf::from(&sh));
        assert_eq!(e.launch.args, vec!["-c", "cat"]);
        assert_eq!(e.options["threads"], 2);
        assert_eq!(e.options["book"], true);
        assert_eq!(e.strength, None);
        // With an engine, a score policy is allowed.
        assert!(with(&format!(
            "{engine}\n[play.resign]\nmax_win = 20\nmin_loss = 900\nstreak = 3"
        ))
        .is_ok());
        assert_eq!(
            with(&format!(
                "{engine}\n[play.resign]\nmax_win = 900\nmin_loss = 20\nstreak = 3"
            ))
            .unwrap_err()
            .key,
            "play.resign"
        );
        // A name in PATH resolves; a missing program does not.
        assert!(with(&engine.replace(&format!("command = \"{sh}\""), "command = \"sh\"")).is_ok());
        assert_eq!(
            with(&engine.replace(
                &format!("command = \"{sh}\""),
                "command = \"/nonexistent/engine\""
            ))
            .unwrap_err()
            .key,
            "engine.command"
        );
        // A cwd under data_dir or the key's directory.
        assert_eq!(
            with(&engine.replace(
                &format!("cwd = \"{}\"", dir.display()),
                "cwd = \"/var/lib/sanki-bot/engine\""
            ))
            .unwrap_err()
            .key,
            "engine.cwd"
        );
        // A non-scalar option.
        assert_eq!(
            with(&engine.replace("style = \"solid\"", "style = [1]"))
                .unwrap_err()
                .key,
            "engine.options.style"
        );
        // Domains.
        assert_eq!(
            with(&engine.replace("launch_ms = 5000", "launch_ms = 500"))
                .unwrap_err()
                .key,
            "engine.launch_ms"
        );
        assert_eq!(
            with(&engine.replace(
                "max_relaunches_per_game = 2",
                "max_relaunches_per_game = 11"
            ))
            .unwrap_err()
            .key,
            "engine.max_relaunches_per_game"
        );
    }

    fn which_sh() -> String {
        for candidate in ["/bin/sh", "/usr/bin/sh"] {
            if Path::new(candidate).exists() {
                return candidate.to_owned();
            }
        }
        panic!("no sh");
    }

    #[test]
    fn outgoing_challenges() {
        let outgoing = format!(
            "[challenges.outgoing]\ntargets = [\"{KEY_A}\"]\ntime_control = [[180, 2]]\nvariant = \"chess\"\nevery_secs = 600\naccept_secs = 120\nmax_per_day = 24\n"
        );
        let config = with(&outgoing).unwrap();
        let o = config.challenges().outgoing.as_ref().unwrap();
        assert_eq!(o.cadence, Cadence::Blitz);
        assert_eq!(o.time_control, vec![[Some(180), Some(2), None]]);
        // Not playable: 3 + 2 offers 6.5 s a move at min_move_secs 5 — playable;
        // 1 + 0 offers 1.5 s: refused.
        assert_eq!(
            with(&outgoing.replace("[[180, 2]]", "[[60]]"))
                .unwrap_err()
                .key,
            "challenges.outgoing.time_control"
        );
        // Its family has no cap: byōyomi is not played by the built-in bot.
        assert_eq!(
            with(&outgoing.replace("[[180, 2]]", "[[0, 10, 1]]"))
                .unwrap_err()
                .key,
            "challenges.outgoing.time_control"
        );
        // Rapid, 15 + 10: playable, family capped.
        assert!(with(&outgoing.replace("[[180, 2]]", "[[900, 10]]")).is_ok());
        assert_eq!(
            with(&outgoing.replace("[[180, 2]]", "[[0, 10]]"))
                .unwrap_err()
                .key,
            "challenges.outgoing.time_control[0]"
        );
        assert_eq!(
            with(&outgoing.replace("[[180, 2]]", "[[180, 2, 0]]"))
                .unwrap_err()
                .key,
            "challenges.outgoing.time_control[0]"
        );
        // The variant must be on both sides.
        assert_eq!(
            with(&outgoing.replace("variant = \"chess\"", "variant = \"ogi\""))
                .unwrap_err()
                .key,
            "challenges.outgoing.variant"
        );
        // Targets: non-empty, not blocked, and capacity counts the challenge.
        assert_eq!(
            with(&outgoing.replace(&format!("targets = [\"{KEY_A}\"]"), "targets = []"))
                .unwrap_err()
                .key,
            "challenges.outgoing.targets"
        );
        assert_eq!(
            with(&outgoing.replace("every_secs = 600", "every_secs = 30"))
                .unwrap_err()
                .key,
            "challenges.outgoing.every_secs"
        );
        // 2 games × 13 + 1 = 27 ≤ 27: still fits; at rate 29 (supply 26) it does not.
        assert_eq!(
            Config::from_toml(&format!(
                "{}\n{outgoing}",
                example().replace("rate_per_minute = 30", "rate_per_minute = 29")
            ))
            .unwrap_err()
            .key,
            "play.max_concurrent"
        );
    }

    #[test]
    fn the_per_move_share() {
        assert_eq!(per_move_share(&[[Some(180), Some(2), None]]), 6);
        assert_eq!(per_move_share(&[[Some(0), Some(10), Some(1)]]), 10);
        assert_eq!(
            per_move_share(&[
                [Some(5400), Some(30), Some(40)],
                [Some(1800), Some(30), None]
            ]),
            75
        );
        assert_eq!(
            per_move_share(&[[Some(3600), None, None], [Some(0), Some(30), Some(1)]]),
            30
        );
        assert_eq!(per_move_share(&[]), 0);
    }
}
