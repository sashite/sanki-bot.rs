// SPDX-License-Identifier: Apache-2.0
//! A turn (ADR-0045 §3 *A turn*): one `search`, its events read until the
//! `done` or the hard stop, every Move the engine names **judged by the
//! caller** — `parse_canonical`, then membership in the module's
//! `legal_moves` — so that the published string is always the module's own
//! content, and the engine's text only selects it (§8: validation is the
//! runtime's, not the engine's).
//!
//! - every `info` whose first variation begins with a Move the judge accepts
//!   is kept as the **provisional answer** (SEI §8.4, the safety net);
//! - the `done`'s `best` goes through the same judge; it is the answer. A
//!   `best` the judge refuses is an engine failure, and the provisional
//!   answer or the fallback plays;
//! - an `error` attached to the search is a refusal: the caller's fallback
//!   plays, and the code says whether the engine and the bot disagree on the
//!   rules (`invalid`, `illegal`, `unsupported`: the engine is kept, every
//!   later turn is the fallback) or the engine failed (`internal`: the
//!   relaunch policy);
//! - at the hard stop with no `done`, `cancel` is sent and the bounded-stop
//!   grace waited: a `done` within it **is** the answer and the engine is in
//!   order; nothing within it is an engine failure;
//! - `ping` every second; a `ping` unanswered for a second, or any protocol
//!   violation, is an engine failure.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tokio::sync::Notify;

use super::announce::RULES;
use super::process::Engine;
use super::wire::{Code, EngineError, Kind};
use super::EngineFailure;

/// The bounded-stop grace after `cancel` (SEI §8.4 *Bounded stop*).
pub const STOP_GRACE: Duration = Duration::from_millis(50);

/// The interval between two `ping`s during a search.
pub const PING_EVERY: Duration = Duration::from_secs(1);

/// How long a `ping` may go unanswered (SEI §8.2 *Monitoring*).
pub const PING_GRACE: Duration = Duration::from_secs(1);

/// A `search` request, minus what the host fixes (`rules`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchRequest {
    /// The initial position (FEEN).
    pub position: String,
    /// The history, in canonical PMN.
    pub moves: Vec<String>,
    /// The clock, as [`super::clock::search_clock`] builds it.
    pub clock: Option<Value>,
    /// The limits, when the search is capped instead of clocked.
    pub limits: Option<Value>,
    /// The roots, when the engine announces the feature: every legal move.
    pub roots: Option<Vec<String>>,
    /// The strength, when the engine announces the feature and the
    /// configuration sets it.
    pub strength: Option<i64>,
    /// Answer as a fresh process would (the first search of a game).
    pub fresh: bool,
}

impl SearchRequest {
    /// The request's fields.
    #[must_use]
    pub fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("rules".to_owned(), json!(RULES));
        fields.insert("position".to_owned(), json!(self.position));
        if !self.moves.is_empty() {
            fields.insert("moves".to_owned(), json!(self.moves));
        }
        if let Some(clock) = &self.clock {
            fields.insert("clock".to_owned(), clock.clone());
        }
        if let Some(limits) = &self.limits {
            fields.insert("limits".to_owned(), limits.clone());
        }
        if let Some(roots) = &self.roots {
            fields.insert("roots".to_owned(), json!(roots));
        }
        if let Some(elo) = self.strength {
            fields.insert("strength".to_owned(), json!({ "elo": elo }));
        }
        if self.fresh {
            fields.insert("fresh".to_owned(), json!(true));
        }
        fields
    }
}

/// A score (SEI §9.1), the parts a bot decides on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Score {
    /// The engine's own scale.
    pub cp: Option<i64>,
    /// Mate in half-moves, signed.
    pub mate: Option<i64>,
    /// Win, draw, loss, in per mille.
    pub wdl: Option<[u16; 3]>,
}

impl Score {
    /// Reads a `score` object.
    #[must_use]
    pub fn read(score: &Value) -> Self {
        let wdl = score.get("wdl").and_then(Value::as_array).and_then(|list| {
            let values: Vec<u16> = list
                .iter()
                .map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                .collect::<Option<_>>()?;
            let [w, d, l] = values.as_slice() else {
                return None;
            };
            (u32::from(*w)
                .saturating_add(u32::from(*d))
                .saturating_add(u32::from(*l))
                == 1000)
                .then_some([*w, *d, *l])
        });
        Self {
            cp: score.get("cp").and_then(Value::as_i64),
            mate: score
                .get("mate")
                .and_then(Value::as_i64)
                .filter(|m| *m != 0),
            wdl,
        }
    }
}

/// The engine's advice (SEI §10 `advice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advice {
    /// The engine judges its position lost.
    Resign,
    /// The engine judges a draw the best outcome.
    Draw,
}

/// A validated answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The engine's Move, canonical PMN.
    pub pmn: String,
    /// The module's content the judge matched it to — what is published.
    pub content: String,
    /// From an `info` (the safety net), not a `done`.
    pub provisional: bool,
    /// The first variation's score, when given.
    pub score: Option<Score>,
    /// The engine's advice, when given.
    pub advice: Option<Advice>,
}

/// The engine's state after the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The `done` came, with a `best` the judge accepted.
    InOrder,
    /// The engine refused the search with an attached error.
    Refused(EngineError),
    /// The engine holds the position terminal (`best: null`) where the
    /// module does not: the two disagree on the rules; the engine is kept.
    NoMove,
    /// The engine failed; its process is ended.
    Failed(EngineFailure),
}

impl Verdict {
    /// Whether the engine and the bot disagree on the rules document
    /// (ADR-0045 §3: the engine is not relaunched but kept, and every later
    /// turn of the game is the fallback).
    #[must_use]
    pub const fn disagrees_on_rules(&self) -> bool {
        matches!(
            self,
            Self::NoMove
                | Self::Refused(EngineError {
                    code: Code::Invalid | Code::Illegal | Code::Unsupported,
                    ..
                })
        )
    }
}

/// What a turn produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The answer to play, when the engine gave one: the `done`'s, or the
    /// provisional one when the `done` never came or was refused.
    pub answer: Option<Answer>,
    /// The engine's state.
    pub verdict: Verdict,
}

/// Reads the first Move of the first variation and its score.
fn first_variation(fields: &Map<String, Value>) -> (Option<&str>, Option<Score>) {
    let Some(first) = fields
        .get("variations")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
    else {
        return (None, None);
    };
    let pmn = first
        .get("pv")
        .and_then(Value::as_array)
        .and_then(|pv| pv.first())
        .and_then(Value::as_str);
    let score = first.get("score").map(Score::read);
    (pmn, score)
}

/// Runs one search and reads it to its end.
///
/// `judge` maps a PMN the engine names to the module's content when it is
/// the canonical PMN of a legal move of the turn, and to `None` otherwise.
/// `hard_stop` is `H` (ADR-0045 §6): past it the caller plays what it has.
/// `grace` is the bounded-stop grace after `cancel`. `interrupt`, when
/// notified, ends the search the way the hard stop does (a withdrawn turn).
pub async fn search(
    engine: &mut Engine,
    request: &SearchRequest,
    mut judge: impl FnMut(&str) -> Option<String>,
    hard_stop: Instant,
    grace: Duration,
    interrupt: &Notify,
) -> Turn {
    let mut provisional: Option<Answer> = None;
    let id = match engine.send("search", request.fields()).await {
        Ok(id) => id,
        Err(failure) => {
            return Turn {
                answer: None,
                verdict: Verdict::Failed(failure),
            }
        }
    };

    // Liveness: a ping every second, each answered within a second. The
    // pending pings are judged only once the engine's output has been read
    // dry (a `recv` that timed out), on the instant the last event was read
    // from the pipe — never on the host's own delay in consuming it.
    let mut pings: BTreeSet<u64> = BTreeSet::new();
    let mut next_ping = Instant::now().checked_add(PING_EVERY).unwrap_or(hard_stop);
    let mut ping_due: Option<Instant> = None;

    loop {
        let now = Instant::now();
        if now >= hard_stop {
            break;
        }
        if now >= next_ping {
            match engine.send("ping", Map::new()).await {
                Ok(ping_id) => {
                    pings.insert(ping_id);
                    if ping_due.is_none() {
                        ping_due = now.checked_add(PING_GRACE);
                    }
                }
                Err(failure) => {
                    return Turn {
                        answer: provisional,
                        verdict: Verdict::Failed(failure),
                    }
                }
            }
            next_ping = now.checked_add(PING_EVERY).unwrap_or(hard_stop);
        }
        let until = hard_stop.min(next_ping).min(ping_due.unwrap_or(hard_stop));
        // An interruption by the caller (a withdrawn turn) ends the search
        // as the hard stop does: `cancel`, then the grace.
        let received = tokio::select! {
            received = engine.recv(until) => received,
            () = interrupt.notified() => break,
        };
        let event = match received {
            Ok(Some(event)) => event,
            Ok(None) => {
                if ping_due.is_some_and(|due| Instant::now() >= due) {
                    return Turn {
                        answer: provisional,
                        verdict: Verdict::Failed(engine.fail(EngineFailure::Unresponsive)),
                    };
                }
                continue;
            }
            Err(failure) => {
                return Turn {
                    answer: provisional,
                    verdict: Verdict::Failed(failure),
                }
            }
        };
        match (event.re, event.kind) {
            (Some(re), Kind::Done(_)) if pings.remove(&re) => {
                ping_due = if pings.is_empty() {
                    None
                } else {
                    engine.last_receipt().checked_add(PING_GRACE)
                };
            }
            (Some(re), Kind::Error(_) | Kind::Info(_)) if pings.contains(&re) => {
                // A ping's form is frozen and its answer is a done (SEI
                // §8.2): anything else is a violation.
                return Turn {
                    answer: provisional,
                    verdict: Verdict::Failed(engine.fail(EngineFailure::Violation(
                        "an event other than done attached to a ping".to_owned(),
                    ))),
                };
            }
            (Some(re), Kind::Info(fields)) if re == id => {
                let (pmn, score) = first_variation(&fields);
                if let Some(pmn) = pmn {
                    match judge(pmn) {
                        Some(content) => {
                            provisional = Some(Answer {
                                pmn: pmn.to_owned(),
                                content,
                                provisional: true,
                                score,
                                advice: None,
                            });
                        }
                        None => {
                            tracing::debug!(pmn = %super::wire::escape(pmn), "an info the judge refused")
                        }
                    }
                }
            }
            (Some(re), Kind::Done(fields)) if re == id => {
                return finish(engine, &fields, &mut judge, provisional);
            }
            (Some(re), Kind::Error(error)) if re == id => {
                return Turn {
                    answer: provisional,
                    verdict: Verdict::Refused(error),
                };
            }
            // An event of a request this turn did not send: the process
            // layer admitted it as attached to an open request; nothing to
            // read here.
            _ => {}
        }
    }

    // The hard stop: cancel, then the bounded-stop grace.
    if let Err(failure) = engine.send("cancel", Map::new()).await {
        return Turn {
            answer: provisional,
            verdict: Verdict::Failed(failure),
        };
    }
    let until = Instant::now()
        .checked_add(grace)
        .unwrap_or_else(Instant::now);
    loop {
        match engine.recv(until).await {
            Ok(Some(event)) => match (event.re, event.kind) {
                (Some(re), Kind::Done(fields)) if re == id => {
                    return finish(engine, &fields, &mut judge, provisional);
                }
                (Some(re), Kind::Error(error)) if re == id => {
                    return Turn {
                        answer: provisional,
                        verdict: Verdict::Refused(error),
                    };
                }
                _ => {}
            },
            Ok(None) => {
                return Turn {
                    answer: provisional,
                    verdict: Verdict::Failed(engine.fail(EngineFailure::Overrun)),
                }
            }
            Err(failure) => {
                return Turn {
                    answer: provisional,
                    verdict: Verdict::Failed(failure),
                }
            }
        }
    }
}

/// Reads a search's `done`.
fn finish(
    engine: &mut Engine,
    fields: &Map<String, Value>,
    judge: &mut impl FnMut(&str) -> Option<String>,
    provisional: Option<Answer>,
) -> Turn {
    let best = fields.get("best");
    let (_, score) = first_variation(fields);
    let advice = match fields.get("advice").and_then(Value::as_str) {
        Some("resign") => Some(Advice::Resign),
        Some("draw") => Some(Advice::Draw),
        _ => None,
    };
    match best {
        Some(Value::Null) => Turn {
            answer: provisional,
            verdict: Verdict::NoMove,
        },
        Some(Value::String(pmn)) => match judge(pmn) {
            Some(content) => Turn {
                answer: Some(Answer {
                    pmn: pmn.clone(),
                    content,
                    provisional: false,
                    score,
                    advice,
                }),
                verdict: Verdict::InOrder,
            },
            None => Turn {
                answer: provisional,
                verdict: Verdict::Failed(engine.fail(EngineFailure::Violation(format!(
                    "best {} is not the canonical PMN of a legal move",
                    super::wire::escape(pmn)
                )))),
            },
        },
        _ => Turn {
            answer: provisional,
            verdict: Verdict::Failed(engine.fail(EngineFailure::Violation(
                "a search's done without best".to_owned(),
            ))),
        },
    }
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

    #[test]
    fn the_request_writes_only_what_is_set() {
        let request = SearchRequest {
            position: "p".to_owned(),
            ..SearchRequest::default()
        };
        let fields = request.fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields["rules"], RULES);
        let full = SearchRequest {
            position: "p".to_owned(),
            moves: vec!["e2-e4".to_owned()],
            clock: Some(json!({"own": {"deadline": 1}})),
            limits: None,
            roots: Some(vec!["e7-e5".to_owned()]),
            strength: Some(1500),
            fresh: true,
        };
        let fields = full.fields();
        assert_eq!(fields["strength"], json!({"elo": 1500}));
        assert_eq!(fields["fresh"], true);
        assert_eq!(fields["roots"], json!(["e7-e5"]));
    }

    #[test]
    fn scores_are_read_with_their_domains() {
        let s = Score::read(&json!({"cp": 12, "wdl": [500, 300, 200]}));
        assert_eq!(s.cp, Some(12));
        assert_eq!(s.wdl, Some([500, 300, 200]));
        assert_eq!(s.mate, None);
        let bad = Score::read(&json!({"mate": 0, "wdl": [500, 300, 300]}));
        assert_eq!(bad.mate, None);
        assert_eq!(bad.wdl, None);
        assert_eq!(Score::read(&json!({"mate": -3})).mate, Some(-3));
    }

    #[test]
    fn rules_disagreements() {
        let refused = |code| {
            Verdict::Refused(EngineError {
                code,
                path: None,
                message: None,
            })
        };
        assert!(refused(Code::Illegal).disagrees_on_rules());
        assert!(refused(Code::Invalid).disagrees_on_rules());
        assert!(refused(Code::Unsupported).disagrees_on_rules());
        assert!(!refused(Code::Internal).disagrees_on_rules());
        assert!(Verdict::NoMove.disagrees_on_rules());
        assert!(!Verdict::InOrder.disagrees_on_rules());
    }
}
