//! Courtship decisions — pool entries (kind 3418) and Direct Challenges
//! (kinds 3420/3421), per ADR-0014 §6.2–§6.3.
//!
//! Everything here is **pure** over borrowed events and persona data. The
//! async layer performs the fetches a decision may require (contact lists,
//! mute lists) and enforces the fleet ledger; refusals are by silence, so a
//! `Skip` simply produces a debug log upstream.

use nostr_sdk::prelude::*;

use crate::config::PlayConfig;
use crate::prng::SplitMix64;
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
    /// The entry's `accept_until` (unix seconds).
    pub accept_until: u64,
    /// Whether the entry carries a `following` filter — the async layer must
    /// verify the counterparty follows the bot (fail-closed) before entering.
    pub needs_following_check: bool,
}

/// Why an entry (or challenge) is not courted. Logged, never answered.
pub type Skip = &'static str;

/// Evaluate an Open Challenge from the bot's side (§6.2, reactive path).
/// `now` in unix seconds; `margin_secs` is the minimum life the entry must
/// still have for our own entry to land and pair.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_open_challenge(
    event: &Event,
    me: &PublicKey,
    matchmaker: &PublicKey,
    arbiter: &PublicKey,
    game: &str,
    play: &PlayConfig,
    now: u64,
    margin_secs: u64,
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
    // Pairing requires both entries to designate the same arbiter and the
    // same timing mode: ours names the configured arbiter and no
    // timestamper, so anything else can never pair with us (§6.2).
    if tags::pubkey_with_role(event, "arbiter").as_ref() != Some(arbiter) {
        return Err("other arbiter");
    }
    if tags::pubkey_with_role(event, "timestamper").is_some() {
        return Err("attested mode (v1 is self-timed)");
    }
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
            return Err("no variant term to mirror (persona draw is for spontaneous entries)")
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

    Ok(PoolCandidate {
        challenger: event.pubkey,
        variant: variant.to_owned(),
        mirror,
        spec: rows,
        accept_until,
        needs_following_check,
    })
}

/// A Direct Challenge (kind 3420) the persona would accept, with what the
/// acceptance must supply (§6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptPlan {
    /// The challenger.
    pub challenger: PublicKey,
    /// The bot's own variant: the challenge's imposition, or a persona draw.
    pub my_variant: String,
    /// The bot's variant to DECLARE on the acceptance — `Some` only when the
    /// challenge left it open. Kind 3421 constraint 7: the acceptance MUST NOT
    /// re-declare a variant the challenge already fixed, or the pair is invalid
    /// and the arbiter never founds the game.
    pub supply_my_variant: Option<String>,
    /// The challenger's variant to supply on the acceptance (mirror rule),
    /// when the challenge delegated it.
    pub supply_challenger_variant: Option<String>,
    /// The seat to supply (uniform draw), when the challenge left it open.
    pub supply_seat: Option<&'static str>,
    /// The challenge's `accept_until`.
    pub accept_until: u64,
    /// Whether the cadence is correspondence-paced (acceptance at any hour —
    /// resolved question 5; live challenges only while present).
    pub correspondence: bool,
}

/// Evaluate a Direct Challenge addressed to the bot (§6.3). `rng` covers the
/// persona draws (variant when free, seat when open).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_direct_challenge(
    event: &Event,
    me: &PublicKey,
    arbiter: &PublicKey,
    game: &str,
    play: &PlayConfig,
    now: u64,
    margin_secs: u64,
    rng: &mut SplitMix64,
) -> Result<AcceptPlan, Skip> {
    if tags::pubkey_with_role(event, "opponent").as_ref() != Some(me) {
        return Err("not addressed to us");
    }
    if tags::game(event) != Some(game) {
        return Err("other game");
    }
    if tags::pubkey_with_role(event, "arbiter").as_ref() != Some(arbiter) {
        return Err("arbiter we do not play under");
    }
    if tags::pubkey_with_role(event, "timestamper").is_some() {
        return Err("attested mode (v1 is self-timed)");
    }
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

    let rows = tags::time_control_rows(event);
    if !play.accept_any_time_control
        && !play
            .time_controls
            .iter()
            .any(|preference| preference.spec == rows)
    {
        return Err("cadence outside the persona");
    }

    // Variant terms (§6.3): every ASYMMETRIC imposition of our variant is
    // refused as persona policy (needs no premium lookup); a MIRROR
    // imposition is judged on the variant itself.
    let their_variant = tags::variant_for(event, &challenger).map(str::to_owned);
    let imposed_mine = tags::variant_for(event, me).map(str::to_owned);
    let my_variant = match (&their_variant, &imposed_mine) {
        (Some(theirs), Some(mine)) if theirs != mine => {
            // Explicit cross-variant: the challenger plays `theirs` and assigns us
            // the DIFFERENT `mine`. Refused as persona policy UNLESS the persona
            // opts in (`accept_imposed_variant`), and even then only for a variant
            // it actually plays.
            if !play.accept_imposed_variant {
                return Err("asymmetric variant imposition (persona policy)");
            }
            if play.variants.get(mine).copied().unwrap_or(0.0) <= 0.0 {
                return Err("imposed variant outside the persona");
            }
            mine.clone()
        }
        (None, Some(_)) => return Err("imposed variant while theirs is open (asymmetric)"),
        (_, Some(mine)) => {
            if play.variants.get(mine).copied().unwrap_or(0.0) <= 0.0 {
                return Err("imposed variant outside the persona");
            }
            mine.clone()
        }
        (_, None) => crate::persona::draw_weighted_key(&play.variants, rng)
            .ok_or("no persona variant")?
            .to_owned(),
    };

    // The acceptance supplies exactly what the challenge left open, and MUST NOT
    // re-declare a term the challenge already fixed (kind 3421 constraint 7 — the
    // same-player variant tag present in BOTH events invalidates the pair): the
    // bot's own variant only when it was not imposed, the challenger's by the
    // mirror rule when delegated, the seat by a uniform draw when open.
    let supply_my_variant = if imposed_mine.is_none() {
        Some(my_variant.clone())
    } else {
        None
    };
    let supply_challenger_variant = if their_variant.is_none() {
        Some(my_variant.clone())
    } else {
        None
    };
    let supply_seat = if tags::seat(event).is_none() {
        Some(if rng.next_index(2) == 0 {
            "first"
        } else {
            "second"
        })
    } else {
        None
    };

    Ok(AcceptPlan {
        challenger,
        my_variant,
        supply_my_variant,
        supply_challenger_variant,
        supply_seat,
        accept_until,
        correspondence: is_correspondence(&rows),
    })
}

/// Whether a cadence is correspondence-paced: any period whose per-move
/// allowance or bank is measured in hours (§6.3, resolved question 5).
#[must_use]
pub fn is_correspondence(rows: &[Vec<String>]) -> bool {
    rows.iter().any(|row| {
        let duration: u64 = row.first().and_then(|v| v.parse().ok()).unwrap_or(0);
        let increment: u64 = row.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
        duration >= 7_200 || increment >= 3_600
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{PlayConfig, StrengthConfig, WeightedTimeControl};
    use std::collections::BTreeMap;

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
            max_live: 1,
            max_correspondence: 4,
            strength: StrengthConfig {
                time_ms: 1000,
                depth: 4,
            },
            resign_threshold: -700,
            draw_offer: crate::config::DrawOffer::Balanced,
            challenge_policy: "everyone".to_owned(),
        }
    }

    fn tag(name: &str, values: &[&str]) -> Tag {
        Tag::custom(
            TagKind::custom(name),
            values.iter().map(ToString::to_string).collect::<Vec<_>>(),
        )
    }

    fn p_role(pubkey: &PublicKey, role: &str) -> Tag {
        Tag::custom(
            TagKind::p(),
            [pubkey.to_hex(), String::new(), role.to_string()],
        )
    }

    fn open_challenge(
        signer: &Keys,
        matchmaker: &PublicKey,
        arbiter: &PublicKey,
        extra: Vec<Tag>,
    ) -> Event {
        let mut tags = vec![
            p_role(matchmaker, "matchmaker"),
            p_role(arbiter, "arbiter"),
            tag("game", &["sanki"]),
            tag("time_control", &["0", "10", "1"]),
            tag("accept_until", &["2000000300"]),
        ];
        tags.extend(extra);
        EventBuilder::new(Kind::Custom(3418), "")
            .tags(tags)
            .sign_with_keys(signer)
            .expect("sign")
    }

    #[test]
    fn courts_a_compatible_mirror_entry() {
        let (me, human, mm, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
            Keys::generate().public_key(),
        );
        let entry = open_challenge(
            &human,
            &mm,
            &arb,
            vec![
                tag("variant", &["self", "ogi"]),
                tag("variant", &["opponent", "ogi"]),
            ],
        );
        let candidate = evaluate_open_challenge(
            &entry,
            &me.public_key(),
            &mm,
            &arb,
            "sanki",
            &play(),
            2_000_000_000,
            60,
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
        let (me, human, mm, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
            Keys::generate().public_key(),
        );
        let entry = open_challenge(
            &human,
            &mm,
            &arb,
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
            &mm,
            &arb,
            "sanki",
            &opted_in,
            2_000_000_000,
            60,
        )
        .unwrap();
        assert_eq!(candidate.variant, "chess");
        assert!(!candidate.mirror);
        // The imposed variant must still be one the persona plays.
        let imposes_xiongqi = open_challenge(
            &human,
            &mm,
            &arb,
            vec![
                tag("variant", &["self", "ogi"]),
                tag("variant", &["opponent", "xiongqi"]),
            ],
        );
        assert!(evaluate_open_challenge(
            &imposes_xiongqi,
            &me.public_key(),
            &mm,
            &arb,
            "sanki",
            &opted_in,
            2_000_000_000,
            60,
        )
        .is_err());
    }

    #[test]
    fn skips_incompatible_entries() {
        let (me, human, mm, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
            Keys::generate().public_key(),
        );
        let me_pk = me.public_key();
        let base = |extra| open_challenge(&human, &mm, &arb, extra);
        let eval = |event: &Event| {
            evaluate_open_challenge(
                event,
                &me_pk,
                &mm,
                &arb,
                "sanki",
                &play(),
                2_000_000_000,
                60,
            )
        };

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
        // Expiring too soon.
        assert!(evaluate_open_challenge(
            &base(vec![tag("variant", &["self", "ogi"])]),
            &me_pk,
            &mm,
            &arb,
            "sanki",
            &play(),
            2_000_000_290,
            60
        )
        .is_err());
    }

    fn direct_challenge(
        signer: &Keys,
        me: &PublicKey,
        arbiter: &PublicKey,
        extra: Vec<Tag>,
    ) -> Event {
        let mut tags = vec![
            p_role(me, "opponent"),
            p_role(arbiter, "arbiter"),
            tag("game", &["sanki"]),
            tag("time_control", &["300", "3"]),
            tag("accept_until", &["2000000300"]),
        ];
        tags.extend(extra);
        EventBuilder::new(Kind::Custom(3420), "")
            .tags(tags)
            .sign_with_keys(signer)
            .expect("sign")
    }

    #[test]
    fn accepts_a_free_challenge_supplying_the_open_pieces() {
        let (me, human, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let challenge = direct_challenge(&human, &me.public_key(), &arb, vec![]);
        let mut rng = SplitMix64::new(1);
        let plan = evaluate_direct_challenge(
            &challenge,
            &me.public_key(),
            &arb,
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .unwrap();
        // Everything was left open: the acceptance supplies variant (mirror)
        // and seat.
        assert_eq!(
            plan.supply_challenger_variant.as_deref(),
            Some(plan.my_variant.as_str())
        );
        // The bot's own variant was open too, so it is declared on the acceptance.
        assert_eq!(
            plan.supply_my_variant.as_deref(),
            Some(plan.my_variant.as_str())
        );
        assert!(plan.supply_seat.is_some());
        assert!(!plan.correspondence);
    }

    #[test]
    fn refuses_asymmetric_impositions_and_foreign_arbiters() {
        let (me, human, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let me_pk = me.public_key();
        let human_pk = human.public_key();
        let mut rng = SplitMix64::new(1);
        let mut eval = |event: &Event| {
            evaluate_direct_challenge(
                event,
                &me_pk,
                &arb,
                "sanki",
                &play(),
                2_000_000_000,
                60,
                &mut rng,
            )
        };

        // Asymmetric: our variant imposed differently from theirs.
        let asymmetric = direct_challenge(
            &human,
            &me_pk,
            &arb,
            vec![
                Tag::custom(
                    TagKind::custom("variant"),
                    [human_pk.to_hex(), "chess".into()],
                ),
                Tag::custom(TagKind::custom("variant"), [me_pk.to_hex(), "ogi".into()]),
            ],
        );
        assert!(eval(&asymmetric).is_err());

        // Mirror imposition: judged on the variant (ogi is in the persona).
        let mirror = direct_challenge(
            &human,
            &me_pk,
            &arb,
            vec![
                Tag::custom(
                    TagKind::custom("variant"),
                    [human_pk.to_hex(), "ogi".into()],
                ),
                Tag::custom(TagKind::custom("variant"), [me_pk.to_hex(), "ogi".into()]),
            ],
        );
        let plan = eval(&mirror).unwrap();
        assert_eq!(plan.my_variant, "ogi");
        assert_eq!(plan.supply_challenger_variant, None);
        // Both variants fixed by the challenge — the acceptance declares neither.
        assert_eq!(plan.supply_my_variant, None);

        // A challenge under another arbiter is ignored.
        let foreign = direct_challenge(&human, &me_pk, &Keys::generate().public_key(), vec![]);
        assert!(eval(&foreign).is_err());
    }

    #[test]
    fn accept_any_time_control_bypasses_the_cadence_gate() {
        let (me, human, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let me_pk = me.public_key();
        // A cadence absent from the persona (which has 10 s/move and 5 + 3).
        let off_cadence = EventBuilder::new(Kind::Custom(3420), "")
            .tags(vec![
                p_role(&me_pk, "opponent"),
                p_role(&arb, "arbiter"),
                tag("game", &["sanki"]),
                tag("time_control", &["600", "5"]),
                tag("accept_until", &["2000000300"]),
            ])
            .sign_with_keys(&human)
            .expect("sign");
        let mut rng = SplitMix64::new(1);

        // Default persona: an off-persona cadence is refused.
        assert!(evaluate_direct_challenge(
            &off_cadence,
            &me_pk,
            &arb,
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
            &arb,
            "sanki",
            &any,
            2_000_000_000,
            60,
            &mut rng,
        )
        .is_ok());
    }

    #[test]
    fn accept_imposed_variant_allows_explicit_cross_variant() {
        let (me, human, arb) = (
            Keys::generate(),
            Keys::generate(),
            Keys::generate().public_key(),
        );
        let me_pk = me.public_key();
        let human_pk = human.public_key();
        // The challenger plays chess and imposes ogi on us — an explicit
        // cross-variant game. The cadence is a persona one, so only the variant
        // terms are under test.
        let cross = EventBuilder::new(Kind::Custom(3420), "")
            .tags(vec![
                p_role(&me_pk, "opponent"),
                p_role(&arb, "arbiter"),
                tag("game", &["sanki"]),
                tag("time_control", &["0", "10", "1"]),
                tag("accept_until", &["2000000300"]),
                Tag::custom(
                    TagKind::custom("variant"),
                    [human_pk.to_hex(), "chess".into()],
                ),
                Tag::custom(TagKind::custom("variant"), [me_pk.to_hex(), "ogi".into()]),
            ])
            .sign_with_keys(&human)
            .expect("sign");
        let mut rng = SplitMix64::new(1);

        // Default persona: the asymmetric imposition is refused.
        assert!(evaluate_direct_challenge(
            &cross,
            &me_pk,
            &arb,
            "sanki",
            &play(),
            2_000_000_000,
            60,
            &mut rng,
        )
        .is_err());

        // With accept_imposed_variant set, the bot takes the imposed ogi and
        // supplies nothing (the challenger already fixed both variants).
        let mut any = play();
        any.accept_imposed_variant = true;
        let plan = evaluate_direct_challenge(
            &cross,
            &me_pk,
            &arb,
            "sanki",
            &any,
            2_000_000_000,
            60,
            &mut rng,
        )
        .expect("accept the imposed variant");
        assert_eq!(plan.my_variant, "ogi");
        assert_eq!(plan.supply_challenger_variant, None);
        // The imposed variant is NOT re-declared on the acceptance (3421 c7).
        assert_eq!(plan.supply_my_variant, None);
    }

    #[test]
    fn correspondence_paces_are_recognized() {
        assert!(is_correspondence(&[vec![
            "0".into(),
            "259200".into(),
            "1".into()
        ]]));
        assert!(is_correspondence(&[vec!["86400".into()]]));
        assert!(!is_correspondence(&[vec!["300".into(), "3".into()]]));
        assert!(!is_correspondence(&[vec![
            "0".into(),
            "10".into(),
            "1".into()
        ]]));
    }
}
