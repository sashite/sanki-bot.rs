// SPDX-License-Identifier: Apache-2.0
//! The standing events (ADR-0045 §7 *Standing events*): the profile (kind
//! `0`, `bot: true`, the optional fields when configured), the Challenge
//! Policy (kind `30420`, `d = sanki`), under `following` the contact list (kind `3`,
//! equal to `follows`), and the mute list (kind `10000`, equal to `blocks`;
//! an empty list published only to replace a non-empty one). Each is
//! compared with the relay's copy, read as `Found` or `ConfirmedAbsent`,
//! and published only when it differs — the configuration is the whole
//! truth for these events. An unknown read fails the start.

use std::collections::BTreeSet;

use nostr_sdk::prelude::*;
use sashite_sanki_client::drafts;
use sashite_sanki_client::publisher::{Outcome, Publisher, CLIENT_TAG};
use sashite_sanki_client::readers::{self, KIND_CHALLENGE_POLICY, KIND_MUTE_LIST};
use serde_json::{Map, Value};

use super::reads::{self, Read};
use crate::config::{Config, Policy};
use crate::identity::Identity;

/// The game the policy is scoped to.
const GAME: &str = "sanki";

/// Why the standing events could not be reconciled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StandingError {
    /// The relay did not answer the read of this kind.
    UnknownRead(u16),
    /// The publisher is closed.
    Closed,
}

impl std::fmt::Display for StandingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRead(kind) => {
                write!(f, "the relay did not answer the read of kind {kind}")
            }
            Self::Closed => f.write_str("the publisher is closed"),
        }
    }
}

/// What was published.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// The kinds published and acknowledged, in order.
    pub published: Vec<u16>,
    /// The kinds sent and not acknowledged: the next start compares again.
    pub unacknowledged: Vec<u16>,
    /// The kinds whose relay copy is stamped beyond the relay's future
    /// tolerance: no stamp of ours can replace it; left, logged at
    /// `error`.
    pub unreplaceable: Vec<u16>,
}

/// The profile's metadata, as configured.
#[must_use]
pub fn metadata(config: &Config) -> Map<String, Value> {
    let profile = config.profile();
    let mut map = Map::new();
    map.insert("name".to_owned(), Value::String(profile.name.clone()));
    map.insert("about".to_owned(), Value::String(profile.about.clone()));
    map.insert("bot".to_owned(), Value::Bool(true));
    for (key, value) in [
        ("display_name", &profile.display_name),
        ("picture", &profile.picture),
        ("nip05", &profile.nip05),
        ("website", &profile.website),
    ] {
        if let Some(value) = value {
            map.insert(key.to_owned(), Value::String(value.clone()));
        }
    }
    map
}

/// The policy to publish, from the configuration.
#[must_use]
pub fn policy(config: &Config) -> drafts::Policy {
    match &config.challenges().policy {
        Policy::Everyone => drafts::Policy::Everyone,
        Policy::Nobody => drafts::Policy::Nobody,
        Policy::Following(_) => drafts::Policy::Following,
        Policy::Rating { max_delta, .. } => drafts::Policy::Rating {
            max_delta: *max_delta,
        },
    }
}

/// Whether the relay's profile says what the configuration says.
#[must_use]
pub fn profile_matches(event: &Event, wanted: &Map<String, Value>) -> bool {
    readers::has_client_tag(event, CLIENT_TAG)
        && readers::profile(event).is_ok_and(|p| p.metadata == *wanted)
}

/// Whether the relay's policy says what the configuration says.
#[must_use]
pub fn policy_matches(event: &Event, wanted: drafts::Policy) -> bool {
    readers::challenge_policy(event).is_ok_and(|p| {
        p.scope == GAME
            && p.mode == wanted.mode()
            && p.max_delta
                == match wanted {
                    drafts::Policy::Rating { max_delta } => Some(max_delta),
                    _ => None,
                }
    })
}

/// Whether a list event names exactly `wanted`, whatever the order.
fn list_matches(
    read: Result<Vec<PublicKey>, readers::NonConforming>,
    wanted: &[PublicKey],
) -> bool {
    read.is_ok_and(|keys| {
        keys.iter().collect::<BTreeSet<_>>() == wanted.iter().collect::<BTreeSet<_>>()
    })
}

/// Reads each standing event and publishes those that differ.
///
/// # Errors
///
/// An unknown read, or a closed publisher. A rejected publication is
/// logged at `error` and does not fail the start: the next start compares
/// again.
pub async fn reconcile(
    client: &Client,
    publisher: &Publisher<Identity>,
    config: &Config,
    me: PublicKey,
) -> Result<Reconciled, StandingError> {
    let relay = &publisher.settings().relay;
    let by_me = |kind: Kind| Filter::new().author(me).kind(kind);
    let mut out = Reconciled::default();

    // The profile.
    let wanted = metadata(config);
    let copy = match reads::latest(client, relay, by_me(Kind::Metadata)).await {
        Read::Found(event) => {
            (!profile_matches(&event, &wanted)).then_some(Some(event.created_at.as_secs()))
        }
        Read::ConfirmedAbsent => Some(None),
        Read::Unknown => return Err(StandingError::UnknownRead(0)),
    };
    if let Some(copy_at) = copy {
        publish(
            publisher,
            drafts::Profile { metadata: wanted },
            copy_at,
            0,
            &mut out,
        )
        .await?;
    }

    // The policy.
    let wanted = policy(config);
    let filter = by_me(Kind::Custom(KIND_CHALLENGE_POLICY)).identifier(GAME);
    let copy = match reads::latest(client, relay, filter).await {
        Read::Found(event) => {
            (!policy_matches(&event, wanted)).then_some(Some(event.created_at.as_secs()))
        }
        Read::ConfirmedAbsent => Some(None),
        Read::Unknown => return Err(StandingError::UnknownRead(KIND_CHALLENGE_POLICY)),
    };
    if let Some(copy_at) = copy {
        let draft = drafts::ChallengePolicy {
            game: GAME.to_owned(),
            policy: wanted,
        };
        publish(publisher, draft, copy_at, KIND_CHALLENGE_POLICY, &mut out).await?;
    }

    // The contact list, under `following`.
    if let Policy::Following(follows) = &config.challenges().policy {
        let wanted = follows.as_slice().to_vec();
        let copy = match reads::latest(client, relay, by_me(Kind::ContactList)).await {
            Read::Found(event) => {
                let differs = !readers::has_client_tag(&event, CLIENT_TAG)
                    || !list_matches(readers::contacts(&event), &wanted);
                differs.then_some(Some(event.created_at.as_secs()))
            }
            Read::ConfirmedAbsent => Some(None),
            Read::Unknown => return Err(StandingError::UnknownRead(3)),
        };
        if let Some(copy_at) = copy {
            publish(publisher, drafts::Contacts(wanted), copy_at, 3, &mut out).await?;
        }
    }

    // The mute list.
    let wanted = config.challenges().blocks.clone();
    let copy = match reads::latest(client, relay, by_me(Kind::Custom(KIND_MUTE_LIST))).await {
        Read::Found(event) => {
            let differs = !readers::has_client_tag(&event, CLIENT_TAG)
                || !list_matches(readers::mute_list(&event), &wanted);
            differs.then_some(Some(event.created_at.as_secs()))
        }
        // An empty list is published only to replace a non-empty one.
        Read::ConfirmedAbsent => (!wanted.is_empty()).then_some(None),
        Read::Unknown => return Err(StandingError::UnknownRead(KIND_MUTE_LIST)),
    };
    if let Some(copy_at) = copy {
        publish(
            publisher,
            drafts::MuteList(wanted),
            copy_at,
            KIND_MUTE_LIST,
            &mut out,
        )
        .await?;
    }
    Ok(out)
}

/// Publishes `draft`, stamped after the relay's copy when there is one.
async fn publish(
    publisher: &Publisher<Identity>,
    draft: impl drafts::Publishable + 'static,
    copy_at: Option<u64>,
    kind: u16,
    out: &mut Reconciled,
) -> Result<(), StandingError> {
    let outcome = match copy_at {
        Some(copy_at) => {
            // A copy ahead of the relay's window (the kind is not timed, a
            // hand edit from a wrong clock) cannot be replaced by a stamp
            // the relay admits: not waited for.
            let horizon = publisher
                .now()
                .saturating_add(publisher.settings().future_tolerance);
            if copy_at >= horizon {
                tracing::error!(
                    kind,
                    copy_at,
                    "the relay's copy is stamped beyond its future tolerance; not replaced"
                );
                out.unreplaceable.push(kind);
                return Ok(());
            }
            publisher
                .publish(drafts::Replacing { draft, copy_at })
                .await
        }
        None => publisher.publish(draft).await,
    };
    match outcome {
        Outcome::Accepted(_) => {
            tracing::info!(kind, "standing event published");
            out.published.push(kind);
            Ok(())
        }
        Outcome::Unknown(_) => {
            // Replaceable: the relay holds the newest; the next start
            // compares again.
            tracing::warn!(kind, "standing event not acknowledged");
            out.unacknowledged.push(kind);
            Ok(())
        }
        Outcome::Rejected(rejection) => {
            tracing::error!(kind, %rejection, "standing event rejected");
            Ok(())
        }
        Outcome::Withheld(why) => {
            tracing::error!(kind, %why, "standing event withheld");
            Ok(())
        }
        Outcome::Failed(reason) => {
            tracing::error!(kind, %reason, "standing event could not be built");
            Ok(())
        }
        Outcome::Closed => Err(StandingError::Closed),
    }
}
