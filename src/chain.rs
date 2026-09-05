//! The bot's live view of a session — computed by the session's **module**,
//! never by a reimplementation (ADR-0034; ADR-0014 §Chain and clock logic):
//! what the rule system yields right now is what the bot believes.
//!
//! In self-timed mode every event's canonical timing is its own `created_at`,
//! so the module's `natural_state` **at the present instant** is a live oracle
//! of the session: the selected chain, the reached position, both clocks, and
//! whose turn it is. `verdict_at` at the same instant, with the bot's own seat
//! as the invoker, is the prediction of the claim a Conclusion published now
//! MUST carry to conform (kind `3425` §Client guidelines, item 7).
//!
//! The search's history bookkeeping — the occurrence counts along the chain
//! and the half-move clock — is replayed through the module's `apply`, one
//! Ply at a time from the initial position: the positions it visits are the
//! rule system's, and the `irreversible` bit it reports is the rule the
//! move-limit counter runs on.

use anyhow::{anyhow, Result};
use sashite_sanki_player::{Occurrences, Position};
use serde_json::{json, Value};

use crate::clockmath::max_affordable;
use crate::module::{self, End, Oracle, Verdict, VerdictAt};
use crate::session::{Events, Seat, SessionTerms};

/// What the bot knows about a session at an instant.
#[derive(Debug, Clone)]
pub struct SessionView {
    /// The canonical chain's length (applied half-moves).
    pub chain_len: usize,
    /// The next play-order position (chain length + 1).
    pub next_half_move: u32,
    /// The seat on move at that position.
    pub on_move: Seat,
    /// The mover's own step ordinal there.
    pub step: u32,
    /// The chain already reached a terminal verdict (a Conclusion is due).
    pub terminal: bool,
    /// The tip position, as the module encodes it (FEEN).
    pub tip: String,
    /// Occurrence counts along initial + chain, keyed by the search engine's
    /// own canonical FEEN of each position — the caller obligation of
    /// ADR-0015 §2, mirrored from the module's replay.
    pub occurrences: Occurrences,
    /// The tip's half-move clock: plies since the last irreversible move.
    pub halfmove_clock: u32,
    /// The last canonical timing (t₀ or the last selected ply's) — the
    /// anchor the NEXT ply's elapsed runs from.
    pub anchor: u64,
    /// Seconds the mover may take past `anchor` before flagging.
    pub affordable: u64,
    /// Whether the LAST chain ply carries the `draw` offer flag.
    pub last_ply_offers_draw: bool,
}

/// The session members of a request: the terms (given t₀), the Plies and the
/// attestations, in the ABI's shape — the base every session operation
/// extends with its `op`.
#[must_use]
pub fn session_request(terms: &SessionTerms, start: u64, events: &Events) -> Value {
    json!({
        "session": terms.to_json(start),
        "plies": events.plies,
        "attestations": events.attestations,
    })
}

/// The seat on move at a 1-based play-order position.
#[must_use]
pub const fn seat_at(half_move: u32) -> Seat {
    if half_move % 2 == 1 {
        Seat::First
    } else {
        Seat::Second
    }
}

/// The mover's own step ordinal at a 1-based play-order position.
#[must_use]
pub const fn step_at(half_move: u32) -> u32 {
    half_move.div_ceil(2)
}

/// Compute the live view at `now`.
///
/// # Errors
///
/// When the module gives no usable answer, or a selected Ply cannot be found
/// among the events offered (a module answering with an id it was not given —
/// a build defect, never a session fact).
pub fn session_view(
    oracle: &mut impl Oracle,
    terms: &SessionTerms,
    start: u64,
    events: &Events,
    now: u64,
) -> Result<SessionView> {
    let request = session_request(terms, start, events);
    let state = module::natural_state(oracle, &request, now)?;

    // Mirror the history bookkeeping by replaying the selected chain through
    // the module's `apply`.
    let mut occurrences = Occurrences::new();
    let mut position = terms.position.clone();
    let mut halfmove_clock: u32 = 0;
    let mut last_ply_offers_draw = false;
    bump(&mut occurrences, &position);
    for link in &state.chain {
        let ply = events
            .plies
            .iter()
            .find(|ply| ply.get("id").and_then(Value::as_str) == Some(link.id.as_str()))
            .ok_or_else(|| anyhow!("the module selected an unknown Ply {}", link.id))?;
        let content = ply
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("a Ply offered without content"))?;
        let applied = module::apply(oracle, &position, content)?;
        position = applied.position;
        halfmove_clock = if applied.irreversible {
            0
        } else {
            halfmove_clock.saturating_add(1)
        };
        last_ply_offers_draw = ply.get("draw").and_then(Value::as_bool).unwrap_or(false);
        bump(&mut occurrences, &position);
    }

    let chain_len = state.chain.len();
    let (terminal, next_half_move, tip, anchor, affordable) = match state.end {
        End::Terminal { at, .. } => {
            let next = u32::try_from(chain_len.saturating_add(1)).unwrap_or(u32::MAX);
            (true, next, position, at, 0)
        }
        End::Ongoing {
            anchor,
            clocks,
            half_move,
            position: end_position,
        } => {
            if end_position != position {
                return Err(anyhow!(
                    "the replayed tip {position} differs from the module's end position {end_position}"
                ));
            }
            let clock = match seat_at(half_move) {
                Seat::First => clocks.first,
                Seat::Second => clocks.second,
            };
            (
                false,
                half_move,
                end_position,
                anchor,
                max_affordable(&terms.time_control, clock),
            )
        }
    };

    Ok(SessionView {
        chain_len,
        next_half_move,
        on_move: seat_at(next_half_move),
        step: step_at(next_half_move),
        terminal,
        tip,
        occurrences,
        halfmove_clock,
        anchor,
        affordable,
        last_ply_offers_draw,
    })
}

/// Counts one occurrence of `feen`, keyed by the search engine's canonical
/// form when it parses it (it is the form the search keys its own visits by),
/// else by the module's string.
fn bump(occurrences: &mut Occurrences, feen: &str) {
    let key = Position::parse(feen).map_or_else(|_| feen.to_owned(), |p| p.to_feen());
    let count = occurrences.entry(key).or_insert(0);
    *count = count.saturating_add(1);
}

/// The verdict a Conclusion signed by the player seated `me` and timed `now`
/// would have to claim — the prediction, via the module itself. `None` before
/// t₀.
///
/// # Errors
///
/// When the module gives no usable answer.
pub fn predicted_verdict(
    oracle: &mut impl Oracle,
    terms: &SessionTerms,
    start: u64,
    events: &Events,
    me: Seat,
    now: u64,
) -> Result<Option<Verdict>> {
    let request = session_request(terms, start, events);
    Ok(
        match module::verdict_at(oracle, &request, me.name(), now)? {
            VerdictAt::Verdict(verdict) => Some(verdict),
            VerdictAt::NoVerdict(_) => None,
        },
    )
}

/// Whether `verdict` is a win for `seat`.
#[must_use]
pub const fn wins(verdict: &Verdict, seat: Seat) -> bool {
    match seat {
        Seat::First => verdict.result.first > verdict.result.second,
        Seat::Second => verdict.result.second > verdict.result.first,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use crate::module::native::Native;
    use crate::session::fixtures::World;
    use crate::session::{terms, Events};
    use nostr_sdk::prelude::*;

    /// A chess/chess session founded at t₀ = 1 000, 5 + 3 Fischer.
    fn world() -> (World, SessionTerms) {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_000);
        let t = terms(&session, &pairing).unwrap();
        (w, t)
    }

    #[test]
    fn empty_session_is_the_first_players_turn_from_t0() {
        let (_, t) = world();
        let view = session_view(&mut Native, &t, 1_000, &Events::default(), 2_000).unwrap();
        assert_eq!(view.chain_len, 0);
        assert_eq!(view.next_half_move, 1);
        assert_eq!(view.on_move, Seat::First);
        assert_eq!(view.step, 1);
        assert!(!view.terminal);
        assert_eq!(view.anchor, 1_000); // t₀
        assert_eq!(view.affordable, 300);
        assert_eq!(view.occurrences.len(), 1);
        assert_eq!(view.tip, t.position);
    }

    #[test]
    fn chain_advances_turn_anchor_and_occurrences() {
        let (w, t) = world();
        let a = w.ply(&t.id, &w.first, 1, r#"["e2","e4",null]"#, 1_010);
        let mut b = w.ply(&t.id, &w.second, 1, r#"["e7","e5",null]"#, 1_025);
        // The second Ply offers a draw.
        b = EventBuilder::new(b.kind, b.content.clone())
            .tags(
                b.tags
                    .iter()
                    .cloned()
                    .chain([Tag::parse(["draw"]).unwrap()]),
            )
            .custom_created_at(b.created_at)
            .finalize(&w.second)
            .unwrap();
        let events = Events::from_relay([&a, &b], &t, 300);
        let view = session_view(&mut Native, &t, 1_000, &events, 2_000).unwrap();
        assert_eq!(view.chain_len, 2);
        assert_eq!(view.next_half_move, 3);
        assert_eq!(view.on_move, Seat::First);
        assert_eq!(view.step, 2);
        assert_eq!(view.anchor, 1_025);
        assert!(view.last_ply_offers_draw);
        assert_eq!(view.occurrences.values().sum::<u32>(), 3);
        // Both pawn moves are irreversible.
        assert_eq!(view.halfmove_clock, 0);
        // The first player spent 10 s and earned the increment: 300 − 10 + 3,
        // still to be spent at half-move 3.
        assert_eq!(view.affordable, 293);
        // The tip is a position the search engine parses.
        assert!(sashite_sanki_player::Position::parse(&view.tip).is_ok());
    }

    #[test]
    fn prediction_matches_the_session_state() {
        let (_, t) = world();
        let events = Events::default();
        // No plies, probe far past the budget: the on-move first player has
        // flagged by abandonment — the second player predicts a win on time.
        let verdict = predicted_verdict(&mut Native, &t, 1_000, &events, Seat::Second, 40_000)
            .unwrap()
            .unwrap();
        assert_eq!(verdict.status, "timeout");
        assert!(wins(&verdict, Seat::Second));
        // The same probe just after t₀ resolves as the residual resignation
        // AGAINST THE INVOKER — which is exactly why the bot never concludes
        // without wanting the predicted verdict.
        let early = predicted_verdict(&mut Native, &t, 1_000, &events, Seat::Second, 1_010)
            .unwrap()
            .unwrap();
        assert_eq!(early.status, "resignation");
        assert!(wins(&early, Seat::First));
        // Before t₀: no verdict.
        assert!(
            predicted_verdict(&mut Native, &t, 1_000, &events, Seat::Second, 999)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn seats_and_steps_follow_the_alternation() {
        assert_eq!(seat_at(1), Seat::First);
        assert_eq!(seat_at(2), Seat::Second);
        assert_eq!(seat_at(7), Seat::First);
        assert_eq!(step_at(1), 1);
        assert_eq!(step_at(2), 1);
        assert_eq!(step_at(3), 2);
        assert_eq!(step_at(4), 2);
    }
}
