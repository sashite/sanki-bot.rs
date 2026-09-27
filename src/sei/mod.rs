// SPDX-License-Identifier: Apache-2.0
//! The SEI host (ADR-0045 §3; [SEI 1.0.0](https://sashite.dev/specs/sei/1.0.0/)
//! §5, §8, §11): the engine is a program named by the configuration,
//! launched as a child process, spoken to in JSON Lines over its standard
//! streams, and killed when the bot has no more use for it. One process per
//! game.
//!
//! | Module | What it does |
//! |---|---|
//! | [`process`] | the engine's process: launch as SEI §5 prescribes, the reader of its output, the drain of its error stream, the end of the session |
//! | [`wire`] | the lines: events read tolerantly, requests written strictly, strings escaped |
//! | [`announce`] | what the engine announces in `hello`: version, rules and pairings, features, options — and the gaps a host finds in it |
//! | [`mod@probe`] | the opening (`hello`, `configure`, `ping` within `launch_ms`) and the probe at start (the gaps, `engine_rtt`) |
//! | [`mod@clock`] | the session's clocks as SEI's `clock`, at the emission |
//! | [`mod@turn`] | one search: the provisional answer, the `done`, the refusals, the hard stop with `cancel`, liveness by `ping` |
//!
//! **Trust.** The bot trusts the engine for nothing: its output is
//! validated by the caller's judge (the module's `legal_moves`, the
//! notation's `parse_canonical`), its liveness is measured, its process is
//! disposable, and it receives no secret through any channel the bot
//! controls — arguments, environment, descriptors, messages.

pub mod announce;
pub mod clock;
pub mod probe;
pub mod process;
pub mod turn;
pub mod wire;

use std::fmt;

pub use announce::{Announcement, Features, OptionSpec, RULES};
pub use probe::{open, probe, Needs, Probe, ProbeError};
pub use process::{Engine, Launch};
pub use turn::{search, Advice, Answer, Score, SearchRequest, Turn, Verdict};
pub use wire::{Code, EngineError};

/// How the engine's diagnostics name the host (SEI §8.1 `host`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The host's name.
    pub name: String,
    /// Its version.
    pub version: String,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            name: "sanki-bot".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Why an engine is no longer usable (ADR-0045 §3 *Engine failure*): its
/// process is ended, and the runtime's relaunch policy decides what
/// follows.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineFailure {
    /// The process could not be started.
    Launch(String),
    /// The engine ended the session on its own: its output closed, or its
    /// input can no longer be written (SEI §5 *Unexpected end*).
    Exited,
    /// The engine emitted an error without `re` (SEI §7, rule 7).
    Fatal(EngineError),
    /// The engine broke the protocol (SEI §11): a line that is not an event,
    /// an event attached to no pending request, a `best` that is not a legal
    /// canonical move.
    Violation(String),
    /// A `ping` went unanswered beyond the grace (SEI §8.2).
    Unresponsive,
    /// No `done` within the grace after `cancel` (SEI §8.4 *Overrun*).
    Overrun,
    /// `hello` or `configure` was late or refused (ADR-0045 §3 *Opening*).
    Opening(String),
}

impl fmt::Display for EngineFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launch(reason) => write!(f, "the engine could not be launched: {reason}"),
            Self::Exited => f.write_str("the engine ended the session"),
            Self::Fatal(error) => write!(f, "the engine failed fatally: {error}"),
            Self::Violation(reason) => write!(f, "the engine violated the protocol: {reason}"),
            Self::Unresponsive => f.write_str("the engine stopped answering ping"),
            Self::Overrun => f.write_str("the engine gave no done within the grace after cancel"),
            Self::Opening(reason) => write!(f, "the engine did not open: {reason}"),
        }
    }
}

impl std::error::Error for EngineFailure {}
