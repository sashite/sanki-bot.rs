//! Marker-aware readers over a Nostr event's tags.
//!
//! The suite's events carry their references and parameters in tags: NIP-10-style
//! markers in the fourth element of `e`/`p` tags, and `game`/`variant`/`seat`
//! payloads in dedicated tags. These helpers extract those values without
//! interpreting them. They are pure functions over a borrowed [`Event`], so they
//! are independently testable and shared by the session and adjudication builders.

use nostr_sdk::prelude::*;

/// The pubkey of the `p` tag carrying the given role marker, if any
/// (`["p", "<pubkey>", "<relay>", "<role>"]`).
pub fn pubkey_with_role(event: &Event, role: &str) -> Option<PublicKey> {
    marked_value(event, "p", role).and_then(|v| PublicKey::parse(v).ok())
}

/// Every pubkey of `p` tags carrying the given role marker, in tag order
/// (e.g. the two `player`-marked tags of a Game Session).
pub fn pubkeys_with_role(event: &Event, role: &str) -> Vec<PublicKey> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(String::as_str) == Some("p")
                && s.get(3).map(String::as_str) == Some(role)
            {
                s.get(1).and_then(|v| PublicKey::parse(v).ok())
            } else {
                None
            }
        })
        .collect()
}

/// The event id of the `e` tag carrying the given marker, if any
/// (`["e", "<id>", "<relay>", "<marker>"]`).
pub fn event_with_marker(event: &Event, marker: &str) -> Option<EventId> {
    marked_value(event, "e", marker).and_then(|v| EventId::parse(v).ok())
}

/// The event id of the `e` tag carrying the given marker, when the event carries
/// **exactly one** such tag and its id parses — the shape a specification's
/// "exactly one" requirement demands (e.g. a Rematch Offer's `concluded_by`,
/// kind `6430` §Semantic constraints). `None` when the marker is absent,
/// repeated, or carried by a tag whose id is missing or unparseable; a repeated
/// marker is a malformation [`event_with_marker`] (first match wins) would let
/// through.
pub fn sole_event_with_marker(event: &Event, marker: &str) -> Option<EventId> {
    let mut marked = event.tags.iter().filter_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some("e")
            && s.get(3).map(String::as_str) == Some(marker)
        {
            Some(s.get(1).map_or("", String::as_str))
        } else {
            None
        }
    });
    let only = marked.next()?;
    if marked.next().is_some() {
        return None; // repeated marker: not "exactly one"
    }
    EventId::parse(only).ok()
}

/// Every event id of `e` tags carrying the given marker, in tag order — e.g. the
/// **two** `rematch_offer`-marked tags of a rematch-founded Game Session (kind
/// `6422` §Founding reference). `None` if any marked tag's id is missing or
/// unparseable, so a caller checking "exactly N" against the returned length
/// never sees a malformed reference silently drop out of the count.
pub fn events_with_marker(event: &Event, marker: &str) -> Option<Vec<EventId>> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(String::as_str) == Some("e")
                && s.get(3).map(String::as_str) == Some(marker)
            {
                Some(s.get(1).map_or("", String::as_str))
            } else {
                None
            }
        })
        .map(|v| EventId::parse(v).ok())
        .collect()
}

/// The event id of the first `e` tag, regardless of marker. Used for references
/// that carry no marker, such as the Accepted Challenge's pointer to its Direct
/// Challenge.
pub fn first_event_ref(event: &Event) -> Option<EventId> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some("e") {
            s.get(1).and_then(|v| EventId::parse(v).ok())
        } else {
            None
        }
    })
}

/// The value of the singleton `game` tag (`["game", "<id>"]`).
pub fn game(event: &Event) -> Option<&str> {
    positional_value(event, "game", 1)
}

/// The seat-name of the singleton `seat` tag (`["seat", "<seat>"]`) carried by a
/// founding event. Distinct from the Game Session's per-player `seat` tags (the
/// three-element, pubkey-keyed form).
pub fn seat(event: &Event) -> Option<&str> {
    positional_value(event, "seat", 1)
}

/// The value of the singleton `accept_until` tag (`["accept_until", "<unix-seconds>"]`)
/// carried by a founding challenge.
pub fn accept_until(event: &Event) -> Option<&str> {
    positional_value(event, "accept_until", 1)
}

/// The variant assigned to `pubkey` (`["variant", "<pubkey>", "<variant>"]`).
pub fn variant_for<'a>(event: &'a Event, pubkey: &PublicKey) -> Option<&'a str> {
    keyed_value(event, "variant", pubkey)
}

/// The seat assigned to `pubkey` by a Game Session (`["seat", "<pubkey>", "<seat>"]`).
pub fn seat_for<'a>(event: &'a Event, pubkey: &PublicKey) -> Option<&'a str> {
    keyed_value(event, "seat", pubkey)
}

/// Second element of the first tag named `name` whose fourth element is `marker`.
fn marked_value<'a>(event: &'a Event, name: &str, marker: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some(name)
            && s.get(3).map(String::as_str) == Some(marker)
        {
            s.get(1).map(String::as_str)
        } else {
            None
        }
    })
}

/// Element at `index` of the first tag named `name`.
fn positional_value<'a>(event: &'a Event, name: &str, index: usize) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some(name) {
            s.get(index).map(String::as_str)
        } else {
            None
        }
    })
}

/// Third element of the first tag named `name` whose second element parses to
/// `pubkey` (the `["<name>", "<pubkey>", "<value>"]` shape).
fn keyed_value<'a>(event: &'a Event, name: &str, pubkey: &PublicKey) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some(name)
            && s.get(1).and_then(|v| PublicKey::parse(v).ok()).as_ref() == Some(pubkey)
        {
            s.get(2).map(String::as_str)
        } else {
            None
        }
    })
}

/// Every `time_control` tag's elements after the name, in tag order — the raw
/// period rows (`["<duration>", "<increment>?", "<plies>?"]`). Byte-level
/// comparison of two events' rows is the matchmaker's pairing criterion, so the
/// persona's preferences are stored and compared in exactly this shape.
pub fn time_control_rows(event: &Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(String::as_str) == Some("time_control") {
                Some(s.iter().skip(1).cloned().collect())
            } else {
                None
            }
        })
        .collect()
}

/// The role-keyed variant of an Open Challenge (`["variant", "self"|"opponent",
/// "<variant>"]` — kind 6418's role-based form, before pubkeys are known).
pub fn role_variant<'a>(event: &'a Event, role: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some("variant")
            && s.get(1).map(String::as_str) == Some(role)
        {
            s.get(2).map(String::as_str)
        } else {
            None
        }
    })
}

/// The first `filter` tag's elements after the name, if any (kind 6418
/// §Match-terms tags: `["filter", "following"]` or
/// `["filter", "rating", "<max_delta>", "<authority>", "<kind>"]`).
pub fn filter_row(event: &Event) -> Option<Vec<String>> {
    event.tags.iter().find_map(|tag| {
        let s = tag.as_slice();
        if s.first().map(String::as_str) == Some("filter") {
            Some(s.iter().skip(1).cloned().collect())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    // Tests favor concise `expect`/`unwrap` on values statically known to be
    // present; the panic-avoidance lints target production code paths.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn keys() -> Keys {
        Keys::generate()
    }

    fn an_event_id() -> EventId {
        EventBuilder::new(Kind::Custom(1), "")
            .sign_with_keys(&keys())
            .expect("sign")
            .id
    }

    fn signed(tags: Vec<Tag>, signer: &Keys) -> Event {
        EventBuilder::new(Kind::Custom(6422), "")
            .tags(tags)
            .sign_with_keys(signer)
            .expect("sign test event")
    }

    fn p(pubkey: &PublicKey, role: &str) -> Tag {
        Tag::custom(
            TagKind::p(),
            [pubkey.to_hex(), String::new(), role.to_string()],
        )
    }

    fn e_marked(id: &EventId, marker: &str) -> Tag {
        Tag::custom(
            TagKind::e(),
            [id.to_hex(), String::new(), marker.to_string()],
        )
    }

    fn kv(name: &str, pubkey: &PublicKey, value: &str) -> Tag {
        Tag::custom(TagKind::custom(name), [pubkey.to_hex(), value.to_string()])
    }

    fn single(name: &str, value: &str) -> Tag {
        Tag::custom(TagKind::custom(name), [value.to_string()])
    }

    #[test]
    fn reads_roles_and_markers() {
        let signer = keys();
        let arbiter = keys().public_key();
        let opponent = keys().public_key();
        let gs = an_event_id();
        let event = signed(
            vec![
                p(&arbiter, "arbiter"),
                p(&opponent, "opponent"),
                e_marked(&gs, "game_session"),
            ],
            &signer,
        );

        assert_eq!(pubkey_with_role(&event, "arbiter"), Some(arbiter));
        assert_eq!(pubkey_with_role(&event, "opponent"), Some(opponent));
        assert_eq!(pubkey_with_role(&event, "timestamper"), None);
        assert_eq!(event_with_marker(&event, "game_session"), Some(gs));
        assert_eq!(event_with_marker(&event, "triggered_by"), None);
    }

    #[test]
    fn sole_marker_requires_exactly_one_parseable_reference() {
        let signer = keys();
        let adjudication = an_event_id();
        let other = an_event_id();

        // Exactly one: read.
        let one = signed(vec![e_marked(&adjudication, "concluded_by")], &signer);
        assert_eq!(
            sole_event_with_marker(&one, "concluded_by"),
            Some(adjudication)
        );
        // Absent: none.
        assert_eq!(sole_event_with_marker(&one, "rematch_of"), None);

        // Repeated: none — where `event_with_marker` would take the first.
        let two = signed(
            vec![
                e_marked(&adjudication, "concluded_by"),
                e_marked(&other, "concluded_by"),
            ],
            &signer,
        );
        assert_eq!(sole_event_with_marker(&two, "concluded_by"), None);
        assert_eq!(
            event_with_marker(&two, "concluded_by"),
            Some(adjudication),
            "the lax reader still takes the first — the strict one is the guard"
        );

        // Present once but unparseable: none.
        let bad = signed(
            vec![Tag::custom(
                TagKind::e(),
                [
                    "not-an-event-id".to_string(),
                    String::new(),
                    "concluded_by".to_string(),
                ],
            )],
            &signer,
        );
        assert_eq!(sole_event_with_marker(&bad, "concluded_by"), None);
    }

    #[test]
    fn collects_every_marked_reference_in_tag_order() {
        let signer = keys();
        let (a, b) = (an_event_id(), an_event_id());
        // A rematch-founded Game Session carries exactly two `rematch_offer`s.
        let session = signed(
            vec![
                e_marked(&a, "rematch_offer"),
                e_marked(&b, "rematch_offer"),
                e_marked(&an_event_id(), "pairing"),
            ],
            &signer,
        );
        assert_eq!(
            events_with_marker(&session, "rematch_offer"),
            Some(vec![a, b])
        );
        // Absent marker: an empty vector, not `None` — "exactly two" fails on
        // the length, and the caller says so in its own words.
        assert_eq!(
            events_with_marker(&session, "accepted_challenge"),
            Some(vec![])
        );

        // One unparseable id poisons the whole read: a silent drop would turn a
        // malformed three-reference session into a well-formed-looking pair.
        let malformed = signed(
            vec![
                e_marked(&a, "rematch_offer"),
                Tag::custom(
                    TagKind::e(),
                    [
                        "not-an-event-id".to_string(),
                        String::new(),
                        "rematch_offer".to_string(),
                    ],
                ),
            ],
            &signer,
        );
        assert_eq!(events_with_marker(&malformed, "rematch_offer"), None);
    }

    #[test]
    fn reads_unmarked_first_event_ref() {
        let signer = keys();
        let direct = an_event_id();
        // An Accepted Challenge points at its Direct Challenge with no marker.
        let event = signed(vec![Tag::custom(TagKind::e(), [direct.to_hex()])], &signer);
        assert_eq!(first_event_ref(&event), Some(direct));
    }

    #[test]
    fn reads_players_seats_variants_game_seat() {
        let signer = keys();
        let alice = keys().public_key();
        let bob = keys().public_key();
        let event = signed(
            vec![
                single("game", "sanki"),
                single("seat", "first"),
                p(&alice, "player"),
                p(&bob, "player"),
                kv("seat", &alice, "first"),
                kv("seat", &bob, "second"),
                kv("variant", &alice, "chess"),
                kv("variant", &bob, "ogi"),
            ],
            &signer,
        );

        assert_eq!(pubkeys_with_role(&event, "player"), vec![alice, bob]);
        assert_eq!(game(&event), Some("sanki"));
        assert_eq!(seat(&event), Some("first"));
        assert_eq!(seat_for(&event, &alice), Some("first"));
        assert_eq!(seat_for(&event, &bob), Some("second"));
        assert_eq!(variant_for(&event, &alice), Some("chess"));
        assert_eq!(variant_for(&event, &bob), Some("ogi"));
        assert_eq!(variant_for(&event, &keys().public_key()), None);
    }
}
