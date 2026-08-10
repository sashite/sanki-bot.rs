//! The bot's live view of a session — computed with the ARBITER's own crate,
//! never a reimplementation (ADR-0014 §Chain and clock logic): what the
//! arbiter would rule right now is what the bot believes.
//!
//! In self-timed mode a Request's canonical timing is its own `created_at`,
//! so a **synthetic probe request timed "now"** turns `natural_state` into a
//! live-chain oracle: the selected chain, the reached position, both clocks,
//! and whose turn it is. The same probe through `verdict::adjudicate` is the
//! §6.6 prediction: the verdict an invocation published now would yield.

use anyhow::{anyhow, Result};
use sashite_sanki_arbiter::event::{self, AdjudicationRequest, Attestation, Ply};
use sashite_sanki_arbiter::natural_state::{natural_state, Conclusion};
use sashite_sanki_arbiter::session::SessionParams;
use sashite_sanki_arbiter::verdict::{adjudicate, Adjudication};
use sashite_sanki_engine::domain::half_move::Move;
use sashite_sanki_engine::domain::side::Side;
use sashite_sanki_engine::domain::time::Timestamp;
use sashite_sanki_engine::engine;
use sashite_sanki_player::Occurrences;

use crate::clockmath::max_affordable;

/// The probe request id — never published; only its timing matters.
fn probe_request(
    params: &SessionParams,
    me: event::PublicKey,
    now: u64,
) -> Result<AdjudicationRequest> {
    let id = event::EventId::parse(&"f".repeat(64)).ok_or_else(|| anyhow!("probe id"))?;
    let at = i64::try_from(now).map_err(|_| anyhow!("now out of range"))?;
    Ok(AdjudicationRequest::new(
        id,
        me,
        params.session(),
        params.arbiter(),
        Timestamp::from_unix(at),
    ))
}

/// What the bot knows about a session at an instant.
#[derive(Debug)]
pub struct SessionView {
    /// The canonical chain's length (applied half-moves).
    pub chain_len: usize,
    /// The next play-order position (chain length + 1).
    pub next_half_move: u32,
    /// The player on move at that position.
    pub on_move: event::PublicKey,
    /// The mover's own step ordinal there.
    pub step: u32,
    /// The chain already reached a terminal verdict (awaiting the 3425).
    pub terminal: bool,
    /// The tip position, for `sashite-sanki-player`.
    pub tip: sashite_sanki_engine::position::Position,
    /// FEEN occurrence counts along initial + chain (the kernel's own
    /// bookkeeping, mirrored — ADR-0015 §2 caller obligation).
    pub occurrences: Occurrences,
    /// The tip's half-move clock.
    pub halfmove_clock: u32,
    /// The last canonical timing (t₀ or the last selected ply's) — the
    /// anchor the NEXT ply's elapsed runs from.
    pub anchor: u64,
    /// Seconds the mover may take past `anchor` before flagging.
    pub affordable: u64,
    /// Whether the LAST chain ply carries the `draw` offer flag.
    pub last_ply_offers_draw: bool,
}

/// Compute the live view. Returns `Err` only on malformed inputs (a probe id
/// or timestamp overflow — practically unreachable).
pub fn session_view(
    params: &SessionParams,
    plies: &[Ply],
    attestations: &[Attestation],
    me: event::PublicKey,
    now: u64,
) -> Result<SessionView> {
    let probe = probe_request(params, me, now)?;
    let state = natural_state(params, plies, attestations, &probe)
        .ok_or_else(|| anyhow!("self-timed probe must always have a cutoff"))?;

    // Mirror the kernel's occurrence bookkeeping by replaying the selected
    // chain over the engine (the kernel's map is not public — ADR-0015 §2).
    let mut occurrences = Occurrences::new();
    let mut position = params.initial_position().clone();
    occurrences.insert(position.to_feen(), 1);
    for canonical in &state.chain {
        let mv = Move::parse(&canonical.ply.content)
            .map_err(|e| anyhow!("canonical ply must parse: {e:?}"))?;
        position = engine::apply(&position, &mv)
            .map_err(|e| anyhow!("canonical ply must apply: {e:?}"))?;
        let count = occurrences.entry(position.to_feen()).or_insert(0);
        *count = count.saturating_add(1);
    }

    let next_half_move = state.next_half_move();
    let on_move = params.player_at(next_half_move);
    let step = params.step_at(next_half_move);
    let last_ply_offers_draw = state
        .chain
        .last()
        .is_some_and(|canonical| canonical.ply.draw);

    let (terminal, halfmove_clock, anchor, affordable) = match &state.conclusion {
        Conclusion::Terminal(_, at) => (true, 0, unix(*at), 0),
        Conclusion::Ongoing(session_state) => {
            let side = side_at(params, next_half_move);
            let clock = session_state.clocks().get(side);
            (
                false,
                session_state.halfmove_clock(),
                unix(session_state.last_attestation()),
                max_affordable(session_state.time_control(), clock),
            )
        }
    };

    Ok(SessionView {
        chain_len: state.chain.len(),
        next_half_move,
        on_move,
        step,
        terminal,
        tip: position,
        occurrences,
        halfmove_clock,
        anchor,
        affordable,
        last_ply_offers_draw,
    })
}

/// The verdict an Adjudication Request signed by `me` and timed `now` would
/// yield — the §6.6 prediction, via the arbiter crate itself. `None` when
/// the probe would be non-conforming (not this session's player, …).
#[must_use]
pub fn predicted_verdict(
    params: &SessionParams,
    plies: &[Ply],
    attestations: &[Attestation],
    me: event::PublicKey,
    now: u64,
) -> Option<Adjudication> {
    let probe = probe_request(params, me, now).ok()?;
    adjudicate(params, plies, attestations, &probe)
}

/// The side on move at a 1-based play-order position.
#[must_use]
pub fn side_at(params: &SessionParams, half_move: u32) -> Side {
    params.side_at(half_move)
}

fn unix(at: Timestamp) -> u64 {
    u64::try_from(at.as_unix()).unwrap_or(0)
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
    use sashite_sanki_engine::domain::status::Status;
    use sashite_sanki_engine::domain::time::Duration;
    use sashite_sanki_engine::domain::time_control::{Period, TimeControl};
    use sashite_sanki_engine::position::Position;

    const START: &str = "4k^3/8/8/8/8/8/8/R3K^3 / W/w";

    fn key(byte: u8) -> event::PublicKey {
        event::PublicKey::parse(&format!("{:02x}", byte).repeat(32)).unwrap()
    }

    fn id(byte: u8) -> event::EventId {
        event::EventId::parse(&format!("{:02x}", byte).repeat(32)).unwrap()
    }

    fn params() -> SessionParams {
        let period = Period::new(Duration::from_secs(300), Some(Duration::from_secs(3)), None)
            .expect("period");
        SessionParams::new(
            id(0xAA),
            key(2),
            None, // self-timed
            key(0x10),
            key(0x20),
            TimeControl::from_periods(vec![period]).unwrap(),
            Position::parse(START).unwrap(),
            Timestamp::from_unix(1_000),
        )
    }

    fn ply(
        byte: u8,
        signer: event::PublicKey,
        step: u32,
        content: &str,
        at: i64,
        draw: bool,
    ) -> Ply {
        Ply::new(
            id(byte),
            signer,
            id(0xAA),
            step,
            draw,
            content.to_owned(),
            Timestamp::from_unix(at),
        )
    }

    #[test]
    fn empty_session_is_the_first_players_turn_from_t0() {
        let params = params();
        let view = session_view(&params, &[], &[], key(0x10), 2_000).unwrap();
        assert_eq!(view.chain_len, 0);
        assert_eq!(view.next_half_move, 1);
        assert_eq!(view.on_move, key(0x10));
        assert_eq!(view.step, 1);
        assert!(!view.terminal);
        assert_eq!(view.anchor, 1_000); // t₀
        assert_eq!(view.affordable, 300);
        assert_eq!(view.occurrences.len(), 1);
    }

    #[test]
    fn chain_advances_turn_anchor_and_occurrences() {
        let params = params();
        let plies = vec![
            ply(1, key(0x10), 1, r#"["a1","a4",null]"#, 1_010, false),
            ply(2, key(0x20), 1, r#"["e8","e7",null]"#, 1_025, true),
        ];
        let view = session_view(&params, &plies, &[], key(0x10), 2_000).unwrap();
        assert_eq!(view.chain_len, 2);
        assert_eq!(view.next_half_move, 3);
        assert_eq!(view.on_move, key(0x10));
        assert_eq!(view.step, 2);
        assert_eq!(view.anchor, 1_025);
        assert!(view.last_ply_offers_draw);
        assert_eq!(view.occurrences.values().sum::<u32>(), 3);
        // Both movers spent time and earned the increment: first 300−10+3,
        // still to be spent by the mover at half-move 3.
        assert_eq!(view.affordable, 293);
    }

    #[test]
    fn prediction_matches_the_session_state() {
        let params = params();
        // No plies, probe far past the budget: the on-move first player has
        // flagged by abandonment — the arbiter would rule timeout against
        // them; the second player predicts a win on time.
        let verdict = predicted_verdict(&params, &[], &[], key(0x20), 40_000).unwrap();
        assert_eq!(verdict.status(), Status::Timeout);
        // The same probe just after t₀ resolves as the residual resignation
        // AGAINST THE INVOKER (calling for no reason) — which is exactly why
        // the bot never publishes without wanting the predicted verdict.
        let early = predicted_verdict(&params, &[], &[], key(0x20), 1_010).unwrap();
        assert_eq!(early.status(), Status::Resignation);
    }
}
