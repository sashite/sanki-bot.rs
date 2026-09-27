// SPDX-License-Identifier: Apache-2.0
//! The fallback move (ADR-0045 §3): with no answer from the engine — none
//! configured, failed, refused — the bot plays a legal move drawn by
//! `HMAC-SHA256(k_fallback, session ‖ step)`: **unpredictable to others**
//! (the key is the bot's) and **identical after a crash** (the draw depends
//! on nothing but the key, the session and the step, so a restart in the
//! same slot plays the same content — one Ply content per step, ADR-0045
//! §8). SEI §11 asks a host's fallback to be unpredictable and logged; the
//! logging is the caller's.
//!
//! The draw is over the module's `legal_moves` **as the module lists them**:
//! the list is the rule system's, in its order, so the index selects a
//! content the module itself produced.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// The key the fallback is drawn with: 32 bytes the identity derives once
/// from its secret (never the secret itself), so that the draw is the bot's
/// and reveals nothing of the key.
#[derive(Clone)]
pub struct FallbackKey([u8; 32]);

impl FallbackKey {
    /// A key from 32 bytes the identity derived.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for FallbackKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FallbackKey(…)")
    }
}

/// The index of the fallback move among `count` legal moves, for the Ply
/// at `step` of `session` (the Game Session's id, 32 bytes): the first 8
/// bytes of `HMAC-SHA256(key, session ‖ step)` as a big-endian integer,
/// modulo `count`. `None` when there is no move to draw from.
#[must_use]
pub fn draw(key: &FallbackKey, session: &[u8; 32], step: u32, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key.0).ok()?;
    mac.update(session);
    mac.update(&step.to_be_bytes());
    let tag = mac.finalize().into_bytes();
    let head: [u8; 8] = tag.get(..8)?.try_into().ok()?;
    let value = u64::from_be_bytes(head);
    let count = u64::try_from(count).ok()?;
    usize::try_from(value.checked_rem(count)?).ok()
}

/// The fallback move itself: the element of `legal_moves` the draw selects.
#[must_use]
pub fn choose<'a>(
    key: &FallbackKey,
    session: &[u8; 32],
    step: u32,
    legal_moves: &'a [String],
) -> Option<&'a String> {
    draw(key, session, step, legal_moves.len()).and_then(|index| legal_moves.get(index))
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

    fn key(byte: u8) -> FallbackKey {
        FallbackKey::new([byte; 32])
    }

    #[test]
    fn the_draw_is_a_function_of_key_session_and_step() {
        let session = [7u8; 32];
        let moves: Vec<String> = (0..37).map(|i| format!("m{i}")).collect();
        let a = choose(&key(1), &session, 5, &moves).unwrap();
        let again = choose(&key(1), &session, 5, &moves).unwrap();
        assert_eq!(a, again);
        // Another step, session or key draws independently.
        let draws: Vec<usize> = (1..200)
            .map(|step| draw(&key(1), &session, step, 37).unwrap())
            .collect();
        assert!(draws.iter().any(|d| d != draws.first().unwrap()));
        assert!(draws.iter().all(|d| *d < 37));
        let other_key = draw(&key(2), &session, 5, 37).unwrap();
        let other_session = draw(&key(1), &[8u8; 32], 5, 37).unwrap();
        let own = draw(&key(1), &session, 5, 37).unwrap();
        assert!(other_key != own || other_session != own);
    }

    #[test]
    fn nothing_to_draw_from() {
        assert_eq!(draw(&key(1), &[0; 32], 1, 0), None);
        assert_eq!(choose(&key(1), &[0; 32], 1, &[]), None);
        assert_eq!(draw(&key(1), &[0; 32], 1, 1), Some(0));
    }

    #[test]
    fn a_known_vector() {
        // Pinned so that a bot restarted from another build of this crate
        // draws the same move in the same slot.
        let index = draw(&FallbackKey::new([0x11; 32]), &[0x22; 32], 3, 1_000).unwrap();
        assert_eq!(index, 771);
    }
}
