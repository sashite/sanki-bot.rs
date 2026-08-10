//! Self-timed publishing discipline (Nostr Integration §Client obligations
//! in self-timed mode) + NIP-13 mining.
//!
//! Every event this bot publishes to the strict timing relay is stamped with
//! its best estimate of the **relay's** clock plus a minimal forward buffer —
//! never blindly the local clock — and mined to the configured NIP-13
//! difficulty. A rejection whose reason carries the `created_at` token means
//! the stamp was stale: the event was never stored, carries no penalty, and
//! is simply re-signed with a bumped `created_at` (raising the estimate for
//! the next publish). Any other rejection is surfaced, never blind-retried.

use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{anyhow, Result};
use nostr_sdk::prelude::*;

/// Forward buffer over the estimated relay clock (seconds). Kept minimal:
/// any surplus is charged to the mover as elapsed time.
const FORWARD_BUFFER_SECS: u64 = 1;

/// Per-retry bump on a stale rejection (seconds).
const RETRY_BUMP_SECS: i64 = 2;

/// Cap on the maintained skew estimate (seconds, either direction).
const MAX_SKEW_SECS: i64 = 60;

/// Timing attempts per publish before giving up.
const MAX_TIMING_ATTEMPTS: u32 = 6;

/// The per-connection relay-clock skew estimate (relay − local), signed.
/// Shared by every publish of one bot; starts at zero (local UTC).
#[derive(Debug, Default)]
pub struct RelayClock {
    skew_secs: AtomicI64,
}

impl RelayClock {
    /// A fresh estimate (no observed skew).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            skew_secs: AtomicI64::new(0),
        }
    }

    /// The `created_at` to stamp right now.
    #[must_use]
    pub fn stamp(&self) -> Timestamp {
        let now = Timestamp::now().as_secs();
        let skew = self.skew_secs.load(Ordering::Relaxed);
        let stamped = i64::try_from(now)
            .unwrap_or(i64::MAX)
            .saturating_add(skew)
            .saturating_add(i64::try_from(FORWARD_BUFFER_SECS).unwrap_or(1));
        Timestamp::from_secs(u64::try_from(stamped.max(0)).unwrap_or(0))
    }

    /// Raise the estimate after a stale rejection.
    fn bump(&self) {
        let _ = self
            .skew_secs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |skew| {
                Some(skew.saturating_add(RETRY_BUMP_SECS).min(MAX_SKEW_SECS))
            });
    }

    /// Lower the estimate after a too-far-future rejection.
    fn lower(&self) {
        let _ = self
            .skew_secs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |skew| {
                Some(skew.saturating_sub(RETRY_BUMP_SECS).max(-MAX_SKEW_SECS))
            });
    }
}

/// Whether a relay rejection reason denotes a STALE `created_at` (the strict
/// relay's contract: the token `created_at` appears in the reason). A
/// future-dated rejection deliberately does NOT carry the token; recognize
/// its usual wordings separately so the skew moves the right way.
#[must_use]
pub fn is_stale_reason(reason: &str) -> bool {
    reason.contains("created_at") && !is_future_reason(reason)
}

/// Whether a rejection reason denotes a too-far-future `created_at`.
#[must_use]
pub fn is_future_reason(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("future") || lower.contains("ahead") || lower.contains("too far forward")
}

/// Build, mine, sign and publish an event whose tags may depend on the
/// stamped `created_at` (e.g. an `accept_until` window). Retries stale
/// rejections with a bumped stamp; returns the accepted event.
pub async fn publish_self_timed<F>(
    client: &Client,
    keys: &Keys,
    relay_clock: &RelayClock,
    kind: Kind,
    pow_difficulty: u8,
    build: F,
) -> Result<Event>
where
    F: Fn(Timestamp) -> (Vec<Tag>, String),
{
    for _attempt in 0..MAX_TIMING_ATTEMPTS {
        let created_at = relay_clock.stamp();
        let (mut tags, content) = build(created_at);
        let mut builder = EventBuilder::new(kind, content).custom_created_at(created_at);
        if pow_difficulty > 0 {
            // Mining adds the NIP-13 `nonce` tag as a side effect.
            builder = builder.tags(tags).pow(pow_difficulty);
        } else {
            // The `nonce` tag is STRUCTURALLY required on a clock-timed suite event —
            // a Ply (kind 3423, §Proof-of-work tag / constraint 6) and an Adjudication
            // Request — independently of the relay's difficulty policy: a conforming
            // client (e.g. the Sanki app's `parsePly`) rejects a Ply that carries none,
            // so its half-move never joins the canonical chain and the board wedges.
            // At difficulty 0 (a dev relay enforcing no PoW) `.pow()` is skipped, so the
            // tag would be absent; add the trivially-satisfied 0-target nonce explicitly,
            // mirroring the app's own miner, which emits `["nonce", "0", "0"]` at
            // difficulty 0.
            tags.push(Tag::custom(TagKind::custom("nonce"), ["0", "0"]));
            builder = builder.tags(tags);
        }
        let event = builder
            .sign_with_keys(keys)
            .map_err(|e| anyhow!("signing failed: {e}"))?;

        match client.send_event(&event).await {
            Ok(_) => return Ok(event),
            Err(error) => {
                let reason = error.to_string();
                if is_stale_reason(&reason) {
                    relay_clock.bump();
                    tracing::debug!(%reason, "stale created_at; re-signing with a bumped stamp");
                    continue;
                }
                if is_future_reason(&reason) {
                    relay_clock.lower();
                    tracing::debug!(%reason, "future created_at; re-signing with a lowered stamp");
                    continue;
                }
                return Err(anyhow!("relay rejected the event: {reason}"));
            }
        }
    }
    Err(anyhow!("relay kept rejecting the event's timing"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn classifies_rejection_reasons() {
        assert!(is_stale_reason("invalid: created_at is in the past"));
        assert!(!is_stale_reason("blocked: too many events"));
        // The future wording moves the skew the other way, even when a relay
        // (non-conformingly) includes the token.
        assert!(is_future_reason("invalid: timestamp too far in the future"));
        assert!(!is_stale_reason("invalid: created_at too far ahead"));
    }

    #[test]
    fn skew_is_bounded_both_ways() {
        let clock = RelayClock::new();
        for _ in 0..100 {
            clock.bump();
        }
        assert_eq!(clock.skew_secs.load(Ordering::Relaxed), MAX_SKEW_SECS);
        for _ in 0..200 {
            clock.lower();
        }
        assert_eq!(clock.skew_secs.load(Ordering::Relaxed), -MAX_SKEW_SECS);
    }

    #[test]
    fn stamp_carries_the_forward_buffer() {
        let clock = RelayClock::new();
        let now = Timestamp::now().as_secs();
        let stamped = clock.stamp().as_secs();
        assert!(stamped >= now + FORWARD_BUFFER_SECS);
        assert!(stamped <= now + FORWARD_BUFFER_SECS + 2);
    }
}
