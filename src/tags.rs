//! Marker-aware readers over a Nostr event's tags.
//!
//! The suite's events carry their references and parameters in tags: NIP-10-style
//! markers in the fourth element of `e`/`p` tags, and `game`/`variant`/`seat`
//! payloads in dedicated tags. These helpers extract those values without
//! interpreting them. They are pure functions over a borrowed [`Event`], so they
//! are independently testable and shared by the courtship, session and
//! conclusion layers.

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

/// The event ids of every `e` tag carrying the given marker, in tag order —
/// so that a doubled reference is visible to the caller ("exactly one" is
/// `[id]`). An id that does not parse is skipped.
pub fn events_with_marker(event: &Event, marker: &str) -> Vec<EventId> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(String::as_str) == Some("e")
                && s.get(3).map(String::as_str) == Some(marker)
            {
                s.get(1).and_then(|v| EventId::parse(v).ok())
            } else {
                None
            }
        })
        .collect()
}

/// The second element of every tag named `name`, in tag order (empty when the
/// tag has no second element).
pub fn values<'a>(event: &'a Event, name: &str) -> Vec<&'a str> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let s = tag.as_slice();
            if s.first().map(String::as_str) == Some(name) {
                Some(s.get(1).map_or("", String::as_str))
            } else {
                None
            }
        })
        .collect()
}

/// How many tags are named `name`.
pub fn count_named(event: &Event, name: &str) -> usize {
    event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(name))
        .count()
}

/// The value of the singleton `game` tag (`["game", "<id>"]`).
pub fn game(event: &Event) -> Option<&str> {
    positional_value(event, "game", 1)
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

/// The result assigned to `pubkey` by a Conclusion (`["result", "<pubkey>",
/// "<integer>"]`). The raw string; the caller parses it.
pub fn result_for<'a>(event: &'a Event, pubkey: &PublicKey) -> Option<&'a str> {
    keyed_value(event, "result", pubkey)
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
/// "<variant>"]` — kind 3418's role-based form, before pubkeys are known).
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

/// The first `filter` tag's elements after the name, if any (kind 3418
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

/// A relay URL normalized for designation comparison: the trailing slash is
/// insignificant (`wss://r.example.com/` designates `wss://r.example.com`).
pub fn norm_relay(url: &str) -> &str {
    url.trim_end_matches('/')
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
            .finalize(&keys())
            .expect("sign")
            .id
    }

    fn signed(tags: Vec<Tag>, signer: &Keys) -> Event {
        EventBuilder::new(Kind::Custom(3422), "")
            .tags(tags)
            .finalize(signer)
            .expect("sign test event")
    }

    fn p(pubkey: &PublicKey, role: &str) -> Tag {
        Tag::custom("p", [pubkey.to_hex(), String::new(), role.to_string()])
    }

    fn e_marked(id: &EventId, marker: &str) -> Tag {
        Tag::custom("e", [id.to_hex(), String::new(), marker.to_string()])
    }

    fn kv(name: &str, pubkey: &PublicKey, value: &str) -> Tag {
        Tag::custom(name, [pubkey.to_hex(), value.to_string()])
    }

    fn single(name: &str, value: &str) -> Tag {
        Tag::custom(name, [value.to_string()])
    }

    #[test]
    fn reads_roles_and_markers() {
        let signer = keys();
        let matchmaker = keys().public_key();
        let opponent = keys().public_key();
        let gs = an_event_id();
        let event = signed(
            vec![
                p(&matchmaker, "matchmaker"),
                p(&opponent, "opponent"),
                e_marked(&gs, "game_session"),
            ],
            &signer,
        );

        assert_eq!(pubkey_with_role(&event, "matchmaker"), Some(matchmaker));
        assert_eq!(pubkey_with_role(&event, "opponent"), Some(opponent));
        assert_eq!(pubkey_with_role(&event, "timestamper"), None);
        assert_eq!(event_with_marker(&event, "game_session"), Some(gs));
        assert_eq!(event_with_marker(&event, "triggered_by"), None);
    }

    #[test]
    fn collects_every_marked_reference_in_tag_order() {
        let signer = keys();
        let (a, b) = (an_event_id(), an_event_id());
        let session = signed(
            vec![
                e_marked(&a, "rules"),
                e_marked(&b, "rules"),
                e_marked(&an_event_id(), "pairing"),
            ],
            &signer,
        );
        // A doubled reference is visible: "exactly one" is `[id]`.
        assert_eq!(events_with_marker(&session, "rules"), vec![a, b]);
        assert_eq!(events_with_marker(&session, "direct_challenge"), vec![]);
        assert_eq!(count_named(&session, "e"), 3);
        assert_eq!(values(&session, "e").len(), 3);
        // An unparseable id is skipped.
        let malformed = signed(
            vec![
                e_marked(&a, "rules"),
                Tag::custom(
                    "e",
                    [
                        "not-an-event-id".to_string(),
                        String::new(),
                        "rules".to_string(),
                    ],
                ),
            ],
            &signer,
        );
        assert_eq!(events_with_marker(&malformed, "rules"), vec![a]);
    }

    #[test]
    fn reads_players_seats_variants_game_seat() {
        let signer = keys();
        let alice = keys().public_key();
        let bob = keys().public_key();
        let event = signed(
            vec![
                single("game", "sanki"),
                p(&alice, "player"),
                p(&bob, "player"),
                kv("seat", &alice, "first"),
                kv("seat", &bob, "second"),
                kv("variant", &alice, "chess"),
                kv("variant", &bob, "ogi"),
                kv("result", &alice, "100"),
                kv("result", &bob, "0"),
            ],
            &signer,
        );

        assert_eq!(pubkeys_with_role(&event, "player"), vec![alice, bob]);
        assert_eq!(result_for(&event, &alice), Some("100"));
        assert_eq!(result_for(&event, &bob), Some("0"));
        assert_eq!(game(&event), Some("sanki"));
        assert_eq!(seat_for(&event, &alice), Some("first"));
        assert_eq!(seat_for(&event, &bob), Some("second"));
        assert_eq!(variant_for(&event, &alice), Some("chess"));
        assert_eq!(variant_for(&event, &bob), Some("ogi"));
        assert_eq!(variant_for(&event, &keys().public_key()), None);
    }
}
