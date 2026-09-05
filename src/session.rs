// SPDX-License-Identifier: Apache-2.0
//! Assembling a session for the module — *Kernel ABI — Sanki* §The consumer's
//! side: from the session's Nostr events to the `session`, `plies`,
//! `attestations` and `conclusions` members of a request.
//!
//! The bot hands the module only what has passed the checks that are the
//! **consumer's**, and none of the checks that are the module's:
//!
//! - **terms** ([`terms`]) — the Game Session (kind `3422`) against its founding
//!   (a Pairing, kind `3419`, or a Direct Challenge, kind `3420`): the players,
//!   their seats and variants, the game, the `rules` reference, the timing
//!   designation, and the time control the founding fixed. What the bot can
//!   check without the module (kind `3422` §Semantic constraints, the
//!   cross-event items) it checks here; the initial position (constraint 7) is
//!   checked against the module's `describe` by the actor;
//! - **conformance** — a Ply's structural constraints (kind `3423`, items 1–4, 6
//!   and the transport half of 5), a Conclusion's (kind `3425`, items 1–7), an
//!   attestation's; signatures the relay pool verified on receipt, and the bot
//!   verifies again ([`Events::from_relay`]);
//! - **scope and relevance** — only events referencing this session, signed by a
//!   player (or the designated timestamper), and Plies whose `step` is within the
//!   module's `max_step`;
//! - **timing** — in self-timed mode, only events the bot read from the timing
//!   relay the session designates (its own relay — the only one it plays on);
//!   in attested mode, only attestations by the designated timestamper, the
//!   module resolving canonical timing from them.
//!
//! The bot judges no Ply's `content` and no Conclusion's claim: an unparseable
//! content is an illegal candidate the module skips, and a claim is judged by
//! `check`. Both are the module's (ADR-0034).

use crate::tags;
use nostr_sdk::prelude::*;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt;

/// The Game Session kind.
pub const KIND_GAME_SESSION: u16 = 3422;
/// The Ply kind.
pub const KIND_PLY: u16 = 3423;
/// The Conclusion kind.
pub const KIND_CONCLUSION: u16 = 3425;
/// The Event Timestamp Attestation kind.
pub const KIND_ATTESTATION: u16 = 3410;
/// The Pairing kind (a matchmade founding).
pub const KIND_PAIRING: u16 = 3419;
/// The Direct Challenge kind (a directed founding).
pub const KIND_DIRECT_CHALLENGE: u16 = 3420;

/// A Ply's `content` is at most this many code points (kind `3423` §Content).
const CONTENT_MAX_CODE_POINTS: usize = 256;

/// A session's timing designation (Canonical Timing NIP §Timing modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Timing {
    /// Self-timed: the designated timing relay, verbatim.
    SelfTimed(String),
    /// Attested: the designated timestamper.
    Attested(PublicKey),
}

/// A seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seat {
    /// Moves first.
    First,
    /// Moves second.
    Second,
}

impl Seat {
    /// The seat named `name` (`first` / `second`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "first" => Some(Self::First),
            "second" => Some(Self::Second),
            _ => None,
        }
    }

    /// The seat-name, as the tags carry it — the ABI's side token too.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Second => "second",
        }
    }

    /// The other seat.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::First => Self::Second,
            Self::Second => Self::First,
        }
    }
}

/// What the founding events fixed — the `session` member of a request, plus
/// what the bot itself needs (the game, the rule system, the timing
/// designation, the per-seat variants and pubkeys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTerms {
    /// The Game Session's id.
    pub id: EventId,
    /// The game identifier.
    pub game: String,
    /// The Rule System event the session is played under.
    pub rules: EventId,
    /// The player in seat `first`.
    pub first: PublicKey,
    /// The player in seat `second`.
    pub second: PublicKey,
    /// The `first` player's variant.
    pub first_variant: String,
    /// The `second` player's variant.
    pub second_variant: String,
    /// The timing designation.
    pub timing: Timing,
    /// The time control, in the ABI's encoding (period triples).
    pub time_control: Vec<[Option<u64>; 3]>,
    /// The initial position (the Game Session's `content`).
    pub position: String,
    /// The scheduled start, if any (`start_at`).
    pub start_at: Option<u64>,
}

impl SessionTerms {
    /// Whether `pubkey` is one of the two players.
    #[must_use]
    pub fn is_player(&self, pubkey: &PublicKey) -> bool {
        *pubkey == self.first || *pubkey == self.second
    }

    /// The seat of `pubkey`, if a player.
    #[must_use]
    pub fn seat_of(&self, pubkey: &PublicKey) -> Option<Seat> {
        if *pubkey == self.first {
            Some(Seat::First)
        } else if *pubkey == self.second {
            Some(Seat::Second)
        } else {
            None
        }
    }

    /// The other player of `pubkey`, if a player.
    #[must_use]
    pub fn opponent_of(&self, pubkey: &PublicKey) -> Option<PublicKey> {
        match self.seat_of(pubkey)? {
            Seat::First => Some(self.second),
            Seat::Second => Some(self.first),
        }
    }

    /// The player seated `seat`.
    #[must_use]
    pub const fn player(&self, seat: Seat) -> PublicKey {
        match seat {
            Seat::First => self.first,
            Seat::Second => self.second,
        }
    }

    /// The pairing key of the initial position in `describe.positions`:
    /// `"<first variant>/<second variant>"`.
    #[must_use]
    pub fn pairing(&self) -> String {
        format!("{}/{}", self.first_variant, self.second_variant)
    }

    /// The `session` member of a request, given t₀.
    #[must_use]
    pub fn to_json(&self, start: u64) -> Value {
        let time_control: Vec<Value> = self
            .time_control
            .iter()
            .map(|[d, i, p]| json!([d, i, p]))
            .collect();
        json!({
            "id": self.id.to_hex(),
            "first": self.first.to_hex(),
            "second": self.second.to_hex(),
            "timestamper": match &self.timing {
                Timing::Attested(pubkey) => Value::from(pubkey.to_hex()),
                Timing::SelfTimed(_) => Value::Null,
            },
            "time_control": time_control,
            "position": self.position,
            "start": start,
        })
    }
}

/// Why a Game Session and its founding do not yield session terms.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TermsError {
    /// The Game Session is not of kind `3422`, or the founding is not of the
    /// kind its marker announces.
    WrongKind,
    /// The Game Session's founding reference is absent, doubled, or does not
    /// name the founding offered.
    Founding,
    /// The `rules` reference is absent, doubled, or differs from the founding's.
    Rules,
    /// The `game` tag is absent, doubled, or differs from the founding's.
    Game,
    /// The players are not exactly two distinct `player`-marked pubkeys equal to
    /// the founding's, including the signer.
    Players,
    /// The seats are not one `first` and one `second`, one per player, or
    /// disagree with the founding.
    Seats,
    /// The variants are not one per player, or disagree with the founding.
    Variants,
    /// The timing designation is not exactly one of the two forms, or does not
    /// mirror the founding's.
    Timing,
    /// The founding's time control is absent or malformed.
    TimeControl,
    /// The `content` is empty or over 4096 bytes.
    Position,
    /// The `start_at` tag is malformed.
    StartAt,
}

impl fmt::Display for TermsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self {
            Self::WrongKind => "wrong event kinds",
            Self::Founding => "the founding reference",
            Self::Rules => "the rules reference",
            Self::Game => "the game tag",
            Self::Players => "the players",
            Self::Seats => "the seats",
            Self::Variants => "the variants",
            Self::Timing => "the timing designation",
            Self::TimeControl => "the founding's time control",
            Self::Position => "the initial position's form",
            Self::StartAt => "the start_at tag",
        };
        write!(f, "the Game Session does not conform: {what}")
    }
}

impl std::error::Error for TermsError {}

/// The founding reference of a Game Session: the marker and the id.
fn founding_ref(session: &Event) -> Result<(u16, EventId), TermsError> {
    let pairing = tags::events_with_marker(session, "pairing");
    let direct = tags::events_with_marker(session, "direct_challenge");
    match (pairing.as_slice(), direct.as_slice()) {
        ([id], []) => Ok((KIND_PAIRING, *id)),
        ([], [id]) => Ok((KIND_DIRECT_CHALLENGE, *id)),
        _ => Err(TermsError::Founding),
    }
}

/// Exactly one value of the singleton tag `name`.
pub(crate) fn exactly_one<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    let values = tags::values(event, name);
    match values.as_slice() {
        [value] => Some(value),
        _ => None,
    }
}

/// The `rules` reference of a founding-family event.
pub(crate) fn rules_ref(event: &Event) -> Option<EventId> {
    match tags::events_with_marker(event, "rules").as_slice() {
        [id] => Some(*id),
        _ => None,
    }
}

/// The timing designation of an event: exactly one of the two forms.
pub(crate) fn timing_of(event: &Event) -> Option<Timing> {
    let timestampers = tags::pubkeys_with_role(event, "timestamper");
    let relays = tags::values(event, "timing_relay");
    match (timestampers.as_slice(), relays.as_slice()) {
        ([pubkey], []) => Some(Timing::Attested(*pubkey)),
        ([], [relay]) if !relay.is_empty() => Some(Timing::SelfTimed((*relay).to_owned())),
        _ => None,
    }
}

/// A founding's time control in the ABI's encoding: each tag a period
/// `[duration, increment | null, plies | null]` (kind `3420` §Match-terms
/// tags); the module validates the configuration itself.
pub(crate) fn time_control_of(founding: &Event) -> Result<Vec<[Option<u64>; 3]>, TermsError> {
    let mut periods = Vec::new();
    for tag in founding.tags.iter() {
        let s = tag.as_slice();
        if s.first().map(String::as_str) != Some("time_control") {
            continue;
        }
        let element = |index: usize| -> Result<Option<u64>, TermsError> {
            match s.get(index) {
                None => Ok(None),
                Some(text) => decimal(text).map(Some).ok_or(TermsError::TimeControl),
            }
        };
        if s.len() > 4 {
            return Err(TermsError::TimeControl);
        }
        let duration = element(1)?.ok_or(TermsError::TimeControl)?;
        periods.push([Some(duration), element(2)?, element(3)?]);
    }
    if periods.is_empty() {
        return Err(TermsError::TimeControl);
    }
    Ok(periods)
}

/// A bare decimal integer without leading zeros, bounded to `u64`.
pub(crate) fn decimal(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 19 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes.first() == Some(&b'0') {
        return None;
    }
    text.parse().ok()
}

/// The session's terms from its Game Session and the founding it references,
/// checked against each other (kind `3422` §Semantic constraints, the
/// cross-event items the bot can decide).
///
/// # Errors
///
/// The first violated item, as a [`TermsError`].
pub fn terms(session: &Event, founding: &Event) -> Result<SessionTerms, TermsError> {
    if session.kind != Kind::Custom(KIND_GAME_SESSION) {
        return Err(TermsError::WrongKind);
    }
    let (founding_kind, founding_id) = founding_ref(session)?;
    if founding.kind != Kind::Custom(founding_kind) || founding.id != founding_id {
        return Err(TermsError::Founding);
    }
    // The only `e` tags are the founding reference and the rules reference.
    let e_tags = tags::count_named(session, "e");
    if e_tags != 2 {
        return Err(TermsError::Founding);
    }

    let rules = rules_ref(session).ok_or(TermsError::Rules)?;
    if rules_ref(founding) != Some(rules) {
        return Err(TermsError::Rules);
    }
    let game = exactly_one(session, "game").ok_or(TermsError::Game)?;
    if exactly_one(founding, "game") != Some(game) {
        return Err(TermsError::Game);
    }

    // The players: two distinct `player` pubkeys, equal to the founding's set,
    // including the signer.
    let players = tags::pubkeys_with_role(session, "player");
    let [a, b] = players.as_slice() else {
        return Err(TermsError::Players);
    };
    if a == b || (session.pubkey != *a && session.pubkey != *b) {
        return Err(TermsError::Players);
    }
    let founded: HashSet<PublicKey> = match founding_kind {
        KIND_PAIRING => tags::pubkeys_with_role(founding, "player")
            .into_iter()
            .collect(),
        _ => {
            let opponent = tags::pubkeys_with_role(founding, "opponent");
            let [opponent] = opponent.as_slice() else {
                return Err(TermsError::Players);
            };
            // On the directed path the Game Session is the acceptance: its
            // signer MUST be the challenged player (kind 3422 §Signing
            // party); the challenger cannot found on their own challenge.
            if session.pubkey != *opponent {
                return Err(TermsError::Players);
            }
            HashSet::from([founding.pubkey, *opponent])
        }
    };
    if founded.len() != 2 || !founded.contains(a) || !founded.contains(b) {
        return Err(TermsError::Players);
    }

    // The seats: one per player, `first` and `second` one each; equal to the
    // Pairing's, or consistent with the Direct Challenge's declared seat.
    let seat_a = tags::seat_for(session, a)
        .and_then(Seat::parse)
        .ok_or(TermsError::Seats)?;
    let seat_b = tags::seat_for(session, b)
        .and_then(Seat::parse)
        .ok_or(TermsError::Seats)?;
    if seat_a == seat_b || tags::count_named(session, "seat") != 2 {
        return Err(TermsError::Seats);
    }
    let (first, second) = if seat_a == Seat::First {
        (*a, *b)
    } else {
        (*b, *a)
    };
    match founding_kind {
        KIND_PAIRING => {
            if tags::seat_for(founding, &first) != Some("first")
                || tags::seat_for(founding, &second) != Some("second")
            {
                return Err(TermsError::Seats);
            }
        }
        _ => {
            if let Some(declared) = exactly_one(founding, "seat") {
                let challenger = founding.pubkey;
                let holds = if declared == "first" { first } else { second };
                if Seat::parse(declared).is_none() || holds != challenger {
                    return Err(TermsError::Seats);
                }
            }
        }
    }

    // The variants: one per player; equal to the founding's where it fixed one.
    if tags::count_named(session, "variant") != 2 {
        return Err(TermsError::Variants);
    }
    let first_variant = tags::variant_for(session, &first).ok_or(TermsError::Variants)?;
    let second_variant = tags::variant_for(session, &second).ok_or(TermsError::Variants)?;
    for (player, variant) in [(first, first_variant), (second, second_variant)] {
        if !is_identifier(variant) {
            return Err(TermsError::Variants);
        }
        if let Some(fixed) = tags::variant_for(founding, &player) {
            if fixed != variant {
                return Err(TermsError::Variants);
            }
        } else if founding_kind == KIND_PAIRING {
            // A Pairing fixes both variants unconditionally.
            return Err(TermsError::Variants);
        }
    }

    // The timing designation, mirrored verbatim.
    let timing = timing_of(session).ok_or(TermsError::Timing)?;
    if timing_of(founding).as_ref() != Some(&timing) {
        return Err(TermsError::Timing);
    }

    let time_control = time_control_of(founding)?;

    if session.content.is_empty() || session.content.len() > 4096 {
        return Err(TermsError::Position);
    }
    let start_at = match tags::values(session, "start_at").as_slice() {
        [] => None,
        [value] => Some(decimal(value).ok_or(TermsError::StartAt)?),
        _ => return Err(TermsError::StartAt),
    };

    Ok(SessionTerms {
        id: session.id,
        game: game.to_owned(),
        rules,
        first,
        second,
        first_variant: first_variant.to_owned(),
        second_variant: second_variant.to_owned(),
        timing,
        time_control,
        position: session.content.clone(),
        start_at,
    })
}

/// `^[a-z][a-z0-9]{0,31}$`
pub(crate) fn is_identifier(s: &str) -> bool {
    let bytes = s.as_bytes();
    matches!(bytes.first(), Some(b'a'..=b'z'))
        && bytes.len() <= 32
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The NIP-13 leading zero bits of an event id.
fn leading_zero_bits(id: &EventId) -> u32 {
    let mut bits = 0u32;
    for byte in id.as_bytes() {
        if *byte == 0 {
            bits = bits.saturating_add(8);
        } else {
            bits = bits.saturating_add(byte.leading_zeros());
            break;
        }
    }
    bits
}

/// Whether the event carries exactly one `nonce` tag committing to a
/// difficulty its id achieves (NIP-13, as the suite's kinds require it).
#[must_use]
pub fn pow_ok(event: &Event) -> bool {
    let nonces: Vec<&[String]> = event
        .tags
        .iter()
        .map(Tag::as_slice)
        .filter(|s| s.first().map(String::as_str) == Some("nonce"))
        .collect();
    let [nonce] = nonces.as_slice() else {
        return false;
    };
    let Some(committed) = nonce.get(2).and_then(|text| text.parse::<u32>().ok()) else {
        return false;
    };
    leading_zero_bits(&event.id) >= committed
}

/// The transport half of a Ply's content constraint (kind `3423` §Content): a
/// non-empty string of at most 256 code points, free of C0/C1 controls and of
/// the bidirectional formatting characters U+202A–U+202E.
#[must_use]
pub fn content_ok(content: &str) -> bool {
    !content.is_empty()
        && content.chars().count() <= CONTENT_MAX_CODE_POINTS
        && content.chars().all(|c| {
            !matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}' | '\u{202A}'..='\u{202E}')
        })
}

/// A Ply of this session as the ABI's `ply` object, or `None` when the event
/// fails a consumer-side filter: not a Ply, not referencing this session, not
/// signed by a player, structurally non-conforming (one `game_session` `e`
/// tag and no other `e` tag, one `opponent` `p` tag naming the other player,
/// one positive-integer `step`, at most one bare `draw` tag, a content within
/// the transport constraints, a nonce its id honours), or a `step` beyond
/// `max_step`. The event's signature is the caller's to have verified.
#[must_use]
pub fn ply(event: &Event, terms: &SessionTerms, max_step: u32) -> Option<Value> {
    if event.kind != Kind::Custom(KIND_PLY) || !terms.is_player(&event.pubkey) {
        return None;
    }
    let sessions = tags::events_with_marker(event, "game_session");
    if sessions != [terms.id] || tags::count_named(event, "e") != 1 {
        return None;
    }
    let opponents = tags::pubkeys_with_role(event, "opponent");
    let [opponent] = opponents.as_slice() else {
        return None;
    };
    let other = if event.pubkey == terms.first {
        terms.second
    } else {
        terms.first
    };
    if *opponent != other || tags::count_named(event, "p") != 1 {
        return None;
    }
    let step = exactly_one(event, "step")
        .and_then(decimal)
        .filter(|step| *step >= 1)?;
    let step = u32::try_from(step).ok()?;
    if step > max_step {
        return None;
    }
    let draw = match tags::count_named(event, "draw") {
        0 => false,
        1 => {
            // Bare: no value beyond the tag name.
            let bare = event
                .tags
                .iter()
                .map(Tag::as_slice)
                .any(|s| s.first().map(String::as_str) == Some("draw") && s.len() == 1);
            if !bare {
                return None;
            }
            true
        }
        _ => return None,
    };
    if !content_ok(&event.content) || !pow_ok(event) {
        return None;
    }
    Some(json!({
        "id": event.id.to_hex(),
        "signer": event.pubkey.to_hex(),
        "session": terms.id.to_hex(),
        "step": step,
        "draw": draw,
        "content": event.content,
        "created_at": event.created_at.as_secs(),
    }))
}

/// A Conclusion of this session as the ABI's `conclusion` object — its claim
/// on the seat axis — or `None` when the event fails a consumer-side filter
/// (kind `3425` §Semantic constraints, items 1–7: one `game_session` `e` tag
/// and no other, the signer a player, two `player` `p` tags equal to the
/// players, seats mirroring the session's, two `result` tags in `^(100|[1-9]?[0-9])$`
/// summing to 100, a content in `^[a-z]{1,32}$`, a nonce its id honours).
#[must_use]
pub fn conclusion(event: &Event, terms: &SessionTerms) -> Option<Value> {
    if event.kind != Kind::Custom(KIND_CONCLUSION) || !terms.is_player(&event.pubkey) {
        return None;
    }
    if tags::events_with_marker(event, "game_session") != [terms.id]
        || tags::count_named(event, "e") != 1
    {
        return None;
    }
    let players: HashSet<PublicKey> = tags::pubkeys_with_role(event, "player")
        .into_iter()
        .collect();
    if players.len() != 2
        || tags::count_named(event, "p") != 2
        || !players.contains(&terms.first)
        || !players.contains(&terms.second)
    {
        return None;
    }
    if tags::count_named(event, "seat") != 2
        || tags::seat_for(event, &terms.first) != Some("first")
        || tags::seat_for(event, &terms.second) != Some("second")
    {
        return None;
    }
    if tags::count_named(event, "result") != 2 {
        return None;
    }
    let first = result_of(event, &terms.first)?;
    let second = result_of(event, &terms.second)?;
    if first.checked_add(second)? != 100 {
        return None;
    }
    let status = &event.content;
    if status.is_empty() || status.len() > 32 || !status.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    if !pow_ok(event) {
        return None;
    }
    Some(json!({
        "id": event.id.to_hex(),
        "signer": event.pubkey.to_hex(),
        "session": terms.id.to_hex(),
        "status": status,
        "result": { "first": first, "second": second },
        "created_at": event.created_at.as_secs(),
    }))
}

/// A player's `result` value: `^(100|[1-9]?[0-9])$`.
fn result_of(event: &Event, player: &PublicKey) -> Option<u32> {
    let raw = tags::result_for(event, player)?;
    let value = decimal(raw)?;
    u32::try_from(value).ok().filter(|v| *v <= 100)
}

/// An Event Timestamp Attestation as the ABI's `attestation` object, or `None`
/// when it is not one by the session's designated timestamper: in self-timed
/// mode there is none to offer.
#[must_use]
pub fn attestation(event: &Event, terms: &SessionTerms) -> Option<Value> {
    let Timing::Attested(timestamper) = &terms.timing else {
        return None;
    };
    if event.kind != Kind::Custom(KIND_ATTESTATION) || event.pubkey != *timestamper {
        return None;
    }
    let attests = tags::events_with_marker(event, "attests");
    let [attests] = attests.as_slice() else {
        return None;
    };
    Some(json!({
        "id": event.id.to_hex(),
        "signer": event.pubkey.to_hex(),
        "attests": attests.to_hex(),
        "created_at": event.created_at.as_secs(),
    }))
}

/// The session's events, as fetched from the relay and filtered, ready to be
/// offered to the module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Events {
    /// The Plies, as ABI objects.
    pub plies: Vec<Value>,
    /// The attestations by the designated timestamper (attested mode only).
    pub attestations: Vec<Value>,
    /// The Conclusions, as ABI objects, paired with their event ids.
    pub conclusions: Vec<(EventId, Value)>,
    /// The attestations' raw `(attests, created_at)` pairs by the designated
    /// timestamper — for t₀ (the Session Start Attestation) and the canonical
    /// Game Session of the slot, which the bot resolves itself.
    pub attested_at: Vec<(EventId, u64)>,
}

impl Events {
    /// Filters a batch of relay events for `terms`: each event's signature is
    /// verified again, then classified by kind and offered or dropped by the
    /// rules above.
    #[must_use]
    pub fn from_relay<'a>(
        events: impl IntoIterator<Item = &'a Event>,
        terms: &SessionTerms,
        max_step: u32,
    ) -> Self {
        let mut out = Self::default();
        let mut seen: HashSet<EventId> = HashSet::new();
        for event in events {
            if !seen.insert(event.id) || event.verify().is_err() {
                continue;
            }
            match event.kind.as_u16() {
                KIND_PLY => out.plies.extend(ply(event, terms, max_step)),
                KIND_CONCLUSION => {
                    out.conclusions
                        .extend(conclusion(event, terms).map(|c| (event.id, c)));
                }
                KIND_ATTESTATION => {
                    if let Some(att) = attestation(event, terms) {
                        if let Some(attests) = tags::events_with_marker(event, "attests").first() {
                            out.attested_at.push((*attests, event.created_at.as_secs()));
                        }
                        out.attestations.push(att);
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Signed suite events for the tests: a matchmade founding, its Game
    //! Session, Plies and Conclusions — in the reference module's encodings.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The initial position of a chess-vs-chess session (`describe.positions`
    /// `"chess/chess"` of the reference module).
    pub const CHESS_CHESS: &str =
        "-rnbqk^bn-r/+p+p+p+p+p+p+p+p/8/8/8/8/+P+P+P+P+P+P+P+P/-RNBQK^BN-R / W/w";

    /// The two players, the matchmaker, a timestamper and a rule-system id.
    pub struct World {
        pub first: Keys,
        pub second: Keys,
        pub matchmaker: Keys,
        pub timestamper: Keys,
        pub rules: EventId,
    }

    impl World {
        pub fn new() -> Self {
            Self {
                first: Keys::generate(),
                second: Keys::generate(),
                matchmaker: Keys::generate(),
                timestamper: Keys::generate(),
                rules: EventId::from_hex(&"7".repeat(64)).unwrap(),
            }
        }

        pub fn e(id: &EventId, marker: &str) -> Tag {
            Tag::parse(["e", &id.to_hex(), "", marker]).unwrap()
        }

        pub fn p(pubkey: &PublicKey, marker: &str) -> Tag {
            Tag::parse(["p", &pubkey.to_hex(), "", marker]).unwrap()
        }

        /// A self-timed Pairing of the two players (first/second as drawn).
        pub fn pairing(&self) -> Event {
            self.pairing_with(
                Timing::SelfTimed("wss://relay.example.com".to_owned()),
                "chess",
                "chess",
            )
        }

        pub fn pairing_with(&self, timing: Timing, fv: &str, sv: &str) -> Event {
            let mut tags = vec![
                Self::p(&self.first.public_key(), "player"),
                Self::p(&self.second.public_key(), "player"),
                Tag::parse(["game", "sanki"]).unwrap(),
                Self::e(&self.rules, "rules"),
                Tag::parse(["variant", &self.first.public_key().to_hex(), fv]).unwrap(),
                Tag::parse(["variant", &self.second.public_key().to_hex(), sv]).unwrap(),
                Tag::parse(["seat", &self.first.public_key().to_hex(), "first"]).unwrap(),
                Tag::parse(["seat", &self.second.public_key().to_hex(), "second"]).unwrap(),
                Tag::parse(["time_control", "300", "3"]).unwrap(),
                Tag::parse(["found_until", "2000000000"]).unwrap(),
            ];
            match timing {
                Timing::SelfTimed(relay) => {
                    tags.push(Tag::parse(["timing_relay", &relay]).unwrap())
                }
                Timing::Attested(ts) => tags.push(Self::p(&ts, "timestamper")),
            }
            EventBuilder::new(Kind::Custom(KIND_PAIRING), "")
                .tags(tags)
                .custom_created_at(Timestamp::from(1_700_000_000))
                .finalize(&self.matchmaker)
                .unwrap()
        }

        /// The Game Session founded by `first` on `pairing`, at `created_at`.
        pub fn session(&self, pairing: &Event, created_at: u64) -> Event {
            self.session_with(pairing, created_at, CHESS_CHESS, &self.first)
        }

        pub fn session_with(
            &self,
            pairing: &Event,
            created_at: u64,
            position: &str,
            founder: &Keys,
        ) -> Event {
            let mut tags = vec![
                Self::e(&pairing.id, "pairing"),
                Self::e(&self.rules, "rules"),
                Tag::parse(["game", "sanki"]).unwrap(),
                Self::p(&self.first.public_key(), "player"),
                Self::p(&self.second.public_key(), "player"),
                Tag::parse(["seat", &self.first.public_key().to_hex(), "first"]).unwrap(),
                Tag::parse(["seat", &self.second.public_key().to_hex(), "second"]).unwrap(),
            ];
            for tag in pairing.tags.iter() {
                let s = tag.as_slice();
                match s.first().map(String::as_str) {
                    Some("variant") => tags.push(tag.clone()),
                    Some("timing_relay") => tags.push(tag.clone()),
                    Some("p") if s.get(3).map(String::as_str) == Some("timestamper") => {
                        tags.push(tag.clone());
                    }
                    _ => {}
                }
            }
            EventBuilder::new(Kind::Custom(KIND_GAME_SESSION), position)
                .tags(tags)
                .custom_created_at(Timestamp::from(created_at))
                .finalize(founder)
                .unwrap()
        }

        /// A mined event: `builder` finalised with a nonce its id honours at
        /// difficulty 0 (every id achieves zero bits), so `pow_ok` holds.
        fn mined(builder: EventBuilder, signer: &Keys) -> Event {
            builder
                .tag(Tag::parse(["nonce", "0", "0"]).unwrap())
                .finalize(signer)
                .unwrap()
        }

        /// A Ply by `signer` at `step` with `content`, timed `created_at`.
        pub fn ply(
            &self,
            session: &EventId,
            signer: &Keys,
            step: u32,
            content: &str,
            created_at: u64,
        ) -> Event {
            let opponent = if signer.public_key() == self.first.public_key() {
                self.second.public_key()
            } else {
                self.first.public_key()
            };
            Self::mined(
                EventBuilder::new(Kind::Custom(KIND_PLY), content)
                    .tags(vec![
                        Self::e(session, "game_session"),
                        Self::p(&opponent, "opponent"),
                        Tag::parse(["step", &step.to_string()]).unwrap(),
                    ])
                    .custom_created_at(Timestamp::from(created_at)),
                signer,
            )
        }

        /// A Conclusion by `signer` claiming `status` and `(first, second)`.
        pub fn conclusion(
            &self,
            session: &EventId,
            signer: &Keys,
            status: &str,
            first: u32,
            second: u32,
            created_at: u64,
        ) -> Event {
            Self::mined(
                EventBuilder::new(Kind::Custom(KIND_CONCLUSION), status)
                    .tags(vec![
                        Self::e(session, "game_session"),
                        Self::p(&self.first.public_key(), "player"),
                        Self::p(&self.second.public_key(), "player"),
                        Tag::parse(["seat", &self.first.public_key().to_hex(), "first"]).unwrap(),
                        Tag::parse(["seat", &self.second.public_key().to_hex(), "second"]).unwrap(),
                        Tag::parse([
                            "result",
                            &self.first.public_key().to_hex(),
                            &first.to_string(),
                        ])
                        .unwrap(),
                        Tag::parse([
                            "result",
                            &self.second.public_key().to_hex(),
                            &second.to_string(),
                        ])
                        .unwrap(),
                    ])
                    .custom_created_at(Timestamp::from(created_at)),
                signer,
            )
        }

        /// An attestation of `attests` by the timestamper at `created_at`.
        pub fn attestation(&self, attests: &EventId, created_at: u64) -> Event {
            EventBuilder::new(Kind::Custom(KIND_ATTESTATION), "")
                .tags(vec![Self::e(attests, "attests")])
                .custom_created_at(Timestamp::from(created_at))
                .finalize(&self.timestamper)
                .unwrap()
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

    use super::fixtures::{World, CHESS_CHESS};
    use super::*;

    #[test]
    fn terms_of_a_matchmade_session() {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_700_000_100);
        let t = terms(&session, &pairing).unwrap();
        assert_eq!(t.id, session.id);
        assert_eq!(t.game, "sanki");
        assert_eq!(t.rules, w.rules);
        assert_eq!(t.first, w.first.public_key());
        assert_eq!(t.second, w.second.public_key());
        assert_eq!(t.pairing(), "chess/chess");
        assert_eq!(
            t.timing,
            Timing::SelfTimed("wss://relay.example.com".to_owned())
        );
        assert_eq!(t.time_control, vec![[Some(300), Some(3), None]]);
        assert_eq!(t.position, CHESS_CHESS);
        assert_eq!(t.start_at, None);
        let json = t.to_json(1_700_000_100);
        assert_eq!(json["timestamper"], Value::Null);
        assert_eq!(json["time_control"], json!([[300, 3, null]]));
        assert_eq!(json["start"], 1_700_000_100);
    }

    #[test]
    fn terms_refuse_a_session_disagreeing_with_its_founding() {
        let w = World::new();
        let pairing = w.pairing();
        // Another pairing (other seats) than the one referenced.
        let other = w.pairing_with(
            Timing::SelfTimed("wss://relay.example.com".to_owned()),
            "ogi",
            "ogi",
        );
        let session = w.session(&pairing, 1_700_000_100);
        assert_eq!(terms(&session, &other), Err(TermsError::Founding));
        // A session mirroring another rules reference than the founding's.
        let mut w2 = World::new();
        let p2 = w2.pairing();
        w2.rules = EventId::from_hex(&"8".repeat(64)).unwrap();
        let s2 = w2.session(&p2, 1_700_000_100);
        assert_eq!(terms(&s2, &p2), Err(TermsError::Rules));
        // A session founded by a stranger.
        let stranger = Keys::generate();
        let s3 = w.session_with(&pairing, 1_700_000_100, CHESS_CHESS, &stranger);
        assert_eq!(terms(&s3, &pairing), Err(TermsError::Players));
        // An empty position.
        let s4 = w.session_with(&pairing, 1_700_000_100, "", &w.first);
        assert_eq!(terms(&s4, &pairing), Err(TermsError::Position));
    }

    #[test]
    fn terms_of_an_attested_session() {
        let w = World::new();
        let pairing = w.pairing_with(Timing::Attested(w.timestamper.public_key()), "chess", "ogi");
        let session = w.session_with(&pairing, 1_700_000_100, "x", &w.second);
        let t = terms(&session, &pairing).unwrap();
        assert_eq!(t.timing, Timing::Attested(w.timestamper.public_key()));
        assert_eq!(t.pairing(), "chess/ogi");
        assert_eq!(
            t.to_json(5)["timestamper"],
            Value::from(w.timestamper.public_key().to_hex())
        );
    }

    #[test]
    fn plies_are_filtered_as_the_abi_prescribes() {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_700_000_100);
        let t = terms(&session, &pairing).unwrap();

        let ok = w.ply(
            &session.id,
            &w.first,
            1,
            r#"["e2","e4",null]"#,
            1_700_000_200,
        );
        let value = ply(&ok, &t, 300).unwrap();
        assert_eq!(value["step"], 1);
        assert_eq!(value["draw"], false);
        assert_eq!(value["signer"], w.first.public_key().to_hex());
        assert_eq!(value["created_at"], 1_700_000_200);

        // A stranger's Ply, a step beyond max_step, another session's Ply, an
        // empty content, a control character, an unmined Ply: dropped.
        let stranger = Keys::generate();
        let mut stranger_world = World::new();
        stranger_world.first = stranger;
        assert!(ply(
            &stranger_world.ply(&session.id, &stranger_world.first, 1, "x", 1),
            &t,
            300
        )
        .is_none());
        assert!(ply(&w.ply(&session.id, &w.first, 301, "x", 1), &t, 300).is_none());
        let other = EventId::from_hex(&"9".repeat(64)).unwrap();
        assert!(ply(&w.ply(&other, &w.first, 1, "x", 1), &t, 300).is_none());
        assert!(ply(&w.ply(&session.id, &w.first, 1, "", 1), &t, 300).is_none());
        assert!(ply(&w.ply(&session.id, &w.first, 1, "a\u{0007}b", 1), &t, 300).is_none());
        assert!(ply(&w.ply(&session.id, &w.first, 1, "a\u{202E}b", 1), &t, 300).is_none());
        let unmined = EventBuilder::new(Kind::Custom(KIND_PLY), "x")
            .tags(vec![
                World::e(&session.id, "game_session"),
                World::p(&w.second.public_key(), "opponent"),
                Tag::parse(["step", "1"]).unwrap(),
            ])
            .finalize(&w.first)
            .unwrap();
        assert!(ply(&unmined, &t, 300).is_none());
        // A draw flag with a value is malformed; a bare one is read.
        let flagged = EventBuilder::new(Kind::Custom(KIND_PLY), "x")
            .tags(vec![
                World::e(&session.id, "game_session"),
                World::p(&w.second.public_key(), "opponent"),
                Tag::parse(["step", "2"]).unwrap(),
                Tag::parse(["draw"]).unwrap(),
                Tag::parse(["nonce", "0", "0"]).unwrap(),
            ])
            .finalize(&w.first)
            .unwrap();
        assert_eq!(ply(&flagged, &t, 300).unwrap()["draw"], true);
    }

    #[test]
    fn conclusions_are_mapped_to_the_seat_axis() {
        let w = World::new();
        let pairing = w.pairing();
        let session = w.session(&pairing, 1_700_000_100);
        let t = terms(&session, &pairing).unwrap();
        let c = w.conclusion(&session.id, &w.second, "resignation", 100, 0, 1_700_001_000);
        let value = conclusion(&c, &t).unwrap();
        assert_eq!(value["status"], "resignation");
        assert_eq!(value["result"], json!({ "first": 100, "second": 0 }));
        assert_eq!(value["signer"], w.second.public_key().to_hex());
        // A bad sum, a bad status, a stranger: dropped.
        assert!(conclusion(
            &w.conclusion(&session.id, &w.second, "resignation", 60, 60, 1),
            &t
        )
        .is_none());
        assert!(conclusion(
            &w.conclusion(&session.id, &w.second, "Resign", 100, 0, 1),
            &t
        )
        .is_none());
        let mut sw = World::new();
        sw.first = w.first.clone();
        sw.second = w.second.clone();
        let stranger = Keys::generate();
        assert!(conclusion(
            &sw.conclusion(&session.id, &stranger, "resignation", 100, 0, 1),
            &t
        )
        .is_none());
    }

    #[test]
    fn attestations_count_only_by_the_designated_timestamper() {
        let w = World::new();
        let pairing = w.pairing_with(
            Timing::Attested(w.timestamper.public_key()),
            "chess",
            "chess",
        );
        let session = w.session(&pairing, 1_700_000_100);
        let t = terms(&session, &pairing).unwrap();
        let ply_event = w.ply(&session.id, &w.first, 1, "x", 1_700_000_200);
        let att = w.attestation(&ply_event.id, 1_700_000_201);
        let later = w.attestation(&ply_event.id, 1_700_000_250);
        let start = w.attestation(&session.id, 1_700_000_150);
        let mut stranger = World::new();
        stranger.timestamper = Keys::generate();
        let foreign = stranger.attestation(&ply_event.id, 1);
        let events = Events::from_relay([&ply_event, &att, &later, &start, &foreign], &t, 300);
        assert_eq!(events.plies.len(), 1);
        assert_eq!(events.attestations.len(), 3);
        assert_eq!(events.attested_at.len(), 3);
        // Self-timed: no attestation is offered at all, whoever signed it.
        let self_timed = w.pairing();
        let st = terms(&w.session(&self_timed, 1_700_000_100), &self_timed).unwrap();
        let events = Events::from_relay([&ply_event, &att, &start], &st, 300);
        assert!(events.attestations.is_empty());
        assert!(events.attested_at.is_empty());
    }

    #[test]
    fn pow_reads_the_committed_difficulty_against_the_id() {
        let w = World::new();
        let event = w.ply(
            &EventId::from_hex(&"1".repeat(64)).unwrap(),
            &w.first,
            1,
            "x",
            1,
        );
        assert!(pow_ok(&event)); // committed 0
        let demanding = EventBuilder::new(Kind::Custom(KIND_PLY), "x")
            .tag(Tag::parse(["nonce", "0", "64"]).unwrap())
            .finalize(&w.first)
            .unwrap();
        assert!(!pow_ok(&demanding)); // no id achieves 64 bits by luck
    }
}
