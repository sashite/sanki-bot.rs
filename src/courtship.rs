//! Courtship decisions — pool entries (kind 3418) and Direct Challenges
//! (kind 3420, fresh or rematch), per ADR-0014 §6.2–§6.3 under ADR-0033/0034:
//! no arbiter, and one **rule system** — the Rule System event (kind 3417)
//! the fleet plays under — that every founding must name.
//!
//! Everything here is **pure** over borrowed events and persona data. The
//! async layer performs the fetches a decision may require (contact lists,
//! mute lists, the concluded session of a rematch) and enforces the fleet
//! ledger; refusals are by silence, so a `Skip` simply produces a debug log
//! upstream.

use nostr_sdk::prelude::*;

use crate::cadence::Cadence;
use crate::config::PlayConfig;
use crate::prng::SplitMix64;
use crate::session::{self, Seat, Timing};
use crate::tags;

/// A pool entry (kind 3418) judged compatible from the bot's side, up to the
/// async checks (mute lists; the counterparty's `following` filter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolCandidate {
    /// The counterparty (the entry's signer).
    pub challenger: PublicKey,
    /// The variant the bot would enter with — the shared one of a mirror
    /// entry, or the one an asymmetric (premium) entry imposes on us.
    pub variant: String,
    /// Whether our own entry may impose `variant` back on the opponent (the
    /// free mirror form). `false` when courting an ASYMMETRIC entry: our entry
    /// must fix `self` alone and leave the opponent unconstrained, both to
    /// satisfy the pairing (their `self` differs from our variant) and to stay
    /// in the free tier (constraining the opponent is the premium form).
    pub mirror: bool,
    /// The `time_control` rows to carry — byte-identical to the entry's (the
    /// matchmaker pairs identical configurations).
    pub spec: Vec<Vec<String>>,
    /// The entry's cadence family (*Cadence — Sanki*): the per-family cap
    /// and the per-cadence pool lock are indexed by it.
    pub cadence: Cadence,
    /// The entry carried **no variant term** at all — the whole fleet could
    /// answer it, so the async layer claims it in the ledger first: one
    /// member courts it, not three (ADR-0040 §2).
    pub open: bool,
    /// The entry's `accept_until` (unix seconds).
    pub accept_until: u64,
    /// Whether the entry carries a `following` filter — the async layer must
    /// verify the counterparty follows the bot (fail-closed) before entering.
    pub needs_following_check: bool,
}

/// Why an entry (or challenge) is not courted. Logged, never answered.
pub type Skip = &'static str;

/// Whether `event` designates exactly our relay as its (only) timing relay,
/// and no timestamper: the self-timed designation our own foundings carry
/// (Canonical Timing NIP §Timing modes and mode selection). Trailing slashes
/// are insignificant.
fn designates_our_relay(event: &Event, relay_url: &str) -> Result<(), Skip> {
    match session::timing_of(event) {
        Some(Timing::SelfTimed(relay)) => {
            if tags::norm_relay(&relay) == tags::norm_relay(relay_url) {
                Ok(())
            } else {
                Err("timing_relay designation is not our relay")
            }
        }
        Some(Timing::Attested(_)) => Err("attested mode (v1 is self-timed)"),
        None => Err("no single timing designation"),
    }
}

/// Whether `event` names exactly our rule system by its `rules` reference —
/// the one the fleet holds and runs; anything else cannot be played
/// (kind `3417` §Referencing a rule system).
fn names_our_rules(event: &Event, rules: &EventId) -> Result<(), Skip> {
    match session::rules_ref(event) {
        Some(named) if named == *rules => Ok(()),
        Some(_) => Err("another rule system"),
        None => Err("no single rules reference"),
    }
}

/// Evaluate an Open Challenge from the bot's side (§6.2, reactive path).
/// `now` in unix seconds; `margin_secs` is the minimum life the entry must
/// still have for our own entry to land and pair.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_open_challenge(
    event: &Event,
    me: &PublicKey,
    relay_url: &str,
    matchmaker: &PublicKey,
    rules: &EventId,
    game: &str,
    play: &PlayConfig,
    now: u64,
    margin_secs: u64,
    rng: &mut SplitMix64,
) -> Result<PoolCandidate, Skip> {
    if event.pubkey == *me {
        return Err("own entry");
    }
    if tags::game(event) != Some(game) {
        return Err("other game");
    }
    if tags::pubkey_with_role(event, "matchmaker").as_ref() != Some(matchmaker) {
        return Err("other matchmaker");
    }
    // Pairing requires both entries to reference the same rule system and to
    // carry the same timing designation (kind 3419 §Consent constraints):
    // ours names the fleet's rule system and our relay, so anything else can
    // never pair with us (§6.2).
    names_our_rules(event, rules)?;
    designates_our_relay(event, relay_url)?;
    let accept_until: u64 = tags::accept_until(event)
        .and_then(|value| value.parse().ok())
        .ok_or("missing accept_until")?;
    if accept_until <= now.saturating_add(margin_secs) {
        return Err("expiring too soon");
    }

    // The counterparty's filter: `following` defers to an async check;
    // `rating` is fail-closed for a bot that holds no attestation from the
    // pinned authority (a fresh bot is unrated) — skip (§6.2).
    let needs_following_check = match tags::filter_row(event).as_deref() {
        None => false,
        Some([mode]) if mode == "following" => true,
        Some(_) => return Err("rating filter (unrated, fail-closed)"),
    };

    // Variant resolution (§4 Notes). A MIRROR entry (both terms equal, or a
    // single term) resolves to the shared `v` — the only form the fleet EMITS
    // spontaneously. An ASYMMETRIC entry (the premium form: their `self`
    // differs from the `opponent` variant they impose) is COURTED when the
    // persona opts in (`accept_imposed_variant`, the same knob as the directed
    // path): our variant is the imposed one, and our own entry must then fix
    // `self` alone (see [`PoolCandidate::mirror`]). Either way the persona
    // must actually play the resolved variant.
    let their_self = tags::role_variant(event, "self");
    let their_opponent = tags::role_variant(event, "opponent");
    let mut open = false;
    let (variant, mirror) = match (their_self, their_opponent) {
        (Some(own), Some(imposed)) if own != imposed => {
            if !play.accept_imposed_variant {
                return Err("asymmetric variant terms (persona opts out)");
            }
            (imposed, false)
        }
        (Some(own), _) => (own, true),
        (None, Some(imposed)) => (imposed, true),
        (None, None) => {
            // A FULLY OPEN entry — "anything" — is mirrored with the
            // persona's own draw (ADR-0040 §2); the async layer claims it
            // fleet-wide so one member answers it.
            open = true;
            let drawn = crate::persona::draw_weighted_key(&play.variants, rng)
                .ok_or("no persona variant")?;
            (drawn, true)
        }
    };
    if play.variants.get(variant).copied().unwrap_or(0.0) <= 0.0 {
        return Err("variant outside the persona");
    }

    // The cadence must be one the persona plays, byte-identically.
    let rows = tags::time_control_rows(event);
    if !play
        .time_controls
        .iter()
        .any(|preference| preference.spec == rows)
    {
        return Err("cadence outside the persona");
    }
    // A persona's time controls all classify (config validation), so this
    // cannot fail for a row that just matched one; it is checked, not assumed.
    let cadence = Cadence::of_rows(&rows).ok_or("no cadence (malformed time control)")?;

    Ok(PoolCandidate {
        challenger: event.pubkey,
        variant: variant.to_owned(),
        mirror,
        spec: rows,
        cadence,
        open,
        accept_until,
        needs_following_check,
    })
}

/// The two references of a rematch challenge (kind 3420 §Rematch tags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RematchRefs {
    /// The concluded Game Session (`rematch_of`).
    pub concluded: EventId,
    /// A Conclusion of that session (`concluded_by`).
    pub concluded_by: EventId,
}

/// A Direct Challenge (kind 3420) the persona would accept, with everything
/// the Game Session that accepts it must state (§6.3; kind `3422` §Signing
/// party — the acceptance IS the Game Session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptPlan {
    /// The challenger.
    pub challenger: PublicKey,
    /// The bot's own variant: the challenge's imposition, or a persona draw.
    pub my_variant: String,
    /// The challenger's variant: the challenge's own, or — when the
    /// challenger delegated it by omission — supplied by the mirror rule.
    pub their_variant: String,
    /// The bot's seat: the other value than the challenger's declared seat,
    /// or a uniform draw when the challenge left it open.
    pub my_seat: Seat,
    /// The challenge's `accept_until`.
    pub accept_until: u64,
    /// The challenge's cadence family (*Cadence — Sanki*): the per-family
    /// cap is indexed by it, and its correspondence bit decides acceptance
    /// timing (at any hour — resolved question 5; live only while present).
    pub cadence: Cadence,
    /// The rematch references when the challenge is a rematch challenge —
    /// the async layer verifies them against the concluded session before
    /// accepting (kind 3420 §Rematch challenge).
    pub rematch: Option<RematchRefs>,
    /// The challenge **asymmetrically** imposes the bot's variant — a
    /// different one than the challenger's own, or while their own is left
    /// open — the premium-only form (*Premium* §1.2). The async layer accepts
    /// only if the challenger is premium, asked of the admission service,
    /// fail-closed (§1.3; ADR-0040 §2). `false` for the free forms
    /// (delegation, the mirror imposition) and for a rematch, whose
    /// inherited terms are re-checked by their own rule.
    pub premium_imposition: bool,
}

/// Evaluate a Direct Challenge addressed to the bot (§6.3). `rng` covers the
/// persona draws (variant when free, seat when open). A **rematch challenge**
/// (both rematch tags present) is evaluated for its form and timing only: its
/// variants and cadence are those of a session the bot already played, so the
/// persona gates do not apply — its constraints against the concluded session
/// are the async layer's to verify.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_direct_challenge(
    event: &Event,
    me: &PublicKey,
    relay_url: &str,
    rules: &EventId,
    game: &str,
    play: &PlayConfig,
    now: u64,
    margin_secs: u64,
    rng: &mut SplitMix64,
) -> Result<AcceptPlan, Skip> {
    if tags::pubkeys_with_role(event, "opponent") != [*me] {
        return Err("not addressed to us");
    }
    if tags::game(event) != Some(game) {
        return Err("other game");
    }
    names_our_rules(event, rules)?;
    // Self-timed, and it must name our relay — otherwise our plies would land
    // where no verifier is told to look (Canonical Timing NIP §Timing modes
    // and mode selection). The Game Session mirrors the designation verbatim.
    designates_our_relay(event, relay_url)?;
    let challenger = event.pubkey;
    if challenger == *me {
        return Err("self-challenge");
    }
    let accept_until: u64 = tags::accept_until(event)
        .and_then(|value| value.parse().ok())
        .ok_or("missing accept_until")?;
    if accept_until <= now.saturating_add(margin_secs) {
        return Err("expiring too soon");
    }
    if !session::pow_ok(event) {
        return Err("no honoured nonce");
    }

    // A rematch challenge carries both rematch tags, a fresh one neither
    // (kind 3420 §Rematch tags); its `e` tags are those and `rules`.
    let rematch = match (
        tags::events_with_marker(event, "rematch_of").as_slice(),
        tags::events_with_marker(event, "concluded_by").as_slice(),
    ) {
        ([], []) => None,
        ([concluded], [concluded_by]) => Some(RematchRefs {
            concluded: *concluded,
            concluded_by: *concluded_by,
        }),
        _ => return Err("malformed rematch references"),
    };
    let e_tags = if rematch.is_some() { 3 } else { 1 };
    if tags::count_named(event, "e") != e_tags {
        return Err("unexpected e tags");
    }

    let rows = tags::time_control_rows(event);
    if rematch.is_none()
        && !play.accept_any_time_control
        && !play
            .time_controls
            .iter()
            .any(|preference| preference.spec == rows)
    {
        return Err("cadence outside the persona");
    }
    // Whatever the persona's gate, the first period must classify: a
    // challenge with no cadence has no cap to be admitted under.
    let cadence = Cadence::of_rows(&rows).ok_or("no cadence (malformed time control)")?;

    // Variant terms (§6.3 as amended by ADR-0040 §2): the bot plays its own
    // variant — left open, the persona supplies it; imposed, it must be one
    // the persona plays — and the challenger's is theirs to choose, so a
    // cross-variant game by delegation is accepted as it is. An imposition
    // that is ASYMMETRIC (the challenger plays something else, or left their
    // own open) is the premium-only form: the plan says so, and the async
    // layer honours it only for a premium challenger (*Premium* §1.3). A
    // MIRROR imposition is free and judged on the variant alone. A rematch
    // fixes both as they were: what we played, we play again.
    let their_variant = tags::variant_for(event, &challenger).map(str::to_owned);
    let imposed_mine = tags::variant_for(event, me).map(str::to_owned);
    if rematch.is_some() && (their_variant.is_none() || imposed_mine.is_none()) {
        // Both are inherited terms (kind 3420 §Rematch challenge): a rematch
        // that leaves one open is malformed, not a premium question.
        return Err("a rematch challenge must fix both variants");
    }
    let mut premium_imposition = false;
    let my_variant = match (&their_variant, &imposed_mine) {
        (Some(_), Some(mine)) if rematch.is_some() => mine.clone(),
        (_, Some(mine)) => {
            if play.variants.get(mine).copied().unwrap_or(0.0) <= 0.0 {
                return Err("imposed variant outside the persona");
            }
            premium_imposition = their_variant.as_deref() != Some(mine.as_str());
            mine.clone()
        }
        (_, None) => crate::persona::draw_weighted_key(&play.variants, rng)
            .ok_or("no persona variant")?
            .to_owned(),
    };
    if !session::is_identifier(&my_variant)
        || their_variant
            .as_deref()
            .is_some_and(|v| !session::is_identifier(v))
    {
        return Err("malformed variant identifier");
    }
    // The challenger's variant, by the mirror rule when delegated: we play
    // the same variant on both sides of the board.
    let their_variant = their_variant.unwrap_or_else(|| my_variant.clone());

    // The seat: the other value than the challenger's declared one; a uniform
    // draw when open (a rematch always declares it).
    let my_seat = match tags::values(event, "seat").as_slice() {
        [] if rematch.is_some() => return Err("a rematch challenge must declare the seat"),
        [] => {
            if rng.next_index(2) == 0 {
                Seat::First
            } else {
                Seat::Second
            }
        }
        [declared] => Seat::parse(declared).ok_or("malformed seat")?.other(),
        _ => return Err("several seat tags"),
    };

    Ok(AcceptPlan {
        challenger,
        my_variant,
        their_variant,
        my_seat,
        accept_until,
        cadence,
        rematch,
        premium_imposition,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{MaxConcurrent, PlayConfig, StrengthConfig, WeightedTimeControl};
    use std::collections::BTreeMap;

    const RELAY: &str = "wss://relay.example.com";

    fn play() -> PlayConfig {
        PlayConfig {
            variants: BTreeMap::from([("ogi".to_owned(), 0.8), ("chess".to_owned(), 0.2)]),
            time_controls: vec![
                WeightedTimeControl {
                    spec: vec![vec!["0".into(), "10".into(), "1".into()]],
                    weight: 0.7,
                },
                WeightedTimeControl {
                    spec: vec![vec!["300".into(), "3".into()]],
                    weight: 0.3,
                },
            ],
            accept_any_time_control: false,
            accept_imposed_variant: false,
            max_concurrent: MaxConcurrent::default(),
            strength: StrengthConfig {
                time_ms: 1000,
                depth: 4,
            },
            resign_threshold: -700,
            draw_offer: crate::config::DrawOffer::Balanced,
            challenge_policy: "everyone".to_owned(),
            timeout_courtesy_secs: 5,
        }
    }

    fn tag(name: &str, values: &[&str]) -> Tag {
        Tag::custom(
            name,
            values.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    }

    fn p_role(pubkey: &PublicKey, role: &str) -> Tag {
        Tag::custom("p", [pubkey.to_hex(), String::new(), role.to_string()])
    }

    fn e_marked(id: &EventId, marker: &str) -> Tag {
        Tag::custom("e", [id.to_hex(), String::new(), marker.to_string()])
    }

    fn rules() -> EventId {
        EventId::from_hex(&"7".repeat(64)).unwrap()
    }

    fn open_challenge(
        signer: &Keys,
        matchmaker: &PublicKey,
        rules: &EventId,
        extra: Vec<Tag>,
    ) -> Event {
        let mut tags = vec![
            p_role(matchmaker, "matchmaker"),
            e_marked(rules, "rules"),
            tag("timing_relay", &[RELAY]),
            tag("game", &["sanki"]),
            tag("time_control", &["0", "10", "1"]),
            tag("accept_until", &["2000000300"]),
            tag("nonce", &["0", "0"]),
        ];
        tags.extend(extra);
        EventBuilder::new(Kind::Custom(3418), "")
            .tags(tags)
            .finalize(signer)
            .expect("sign")
    }

    #[test]
    fn courts_a_compatible_mirror_entry() {
        let (me, human, mm) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let entry = open_challenge(
            &human,
            &mm,
            &rules(),
            vec![
                tag("variant", &["self", "ogi"]),
                tag("variant", &["opponent", "ogi"]),
            ],
        );
        let candidate = evaluate_open_challenge(
            &entry,
            &me.public_key(),
            RELAY,
            &mm,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut SplitMix64::new(1),
        )
        .unwrap();
        assert_eq!(candidate.variant, "ogi");
        assert!(candidate.mirror);
        assert_eq!(candidate.spec, vec![vec!["0", "10", "1"]]);
        assert!(!candidate.needs_following_check);
    }

    #[test]
    fn courts_an_asymmetric_entry_when_the_persona_opts_in() {
        // The human plays ogi and imposes chess on us (the premium form). With
        // `accept_imposed_variant`, the persona courts it: our variant is the
        // imposed one, and our own entry must NOT impose back (`mirror: false`).
        let (me, human, mm) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let entry = open_challenge(
            &human,
            &mm,
            &rules(),
            vec![
                tag("variant", &["self", "ogi"]),
                tag("variant", &["opponent", "chess"]),
            ],
        );
        let opted_in = PlayConfig {
            accept_imposed_variant: true,
            ..play()
        };
        let candidate = evaluate_open_challenge(
            &entry,
            &me.public_key(),
            RELAY,
            &mm,
            &rules(),
            "sanki",
            &opted_in,
            2_000_000_000,
            60,
            &mut SplitMix64::new(1),
        )
        .unwrap();
        assert_eq!(candidate.variant, "chess");
        assert!(!candidate.mirror);
        // The imposed variant must still be one the persona plays.
        let imposes_xiongqi = open_challenge(
            &human,
            &mm,
            &rules(),
            vec![
                tag("variant", &["self", "ogi"]),
                tag("variant", &["opponent", "xiongqi"]),
            ],
        );
        assert!(evaluate_open_challenge(
            &imposes_xiongqi,
            &me.public_key(),
            RELAY,
            &mm,
            &rules(),
            "sanki",
            &opted_in,
            2_000_000_000,
            60,
            &mut SplitMix64::new(1),
        )
        .is_err());
    }

    #[test]
    fn skips_incompatible_entries() {
        let (me, human, mm) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let me_pk = me.public_key();
        let base = |extra| open_challenge(&human, &mm, &rules(), extra);
        let eval = |event: &Event| {
            evaluate_open_challenge(
                event,
                &me_pk,
                RELAY,
                &mm,
                &rules(),
                "sanki",
                &play(),
                2_000_000_000,
                60,
                &mut SplitMix64::new(1),
            )
        };
        // A FULLY OPEN entry is courted with the persona's draw, flagged for
        // the fleet-wide claim (ADR-0040 §2).
        let open = eval(&base(vec![])).expect("an open entry is courted");
        assert!(open.open && open.mirror);
        assert!(open.variant == "ogi" || open.variant == "chess");
        assert!(
            !eval(&base(vec![tag("variant", &["self", "ogi"])]))
                .unwrap()
                .open
        );

        // Asymmetric variant terms.
        assert!(eval(&base(vec![
            tag("variant", &["self", "ogi"]),
            tag("variant", &["opponent", "chess"]),
        ]))
        .is_err());
        // Variant outside the persona.
        assert!(eval(&base(vec![tag("variant", &["self", "xiongqi"])])).is_err());
        // Rating filter: fail-closed for an unrated bot.
        assert!(eval(&base(vec![
            tag("variant", &["self", "ogi"]),
            tag("filter", &["rating", "200", &"c".repeat(64), "3426"]),
        ]))
        .is_err());
        // A `following` filter defers to the async check.
        let following = base(vec![
            tag("variant", &["self", "ogi"]),
            tag("filter", &["following"]),
        ]);
        assert!(eval(&following).unwrap().needs_following_check);
        // An attested entry can never pair with ours.
        let attested = base(vec![
            tag("variant", &["self", "ogi"]),
            p_role(&Keys::generate().public_key(), "timestamper"),
        ]);
        assert!(eval(&attested).is_err());
        // Another rule system can never pair with ours.
        let other_rules = open_challenge(
            &human,
            &mm,
            &EventId::from_hex(&"8".repeat(64)).unwrap(),
            vec![tag("variant", &["self", "ogi"])],
        );
        assert_eq!(eval(&other_rules), Err("another rule system"));
        // Expiring too soon.
        assert!(evaluate_open_challenge(
            &base(vec![tag("variant", &["self", "ogi"])]),
            &me_pk,
            RELAY,
            &mm,
            &rules(),
            "sanki",
            &play(),
            2_000_000_290,
            60,
            &mut SplitMix64::new(1),
        )
        .is_err());
    }

    fn direct_challenge(signer: &Keys, me: &PublicKey, rules: &EventId, extra: Vec<Tag>) -> Event {
        let mut tags = vec![
            p_role(me, "opponent"),
            e_marked(rules, "rules"),
            tag("timing_relay", &[RELAY]),
            tag("game", &["sanki"]),
            tag("time_control", &["300", "3"]),
            tag("accept_until", &["2000000300"]),
            tag("nonce", &["0", "0"]),
        ];
        tags.extend(extra);
        EventBuilder::new(Kind::Custom(3420), "")
            .tags(tags)
            .finalize(signer)
            .expect("sign")
    }

    #[test]
    fn accepts_a_free_challenge_supplying_the_open_pieces() {
        let (me, human) = (Keys::generate(), Keys::generate());
        let challenge = direct_challenge(&human, &me.public_key(), &rules(), vec![]);
        let mut rng = SplitMix64::new(1);
        let plan = evaluate_direct_challenge(
            &challenge,
            &me.public_key(),
            RELAY,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .unwrap();
        // Everything was left open: the acceptance supplies both variants
        // (the mirror rule) and the seat.
        assert_eq!(plan.their_variant, plan.my_variant);
        assert!(plan.my_variant == "ogi" || plan.my_variant == "chess");
        assert_eq!(plan.cadence, Cadence::Blitz); // the fixture's 5 + 3
        assert_eq!(plan.rematch, None);
        assert_eq!(plan.challenger, human.public_key());
    }

    #[test]
    fn refuses_asymmetric_impositions_and_foreign_rules() {
        let (me, human) = (Keys::generate(), Keys::generate());
        let me_pk = me.public_key();
        let human_pk = human.public_key();
        let mut rng = SplitMix64::new(1);
        let mut eval = |event: &Event| {
            evaluate_direct_challenge(
                event,
                &me_pk,
                RELAY,
                &rules(),
                "sanki",
                &play(),
                2_000_000_000,
                60,
                &mut rng,
            )
        };

        // Asymmetric: our variant imposed differently from theirs — a plan,
        // flagged for the premium check the async layer performs.
        let asymmetric = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![
                Tag::custom("variant", [human_pk.to_hex(), "chess".into()]),
                Tag::custom("variant", [me_pk.to_hex(), "ogi".into()]),
            ],
        );
        let plan = eval(&asymmetric).expect("a plan pending the premium check");
        assert!(plan.premium_imposition);
        // Asymmetric too: ours imposed while theirs is left open; the mirror
        // rule then gives them ours.
        let open_theirs = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![Tag::custom("variant", [me_pk.to_hex(), "ogi".into()])],
        );
        let plan = eval(&open_theirs).expect("a plan pending the premium check");
        assert!(plan.premium_imposition);
        assert_eq!(
            (plan.my_variant.as_str(), plan.their_variant.as_str()),
            ("ogi", "ogi")
        );
        // An imposition of a variant the persona does not play is refused
        // before any lookup.
        let foreign = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![
                Tag::custom("variant", [human_pk.to_hex(), "chess".into()]),
                Tag::custom("variant", [me_pk.to_hex(), "xiongqi".into()]),
            ],
        );
        assert_eq!(eval(&foreign), Err("imposed variant outside the persona"));

        // Mirror imposition: judged on the variant (ogi is in the persona);
        // the challenger declares `first`, so we hold `second`.
        let mirror = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![
                Tag::custom("variant", [human_pk.to_hex(), "ogi".into()]),
                Tag::custom("variant", [me_pk.to_hex(), "ogi".into()]),
                tag("seat", &["first"]),
            ],
        );
        let plan = eval(&mirror).unwrap();
        assert_eq!(plan.my_variant, "ogi");
        assert_eq!(plan.their_variant, "ogi");
        assert_eq!(plan.my_seat, Seat::Second);

        // A challenge under another rule system is ignored.
        let foreign = direct_challenge(
            &human,
            &me_pk,
            &EventId::from_hex(&"8".repeat(64)).unwrap(),
            vec![],
        );
        assert_eq!(eval(&foreign), Err("another rule system"));
        // A challenge designating another relay too.
        let elsewhere = EventBuilder::new(Kind::Custom(3420), "")
            .tags(vec![
                p_role(&me_pk, "opponent"),
                e_marked(&rules(), "rules"),
                tag("timing_relay", &["wss://other.example.com"]),
                tag("game", &["sanki"]),
                tag("time_control", &["300", "3"]),
                tag("accept_until", &["2000000300"]),
                tag("nonce", &["0", "0"]),
            ])
            .finalize(&human)
            .unwrap();
        assert!(eval(&elsewhere).is_err());
    }

    #[test]
    fn accept_any_time_control_bypasses_the_cadence_gate() {
        let (me, human) = (Keys::generate(), Keys::generate());
        let me_pk = me.public_key();
        // A cadence absent from the persona (which has 10 s/move and 5 + 3).
        let off_cadence = EventBuilder::new(Kind::Custom(3420), "")
            .tags(vec![
                p_role(&me_pk, "opponent"),
                e_marked(&rules(), "rules"),
                tag("timing_relay", &[RELAY]),
                tag("game", &["sanki"]),
                tag("time_control", &["600", "5"]),
                tag("accept_until", &["2000000300"]),
                tag("nonce", &["0", "0"]),
            ])
            .finalize(&human)
            .expect("sign");
        let mut rng = SplitMix64::new(1);

        // Default persona: an off-persona cadence is refused.
        assert!(evaluate_direct_challenge(
            &off_cadence,
            &me_pk,
            RELAY,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .is_err());

        // With accept_any_time_control set, the same challenge is accepted.
        let mut any = play();
        any.accept_any_time_control = true;
        assert!(evaluate_direct_challenge(
            &off_cadence,
            &me_pk,
            RELAY,
            &rules(),
            "sanki",
            &any,
            2_000_000_000,
            60,
            &mut rng,
        )
        .is_ok());
    }

    #[test]
    fn cross_variant_by_delegation_is_free_and_the_pool_knob_does_not_reach_the_direct_path() {
        let (me, human) = (Keys::generate(), Keys::generate());
        let me_pk = me.public_key();
        let human_pk = human.public_key();
        let mut rng = SplitMix64::new(1);
        // The challenger plays chess and leaves ours open: the persona draws,
        // the game is cross-variant, and nothing premium is involved.
        let delegated = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![Tag::custom("variant", [human_pk.to_hex(), "chess".into()])],
        );
        let plan = evaluate_direct_challenge(
            &delegated,
            &me_pk,
            RELAY,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .expect("delegation is free");
        assert!(!plan.premium_imposition);
        assert_eq!(plan.their_variant, "chess");
        assert!(plan.my_variant == "ogi" || plan.my_variant == "chess");
        // The pool knob (`accept_imposed_variant`) changes nothing here: the
        // direct path's asymmetric imposition is a premium question, not a
        // persona one (ADR-0040 §2).
        let cross = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![
                Tag::custom("variant", [human_pk.to_hex(), "chess".into()]),
                Tag::custom("variant", [me_pk.to_hex(), "ogi".into()]),
            ],
        );
        for knob in [false, true] {
            let persona = PlayConfig {
                accept_imposed_variant: knob,
                ..play()
            };
            let plan = evaluate_direct_challenge(
                &cross,
                &me_pk,
                RELAY,
                &rules(),
                "sanki",
                &persona,
                2_000_000_000,
                60,
                &mut rng,
            )
            .expect("a plan pending the premium check");
            assert!(plan.premium_imposition);
            assert_eq!(plan.my_variant, "ogi");
            assert_eq!(plan.their_variant, "chess");
        }
    }

    #[test]
    fn a_rematch_challenge_bypasses_the_persona_gates_but_must_fix_everything() {
        let (me, human) = (Keys::generate(), Keys::generate());
        let me_pk = me.public_key();
        let human_pk = human.public_key();
        let concluded = EventId::from_hex(&"a".repeat(64)).unwrap();
        let conclusion = EventId::from_hex(&"b".repeat(64)).unwrap();
        let mut rng = SplitMix64::new(1);
        // Cross-variant and off-cadence: a fresh challenge like this would be
        // refused by the default persona; as a rematch of a session we played,
        // it is accepted (the async layer verifies the concluded session).
        let rematch = EventBuilder::new(Kind::Custom(3420), "")
            .tags(vec![
                p_role(&me_pk, "opponent"),
                e_marked(&rules(), "rules"),
                e_marked(&concluded, "rematch_of"),
                e_marked(&conclusion, "concluded_by"),
                tag("timing_relay", &[RELAY]),
                tag("game", &["sanki"]),
                tag("time_control", &["600", "5"]),
                Tag::custom("variant", [human_pk.to_hex(), "chess".into()]),
                Tag::custom("variant", [me_pk.to_hex(), "xiongqi".into()]),
                tag("seat", &["second"]),
                tag("accept_until", &["2000000300"]),
                tag("nonce", &["0", "0"]),
            ])
            .finalize(&human)
            .unwrap();
        let plan = evaluate_direct_challenge(
            &rematch,
            &me_pk,
            RELAY,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .unwrap();
        assert_eq!(plan.my_variant, "xiongqi");
        assert_eq!(plan.their_variant, "chess");
        assert_eq!(plan.my_seat, Seat::First);
        assert_eq!(
            plan.rematch,
            Some(RematchRefs {
                concluded,
                concluded_by: conclusion
            })
        );
        // A rematch leaving the seat open, or a variant open, is malformed.
        let seatless = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![
                e_marked(&concluded, "rematch_of"),
                e_marked(&conclusion, "concluded_by"),
                Tag::custom("variant", [human_pk.to_hex(), "ogi".into()]),
                Tag::custom("variant", [me_pk.to_hex(), "ogi".into()]),
            ],
        );
        assert!(evaluate_direct_challenge(
            &seatless,
            &me_pk,
            RELAY,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .is_err());
        // A rematch fixing only OUR variant is malformed too — refused as such,
        // never read as an asymmetric imposition (no premium lookup).
        let ours_only = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![
                e_marked(&concluded, "rematch_of"),
                e_marked(&conclusion, "concluded_by"),
                Tag::custom("variant", [me_pk.to_hex(), "ogi".into()]),
                tag("seat", &["second"]),
            ],
        );
        assert_eq!(
            evaluate_direct_challenge(
                &ours_only,
                &me_pk,
                RELAY,
                &rules(),
                "sanki",
                &play(),
                2_000_000_000,
                60,
                &mut rng,
            ),
            Err("a rematch challenge must fix both variants")
        );
        // One rematch tag without the other is malformed.
        let half = direct_challenge(
            &human,
            &me_pk,
            &rules(),
            vec![e_marked(&concluded, "rematch_of")],
        );
        assert!(evaluate_direct_challenge(
            &half,
            &me_pk,
            RELAY,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .is_err());
    }

    #[test]
    fn a_courted_entry_carries_its_cadence() {
        let (me, human, mm) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let entry = open_challenge(
            &human,
            &mm,
            &rules(),
            vec![tag("variant", &["self", "ogi"])],
        );
        let candidate = evaluate_open_challenge(
            &entry,
            &me.public_key(),
            RELAY,
            &mm,
            &rules(),
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut SplitMix64::new(1),
        )
        .unwrap();
        assert_eq!(candidate.cadence, Cadence::Byoyomi);
    }

    #[test]
    fn a_direct_challenge_without_a_cadence_is_refused_even_at_any_time_control() {
        // `accept_any_time_control` relaxes the persona's gate, not the
        // format: a malformed first period has no cadence and no cap.
        let (me, human) = (Keys::generate(), Keys::generate());
        let mut rng = SplitMix64::new(1);
        let any = PlayConfig {
            accept_any_time_control: true,
            ..play()
        };
        let their_variant = human.public_key().to_hex();
        let tags_of = |rows: &[&[&str]]| -> Vec<Tag> {
            let mut v = vec![
                p_role(&me.public_key(), "opponent"),
                e_marked(&rules(), "rules"),
                tag("timing_relay", &[RELAY]),
                tag("game", &["sanki"]),
                tag("accept_until", &["2000000300"]),
                tag("nonce", &["0", "0"]),
                tag("variant", &[their_variant.as_str(), "ogi"]),
            ];
            for row in rows {
                v.push(tag("time_control", row));
            }
            v
        };
        let sign = |t: Vec<Tag>| {
            EventBuilder::new(Kind::Custom(3420), "")
                .tags(t)
                .finalize(&human)
                .expect("sign")
        };
        let odd = sign(tags_of(&[&["7200"]]));
        let plan = evaluate_direct_challenge(
            &odd,
            &me.public_key(),
            RELAY,
            &rules(),
            "sanki",
            &any,
            2_000_000_000,
            60,
            &mut rng,
        )
        .unwrap();
        // ADR-0039: a two-hour bank is a live rapid game, not correspondence.
        assert_eq!(plan.cadence, Cadence::Rapid);
        assert!(!plan.cadence.is_correspondence());
        let malformed = sign(tags_of(&[&["0", "10"]]));
        assert_eq!(
            evaluate_direct_challenge(
                &malformed,
                &me.public_key(),
                RELAY,
                &rules(),
                "sanki",
                &any,
                2_000_000_000,
                60,
                &mut rng,
            ),
            Err("no cadence (malformed time control)")
        );
    }
}
