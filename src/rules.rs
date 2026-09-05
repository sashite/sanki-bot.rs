// SPDX-License-Identifier: Apache-2.0
//! The rule system the fleet plays under — a **Rule System** event (kind `3417`,
//! ADR-0034) naming, by digest, the executable module a session is played under
//! — loaded in the two verifiable hops of kind `3417` §Retrieval and
//! verification: the event by id (signature, kind, structural constraints), the
//! module by digest (its SHA-256 equal to the event's `x`), then instantiated
//! and asked to state its ABI (equal to the event's `abi` tag) and its game
//! (`describe`, equal to the event's `game` tag). Nothing is believed: a module
//! is what it computes, and its bytes are what they hash to.
//!
//! Both the event and the module are **cached on disk** by id and by digest
//! (`<id>.json`, `<x>.wasm` under the cache directory), so that once loaded the
//! fleet no longer depends on any relay or blob host to play. A file dropped in
//! the cache by hand is used as is, after the same digest check as a fetched
//! one.
//!
//! The rule system is fixed at start-up from the fleet file's `rules` (one
//! event id): a client MUST hold and have instantiated the module before
//! entering a pool, challenging, founding or accepting (kind `3417`
//! §Retrieval and verification), so a fleet that cannot load it does not start.

use crate::module::{self, Describe, Runtime, ABI};
use nostchmaker::rule_system::{RuleSystem, RuleSystemError};
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Rule System kind (ADR-0034).
pub const KIND_RULE_SYSTEM: u16 = 3417;

/// How long to wait for the relay to answer a Rule System lookup.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to wait for a blob host to deliver a module.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// A module larger than this is refused unread: the reference module is under
/// a megabyte, and a conforming one is bounded by its fixed memory anyway.
const MODULE_SIZE_BOUND: usize = 64 * 1024 * 1024;

/// Why a rule system could not be loaded.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The event is neither cached nor retrievable from the relay.
    EventUnavailable(EventId),
    /// The cached or fetched event is not the one asked for, or its signature
    /// is invalid.
    EventInvalid(EventId),
    /// The event is not a conforming Rule System.
    NonConforming(RuleSystemError),
    /// The event's `abi` tag is not the ABI this bot implements.
    Abi(String),
    /// No copy of the module — cached or from the event's `url` hints — has the
    /// digest the event names. Carries the digest and what was tried.
    ModuleUnavailable {
        /// The event's `x` tag.
        digest: String,
        /// One line per attempt.
        attempts: Vec<String>,
    },
    /// The module does not instantiate as a conforming one.
    Module(module::ModuleError),
    /// The module does not answer `describe`.
    Describe(module::AskError),
    /// The module's `describe` disagrees with the event's tags. Carries the
    /// disagreement.
    Mismatch(String),
    /// The cache directory cannot be used.
    Cache(std::io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventUnavailable(id) => write!(f, "Rule System event {id} is not retrievable"),
            Self::EventInvalid(id) => write!(
                f,
                "Rule System event {id} is not the event asked for, or its signature is invalid"
            ),
            Self::NonConforming(err) => write!(f, "not a conforming Rule System: {err}"),
            Self::Abi(abi) => write!(
                f,
                "the Rule System states ABI {abi:?}; this bot implements {ABI}"
            ),
            Self::ModuleUnavailable { digest, attempts } => {
                write!(f, "no module with digest {digest} could be obtained")?;
                for attempt in attempts {
                    write!(f, "; {attempt}")?;
                }
                Ok(())
            }
            Self::Module(err) => write!(f, "{err}"),
            Self::Describe(err) => write!(f, "the module does not describe itself: {err}"),
            Self::Mismatch(reason) => write!(f, "the module disagrees with its event: {reason}"),
            Self::Cache(err) => write!(f, "the rules cache is unusable: {err}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// The rule system the fleet plays under: its event, what its module reports,
/// and the module ready for requests.
#[derive(Debug)]
pub struct LoadedRuleSystem {
    /// The Rule System event, parsed.
    pub event: RuleSystem,
    /// What the module's `describe` reports (the normative members).
    pub describe: Describe,
    /// The module, instantiated.
    pub runtime: Runtime,
}

/// The SHA-256 of `bytes`, as 64 lowercase hex characters.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Instantiates a module's bytes for `event` and checks what it reports
/// against the event: the bytes hash to `x`, the module states this bot's ABI,
/// and `describe` names the event's `game`. Pure — the caller obtained the
/// bytes.
///
/// # Errors
///
/// The first disagreement, as a [`LoadError`].
pub fn instantiate(event: RuleSystem, bytes: &[u8]) -> Result<LoadedRuleSystem, LoadError> {
    if event.abi() != ABI {
        return Err(LoadError::Abi(event.abi().to_owned()));
    }
    let digest = sha256_hex(bytes);
    if digest != event.digest() {
        return Err(LoadError::ModuleUnavailable {
            digest: event.digest().to_owned(),
            attempts: vec![format!("the bytes offered hash to {digest}")],
        });
    }
    let mut runtime = Runtime::new(bytes).map_err(LoadError::Module)?;
    let describe = module::describe(&mut runtime).map_err(LoadError::Describe)?;
    if describe.abi != ABI {
        return Err(LoadError::Mismatch(format!(
            "describe.abi is {:?}, the module's abi() {ABI:?}",
            describe.abi
        )));
    }
    if describe.game != event.game() {
        return Err(LoadError::Mismatch(format!(
            "describe.game is {:?}, the event's game tag {:?}",
            describe.game,
            event.game()
        )));
    }
    Ok(LoadedRuleSystem {
        event,
        describe,
        runtime,
    })
}

/// Parses and checks a Rule System event obtained from anywhere: the id it
/// was asked for, a valid signature, the kind, the NIP's constraints.
///
/// # Errors
///
/// [`LoadError::EventInvalid`] or [`LoadError::NonConforming`].
pub fn accept_event(id: EventId, event: &Event) -> Result<RuleSystem, LoadError> {
    if event.id != id || event.verify().is_err() {
        return Err(LoadError::EventInvalid(id));
    }
    RuleSystem::parse(event).map_err(LoadError::NonConforming)
}

/// Where a rule system's files live in the cache.
fn event_path(cache: &Path, id: &EventId) -> PathBuf {
    cache.join(format!("{}.json", id.to_hex()))
}

fn module_path(cache: &Path, digest: &str) -> PathBuf {
    cache.join(format!("{digest}.wasm"))
}

/// Writes `bytes` to `path` atomically (a sibling temporary file, then a
/// rename), so a reader never sees a partial file.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The cached event, if the cache holds a valid one.
fn cached_event(cache: &Path, id: EventId) -> Option<Event> {
    let text = std::fs::read_to_string(event_path(cache, &id)).ok()?;
    let event = Event::from_json(text).ok()?;
    (event.id == id && event.verify().is_ok()).then_some(event)
}

/// The cached module bytes, if the cache holds a copy with the digest.
fn cached_module(cache: &Path, digest: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(module_path(cache, digest)).ok()?;
    (sha256_hex(&bytes) == digest).then_some(bytes)
}

/// Fetches the Rule System event `id` from the relay.
async fn fetch_event(client: &Client, id: EventId) -> Option<Event> {
    let filter = Filter::new().id(id).kind(Kind::Custom(KIND_RULE_SYSTEM));
    let events = client
        .fetch_events(filter)
        .timeout(FETCH_TIMEOUT)
        .await
        .ok()?;
    events.into_iter().find(|event| event.id == id)
}

/// Downloads the module from `url`, keeping it only if it hashes to `digest`.
/// The `Err` is one line for the attempts log.
async fn download_module(
    http: &reqwest::Client,
    url: &str,
    digest: &str,
) -> Result<Vec<u8>, String> {
    let response = http
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MODULE_SIZE_BOUND as u64)
    {
        return Err(format!("{url}: larger than {MODULE_SIZE_BOUND} bytes"));
    }
    let bytes = response.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() > MODULE_SIZE_BOUND {
        return Err(format!("{url}: larger than {MODULE_SIZE_BOUND} bytes"));
    }
    let actual = sha256_hex(&bytes);
    if actual != digest {
        return Err(format!("{url}: hashes to {actual}, not {digest}"));
    }
    Ok(bytes.to_vec())
}

/// Loads the rule system `id`: the event (cache, else the relay), the module
/// (cache, else the event's `url` hints), verified and instantiated; both are
/// cached on success.
///
/// # Errors
///
/// The first hop that fails, as a [`LoadError`].
pub async fn load(
    client: &Client,
    http: &reqwest::Client,
    cache: &Path,
    id: EventId,
) -> Result<LoadedRuleSystem, LoadError> {
    std::fs::create_dir_all(cache).map_err(LoadError::Cache)?;

    // Hop 1 — the event, by id.
    let (event, from_cache) = match cached_event(cache, id) {
        Some(event) => (event, true),
        None => (
            fetch_event(client, id)
                .await
                .ok_or(LoadError::EventUnavailable(id))?,
            false,
        ),
    };
    let rule_system = accept_event(id, &event)?;
    if !from_cache {
        write_atomically(&event_path(cache, &id), event.as_json().as_bytes())
            .map_err(LoadError::Cache)?;
    }

    // Hop 2 — the module, by digest.
    let digest = rule_system.digest().to_owned();
    let bytes = match cached_module(cache, &digest) {
        Some(bytes) => bytes,
        None => {
            let mut attempts = Vec::new();
            let mut obtained = None;
            for url in rule_system.urls() {
                match download_module(http, url, &digest).await {
                    Ok(bytes) => {
                        obtained = Some(bytes);
                        break;
                    }
                    Err(line) => attempts.push(line),
                }
            }
            if rule_system.urls().is_empty() {
                attempts
                    .push("the event carries no url hint; drop the module in the cache".to_owned());
            }
            let bytes = obtained.ok_or(LoadError::ModuleUnavailable {
                digest: digest.clone(),
                attempts,
            })?;
            write_atomically(&module_path(cache, &digest), &bytes).map_err(LoadError::Cache)?;
            bytes
        }
    };

    instantiate(rule_system, &bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A conforming Rule System event naming `digest` for `game`.
    pub(crate) fn rule_system_event(game: &str, digest: &str, abi: &str) -> Event {
        EventBuilder::new(Kind::Custom(KIND_RULE_SYSTEM), "test")
            .tags(vec![
                Tag::parse(["game", game]).unwrap(),
                Tag::parse(["x", digest]).unwrap(),
                Tag::parse(["abi", abi]).unwrap(),
                Tag::parse(["nonce", "1", "20"]).unwrap(),
            ])
            .finalize(&Keys::generate())
            .unwrap()
    }

    #[test]
    fn accept_event_checks_the_id_and_the_constraints() {
        let event = rule_system_event("sanki", &"a".repeat(64), ABI);
        assert!(accept_event(event.id, &event).is_ok());
        let other = rule_system_event("sanki", &"b".repeat(64), ABI);
        assert!(matches!(
            accept_event(other.id, &event),
            Err(LoadError::EventInvalid(_))
        ));
        let bad = EventBuilder::new(Kind::Custom(KIND_RULE_SYSTEM), "")
            .tags(vec![Tag::parse(["game", "sanki"]).unwrap()])
            .finalize(&Keys::generate())
            .unwrap();
        assert!(matches!(
            accept_event(bad.id, &bad),
            Err(LoadError::NonConforming(_))
        ));
    }

    #[test]
    fn instantiate_refuses_a_digest_or_abi_mismatch_before_running_anything() {
        let bytes = b"whatever";
        let right = sha256_hex(bytes);
        let event = accept_event_of(rule_system_event("sanki", &right, "other.abi/1"));
        assert!(matches!(instantiate(event, bytes), Err(LoadError::Abi(_))));
        let event = accept_event_of(rule_system_event("sanki", &"c".repeat(64), ABI));
        assert!(matches!(
            instantiate(event, bytes),
            Err(LoadError::ModuleUnavailable { .. })
        ));
        // The digest matches but the bytes are no module.
        let event = accept_event_of(rule_system_event("sanki", &right, ABI));
        assert!(matches!(
            instantiate(event, bytes),
            Err(LoadError::Module(_))
        ));
    }

    #[test]
    fn instantiate_checks_describe_against_the_event() {
        let Ok(path) = std::env::var("SANKI_MODULE") else {
            eprintln!("SANKI_MODULE unset; skipping the real-module load");
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let digest = sha256_hex(&bytes);
        let loaded = instantiate(
            accept_event_of(rule_system_event("sanki", &digest, ABI)),
            &bytes,
        )
        .unwrap();
        assert_eq!(loaded.describe.game, "sanki");
        assert_eq!(loaded.describe.max_step, 300);
        // The same module under an event for another game: refused.
        let err = instantiate(
            accept_event_of(rule_system_event("go", &digest, ABI)),
            &bytes,
        )
        .unwrap_err();
        assert!(matches!(err, LoadError::Mismatch(_)), "{err}");
    }

    #[test]
    fn the_cache_round_trips_and_checks_digests() {
        let dir = std::env::temp_dir().join(format!("players-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let event = rule_system_event("sanki", &"d".repeat(64), ABI);
        assert!(cached_event(&dir, event.id).is_none());
        write_atomically(&event_path(&dir, &event.id), event.as_json().as_bytes()).unwrap();
        assert_eq!(cached_event(&dir, event.id).unwrap().id, event.id);
        // A module file whose name lies about its digest is not used.
        let bytes = b"module bytes";
        let digest = sha256_hex(bytes);
        write_atomically(&module_path(&dir, &"e".repeat(64)), bytes).unwrap();
        assert!(cached_module(&dir, &"e".repeat(64)).is_none());
        write_atomically(&module_path(&dir, &digest), bytes).unwrap();
        assert_eq!(cached_module(&dir, &digest).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn accept_event_of(event: Event) -> RuleSystem {
        accept_event(event.id, &event).unwrap()
    }
}
