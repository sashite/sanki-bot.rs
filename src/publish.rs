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

use std::num::NonZeroU8;
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

/// How long the profile relays get to answer the dial (ADR-0042).
const PROFILE_RELAY_DIAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Publish the persona's profile ABROAD (ADR-0042): the kind-0 `metadata`
/// — the same JSON the game relay just received — and a NIP-65 relay list
/// (kind 10002) naming the game relay and the profile relays, on the
/// profile relays only. Not self-timed: these relays keep no `created_at`
/// window, and the timing relay's clock is not theirs. A throwaway client,
/// connected for the two sends and dropped: the bot's own client stays on
/// the game relay alone, so no subscription of the game ever reaches a
/// public relay. Best effort — the answer is how many sends were accepted
/// and each refusal as `relay (what): reason`, for the caller to log; a
/// count alone would say nothing to act on, and a public relay's refusal
/// (rate limit, web-of-trust gate) is never the bot's failure.
pub async fn publish_profile_abroad(
    keys: &Keys,
    metadata: &str,
    game_relay: &str,
    profile_relays: &[String],
) -> Result<(usize, Vec<String>)> {
    let client = Client::builder().build();
    for url in profile_relays {
        client
            .add_relay(url)
            .await
            .map_err(|e| anyhow!("profile relay {url}: {e}"))?;
    }
    client.connect().and_wait(PROFILE_RELAY_DIAL).await;

    let profile = EventBuilder::new(Kind::Metadata, metadata.to_owned())
        .finalize_unsigned(keys.public_key())
        .finalize(keys)
        .map_err(|e| anyhow!("signing the profile: {e}"))?;

    let mut listed: Vec<(RelayUrl, Option<RelayMetadata>)> = Vec::new();
    listed.push((RelayUrl::parse(game_relay)?, None));
    for url in profile_relays {
        listed.push((RelayUrl::parse(url)?, None));
    }
    let relay_list = RelayList::new(listed)
        .into_event_builder()
        .finalize_unsigned(keys.public_key())
        .finalize(keys)
        .map_err(|e| anyhow!("signing the relay list: {e}"))?;

    let mut accepted: usize = 0;
    let mut refused: Vec<String> = Vec::new();
    for (what, event) in [("profile", profile), ("relay list", relay_list)] {
        match client.send_event(&event).await {
            Ok(output) => {
                accepted = accepted.saturating_add(output.success.len());
                refused.extend(
                    output
                        .failed
                        .iter()
                        .map(|(relay, reason)| format!("{relay} ({what}): {reason}")),
                );
            }
            Err(e) => {
                client.disconnect().await;
                return Err(anyhow!("sending to the profile relays: {e}"));
            }
        }
    }
    client.disconnect().await;
    Ok((accepted, refused))
}

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

    /// The relay's clock as estimated now, in unix seconds — the instant
    /// every timing comparison with the relay's events is made at (a cutoff,
    /// a deadline), so that a host clock behind the relay never makes the
    /// bot believe it has more time than it has.
    #[must_use]
    pub fn now_secs(&self) -> u64 {
        let now = Timestamp::now().as_secs();
        let skew = self.skew_secs.load(Ordering::Relaxed);
        let estimated = i64::try_from(now).unwrap_or(i64::MAX).saturating_add(skew);
        u64::try_from(estimated.max(0)).unwrap_or(0)
    }

    /// The `created_at` to stamp right now: the relay's clock plus the
    /// forward buffer.
    #[must_use]
    pub fn stamp(&self) -> Timestamp {
        Timestamp::from_secs(self.now_secs().saturating_add(FORWARD_BUFFER_SECS))
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

/// Whether, and how hard, an event is mined (NIP-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pow {
    /// The kind prescribes no `nonce` tag (a profile, a Game Session, a
    /// reaction): none is added.
    None,
    /// The kind prescribes a `nonce` tag (every player-published founding,
    /// Ply and Conclusion): mined to this difficulty — `0` adds the
    /// trivially-satisfied `["nonce", "0", "0"]`.
    Mined(u8),
}

/// Build, mine, sign and publish an event whose tags may depend on the
/// stamped `created_at` (e.g. an `accept_until` window). Retries stale
/// rejections with a bumped stamp; returns the accepted event.
pub async fn publish_self_timed<F>(
    client: &Client,
    keys: &Keys,
    relay_clock: &RelayClock,
    kind: Kind,
    pow: Pow,
    build: F,
) -> Result<Event>
where
    F: Fn(Timestamp) -> (Vec<Tag>, String),
{
    for _attempt in 0..MAX_TIMING_ATTEMPTS {
        let created_at = relay_clock.stamp();
        let (mut tags, content) = build(created_at);
        let mut builder = EventBuilder::new(kind, content).custom_created_at(created_at);
        let mut difficulty: Option<NonZeroU8> = None;
        match pow {
            Pow::Mined(target) if target > 0 => {
                // Mining adds the NIP-13 `nonce` tag as a side effect. Since
                // nostr 0.45 it happens on the UNSIGNED event rather than on
                // the builder (`EventBuilder::pow` is gone), so the target is
                // carried down to the signing step below.
                difficulty = NonZeroU8::new(target);
                builder = builder.tags(tags);
            }
            Pow::Mined(_) => {
                // The `nonce` tag is STRUCTURALLY required on a player-published
                // suite event — a Ply (kind 3423, constraint 6), a Conclusion, a
                // founding — independently of the relay's difficulty policy: a
                // conforming consumer rejects one that carries none, so a Ply
                // without it never joins the canonical chain and the board
                // wedges. At difficulty 0 (a dev relay enforcing no PoW) mining
                // is skipped, so the tag would be absent; add the trivially-
                // satisfied 0-target nonce explicitly, mirroring the app's own
                // miner, which emits `["nonce", "0", "0"]` at difficulty 0.
                tags.push(Tag::custom("nonce", ["0", "0"]));
                builder = builder.tags(tags);
            }
            Pow::None => builder = builder.tags(tags),
        }
        let unsigned = builder.finalize_unsigned(keys.public_key());
        let unsigned = match difficulty {
            Some(target) => unsigned
                .mine(&SingleThreadPow, target)
                .map_err(|e| anyhow!("mining failed: {e}"))?,
            None => unsigned,
        };
        let event = unsigned
            .finalize(keys)
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
