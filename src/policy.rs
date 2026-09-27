// SPDX-License-Identifier: Apache-2.0
//! Draw offers, draw acceptance and resignation (ADR-0045 §7): each exists
//! only with its section in `[play]`, and each is decided on the **current
//! turn's** evaluation — the first variation's `wdl` and `mate`, and the
//! engine's `advice` when announced — never on a previous turn's.
//!
//! A turn **qualifies** for a policy when its condition holds; a policy
//! fires when `streak` consecutive own turns qualify, the current one
//! included. A turn without a `done` (a fallback), a withdrawn turn and a
//! restart reset every streak.
//!
//! - **Offer.** The Ply carries the `draw` flag when the ply number is at
//!   least `after_ply` and the offer policy fires; a turn qualifies when
//!   `wdl.draw ≥ min_draw` or the advice is `draw`.
//! - **Resign.** A turn qualifies when `wdl.win ≤ max_win` and `wdl.loss ≥
//!   min_loss`, or `mate < 0`, or the advice is `resign`; when the policy
//!   fires, the bot resigns instead of moving. It never resigns while an
//!   opponent's offer stands: with `[play.accept_draw]` it accepts instead;
//!   without, it moves.
//! - **Accept.** When the opponent's last Ply carries the `draw` flag, the
//!   bot accepts instead of moving when `wdl.draw ≥ min_draw` or the advice
//!   is `draw` — on this search.

use crate::config::Play;
use crate::sei::{Advice, Score};

/// A turn's evaluation, from the engine's `done`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// The first variation's score.
    pub score: Option<Score>,
    /// The engine's advice, when the feature is announced.
    pub advice: Option<Advice>,
}

/// The streaks of qualifying turns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Streaks {
    /// Consecutive own turns qualifying for resignation.
    pub resign: u8,
    /// Consecutive own turns qualifying for an offer.
    pub offer: u8,
}

impl Streaks {
    /// Every streak back to zero: a fallback turn, a withdrawn turn, a
    /// restart.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What the bot does this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Play the move.
    Move,
    /// Play the move, with the `draw` flag.
    MoveOfferingDraw,
    /// Accept the opponent's standing offer instead of moving.
    AcceptDraw,
    /// Resign instead of moving.
    Resign,
}

/// The decision of a turn, updating `streaks`. `evaluation` is `None` on a
/// fallback turn. `ply_number` is the number of the Ply the bot is about
/// to play (1-based). `offer_stands` is whether the opponent's last Ply
/// carries the `draw` flag.
#[must_use]
pub fn decide(
    play: &Play,
    evaluation: Option<&Evaluation>,
    ply_number: u32,
    offer_stands: bool,
    streaks: &mut Streaks,
) -> Decision {
    let Some(evaluation) = evaluation else {
        streaks.reset();
        return Decision::Move;
    };
    let wdl = evaluation.score.and_then(|s| s.wdl);
    let mate = evaluation.score.and_then(|s| s.mate);
    let draw_judged = |min_draw: u16| {
        wdl.is_some_and(|[_, d, _]| d >= min_draw) || evaluation.advice == Some(Advice::Draw)
    };

    // Accept: on this search, never on the previous turn's evaluation.
    if offer_stands {
        if let Some(accept) = play.accept_draw {
            if draw_judged(accept.min_draw) {
                streaks.reset();
                return Decision::AcceptDraw;
            }
        }
    }

    // Resign.
    if let Some(resign) = play.resign {
        let qualifies = wdl.is_some_and(|[w, _, l]| w <= resign.max_win && l >= resign.min_loss)
            || mate.is_some_and(|m| m < 0)
            || evaluation.advice == Some(Advice::Resign);
        streaks.resign = if qualifies {
            streaks.resign.saturating_add(1)
        } else {
            0
        };
        if streaks.resign >= resign.streak {
            if offer_stands {
                // Concluding now would accept the offer. With the section,
                // accept; without, move — a loss the operator chose.
                if play.accept_draw.is_some() {
                    streaks.reset();
                    return Decision::AcceptDraw;
                }
            } else {
                return Decision::Resign;
            }
        }
    }

    // Offer.
    if let Some(offer) = play.offer_draw {
        let qualifies = draw_judged(offer.min_draw);
        streaks.offer = if qualifies {
            streaks.offer.saturating_add(1)
        } else {
            0
        };
        if ply_number >= offer.after_ply && streaks.offer >= offer.streak {
            return Decision::MoveOfferingDraw;
        }
    }
    Decision::Move
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
    use crate::config::{AcceptDraw, OfferDraw, Resign, Variant};
    use std::collections::BTreeMap;

    fn play(resign: Option<Resign>, offer: Option<OfferDraw>, accept: Option<AcceptDraw>) -> Play {
        Play {
            variants: crate::config::NonEmpty::test(vec![Variant::Chess]),
            preferred: Variant::Chess,
            opponents: crate::config::NonEmpty::test(vec![Variant::Chess]),
            min_move_secs: 5,
            margin_ms: 300,
            max_concurrent: BTreeMap::new(),
            resign,
            offer_draw: offer,
            accept_draw: accept,
        }
    }

    fn eval(wdl: [u16; 3], mate: Option<i64>, advice: Option<Advice>) -> Evaluation {
        Evaluation {
            score: Some(Score {
                cp: None,
                mate,
                wdl: Some(wdl),
            }),
            advice,
        }
    }

    const RESIGN: Resign = Resign {
        max_win: 20,
        min_loss: 900,
        streak: 2,
    };
    const OFFER: OfferDraw = OfferDraw {
        min_draw: 800,
        streak: 2,
        after_ply: 40,
    };
    const ACCEPT: AcceptDraw = AcceptDraw { min_draw: 600 };

    #[test]
    fn without_a_section_nothing_fires() {
        let play = play(None, None, None);
        let mut streaks = Streaks::default();
        let lost = eval([0, 0, 1000], Some(-3), Some(Advice::Resign));
        for _ in 0..5 {
            assert_eq!(
                decide(&play, Some(&lost), 50, true, &mut streaks),
                Decision::Move
            );
        }
    }

    #[test]
    fn resignation_fires_on_a_streak_and_resets_on_a_fallback() {
        let play = play(Some(RESIGN), None, None);
        let mut streaks = Streaks::default();
        let lost = eval([10, 50, 940], None, None);
        assert_eq!(
            decide(&play, Some(&lost), 10, false, &mut streaks),
            Decision::Move
        );
        assert_eq!(streaks.resign, 1);
        // A fallback turn resets.
        assert_eq!(decide(&play, None, 11, false, &mut streaks), Decision::Move);
        assert_eq!(streaks.resign, 0);
        assert_eq!(
            decide(&play, Some(&lost), 12, false, &mut streaks),
            Decision::Move
        );
        assert_eq!(
            decide(&play, Some(&lost), 13, false, &mut streaks),
            Decision::Resign
        );
        // A better turn breaks the streak.
        let fine = eval([400, 300, 300], None, None);
        assert_eq!(
            decide(&play, Some(&fine), 14, false, &mut streaks),
            Decision::Move
        );
        assert_eq!(streaks.resign, 0);
        // Mate against, or the advice, qualify too.
        let mated = eval([500, 0, 500], Some(-5), None);
        let advised = eval([500, 0, 500], None, Some(Advice::Resign));
        assert_eq!(
            decide(&play, Some(&mated), 15, false, &mut streaks),
            Decision::Move
        );
        assert_eq!(
            decide(&play, Some(&advised), 16, false, &mut streaks),
            Decision::Resign
        );
    }

    #[test]
    fn never_resigns_while_an_offer_stands() {
        let lost = eval([0, 0, 1000], Some(-1), None);
        // Without accept_draw: moves, a loss the operator chose.
        let play_no_accept = play(Some(RESIGN), None, None);
        let mut streaks = Streaks::default();
        assert_eq!(
            decide(&play_no_accept, Some(&lost), 10, true, &mut streaks),
            Decision::Move
        );
        assert_eq!(
            decide(&play_no_accept, Some(&lost), 11, true, &mut streaks),
            Decision::Move
        );
        assert_eq!(streaks.resign, 2);
        // With accept_draw: accepts instead, even when the draw is not judged
        // likely (the alternative is a loss).
        let play_accept = play(Some(RESIGN), None, Some(ACCEPT));
        let mut streaks = Streaks::default();
        assert_eq!(
            decide(&play_accept, Some(&lost), 10, true, &mut streaks),
            Decision::Move
        );
        assert_eq!(
            decide(&play_accept, Some(&lost), 11, true, &mut streaks),
            Decision::AcceptDraw
        );
        // The offer withdrawn (the opponent moved): resigns.
        let mut streaks = Streaks {
            resign: 1,
            offer: 0,
        };
        assert_eq!(
            decide(&play_accept, Some(&lost), 12, false, &mut streaks),
            Decision::Resign
        );
    }

    #[test]
    fn accepting_is_judged_on_this_search() {
        let play = play(None, None, Some(ACCEPT));
        let mut streaks = Streaks::default();
        let drawish = eval([200, 650, 150], None, None);
        assert_eq!(
            decide(&play, Some(&drawish), 10, true, &mut streaks),
            Decision::AcceptDraw
        );
        assert_eq!(
            decide(&play, Some(&drawish), 10, false, &mut streaks),
            Decision::Move
        );
        let winning = eval([800, 100, 100], None, None);
        assert_eq!(
            decide(&play, Some(&winning), 10, true, &mut streaks),
            Decision::Move
        );
        let advised = eval([800, 100, 100], None, Some(Advice::Draw));
        assert_eq!(
            decide(&play, Some(&advised), 10, true, &mut streaks),
            Decision::AcceptDraw
        );
    }

    #[test]
    fn offers_after_the_ply_and_on_a_streak() {
        let play = play(None, Some(OFFER), None);
        let mut streaks = Streaks::default();
        let drawish = eval([100, 850, 50], None, None);
        assert_eq!(
            decide(&play, Some(&drawish), 10, false, &mut streaks),
            Decision::Move
        );
        assert_eq!(
            decide(&play, Some(&drawish), 11, false, &mut streaks),
            Decision::Move,
            "before after_ply"
        );
        assert_eq!(streaks.offer, 2);
        assert_eq!(
            decide(&play, Some(&drawish), 40, false, &mut streaks),
            Decision::MoveOfferingDraw
        );
        let sharp = eval([500, 100, 400], None, None);
        assert_eq!(
            decide(&play, Some(&sharp), 41, false, &mut streaks),
            Decision::Move
        );
        assert_eq!(streaks.offer, 0);
    }

    #[test]
    fn an_engine_without_wdl_leaves_the_policies_inert() {
        let play = play(Some(RESIGN), Some(OFFER), Some(ACCEPT));
        let mut streaks = Streaks::default();
        let cp_only = Evaluation {
            score: Some(Score {
                cp: Some(-900),
                mate: None,
                wdl: None,
            }),
            advice: None,
        };
        for ply in 1..10 {
            assert_eq!(
                decide(&play, Some(&cp_only), 40 + ply, true, &mut streaks),
                Decision::Move
            );
        }
    }
}
