// SPDX-License-Identifier: Apache-2.0
//! The echo detector (ADR-0045 §2 *One writer*): across hosts, an event
//! signed with the bot's key that this instance did not send is read by its
//! NIP-89 `client` tag. The library's own tag means **another instance of
//! the library**: the bot moves to `Halted`. Any other tag, or none, is a
//! person acting with the bot's key — from the app, or from any client —
//! whom the protocol admits and the reconciliation absorbs.
//!
//! Only the bot's kinds `3420`, `3422`, `3423` and `3425` are watched, and
//! only events stamped after `start + created_at_upper_limit`: the previous
//! instance's events in flight cannot trip the detector. Pure: the runtime
//! says whether it signed the event.

use nostr_sdk::prelude::*;
use sashite_sanki_client::publisher::CLIENT_TAG;
use sashite_sanki_client::readers::has_client_tag;
use sashite_sanki_client::relay::SESSION_KINDS;

/// Where an event signed with the bot's key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// This instance sent it.
    Ours,
    /// Another instance of the library sent it: `Halted`.
    Library,
    /// A person acting with the key, from another client.
    Person,
    /// Not watched: another signer, another kind, or stamped before the
    /// watch began.
    Ignored,
}

/// The detector's watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// The bot's key.
    pub me: PublicKey,
    /// Events stamped at or before this instant are not watched:
    /// `start + created_at_upper_limit`.
    pub since: u64,
}

impl Watch {
    /// The watch of `me` from `start` — the relay's clock as the publisher
    /// estimates it, at the quarantine's end (ADR-0045 §7 *Start*, step
    /// 5: nothing of a previous instance is accepted after it) — with the
    /// relay's future tolerance as the margin.
    #[must_use]
    pub const fn new(me: PublicKey, start: u64, future_tolerance: u64) -> Self {
        Self {
            me,
            since: start.saturating_add(future_tolerance),
        }
    }

    /// Classifies `event`; `signed_here` says whether this instance's
    /// publisher signed it.
    #[must_use]
    pub fn origin(&self, event: &Event, signed_here: bool) -> Origin {
        if event.pubkey != self.me
            || !SESSION_KINDS.contains(&event.kind.as_u16())
            || event.created_at.as_secs() <= self.since
        {
            return Origin::Ignored;
        }
        if signed_here {
            Origin::Ours
        } else if has_client_tag(event, CLIENT_TAG) {
            Origin::Library
        } else {
            Origin::Person
        }
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

    fn event(keys: &Keys, kind: u16, at: u64, tagged: bool) -> Event {
        let builder =
            EventBuilder::new(Kind::Custom(kind), "").custom_created_at(Timestamp::from(at));
        let builder = if tagged {
            builder.tag(Tag::custom("client", [CLIENT_TAG.to_owned()]))
        } else {
            builder
        };
        builder.finalize(keys).unwrap()
    }

    #[test]
    fn origins() {
        let me = Keys::generate();
        let other = Keys::generate();
        let watch = Watch::new(me.public_key(), 1_000, 5);
        assert_eq!(watch.since, 1_005);
        // Another signer, another kind, or too early: ignored.
        assert_eq!(
            watch.origin(&event(&other, 3423, 2_000, true), false),
            Origin::Ignored
        );
        assert_eq!(
            watch.origin(&event(&me, 1, 2_000, true), false),
            Origin::Ignored
        );
        assert_eq!(
            watch.origin(&event(&me, 3423, 1_005, true), false),
            Origin::Ignored
        );
        // Ours, whatever the tag.
        assert_eq!(
            watch.origin(&event(&me, 3423, 1_006, true), true),
            Origin::Ours
        );
        // Another instance: the library's tag.
        assert_eq!(
            watch.origin(&event(&me, 3422, 1_006, true), false),
            Origin::Library
        );
        // A person: no tag, or another client's.
        assert_eq!(
            watch.origin(&event(&me, 3420, 1_006, false), false),
            Origin::Person
        );
        let app = EventBuilder::new(Kind::Custom(3425), "")
            .custom_created_at(Timestamp::from(1_006))
            .tag(Tag::custom("client", ["sanki.app".to_owned()]))
            .finalize(&me)
            .unwrap();
        assert_eq!(watch.origin(&app, false), Origin::Person);
    }
}
