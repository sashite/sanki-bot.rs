// SPDX-License-Identifier: Apache-2.0
//! The rule system's module, run as *Kernel ABI — Sanki* §The consumer's side
//! prescribes: instantiated under `wasmi` in a sandbox with no host capabilities,
//! its shape checked (the WebAssembly profile, an empty import section, the four
//! exports, a fixed 128 MiB memory, the `abi` buffer), and driven through the
//! arena protocol — `alloc`, write, `call`, read the response buffer before the
//! next `alloc`.
//!
//! The bot computes no rule of its own (ADR-0034): the state of a session, the
//! legal moves, every verdict it claims or accepts is a response of the module.
//! [`Oracle`] is the one seam — a request's bytes to a response's bytes — so
//! that the module-driven paths can be exercised with the reference module's
//! library face in tests, and [`Runtime`] is the production implementation
//! over the module's verified bytes.
//!
//! Failure policy: a response the module could not place (`0` from `call`), a
//! trap, a buffer beyond the response bound, or a length `alloc` refuses is **no
//! answer** (§The request and the response) — the bot then does nothing on the
//! session until the next tick, and never infers anything from the failure.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use wasmi::{Config, Engine, Linker, Memory, Module, Store, TypedFunc};

/// The ABI this bot implements — the one identifier a Rule System event's
/// `abi` tag, and the module's own `abi()` buffer, must both state.
pub const ABI: &str = "sashite.sanki.kernel-abi/1";

/// A request is at most 32 MiB (Kernel ABI — Sanki §Bounds).
pub const REQUEST_BOUND: usize = 33_554_432;

/// A response is at most 4 MiB (Kernel ABI — Sanki §Bounds).
pub const RESPONSE_BOUND: usize = 4_194_304;

/// The module's memory: `min = max = 2048` pages (128 MiB), never growing.
const MEMORY_PAGES: u64 = 2048;

/// The module as a function of bytes: one request in, one response out, or no
/// answer. Everything the bot asks a rule system goes through here.
pub trait Oracle {
    /// Answers one request (a JSON object per the ABI), or `None` when the
    /// module produced no answer — `oversized`, a trap, a buffer it could not
    /// place. A response is the module's canonical JSON bytes.
    fn answer(&mut self, request: &[u8]) -> Option<Vec<u8>>;
}

/// Why a module could not be instantiated as a conforming one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModuleError {
    /// The bytes do not validate under the ABI's WebAssembly profile.
    Invalid(String),
    /// The import section is not empty (the module asks for host capabilities).
    Imports(usize),
    /// The module does not instantiate.
    Instantiation(String),
    /// A required export is missing or has the wrong type. Carries its name.
    Export(&'static str),
    /// The `memory` export is not `min = max = 2048` pages.
    Memory {
        /// The declared minimum, in pages.
        min: u64,
        /// The declared maximum, in pages (`None` when unbounded).
        max: Option<u64>,
    },
    /// The `abi()` buffer is not this bot's ABI identifier. Carries what the
    /// module states.
    Abi(String),
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "the module does not validate: {reason}"),
            Self::Imports(count) => write!(f, "the module imports {count} item(s); none allowed"),
            Self::Instantiation(reason) => write!(f, "the module does not instantiate: {reason}"),
            Self::Export(name) => write!(f, "the module lacks a conforming `{name}` export"),
            Self::Memory { min, max } => write!(
                f,
                "the module's memory is min {min} / max {max:?} pages; 2048 / 2048 required"
            ),
            Self::Abi(stated) => write!(f, "the module states ABI {stated:?}; {ABI} required"),
        }
    }
}

impl std::error::Error for ModuleError {}

/// The exact WebAssembly profile of *Kernel ABI — Sanki* §WebAssembly profile,
/// and nothing more: a module using any other feature fails to validate.
fn profile() -> Config {
    let mut config = Config::default();
    config
        .wasm_mutable_global(true)
        .wasm_sign_extension(true)
        .wasm_saturating_float_to_int(true)
        .wasm_multi_value(true)
        .wasm_bulk_memory(true)
        .wasm_reference_types(true)
        .wasm_multi_memory(false)
        .wasm_tail_call(false)
        .wasm_extended_const(false)
        .wasm_custom_page_sizes(false)
        .wasm_memory64(false)
        .wasm_wide_arithmetic(false);
    config
}

/// A module instantiated under `wasmi`, ready for requests.
pub struct Runtime {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    call: TypedFunc<(u32, u32), u32>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Runtime")
    }
}

impl Runtime {
    /// Instantiates the module from its verified bytes, checking its shape as
    /// the ABI requires, and that it states this bot's ABI.
    ///
    /// # Errors
    ///
    /// The first shape violation, as a [`ModuleError`].
    pub fn new(bytes: &[u8]) -> Result<Self, ModuleError> {
        let engine = Engine::new(&profile());
        let module =
            Module::new(&engine, bytes).map_err(|e| ModuleError::Invalid(e.to_string()))?;
        let imports = module.imports().count();
        if imports != 0 {
            return Err(ModuleError::Imports(imports));
        }
        let linker: Linker<()> = Linker::new(&engine);
        let mut store = Store::new(&engine, ());
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| ModuleError::Instantiation(e.to_string()))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(ModuleError::Export("memory"))?;
        let ty = memory.ty(&store);
        if ty.minimum() != MEMORY_PAGES || ty.maximum() != Some(MEMORY_PAGES) {
            return Err(ModuleError::Memory {
                min: ty.minimum(),
                max: ty.maximum(),
            });
        }
        let alloc = instance
            .get_typed_func::<u32, u32>(&store, "alloc")
            .map_err(|_| ModuleError::Export("alloc"))?;
        let call = instance
            .get_typed_func::<(u32, u32), u32>(&store, "call")
            .map_err(|_| ModuleError::Export("call"))?;
        let abi = instance
            .get_typed_func::<(), u32>(&store, "abi")
            .map_err(|_| ModuleError::Export("abi"))?;
        let mut runtime = Self {
            store,
            memory,
            alloc,
            call,
        };
        let address = abi
            .call(&mut runtime.store, ())
            .map_err(|_| ModuleError::Export("abi"))?;
        let stated = runtime
            .buffer(address)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or(ModuleError::Export("abi"))?;
        if stated != ABI {
            return Err(ModuleError::Abi(stated));
        }
        Ok(runtime)
    }

    /// Reads a buffer (a 4-byte little-endian length, then the bytes) at
    /// `address`; `None` when the buffer is at `0`, exceeds the response bound
    /// or extends past the memory.
    fn buffer(&self, address: u32) -> Option<Vec<u8>> {
        if address == 0 {
            return None;
        }
        let data = self.memory.data(&self.store);
        let start = usize::try_from(address).ok()?;
        let header = data.get(start..start.checked_add(4)?)?;
        let len = usize::try_from(u32::from_le_bytes(header.try_into().ok()?)).ok()?;
        if len > RESPONSE_BOUND {
            return None;
        }
        let body_start = start.checked_add(4)?;
        let body = data.get(body_start..body_start.checked_add(len)?)?;
        Some(body.to_vec())
    }
}

impl Oracle for Runtime {
    fn answer(&mut self, request: &[u8]) -> Option<Vec<u8>> {
        if request.len() > REQUEST_BOUND {
            return None;
        }
        let len = u32::try_from(request.len()).ok()?;
        let address = self.alloc.call(&mut self.store, len).ok()?;
        if address == 0 {
            return None;
        }
        let start = usize::try_from(address).ok()?;
        self.memory.write(&mut self.store, start, request).ok()?;
        let response = self.call.call(&mut self.store, (address, len)).ok()?;
        self.buffer(response)
    }
}

/// An error response of the ABI (`{ "error": { "code", "message" } }`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AbiError {
    /// The error code (`oversized`, `malformed`, `unsupported_op`, …).
    pub code: String,
    /// Free text for humans — never keyed on.
    #[serde(default)]
    pub message: String,
}

/// Why a request got no usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AskError {
    /// The module produced no answer (§The request and the response).
    NoAnswer,
    /// The response is not the JSON this ABI defines for the operation.
    Unexpected(String),
    /// The module answered with an error.
    Error(AbiError),
}

impl fmt::Display for AskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAnswer => f.write_str("the module produced no answer"),
            Self::Unexpected(reason) => write!(f, "unexpected response: {reason}"),
            Self::Error(error) => write!(f, "{}: {}", error.code, error.message),
        }
    }
}

impl std::error::Error for AskError {}

/// One JSON request through an oracle, its response decoded as `T` — or the
/// ABI error it carried.
///
/// # Errors
///
/// [`AskError`] when there is no answer, the answer is an ABI error, or it does
/// not decode as `T`.
pub fn ask<T: DeserializeOwned>(oracle: &mut impl Oracle, request: &Value) -> Result<T, AskError> {
    let bytes = serde_json::to_vec(request).map_err(|e| AskError::Unexpected(e.to_string()))?;
    let response = oracle.answer(&bytes).ok_or(AskError::NoAnswer)?;
    let value: Value =
        serde_json::from_slice(&response).map_err(|e| AskError::Unexpected(e.to_string()))?;
    if let Some(error) = value.get("error") {
        let error: AbiError = serde_json::from_value(error.clone())
            .map_err(|e| AskError::Unexpected(e.to_string()))?;
        return Err(AskError::Error(error));
    }
    serde_json::from_value(value).map_err(|e| AskError::Unexpected(e.to_string()))
}

/// What `describe` reports that the bot reads (Kernel ABI — Sanki §`describe`):
/// the normative members it checks events against and founds sessions with.
/// The rest of the response is ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Describe {
    /// The ABI identifier the module states.
    pub abi: String,
    /// The game identifier a Rule System event naming this module carries.
    pub game: String,
    /// The largest `step` a Ply can enter a chain with.
    pub max_step: u32,
    /// The initial position of every ordered pairing, keyed `"<first>/<second>"`.
    pub positions: std::collections::BTreeMap<String, String>,
}

/// The `describe` operation.
///
/// # Errors
///
/// See [`ask`].
pub fn describe(oracle: &mut impl Oracle) -> Result<Describe, AskError> {
    ask(oracle, &serde_json::json!({ "op": "describe" }))
}

/// A result on the seat axis: each score `0`, `50` or `100`, summing to `100`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SeatResult {
    /// The score of the `first` seat.
    pub first: u32,
    /// The score of the `second` seat.
    pub second: u32,
}

/// A verdict: a result and a status (Kernel ABI — Sanki §Values).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Verdict {
    /// The result on the seat axis.
    pub result: SeatResult,
    /// The termination status.
    pub status: String,
}

/// The `legal_moves` operation: every legal move of the side to move in
/// `position`, as Ply contents, sorted (Kernel ABI — Sanki §`legal_moves`).
///
/// # Errors
///
/// See [`ask`].
pub fn legal_moves(oracle: &mut impl Oracle, position: &str) -> Result<Vec<String>, AskError> {
    #[derive(Deserialize)]
    struct Moves {
        moves: Vec<String>,
    }
    ask::<Moves>(
        oracle,
        &serde_json::json!({ "op": "legal_moves", "position": position }),
    )
    .map(|answer| answer.moves)
}

/// The `apply` answer (Kernel ABI — Sanki §`apply`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Applied {
    /// The canonical resulting position.
    pub position: String,
    /// Whether the move resets the move-limit counter.
    pub irreversible: bool,
    /// The resulting position's intrinsic status (`ongoing`, `checkmate`, …).
    pub status: String,
}

/// The `apply` operation: `mv` (a Ply content) applied to `position`; an
/// illegal move is the ABI error `illegal`.
///
/// # Errors
///
/// See [`ask`].
pub fn apply(oracle: &mut impl Oracle, position: &str, mv: &str) -> Result<Applied, AskError> {
    ask(
        oracle,
        &serde_json::json!({ "op": "apply", "position": position, "move": mv }),
    )
}

/// A player's clock as the ABI describes it (Kernel ABI — Sanki §Values).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct Clock {
    /// The 0-based period index.
    pub period: u32,
    /// The plies played in the period.
    pub plies_in_period: u32,
    /// The seconds remaining.
    pub remaining: u64,
}

/// Both clocks, by seat.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct Clocks {
    /// The `first` seat's clock.
    pub first: Clock,
    /// The `second` seat's clock.
    pub second: Clock,
}

/// A selected canonical Ply of the chain.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ChainLink {
    /// Its canonical timing.
    pub at: u64,
    /// Its event id (64 lowercase hex characters).
    pub id: String,
}

/// The end of the natural state (Kernel ABI — Sanki §`natural_state`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// The replay reached a rule-system ending or a played-Ply timeout.
    Terminal {
        /// The canonical timing of the Ply that reached it.
        at: u64,
        /// The verdict.
        verdict: Verdict,
    },
    /// The session goes on.
    Ongoing {
        /// The chain's anchor — the value the abandonment clock runs from.
        anchor: u64,
        /// Both clocks as the replay left them.
        clocks: Clocks,
        /// The play-order index of the next half-move (1-based).
        half_move: u32,
        /// The end position.
        position: String,
    },
}

/// The `natural_state` answer.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct NaturalState {
    /// The selected canonical Plies in play order.
    pub chain: Vec<ChainLink>,
    /// The end.
    pub end: End,
}

/// The `natural_state` operation over an assembled session request, at
/// `cutoff`.
///
/// # Errors
///
/// See [`ask`].
pub fn natural_state(
    oracle: &mut impl Oracle,
    session: &Value,
    cutoff: u64,
) -> Result<NaturalState, AskError> {
    let mut request = session.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert("op".to_owned(), Value::from("natural_state"));
        object.insert("cutoff".to_owned(), Value::from(cutoff));
    }
    ask(oracle, &request)
}

/// The `verdict_at` answer: the verdict, or `no_verdict` (`before_start`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerdictAt {
    /// The verdict the rule system yields.
    Verdict(Verdict),
    /// No verdict: the cutoff precedes t₀.
    NoVerdict(String),
}

/// The `verdict_at` operation: the verdict the rule system yields at `cutoff`
/// with `invoker` (`"first"` or `"second"`) as the concluding side — the claim
/// a Conclusion published then MUST carry to conform.
///
/// # Errors
///
/// See [`ask`].
pub fn verdict_at(
    oracle: &mut impl Oracle,
    session: &Value,
    invoker: &str,
    cutoff: u64,
) -> Result<VerdictAt, AskError> {
    let mut request = session.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert("op".to_owned(), Value::from("verdict_at"));
        object.insert("invoker".to_owned(), Value::from(invoker));
        object.insert("cutoff".to_owned(), Value::from(cutoff));
    }
    ask(oracle, &request)
}

/// The `check` answer (Kernel ABI — Sanki §`check`).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// The Conclusion claims exactly the verdict the rule system yields.
    Conforming(Verdict),
    /// The Conclusion claims something else.
    Wrong {
        /// The claim as offered.
        claimed: Value,
        /// What a conforming Conclusion at that cutoff would carry.
        expected: Verdict,
    },
    /// The Conclusion is out of reach: `other_session`, `not_a_player`,
    /// `pending` or `before_start`.
    NoVerdict(String),
}

/// The `check` operation over an assembled session request.
///
/// # Errors
///
/// See [`ask`].
pub fn check(
    oracle: &mut impl Oracle,
    session: &Value,
    conclusion: &Value,
) -> Result<Check, AskError> {
    let mut request = session.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert("op".to_owned(), Value::from("check"));
        object.insert("conclusion".to_owned(), conclusion.clone());
    }
    ask(oracle, &request)
}

/// The canonical Conclusion `select_conclusion` names.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Canonical {
    /// Its cutoff — its canonical timing.
    pub cutoff: u64,
    /// Its event id (64 lowercase hex characters).
    pub id: String,
    /// Its verdict.
    pub verdict: Verdict,
}

/// The `select_conclusion` answer.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct Selection {
    canonical: Option<Canonical>,
}

/// The `select_conclusion` operation over an assembled session request and the
/// session's timed Conclusions: the canonical one, or `None` while none
/// conforms.
///
/// # Errors
///
/// See [`ask`].
pub fn select_conclusion(
    oracle: &mut impl Oracle,
    session: &Value,
    conclusions: &[Value],
) -> Result<Option<Canonical>, AskError> {
    let mut request = session.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert("op".to_owned(), Value::from("select_conclusion"));
        object.insert("conclusions".to_owned(), Value::Array(conclusions.to_vec()));
    }
    ask::<Selection>(oracle, &request).map(|selection| selection.canonical)
}

#[cfg(test)]
pub(crate) mod native {
    //! The reference module's library face as an [`Oracle`]: the same answers
    //! as the module, natively — what the tests drive the verification with.

    use super::Oracle;

    /// The reference `sanki` rule system, answering natively.
    #[derive(Debug, Default)]
    pub struct Native;

    impl Oracle for Native {
        fn answer(&mut self, request: &[u8]) -> Option<Vec<u8>> {
            sashite_sanki_kernel_wasm::answer(request)
        }
    }
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
    use native::Native;

    #[test]
    fn describe_reports_the_reference_module() {
        let d = describe(&mut Native).unwrap();
        assert_eq!(d.abi, ABI);
        assert_eq!(d.game, "sanki");
        assert_eq!(d.max_step, 300);
        assert!(d.positions.contains_key("chess/chess"));
        assert!(d.positions.contains_key("ogi/xiongqi"));
    }

    #[test]
    fn an_abi_error_is_surfaced_as_such() {
        let err = ask::<Describe>(&mut Native, &serde_json::json!({ "op": "nope" })).unwrap_err();
        assert!(
            matches!(err, AskError::Error(AbiError { ref code, .. }) if code == "unsupported_op")
        );
    }

    #[test]
    fn no_answer_is_no_answer() {
        struct Mute;
        impl Oracle for Mute {
            fn answer(&mut self, _: &[u8]) -> Option<Vec<u8>> {
                None
            }
        }
        assert_eq!(describe(&mut Mute).unwrap_err(), AskError::NoAnswer);
    }

    /// The published module, when `SANKI_MODULE` names its bytes: the runtime
    /// instantiates it and answers `describe` exactly as the library face does.
    #[test]
    fn the_real_module_answers_like_the_library() {
        let Ok(path) = std::env::var("SANKI_MODULE") else {
            eprintln!("SANKI_MODULE unset; skipping the wasmi run");
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        let mut runtime = Runtime::new(&bytes).unwrap();
        let request = serde_json::to_vec(&serde_json::json!({ "op": "describe" })).unwrap();
        assert_eq!(runtime.answer(&request), Native.answer(&request));
    }

    #[test]
    fn a_wrong_shaped_module_is_refused() {
        // Not WebAssembly at all.
        assert!(matches!(
            Runtime::new(b"not wasm"),
            Err(ModuleError::Invalid(_))
        ));
        // A valid empty module: no exports.
        let empty = wat::parse_str("(module)").unwrap();
        assert_eq!(
            Runtime::new(&empty).err(),
            Some(ModuleError::Export("memory"))
        );
        // The right exports but a growable memory.
        let growable = wat::parse_str(
            r#"(module
                (memory (export "memory") 1)
                (func (export "abi") (result i32) i32.const 0)
                (func (export "alloc") (param i32) (result i32) i32.const 0)
                (func (export "call") (param i32 i32) (result i32) i32.const 0))"#,
        )
        .unwrap();
        assert!(matches!(
            Runtime::new(&growable),
            Err(ModuleError::Memory { min: 1, max: None })
        ));
        // A module importing a host function.
        let importing = wat::parse_str(r#"(module (import "env" "f" (func)))"#).unwrap();
        assert_eq!(
            Runtime::new(&importing).err(),
            Some(ModuleError::Imports(1))
        );
    }
}
