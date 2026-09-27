// SPDX-License-Identifier: Apache-2.0
//! The SEI host over real processes (ADR-0045 §9 *Engine probes*): the
//! random engine (`sanki-sei-random-engine`, named by the
//! `SANKI_SEI_RANDOM_ENGINE` variable; those tests are skipped without it),
//! and scripted engines — a few lines of Python — that never answer `hello`,
//! answer `ping` but never `done`, emit an illegal or a non-canonical
//! `best`, exit mid-search, flood their output, violate the envelope, refuse
//! the search, hang, or answer only within the grace after `cancel`. Each
//! must cost at most its answers, and never a publication: the judge is the
//! only door to a content.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sashite_sanki_bot::sei::{
    self, Code, Engine, EngineFailure, Host, Launch, Needs, ProbeError, SearchRequest, Verdict,
};
use sashite_sanki_client::notation;
use sashite_sanki_engine::prelude::{engine as rules, Position};
use serde_json::json;

const CHESS_CHESS: &str = "-rnbqk^bn-r/+p+p+p+p+p+p+p+p/8/8/8/8/+P+P+P+P+P+P+P+P/-RNBQK^BN-R / W/w";

/// The judge of a turn, as the bot builds it from the module's
/// `legal_moves`: here from the native rules.
fn judge(feen: &str) -> impl FnMut(&str) -> Option<String> {
    let position = Position::parse(feen).unwrap();
    let legal: BTreeSet<String> = rules::legal_moves(&position)
        .iter()
        .map(notation::content_of)
        .collect();
    move |pmn: &str| {
        let content = notation::to_content(&position, pmn).ok()?;
        legal.contains(&content).then_some(content)
    }
}

fn scripted(mode: &str) -> Launch {
    let dir = std::env::temp_dir().join(format!("sanki-bot-sei-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("engine.py");
    std::fs::write(&script, SCRIPT).unwrap();
    Launch {
        command: PathBuf::from("python3"),
        args: vec![script.to_string_lossy().into_owned(), mode.to_owned()],
        cwd: dir,
        env: Vec::new(),
    }
}

const SCRIPT: &str = r#"
import sys, json, time
mode = sys.argv[1]
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
HELLO = {"version": 1, "versions": [1], "engine": {"name": "scripted"},
         "rules": {"sashite.sanki.kernel/1": {}}, "features": {"roots": {}, "advice": {}},
         "options": {"threads": {"type": "int", "default": 1, "min": 1, "max": 8}}}
if mode == "mute":
    time.sleep(30); sys.exit(0)
pending = None
for line in sys.stdin:
    r = json.loads(line); op = r["op"]; i = r["id"]
    if op == "hello": out({"re": i, "ev": "done", **HELLO})
    elif op == "ping":
        if mode == "error-on-ping" and pending is not None:
            out({"re": i, "ev": "error", "code": "internal"}); pending = None
        else: out({"re": i, "ev": "done"})
    elif op == "configure":
        if mode == "refuse-configure": out({"re": i, "ev": "error", "code": "invalid", "path": "/options/threads"})
        else: out({"re": i, "ev": "done"})
    elif op == "cancel":
        if mode == "late-done" and pending is not None:
            out({"re": pending, "ev": "done", "best": "e2-e4"}); pending = None
        out({"re": i, "ev": "done"})
    elif op == "search":
        info = {"re": i, "ev": "info", "depth": 1, "variations": [{"pv": ["e2-e4"], "score": {"cp": 0}}]}
        if mode == "info-then-silence": out(info)
        elif mode == "late-done": out(info); pending = i
        elif mode == "illegal-best": out({"re": i, "ev": "done", "best": "e2-e5"})
        elif mode == "noncanonical-best": out(info); out({"re": i, "ev": "done", "best": "e2+e4"})
        elif mode == "exit": sys.exit(3)
        elif mode == "flood":
            for _ in range(3): sys.stdout.write("garbage\n")
            sys.stdout.flush()
        elif mode == "bad-re": out({"re": 999, "ev": "done"})
        elif mode == "long-line":
            sys.stdout.write('{"re": %d, "ev": "info", "x": "%s"}\n' % (i, "x" * (5 * 1024 * 1024))); sys.stdout.flush()
        elif mode == "info-after-done":
            out({"re": i, "ev": "done", "best": "e2-e4"}); out(info)
        elif mode == "error-on-ping":
            pending = i  # answered below, on the first ping
        elif mode == "vendor-events":
            out({"ev": "x_vendor_stats", "n": 1}); out({"re": i, "ev": "x_vendor_line", "n": 2})
            out({"re": i, "ev": "done", "best": "e2-e4"})
        elif mode == "refuse": out({"re": i, "ev": "error", "code": "illegal", "path": "/moves/0", "message": "x"})
        elif mode == "hang": time.sleep(30)
        elif mode == "fatal": out({"ev": "error", "code": "internal", "message": "boom"}); sys.exit(1)
        elif mode == "null-best": out({"re": i, "ev": "done", "best": None})
        elif mode == "advice":
            out({"re": i, "ev": "done", "best": "e2-e4", "advice": "resign",
                 "variations": [{"pv": ["e2-e4"], "score": {"cp": -500, "wdl": [10, 90, 900]}}]})
        elif mode == "unknown-events": out({"re": i, "ev": "progress", "x": 1}); out({"re": i, "ev": "done", "best": "e2-e4"})
        else: out(info); out({"re": i, "ev": "done", "best": "e2-e4", "variations": info["variations"]})
"#;

fn needs() -> Needs {
    Needs {
        pairings: ["WW".to_owned()].into_iter().collect(),
        options: BTreeMap::new(),
        strength: None,
        launch_ms: 3_000,
    }
}

async fn opened(mode: &str) -> Engine {
    let mut engine = Engine::launch(&scripted(mode)).unwrap();
    let deadline = engine.started() + Duration::from_secs(3);
    let announcement = sei::open(&mut engine, &Host::default(), &BTreeMap::new(), deadline)
        .await
        .unwrap();
    assert_eq!(announcement.version, Some(1));
    engine
}

fn request() -> SearchRequest {
    SearchRequest {
        position: CHESS_CHESS.to_owned(),
        clock: Some(json!({"own": {"deadline": 2000, "remaining": 60000}, "overhead": 100})),
        fresh: true,
        ..SearchRequest::default()
    }
}

async fn turn(mode: &str, hard_stop_ms: u64) -> (sei::Turn, Duration) {
    turn_with_grace(mode, hard_stop_ms, sei::turn::STOP_GRACE).await
}

async fn turn_with_grace(mode: &str, hard_stop_ms: u64, grace: Duration) -> (sei::Turn, Duration) {
    let mut engine = opened(mode).await;
    let started = Instant::now();
    let turn = sei::search(
        &mut engine,
        &request(),
        judge(CHESS_CHESS),
        started + Duration::from_millis(hard_stop_ms),
        grace,
        &tokio::sync::Notify::new(),
    )
    .await;
    let took = started.elapsed();
    // A failed engine says so; an engine in order is alive.
    match &turn.verdict {
        Verdict::Failed(failure) => {
            assert!(!engine.is_alive());
            assert_eq!(engine.failure(), Some(failure));
        }
        _ => assert!(engine.is_alive()),
    }
    engine.close().await;
    (turn, took)
}

// ---- scripted engines ----

#[tokio::test]
async fn a_well_behaved_script_answers() {
    let (turn, _) = turn("ok", 2_000).await;
    assert_eq!(turn.verdict, Verdict::InOrder);
    let answer = turn.answer.unwrap();
    assert_eq!(answer.pmn, "e2-e4");
    assert_eq!(answer.content, r#"["e2","e4",null]"#);
    assert!(!answer.provisional);
    assert_eq!(answer.score.unwrap().cp, Some(0));
}

#[tokio::test]
async fn advice_and_scores_are_read() {
    let (turn, _) = turn("advice", 2_000).await;
    assert_eq!(turn.verdict, Verdict::InOrder);
    let answer = turn.answer.unwrap();
    assert_eq!(answer.advice, Some(sei::Advice::Resign));
    assert_eq!(answer.score.unwrap().wdl, Some([10, 90, 900]));
}

#[tokio::test]
async fn unknown_events_are_ignored() {
    let (turn, _) = turn("unknown-events", 2_000).await;
    assert_eq!(turn.verdict, Verdict::InOrder);
}

#[tokio::test]
async fn never_answers_hello() {
    let mut engine = Engine::launch(&scripted("mute")).unwrap();
    let deadline = engine.started() + Duration::from_millis(500);
    let err = sei::open(&mut engine, &Host::default(), &BTreeMap::new(), deadline)
        .await
        .unwrap_err();
    assert!(matches!(err, EngineFailure::Opening(_)), "{err}");
    engine.close().await;
}

#[tokio::test]
async fn refuses_configure() {
    let mut engine = Engine::launch(&scripted("refuse-configure")).unwrap();
    let deadline = engine.started() + Duration::from_secs(3);
    let mut options = BTreeMap::new();
    options.insert("threads".to_owned(), json!(2));
    let err = sei::open(&mut engine, &Host::default(), &options, deadline)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, EngineFailure::Opening(r) if r.contains("configure refused")),
        "{err}"
    );
}

#[tokio::test]
async fn pings_but_never_done_plays_the_provisional_answer() {
    let (turn, took) = turn("info-then-silence", 600).await;
    assert_eq!(turn.verdict, Verdict::Failed(EngineFailure::Overrun));
    let answer = turn.answer.unwrap();
    assert!(answer.provisional);
    assert_eq!(answer.content, r#"["e2","e4",null]"#);
    assert!(took >= Duration::from_millis(600) && took < Duration::from_millis(1_500));
}

#[tokio::test]
async fn a_done_within_the_grace_after_cancel_is_the_answer() {
    // A Python process answers `cancel` in a few milliseconds; the grace is
    // widened so that a loaded machine does not turn this into an overrun.
    let (turn, took) = turn_with_grace("late-done", 400, Duration::from_millis(500)).await;
    assert_eq!(turn.verdict, Verdict::InOrder);
    assert!(!turn.answer.unwrap().provisional);
    assert!(took >= Duration::from_millis(400));
}

#[tokio::test]
async fn an_illegal_best_is_an_engine_failure() {
    let (turn, _) = turn("illegal-best", 2_000).await;
    assert!(matches!(
        turn.verdict,
        Verdict::Failed(EngineFailure::Violation(_))
    ));
    assert!(turn.answer.is_none());
}

#[tokio::test]
async fn a_non_canonical_best_fails_and_the_provisional_answer_plays() {
    let (turn, _) = turn("noncanonical-best", 2_000).await;
    assert!(matches!(
        turn.verdict,
        Verdict::Failed(EngineFailure::Violation(_))
    ));
    let answer = turn.answer.unwrap();
    assert!(answer.provisional);
    assert_eq!(answer.pmn, "e2-e4");
}

#[tokio::test]
async fn exits_mid_search() {
    let (turn, _) = turn("exit", 2_000).await;
    assert_eq!(turn.verdict, Verdict::Failed(EngineFailure::Exited));
    assert!(turn.answer.is_none());
}

#[tokio::test]
async fn floods_its_output() {
    let (turn, _) = turn("flood", 2_000).await;
    assert!(matches!(
        turn.verdict,
        Verdict::Failed(EngineFailure::Violation(_))
    ));
}

#[tokio::test]
async fn a_line_beyond_the_bound_is_a_violation() {
    let (turn, _) = turn("long-line", 5_000).await;
    assert!(
        matches!(&turn.verdict, Verdict::Failed(EngineFailure::Violation(r)) if r.contains("longer")),
        "{:?}",
        turn.verdict
    );
}

#[tokio::test]
async fn an_info_after_the_done_is_caught_at_the_next_read() {
    // The turn ends on the done, in order; the stray info is read by the
    // next request's reader and is a violation then.
    let mut engine = opened("info-after-done").await;
    let turn = sei::search(
        &mut engine,
        &request(),
        judge(CHESS_CHESS),
        Instant::now() + Duration::from_secs(2),
        sei::turn::STOP_GRACE,
        &tokio::sync::Notify::new(),
    )
    .await;
    assert_eq!(turn.verdict, Verdict::InOrder);
    let err = engine
        .recv(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(err, EngineFailure::Violation(_)), "{err}");
    assert!(!engine.is_alive());
}

#[tokio::test]
async fn an_error_attached_to_a_ping_is_a_violation() {
    let (turn, took) = turn("error-on-ping", 5_000).await;
    assert!(
        matches!(&turn.verdict, Verdict::Failed(EngineFailure::Violation(r)) if r.contains("ping")),
        "{:?}",
        turn.verdict
    );
    assert!(took < Duration::from_millis(2_500), "{took:?}");
}

#[tokio::test]
async fn vendor_events_are_ignored_wherever_attached() {
    let (turn, _) = turn("vendor-events", 2_000).await;
    assert_eq!(turn.verdict, Verdict::InOrder);
}

#[tokio::test]
async fn violates_the_envelope() {
    let (turn, _) = turn("bad-re", 2_000).await;
    assert!(
        matches!(&turn.verdict, Verdict::Failed(EngineFailure::Violation(r)) if r.contains("999")),
        "{:?}",
        turn.verdict
    );
}

#[tokio::test]
async fn refuses_the_search() {
    let (turn, _) = turn("refuse", 2_000).await;
    match &turn.verdict {
        Verdict::Refused(error) => {
            assert_eq!(error.code, Code::Illegal);
            assert_eq!(error.path.as_deref(), Some("/moves/0"));
        }
        other => panic!("{other:?}"),
    }
    assert!(turn.verdict.disagrees_on_rules());
    assert!(turn.answer.is_none());
}

#[tokio::test]
async fn holds_the_position_terminal() {
    let (turn, _) = turn("null-best", 2_000).await;
    assert_eq!(turn.verdict, Verdict::NoMove);
    assert!(turn.verdict.disagrees_on_rules());
}

#[tokio::test]
async fn hangs_and_stops_answering_ping() {
    let (turn, took) = turn("hang", 10_000).await;
    assert_eq!(turn.verdict, Verdict::Failed(EngineFailure::Unresponsive));
    // The first ping goes out after a second and is given a second.
    assert!(took < Duration::from_millis(3_500), "{took:?}");
}

#[tokio::test]
async fn fails_fatally() {
    let (turn, _) = turn("fatal", 2_000).await;
    match turn.verdict {
        Verdict::Failed(EngineFailure::Fatal(error)) => assert_eq!(error.code, Code::Internal),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_probe_names_the_gaps() {
    let launch = scripted("ok");
    let mut needs = needs();
    needs.pairings.insert("CJ".to_owned());
    let probe = sei::probe(&launch, &Host::default(), &needs).await.unwrap();
    assert_eq!(probe.announcement.name.as_deref(), Some("scripted"));
    assert!(probe.announcement.features.roots);
    assert!(probe.engine_rtt < Duration::from_secs(1));

    needs.strength = Some(1500);
    needs.options.insert("hash".to_owned(), json!(16));
    let err = sei::probe(&launch, &Host::default(), &needs)
        .await
        .unwrap_err();
    match err {
        ProbeError::Unsupported(gaps) => {
            assert_eq!(gaps.len(), 2, "{gaps:?}");
        }
        other => panic!("{other}"),
    }

    let mute = scripted("mute");
    let mut quick = self::needs();
    quick.launch_ms = 1_000;
    let err = sei::probe(&mute, &Host::default(), &quick)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProbeError::Engine(EngineFailure::Opening(_))),
        "{err}"
    );
}

#[tokio::test]
async fn a_missing_program_is_a_launch_failure() {
    let launch = Launch {
        command: PathBuf::from("/nonexistent/engine"),
        args: Vec::new(),
        cwd: std::env::temp_dir(),
        env: Vec::new(),
    };
    assert!(matches!(
        Engine::launch(&launch),
        Err(EngineFailure::Launch(_))
    ));
}

// ---- the random engine ----

fn random_engine() -> Option<Launch> {
    let path = std::env::var_os("SANKI_SEI_RANDOM_ENGINE")?;
    Some(Launch {
        command: PathBuf::from(path),
        args: Vec::new(),
        cwd: std::env::temp_dir(),
        env: Vec::new(),
    })
}

#[tokio::test]
async fn the_random_engine_passes_the_probe_and_plays() {
    let Some(launch) = random_engine() else {
        eprintln!("SANKI_SEI_RANDOM_ENGINE not set: skipped");
        return;
    };
    let mut needs = needs();
    for a in ["W", "J", "C"] {
        for b in ["W", "J", "C"] {
            needs.pairings.insert(format!("{a}{b}"));
        }
    }
    needs.options.insert("seed".to_owned(), json!(7));
    let probe = sei::probe(&launch, &Host::default(), &needs).await.unwrap();
    assert!(probe.announcement.plays("CJ"));
    assert!(probe.announcement.features.roots);
    assert!(
        probe.engine_rtt < Duration::from_millis(500),
        "{:?}",
        probe.engine_rtt
    );

    // A game: one process, several turns, the history growing.
    let mut engine = Engine::launch(&launch).unwrap();
    let deadline = engine.started() + Duration::from_secs(3);
    sei::open(&mut engine, &Host::default(), &needs.options, deadline)
        .await
        .unwrap();
    let mut position = Position::parse(CHESS_CHESS).unwrap();
    let mut moves: Vec<String> = Vec::new();
    for ply in 0..6 {
        let feen = position.to_feen();
        let legal: Vec<String> = rules::legal_moves(&position)
            .iter()
            .map(|mv| sashite_sanki_engine::pmn::to_pmn(&position, mv).unwrap())
            .collect();
        let request = SearchRequest {
            position: CHESS_CHESS.to_owned(),
            moves: moves.clone(),
            clock: Some(json!({"own": {"deadline": 5000, "remaining": 60000}, "overhead": 100})),
            roots: Some(legal.clone()),
            fresh: ply == 0,
            ..SearchRequest::default()
        };
        let turn = sei::search(
            &mut engine,
            &request,
            judge(&feen),
            Instant::now() + Duration::from_secs(2),
            sei::turn::STOP_GRACE,
            &tokio::sync::Notify::new(),
        )
        .await;
        assert_eq!(turn.verdict, Verdict::InOrder, "ply {ply}");
        let answer = turn.answer.unwrap();
        assert!(legal.contains(&answer.pmn));
        let mv = sashite_sanki_engine::pmn::parse_canonical(&position, &answer.pmn).unwrap();
        position = rules::apply(&position, &mv).unwrap();
        moves.push(answer.pmn);
    }
    assert!(engine.is_alive());
    engine.close().await;
}
