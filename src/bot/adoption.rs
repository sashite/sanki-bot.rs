// SPDX-License-Identifier: Apache-2.0
//! Adoption, at every start (ADR-0045 §2): the bot refuses the key when the
//! configured relay holds a kind `0` without `bot: true`, a kind `3` without
//! the library's `client` tag, or a kind `10000` without that tag. A read
//! that is neither `Found` nor `ConfirmedAbsent` fails the start: a
//! person's key is never turned into a bot, and their lists are never
//! replaced.

use nostr_sdk::prelude::*;
use sashite_sanki_client::publisher::CLIENT_TAG;
use sashite_sanki_client::readers::{self, KIND_MUTE_LIST};

use super::reads::{self, Read};

/// Why the key is not adopted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The relay holds a profile of the key without `bot: true`.
    ProfileNotABot,
    /// The relay holds a contact list of the key the library did not write.
    ContactsNotOurs,
    /// The relay holds a mute list of the key the library did not write.
    MuteListNotOurs,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ProfileNotABot => "the relay holds a profile of the key without bot: true",
            Self::ContactsNotOurs => {
                "the relay holds a contact list of the key the library did not write"
            }
            Self::MuteListNotOurs => {
                "the relay holds a mute list of the key the library did not write"
            }
        })
    }
}

/// What the adoption found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adoption {
    /// The key is the bot's.
    Adopted,
    /// The key is a person's.
    Refused(Refusal),
    /// A read proved nothing: the kind not read.
    Unknown(u16),
}

/// The three reads, in order; the first refusal or unknown read decides.
pub async fn check(client: &Client, relay: &RelayUrl, me: PublicKey) -> Adoption {
    let by_me = |kind: Kind| Filter::new().author(me).kind(kind);
    match reads::latest(client, relay, by_me(Kind::Metadata)).await {
        Read::Found(event) => {
            if !readers::profile(&event).is_ok_and(|p| p.bot) {
                return Adoption::Refused(Refusal::ProfileNotABot);
            }
        }
        Read::ConfirmedAbsent => {}
        Read::Unknown => return Adoption::Unknown(0),
    }
    match reads::latest(client, relay, by_me(Kind::ContactList)).await {
        Read::Found(event) if !readers::has_client_tag(&event, CLIENT_TAG) => {
            return Adoption::Refused(Refusal::ContactsNotOurs);
        }
        Read::Found(_) | Read::ConfirmedAbsent => {}
        Read::Unknown => return Adoption::Unknown(3),
    }
    match reads::latest(client, relay, by_me(Kind::Custom(KIND_MUTE_LIST))).await {
        Read::Found(event) if !readers::has_client_tag(&event, CLIENT_TAG) => {
            return Adoption::Refused(Refusal::MuteListNotOurs);
        }
        Read::Found(_) | Read::ConfirmedAbsent => {}
        Read::Unknown => return Adoption::Unknown(KIND_MUTE_LIST),
    }
    Adoption::Adopted
}
