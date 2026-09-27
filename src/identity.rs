// SPDX-License-Identifier: Apache-2.0
//! The identity (ADR-0045 §2): the key lives here, never in the
//! configuration, never in the environment (a child process — the engine —
//! would inherit it).
//!
//! - **The file.** [`Identity::from_file`] reads a file with no group or
//!   other permission bits; [`Identity::generate`] creates one, `0600`.
//! - **The type.** Neither `Clone` nor `Serialize`; its `Debug` shows the
//!   npub; the key's memory is zeroed on drop (`nostr`'s `Keys` does it,
//!   and the derived secrets are zeroed here). It signs, and gives the key
//!   to no one: a caller holds an `Identity`, not a `Keys`.
//! - **Derived secrets**, by HKDF-SHA256 from the secret key, never from
//!   the public key: the fallback move (§3), the open seat of a founding
//!   and the jitter of an outgoing challenge (§5). Each is unpredictable to
//!   others and identical after a crash.
//!
//! The lease of the key on the host is the Publisher's
//! (`sashite_sanki_client::publisher::Lease`, taken by `Publisher::open`);
//! adoption — the refusal of a person's key by what the relay holds — and
//! the echo detector across hosts need the relay: they are the runtime's.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use nostr_sdk::prelude::*;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::fallback::FallbackKey;

/// The HKDF salt, fixed for the crate.
const HKDF_SALT: &[u8] = b"sashite-sanki-bot/identity";

/// Why an identity could not be read, written or leased.
#[derive(Debug)]
#[non_exhaustive]
pub enum IdentityError {
    /// The file cannot be read or written.
    Io(PathBuf, std::io::Error),
    /// The file has group or other permission bits.
    Permissions(PathBuf, u32),
    /// The file does not hold a secret key (`nsec1…` or 64 hex characters).
    NotAKey(PathBuf),
    /// The file exists already (`generate` never overwrites).
    Exists(PathBuf),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "{}: {err}", path.display()),
            Self::Permissions(path, mode) => write!(
                f,
                "{}: mode {mode:o} has group or other bits; the key file must be private (0600)",
                path.display()
            ),
            Self::NotAKey(path) => write!(
                f,
                "{}: not a secret key (nsec1… or 64 hex characters)",
                path.display()
            ),
            Self::Exists(path) => write!(f, "{}: exists already", path.display()),
        }
    }
}

impl std::error::Error for IdentityError {}

/// A secret derived from the key, zeroed on drop.
struct Derived([u8; 32]);

impl Drop for Derived {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// The bot's identity: its key and the secrets derived from it.
pub struct Identity {
    keys: Keys,
    seat: Derived,
    jitter: Derived,
    fallback: FallbackKey,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.npub())
    }
}

impl Identity {
    /// Reads the key from `path`: `nsec1…` or 64 hex characters, surrounded
    /// by whitespace at most. The file must have no group or other bits.
    ///
    /// # Errors
    ///
    /// See [`IdentityError`].
    pub fn from_file(path: &Path) -> Result<Self, IdentityError> {
        let metadata =
            std::fs::metadata(path).map_err(|e| IdentityError::Io(path.to_owned(), e))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(IdentityError::Permissions(path.to_owned(), mode));
        }
        let mut text =
            std::fs::read_to_string(path).map_err(|e| IdentityError::Io(path.to_owned(), e))?;
        let keys = Keys::parse(text.trim()).map_err(|_| IdentityError::NotAKey(path.to_owned()));
        text.zeroize();
        Ok(Self::from_keys(keys?))
    }

    /// Generates a key and writes it to `path` as `nsec1…`, mode `0600`,
    /// never over an existing file.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Exists`] or [`IdentityError::Io`].
    pub fn generate(path: &Path) -> Result<Self, IdentityError> {
        let keys = Keys::generate();
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(IdentityError::Exists(path.to_owned()));
            }
            Err(e) => return Err(IdentityError::Io(path.to_owned(), e)),
        };
        let mut nsec = keys
            .secret_key()
            .to_bech32()
            .map_err(|e| IdentityError::Io(path.to_owned(), std::io::Error::other(e)))?;
        let written = file
            .write_all(nsec.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all());
        nsec.zeroize();
        written.map_err(|e| IdentityError::Io(path.to_owned(), e))?;
        Ok(Self::from_keys(keys))
    }

    /// An identity from keys held in memory (tests).
    #[must_use]
    pub fn from_keys(keys: Keys) -> Self {
        let derive = |info: &[u8]| -> Derived {
            let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), keys.secret_key().as_secret_bytes());
            let mut okm = [0u8; 32];
            // A 32-byte output is always within HKDF-SHA256's bound.
            let _ = hk.expand(info, &mut okm);
            Derived(okm)
        };
        let fallback = FallbackKey::new(derive(b"fallback").0);
        Self {
            seat: derive(b"seat"),
            jitter: derive(b"jitter"),
            fallback,
            keys,
        }
    }

    /// The public key.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The public key as `npub1…`.
    #[must_use]
    pub fn npub(&self) -> String {
        self.keys
            .public_key()
            .to_bech32()
            .unwrap_or_else(|_| self.keys.public_key().to_hex())
    }

    /// The key the fallback move is drawn with (§3).
    #[must_use]
    pub const fn fallback_key(&self) -> &FallbackKey {
        &self.fallback
    }

    /// The seat the bot takes when a challenge leaves it open (§2, §5):
    /// `HMAC(k_seat, challenge_id) & 1` — `true` for `first`.
    #[must_use]
    pub fn open_seat_first(&self, challenge: &EventId) -> bool {
        let tag = hmac_of(&self.seat.0, challenge.as_bytes());
        tag.first().is_some_and(|byte| byte & 1 == 1)
    }

    /// The instant within a minute, in seconds, at which an outgoing
    /// challenge to `target` fires during `hour` (§5):
    /// `HMAC(k_jitter, target ‖ hour) mod 60`.
    #[must_use]
    pub fn jitter_secs(&self, target: &PublicKey, hour: u64) -> u64 {
        let mut message = Vec::with_capacity(40);
        message.extend_from_slice(target.as_bytes());
        message.extend_from_slice(&hour.to_be_bytes());
        let tag = hmac_of(&self.jitter.0, &message);
        let head: [u8; 8] = tag
            .get(..8)
            .and_then(|h| h.try_into().ok())
            .unwrap_or([0; 8]);
        u64::from_be_bytes(head).checked_rem(60).unwrap_or(0)
    }
}

/// `HMAC-SHA256(key, message)`.
fn hmac_of(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(key) else {
        return [0; 32];
    };
    mac.update(message);
    mac.finalize().into_bytes().into()
}

impl GetPublicKey for Identity {
    type Error = std::convert::Infallible;

    fn get_public_key(&self) -> Result<PublicKey, Self::Error> {
        Ok(self.keys.public_key())
    }
}

impl SignEvent for Identity {
    type Error = <Keys as SignEvent>::Error;

    fn sign_event(&self, unsigned: UnsignedEvent) -> Result<Event, Self::Error> {
        self.keys.sign_event(unsigned)
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

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sanki-bot-identity-{}-{}",
            std::process::id(),
            Timestamp::now().as_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generates_reads_and_refuses_a_public_file() {
        let dir = temp_dir();
        let path = dir.join("key.nsec");
        let generated = Identity::generate(&path).unwrap();
        assert!(matches!(
            Identity::generate(&path),
            Err(IdentityError::Exists(_))
        ));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let read = Identity::from_file(&path).unwrap();
        assert_eq!(read.public_key(), generated.public_key());
        assert_eq!(format!("{read:?}"), format!("Identity({})", read.npub()));
        // Derived secrets are a function of the key.
        let id = EventId::from_hex(&"a".repeat(64)).unwrap();
        assert_eq!(read.open_seat_first(&id), generated.open_seat_first(&id));
        assert_eq!(
            read.jitter_secs(&read.public_key(), 5),
            generated.jitter_secs(&read.public_key(), 5)
        );
        // A hex key reads too.
        let hex = dir.join("key.hex");
        std::fs::write(
            &hex,
            format!("{}\n", generated.keys.secret_key().to_secret_hex()),
        )
        .unwrap();
        std::fs::set_permissions(&hex, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            Identity::from_file(&hex).unwrap().public_key(),
            generated.public_key()
        );
        // Group or other bits: refused.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            Identity::from_file(&path),
            Err(IdentityError::Permissions(_, 0o640))
        ));
        // Not a key.
        let junk = dir.join("junk");
        std::fs::write(&junk, "hello\n").unwrap();
        std::fs::set_permissions(&junk, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            Identity::from_file(&junk),
            Err(IdentityError::NotAKey(_))
        ));
    }

    #[test]
    fn derived_secrets_differ_by_purpose_and_key() {
        let a = Identity::from_keys(Keys::generate());
        let b = Identity::from_keys(Keys::generate());
        assert_ne!(a.seat.0, a.jitter.0);
        assert_ne!(a.seat.0, b.seat.0);
        // The seat draw is a coin over challenge ids.
        let firsts = (0..64u8)
            .filter(|i| a.open_seat_first(&EventId::from_slice(&[*i; 32]).unwrap()))
            .count();
        assert!(firsts > 8 && firsts < 56, "{firsts}");
        // The jitter is within the minute.
        assert!(a.jitter_secs(&b.public_key(), 1) < 60);
    }

    #[test]
    fn signs_as_the_key() {
        let identity = Identity::from_keys(Keys::generate());
        let event = EventBuilder::new(Kind::Custom(3423), "x")
            .finalize(&identity)
            .unwrap();
        assert_eq!(event.pubkey, identity.public_key());
        assert!(event.verify().is_ok());
    }
}
