// SPDX-License-Identifier: Apache-2.0
//! `sashite-sanki-bot` — one bot: an identity, a configuration and an SEI
//! engine command, in one process (ADR-0045).
//!
//! The library is built module by module beside the fleet's binary
//! (`players`), which keeps running the Robotto until the v4 runtime exists:
//!
//! | Module | ADR-0045 | What it does |
//! |---|---|---|
//! | [`config`] | §4 | one TOML file, `RawConfig` → `Config`: the types forbid the incoherent cases, capacity included |
//! | [`admit`] | §5 | the local checks on a Direct Challenge, in order, without I/O: an `Admission` or a `Refusal` |
//! | [`outgoing`] | §5 | which target is due for a challenge, and when to fire within the minute |
//! | [`policy`] | §7 | draw offers, draw acceptance and resignation, decided on the current turn's evaluation and its streaks |
//! | [`game`] | §7 | one open session: re-derived through the module on every event and timer, its engine process, the turn, the Conclusions |
//! | [`identity`] | §2 | the key, read from a private file and never given out; the derived secrets |
//! | [`sei`] | §3 | the SEI host: the engine's process, the opening, the probe, the search of a turn with its clock, the safety net, the hard stop, the failures |
//! | [`fallback`] | §3 | the move the bot plays with no answer: `HMAC(k_fallback, session ‖ step)` over the module's legal moves |
//!
//! The protocol — the rule system, the session views, the notation, the
//! relay, publishing — is `sashite-sanki-client`; this crate decides.

pub mod admit;
pub mod config;
pub mod fallback;
pub mod game;
pub mod identity;
pub mod outgoing;
pub mod policy;
pub mod sei;
