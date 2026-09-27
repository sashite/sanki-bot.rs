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
//! | [`identity`] | §2 | the key, read from a private file and never given out; the derived secrets; the lease on the host |
//! | [`sei`] | §3 | the SEI host: the engine's process, the opening, the probe, the search of a turn with its clock, the safety net, the hard stop, the failures |
//! | [`fallback`] | §3 | the move the bot plays with no answer: `HMAC(k_fallback, session ‖ step)` over the module's legal moves |
//!
//! The protocol — the rule system, the session views, the notation, the
//! relay, publishing — is `sashite-sanki-client`; this crate decides.

pub mod config;
pub mod fallback;
pub mod identity;
pub mod sei;
