//! Cadence — the four time-control families a Sashité client names a
//! founding by, and the one classifier that reads a founding's `time_control`
//! rows onto them (*Cadence — Sanki*, ADR-0039).
//!
//! The NIPs know periods, not cadences; the vocabulary is Sashité's, written
//! once in the suite's documents and derived here and in the app. The
//! category-G vectors (`conformance/cadence.json`, vendored from
//! `web-specs.md`) pin this module and the app's `cadence.ts` to the same
//! answers, so a persona's slot table and the lobby's tabs cannot disagree on
//! what a game is. Pure; no I/O.
//!
//! The fleet reads a cadence for two things: the per-family concurrency caps
//! (`[bot.play.max_concurrent]`, ADR-0014 §8 as amended) and the pool's
//! one-entry-per-cadence lock. Everything the four families do *not* decide —
//! which think distribution applies, whether a challenge may be accepted
//! outside the presence window — still turns on the one binary the families
//! subsume, [`Cadence::is_correspondence`].

/// One of the four families, in canonical order — shortest game first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cadence {
    /// A per-move budget and no main time.
    Byoyomi,
    /// A main time of five minutes or less.
    Blitz,
    /// A main time of more than five minutes, at the board.
    Rapid,
    /// A move may legitimately take a day or more.
    Correspondence,
}

/// One day, in seconds: the correspondence boundary (*Cadence — Sanki* §The rules).
const ONE_DAY: u64 = 86_400;

/// Five minutes, in seconds: the blitz/rapid boundary (*Cadence — Sanki* §The rules).
const FIVE_MINUTES: u64 = 300;

impl Cadence {
    /// The four families, in canonical order.
    pub const ALL: [Cadence; 4] = [
        Cadence::Byoyomi,
        Cadence::Blitz,
        Cadence::Rapid,
        Cadence::Correspondence,
    ];

    /// The family's token — lowercase ASCII, no macron: the app's URL segment
    /// and the fleet file's configuration key.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Cadence::Byoyomi => "byoyomi",
            Cadence::Blitz => "blitz",
            Cadence::Rapid => "rapid",
            Cadence::Correspondence => "correspondence",
        }
    }

    /// Whether the family is correspondence-paced: the binary that still
    /// governs the think distribution and out-of-window acceptance (ADR-0014
    /// §6.3, §6.7).
    #[must_use]
    pub const fn is_correspondence(self) -> bool {
        matches!(self, Cadence::Correspondence)
    }

    /// The ordered rule list of *Cadence — Sanki* §The rules, over a
    /// **well-formed** period: `d` its duration, `i` its increment (absent
    /// reads as `0`). Plies never enter. First match wins.
    #[must_use]
    pub const fn of_period(duration: u64, increment: u64) -> Cadence {
        if duration >= ONE_DAY || increment >= ONE_DAY {
            Cadence::Correspondence
        } else if duration == 0 {
            Cadence::Byoyomi
        } else if duration <= FIVE_MINUTES {
            Cadence::Blitz
        } else {
            Cadence::Rapid
        }
    }

    /// The cadence of a founding, from its `time_control` rows in tag order
    /// (each row the tag's values, without the tag name — the shape
    /// `tags::time_control_rows` yields): the **first** period's, or `None`
    /// when that period is absent or malformed (*Cadence — Sanki* §Which
    /// period, §Well-formedness). Later rows never enter.
    #[must_use]
    pub fn of_rows(rows: &[Vec<String>]) -> Option<Cadence> {
        let first = rows.first()?;
        let (duration, increment) = parse_period(first)?;
        Some(Cadence::of_period(duration, increment))
    }
}

/// A well-formed period's `(duration, increment)` — increment `0` when
/// absent — or `None` when the row is malformed by kind `3420` §Match-terms
/// tags: one to three values, each a bare decimal integer with no leading
/// zero (an empty element is malformed, not absent); `plies` strictly
/// positive; a `duration` of `0` only in the three-element per-move form.
fn parse_period(row: &[String]) -> Option<(u64, u64)> {
    if row.is_empty() || row.len() > 3 {
        return None;
    }
    // Each present element must be well-formed; an absent one is `None`.
    let mut values = row.iter().map(|raw| bare_integer(raw));
    let duration = values.next()??;
    let increment = match values.next() {
        None => None,
        Some(parsed) => Some(parsed?),
    };
    let plies = match values.next() {
        None => None,
        Some(parsed) => Some(parsed?),
    };
    if plies.is_some_and(|quota| quota == 0) {
        return None;
    }
    if duration == 0 && plies.is_none() {
        return None;
    }
    Some((duration, increment.unwrap_or(0)))
}

/// `^(0|[1-9][0-9]*)$`, into a `u64`. A well-formed value too large for a
/// `u64` **saturates** rather than being called malformed: the format admits
/// any length, every rule of the classifier compares against a day, and the
/// app's `Number` reads the same digits as a large float — so both say
/// `correspondence` (vector `cadence.boundary.huge-bank`).
fn bare_integer(raw: &str) -> Option<u64> {
    let bytes = raw.as_bytes();
    let well_formed = match bytes {
        [] => false,
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if !well_formed {
        return None;
    }
    Some(raw.parse().unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde::Deserialize;

    fn rows(spec: &[&[&str]]) -> Vec<Vec<String>> {
        spec.iter()
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect()
    }

    #[test]
    fn the_four_presets() {
        assert_eq!(
            Cadence::of_rows(&rows(&[&["0", "10", "1"]])),
            Some(Cadence::Byoyomi)
        );
        assert_eq!(
            Cadence::of_rows(&rows(&[&["180", "2"]])),
            Some(Cadence::Blitz)
        );
        assert_eq!(Cadence::of_rows(&rows(&[&["600"]])), Some(Cadence::Rapid));
        assert_eq!(
            Cadence::of_rows(&rows(&[&["0", "259200", "1"]])),
            Some(Cadence::Correspondence)
        );
    }

    #[test]
    fn tokens_and_order() {
        let tokens: Vec<&str> = Cadence::ALL.iter().map(|c| c.token()).collect();
        assert_eq!(tokens, ["byoyomi", "blitz", "rapid", "correspondence"]);
        assert!(Cadence::Correspondence.is_correspondence());
        assert!(!Cadence::Rapid.is_correspondence());
    }

    #[test]
    fn malformed_rows_have_no_cadence() {
        for bad in [
            &["0", "10"][..],
            &["0"],
            &["0300"],
            &["five"],
            &[""],
            &["300", "", "1"],
            &["300", "3", "0"],
            &["300", "3", "1", "2"],
            &[],
        ] {
            assert_eq!(Cadence::of_rows(&rows(&[bad])), None, "{bad:?}");
        }
        assert_eq!(Cadence::of_rows(&[]), None);
    }

    #[test]
    fn a_value_beyond_u64_saturates_to_correspondence() {
        // Well-formed, unrepresentable: the app reads 1e23 and says
        // correspondence; so do we, by saturation, never by "malformed".
        assert_eq!(
            Cadence::of_rows(&rows(&[&["99999999999999999999999"]])),
            Some(Cadence::Correspondence)
        );
        assert_eq!(
            Cadence::of_rows(&rows(&[&["300", "99999999999999999999999"]])),
            Some(Cadence::Correspondence)
        );
        // A plies quota beyond u64 is still strictly positive: well-formed.
        assert_eq!(
            Cadence::of_rows(&rows(&[&["0", "10", "99999999999999999999999"]])),
            Some(Cadence::Byoyomi)
        );
    }

    /// The shared category-G corpus (`conformance/cadence.json`, vendored
    /// from `web-specs.md`): the same file the app's `cadence.spec.ts` runs.
    #[derive(Deserialize)]
    struct Corpus {
        version: u32,
        category: String,
        vectors: Vec<Vector>,
    }

    #[derive(Deserialize)]
    struct Vector {
        id: String,
        #[serde(default)]
        note: String,
        #[serde(rename = "timeControls")]
        time_controls: Vec<Vec<String>>,
        cadence: Option<String>,
    }

    #[test]
    fn category_g_vectors() {
        let corpus: Corpus =
            serde_json::from_str(include_str!("../conformance/cadence.json")).unwrap();
        assert_eq!(corpus.category, "cadence");
        assert_eq!(corpus.version, 1);
        assert!(!corpus.vectors.is_empty());
        for vector in &corpus.vectors {
            let got = Cadence::of_rows(&vector.time_controls).map(Cadence::token);
            assert_eq!(
                got,
                vector.cadence.as_deref(),
                "{}: {}",
                vector.id,
                vector.note
            );
        }
    }
}
