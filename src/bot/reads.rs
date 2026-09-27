// SPDX-License-Identifier: Apache-2.0
//! Reads on the relay that prove something (ADR-0045 §8 *Nothing is
//! written on an unknown state*): the client's proven query
//! ([`sashite_sanki_client::query::query`] — the `REQ` sent to the relay
//! itself, its `EOSE` required), and its answer as [`Read::Found`],
//! [`Read::ConfirmedAbsent`] (EOSE without an event) or [`Read::Unknown`]
//! (no EOSE within the bound: nothing is proven).
//!
//! A relay caps what one `REQ` returns, whatever the client asks; a read
//! that may exceed a page is [`paged`]: `until` walks back from the newest
//! page until a page comes back short.

use std::collections::HashSet;
use std::time::Duration;

use nostr_sdk::prelude::*;
use sashite_sanki_client::query;

/// How long a query may take.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How many events a page asks for: below the caps relays are known to
/// enforce.
pub const PAGE: usize = 200;

/// A bound on the pages of one read: enough for the longest history the
/// bot rebuilds, never a runaway.
const MAX_PAGES: usize = 500;

/// What a query proved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read<T> {
    /// The relay answered with this.
    Found(T),
    /// The relay answered EOSE with nothing.
    ConfirmedAbsent,
    /// The relay did not answer: nothing is proven.
    Unknown,
}

/// The events `filters` match, or `None` when the relay did not prove its
/// answer in time.
pub async fn fetch(client: &Client, relay: &RelayUrl, filters: Vec<Filter>) -> Option<Vec<Event>> {
    query::query(client, relay, filters, READ_TIMEOUT).await
}

/// The events `filter` matches, as a [`Read`].
pub async fn read(client: &Client, relay: &RelayUrl, filter: Filter) -> Read<Vec<Event>> {
    match fetch(client, relay, vec![filter]).await {
        None => Read::Unknown,
        Some(events) if events.is_empty() => Read::ConfirmedAbsent,
        Some(events) => Read::Found(events),
    }
}

/// The newest event `filter` matches — by `created_at`, then by the lowest
/// id (NIP-01's rule for replaceable events).
pub async fn latest(client: &Client, relay: &RelayUrl, filter: Filter) -> Read<Event> {
    match read(client, relay, filter).await {
        Read::Found(events) => events
            .into_iter()
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            })
            .map_or(Read::ConfirmedAbsent, Read::Found),
        Read::ConfirmedAbsent => Read::ConfirmedAbsent,
        Read::Unknown => Read::Unknown,
    }
}

/// Every event `filter` matches, page by page from the newest: `None` when
/// a page was not proven. A page of events all stamped in one second that
/// fills the page ends the walk with what came (the relay's cap cannot be
/// walked past with `until`).
pub async fn paged(client: &Client, relay: &RelayUrl, filter: Filter) -> Option<Vec<Event>> {
    let mut out = Vec::new();
    let mut seen: HashSet<EventId> = HashSet::new();
    let mut until: Option<u64> = None;
    for _ in 0..MAX_PAGES {
        let mut page_filter = filter.clone().limit(PAGE);
        if let Some(until) = until {
            page_filter = page_filter.until(Timestamp::from(until));
        }
        let page = fetch(client, relay, vec![page_filter]).await?;
        let count = page.len();
        let oldest = page.iter().map(|e| e.created_at.as_secs()).min();
        let mut new = 0usize;
        for event in page {
            if seen.insert(event.id) {
                out.push(event);
                new = new.saturating_add(1);
            }
        }
        // A short page: the relay had nothing older. A page without a new
        // event: the walk is stuck in one second.
        let Some(oldest) = oldest else { break };
        if count < PAGE || new == 0 {
            break;
        }
        // The next page ends at the oldest stamp seen, inclusive: events
        // of that second the page cut are read again and deduplicated.
        until = Some(oldest);
    }
    Some(out)
}
