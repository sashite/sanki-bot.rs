//! Concluding a session — the Conclusion (kind 3425) the bot publishes, and
//! when (ADR-0014 §6.6 under ADR-0033: no arbiter to invoke, the player
//! concludes). A Conclusion is a **claim** the rule system checks: it conforms
//! only when it states exactly the verdict the module yields at its cutoff
//! with its signer as the invoker (kind `3425` §Semantic constraints, item 8).
//! So the bot publishes one only when it **wants** the predicted verdict —
//! and the prediction is the module's own `verdict_at`, at the present
//! instant, with the bot's seat (`chain::predicted_verdict`).
//!
//! Pure: the decision over the prediction and the persona, and the event's
//! tags. The actor paces the timeout claim (a courtesy delay, so that a human
//! whose clock just fell is not flagged to the second) and publishes.

use nostr_sdk::prelude::*;

use crate::chain::{wins, SessionView};
use crate::module::Verdict;
use crate::session::{Seat, SessionTerms};

/// Why a Conclusion published now is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conclude {
    /// The chain itself reached a rule-system ending (or a played-Ply
    /// timeout): the verdict is already fact, either player may state it.
    Terminal,
    /// The opponent's clock has fallen by abandonment: the win on time,
    /// claimed by the winner only (the loser never hurries their own flag).
    WinOnTime,
    /// The opponent's standing draw offer: concluding IS the acceptance.
    AcceptDraw,
    /// The residual resignation, against the signer: the persona gave up.
    Resign,
}

/// What a Conclusion signed by `me` and timed now would achieve, if it is
/// worth publishing — the bot concludes ONLY when it wants `predicted`.
#[must_use]
pub fn decision(
    predicted: &Verdict,
    me: Seat,
    view: &SessionView,
    wants_draw: bool,
    wants_resign: bool,
) -> Option<Conclude> {
    if view.terminal {
        return Some(Conclude::Terminal);
    }
    match predicted.status.as_str() {
        // A rule-system ending the replay reached but the view did not flag
        // as terminal cannot happen (both are the module's); a background
        // draw resolved on the end position is stated by either player.
        "checkmate" | "stalemate" | "nomove" | "insufficient" | "repetition" | "movelimit"
        | "movecap" => Some(Conclude::Terminal),
        // Timeout: claim only as the winner. An abandonment timeout against
        // the invoker would be a loss the module would equally confirm.
        "timeout" => wins(predicted, me).then_some(Conclude::WinOnTime),
        // Agreement: the opponent's standing draw offer — concluding IS the
        // acceptance. Gated by temperament and the standing assessment.
        "agreement" => (view.last_ply_offers_draw && wants_draw).then_some(Conclude::AcceptDraw),
        // Residual resignation (against the invoker): only when the persona
        // has decided to resign (sustained hopeless assessments).
        "resignation" => wants_resign.then_some(Conclude::Resign),
        _ => None,
    }
}

/// The tags and content of the Conclusion stating `verdict` for the session
/// `terms` (kind `3425` §Tags): the session reference, both players, the
/// seats mirrored, the result on each player, the status as content. The
/// `nonce` is the publish path's.
#[must_use]
pub fn conclusion_tags(
    terms: &SessionTerms,
    verdict: &Verdict,
    relay_hint: &str,
) -> (Vec<Tag>, String) {
    let tags = vec![
        Tag::custom(
            "e",
            [
                terms.id.to_hex(),
                relay_hint.to_owned(),
                "game_session".to_owned(),
            ],
        ),
        Tag::custom(
            "p",
            [
                terms.first.to_hex(),
                relay_hint.to_owned(),
                "player".to_owned(),
            ],
        ),
        Tag::custom(
            "p",
            [
                terms.second.to_hex(),
                relay_hint.to_owned(),
                "player".to_owned(),
            ],
        ),
        Tag::custom("seat", [terms.first.to_hex(), "first".to_owned()]),
        Tag::custom("seat", [terms.second.to_hex(), "second".to_owned()]),
        Tag::custom(
            "result",
            [terms.first.to_hex(), verdict.result.first.to_string()],
        ),
        Tag::custom(
            "result",
            [terms.second.to_hex(), verdict.result.second.to_string()],
        ),
    ];
    (tags, verdict.status.clone())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;
    use crate::module::SeatResult;
    use crate::session::fixtures::World;
    use sashite_sanki_player::Occurrences;

    fn view(terminal: bool, draw_offered: bool) -> SessionView {
        SessionView {
            chain_len: 4,
            next_half_move: 5,
            on_move: Seat::First,
            step: 3,
            terminal,
            tip: String::new(),
            occurrences: Occurrences::new(),
            halfmove_clock: 0,
            anchor: 0,
            affordable: 0,
            last_ply_offers_draw: draw_offered,
        }
    }

    fn verdict(status: &str, first: u32, second: u32) -> Verdict {
        Verdict {
            result: SeatResult { first, second },
            status: status.to_owned(),
        }
    }

    #[test]
    fn concludes_only_wanted_verdicts() {
        // A terminal chain: stated whatever the outcome.
        assert_eq!(
            decision(
                &verdict("checkmate", 0, 100),
                Seat::First,
                &view(true, false),
                false,
                false
            ),
            Some(Conclude::Terminal)
        );
        // Timeout: as the winner only.
        assert_eq!(
            decision(
                &verdict("timeout", 0, 100),
                Seat::Second,
                &view(false, false),
                false,
                false
            ),
            Some(Conclude::WinOnTime)
        );
        assert_eq!(
            decision(
                &verdict("timeout", 0, 100),
                Seat::First,
                &view(false, false),
                false,
                false
            ),
            None
        );
        // Agreement: the offer must stand and the persona must want it.
        assert_eq!(
            decision(
                &verdict("agreement", 50, 50),
                Seat::First,
                &view(false, true),
                true,
                false
            ),
            Some(Conclude::AcceptDraw)
        );
        assert_eq!(
            decision(
                &verdict("agreement", 50, 50),
                Seat::First,
                &view(false, true),
                false,
                false
            ),
            None
        );
        // Resignation: only when the persona resigns.
        assert_eq!(
            decision(
                &verdict("resignation", 0, 100),
                Seat::First,
                &view(false, false),
                false,
                true
            ),
            Some(Conclude::Resign)
        );
        assert_eq!(
            decision(
                &verdict("resignation", 0, 100),
                Seat::First,
                &view(false, false),
                true,
                false
            ),
            None
        );
    }

    #[test]
    fn the_conclusion_states_the_verdict_on_both_players() {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_700_000_100);
        let terms = crate::session::terms(&session, &pairing).unwrap();
        let (tags, content) = conclusion_tags(&terms, &verdict("checkmate", 100, 0), "");
        let event = EventBuilder::new(Kind::Custom(crate::session::KIND_CONCLUSION), content)
            .tags(tags)
            .tag(Tag::parse(["nonce", "0", "0"]).unwrap())
            .finalize(&w.second)
            .unwrap();
        let mapped = crate::session::conclusion(&event, &terms).unwrap();
        assert_eq!(mapped["status"], "checkmate");
        assert_eq!(mapped["result"]["first"], 100);
        assert_eq!(mapped["result"]["second"], 0);
    }
}
