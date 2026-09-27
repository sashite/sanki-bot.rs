// SPDX-License-Identifier: Apache-2.0
//! What an engine announces in the `done` of `hello` (SEI §8.1, §8.3, §10),
//! read tolerantly: a field of the wrong shape is read as absent, an option
//! of an unknown `type` is left aside (§9.5). The checks a host makes on the
//! announcement — the rules and pairings it needs, the options it will
//! configure — are here too; their verdict is the probe's (ADR-0045 §3).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

/// The rules identifier the bot plays under (*SEI Rules Document — Sanki*).
pub const RULES: &str = "sashite.sanki.kernel/1";

/// The features of §10 the host reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Features {
    /// `lines`: the largest `search.lines`.
    pub lines: Option<u64>,
    /// `strength`: the domain of `search.strength.elo`.
    pub strength: Option<(i64, i64)>,
    /// `roots`: the host may restrict the search.
    pub roots: bool,
    /// `advice`: `done.advice` may be given.
    pub advice: bool,
}

/// An option's descriptor (§8.3).
#[derive(Debug, Clone, PartialEq)]
pub enum OptionSpec {
    /// A boolean.
    Bool {
        /// Its default.
        default: bool,
    },
    /// An integer within `[min, max]`.
    Int {
        /// Its default.
        default: i64,
        /// The least value.
        min: i64,
        /// The greatest value.
        max: i64,
    },
    /// A string, within `values` when given.
    String {
        /// Its default.
        default: String,
        /// The allowed values, when restricted.
        values: Option<Vec<String>>,
    },
}

impl OptionSpec {
    /// Whether `value` is in the option's domain.
    #[must_use]
    pub fn admits(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Bool { .. }, Value::Bool(_)) => true,
            (Self::Int { min, max, .. }, Value::Number(n)) => {
                n.as_i64().is_some_and(|n| n >= *min && n <= *max)
            }
            (Self::String { values, .. }, Value::String(s)) => values
                .as_ref()
                .is_none_or(|values| values.iter().any(|v| v == s)),
            _ => false,
        }
    }
}

/// The engine as it announces itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Announcement {
    /// The common version, when there is one (§8.1 *Success*).
    pub version: Option<u64>,
    /// `engine.name`, when given.
    pub name: Option<String>,
    /// `engine.version`, when given.
    pub engine_version: Option<String>,
    /// The rules identifiers, each with its pairings — `None` for every
    /// pairing the rules document defines.
    pub rules: BTreeMap<String, Option<BTreeSet<String>>>,
    /// The features.
    pub features: Features,
    /// The options the engine accepts.
    pub options: BTreeMap<String, OptionSpec>,
}

impl Announcement {
    /// Reads the fields of a `hello`'s `done`.
    #[must_use]
    pub fn read(fields: &Map<String, Value>) -> Self {
        let engine = fields.get("engine").and_then(Value::as_object);
        let mut rules = BTreeMap::new();
        if let Some(object) = fields.get("rules").and_then(Value::as_object) {
            for (id, spec) in object {
                // Absent: every pairing. Present: the strings it lists — a
                // malformed list claims nothing, never everything.
                let pairings = spec
                    .as_object()
                    .and_then(|spec| spec.get("pairings"))
                    .map(|list| {
                        list.as_array()
                            .map(|list| {
                                list.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect::<BTreeSet<String>>()
                            })
                            .unwrap_or_default()
                    });
                rules.insert(id.clone(), pairings);
            }
        }
        let features_object = fields.get("features").and_then(Value::as_object);
        let feature = |name: &str| features_object.and_then(|f| f.get(name));
        let features = Features {
            lines: feature("lines")
                .and_then(Value::as_object)
                .and_then(|p| p.get("max"))
                .and_then(Value::as_u64),
            strength: feature("strength")
                .and_then(Value::as_object)
                .and_then(|p| {
                    Some((
                        p.get("min").and_then(Value::as_i64)?,
                        p.get("max").and_then(Value::as_i64)?,
                    ))
                }),
            roots: feature("roots").is_some(),
            advice: feature("advice").is_some(),
        };
        let mut options = BTreeMap::new();
        if let Some(object) = fields.get("options").and_then(Value::as_object) {
            for (name, descriptor) in object {
                if let Some(spec) = read_option(descriptor) {
                    options.insert(name.clone(), spec);
                }
            }
        }
        Self {
            version: fields.get("version").and_then(Value::as_u64),
            name: engine
                .and_then(|e| e.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            engine_version: engine
                .and_then(|e| e.get("version"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            rules,
            features,
            options,
        }
    }

    /// Whether the engine plays `pairing` (two uppercase SIN letters, the
    /// first seat's style then the second's) under [`RULES`].
    #[must_use]
    pub fn plays(&self, pairing: &str) -> bool {
        match self.rules.get(RULES) {
            None => false,
            Some(None) => true,
            Some(Some(pairings)) => pairings.contains(pairing),
        }
    }

    /// The gaps between what the host needs and what the engine announces
    /// (ADR-0045 §3 *The probe*): every pairing of `pairings` under
    /// [`RULES`]; every option of `options` announced and in its domain;
    /// `strength` only with the feature and within its domain. Empty when
    /// the engine will do.
    #[must_use]
    pub fn gaps(
        &self,
        pairings: &BTreeSet<String>,
        options: &BTreeMap<String, Value>,
        strength: Option<i64>,
    ) -> Vec<String> {
        let mut gaps = Vec::new();
        match self.version {
            Some(1) => {}
            Some(other) => gaps.push(format!(
                "the engine answered SEI version {other} to versions [1]"
            )),
            None => gaps.push("no common SEI version".to_owned()),
        }
        if !self.rules.contains_key(RULES) {
            gaps.push(format!("the rules {RULES} are not announced"));
        } else {
            for pairing in pairings {
                if !self.plays(pairing) {
                    gaps.push(format!("the pairing {pairing} is not announced"));
                }
            }
        }
        for (name, value) in options {
            match self.options.get(name) {
                None => gaps.push(format!("the option {name} is not announced")),
                Some(spec) if !spec.admits(value) => {
                    gaps.push(format!("the option {name} = {value} is out of its domain"));
                }
                Some(_) => {}
            }
        }
        if let Some(elo) = strength {
            match self.features.strength {
                None => gaps.push("strength is set without the feature".to_owned()),
                Some((min, max)) if elo < min || elo > max => {
                    gaps.push(format!("strength {elo} is outside [{min}, {max}]"));
                }
                Some(_) => {}
            }
        }
        gaps
    }
}

/// Reads an option descriptor; `None` for an unknown or malformed one.
fn read_option(descriptor: &Value) -> Option<OptionSpec> {
    let object = descriptor.as_object()?;
    match object.get("type").and_then(Value::as_str)? {
        "bool" => Some(OptionSpec::Bool {
            default: object.get("default").and_then(Value::as_bool)?,
        }),
        "int" => {
            let default = object.get("default").and_then(Value::as_i64)?;
            let min = object.get("min").and_then(Value::as_i64)?;
            let max = object.get("max").and_then(Value::as_i64)?;
            (min <= default && default <= max).then_some(OptionSpec::Int { default, min, max })
        }
        "string" => {
            let default = object.get("default").and_then(Value::as_str)?.to_owned();
            let values = match object.get("values") {
                None => None,
                Some(list) => {
                    let values: Vec<String> = list
                        .as_array()?
                        .iter()
                        .map(|v| v.as_str().map(str::to_owned))
                        .collect::<Option<_>>()?;
                    if values.is_empty() || !values.contains(&default) {
                        return None;
                    }
                    Some(values)
                }
            };
            Some(OptionSpec::String { default, values })
        }
        _ => None,
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
    use serde_json::json;

    fn hello() -> Map<String, Value> {
        json!({
            "version": 1, "versions": [1],
            "engine": {"name": "Example", "version": "0.1.0"},
            "rules": {"sashite.sanki.kernel/1": {"pairings": ["WW", "WJ", "JW", "JJ"]}},
            "features": {"lines": {"max": 64}, "roots": {}, "strength": {"min": 800, "max": 2400}},
            "options": {
                "threads": {"type": "int", "default": 1, "min": 1, "max": 64},
                "book": {"type": "bool", "default": false},
                "style": {"type": "string", "default": "solid", "values": ["solid", "wild"]},
                "weird": {"type": "float", "default": 1.5}
            }
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn reads_the_announcement() {
        let a = Announcement::read(&hello());
        assert_eq!(a.version, Some(1));
        assert_eq!(a.name.as_deref(), Some("Example"));
        assert!(a.plays("WJ") && !a.plays("CC"));
        assert_eq!(a.features.lines, Some(64));
        assert_eq!(a.features.strength, Some((800, 2400)));
        assert!(a.features.roots && !a.features.advice);
        assert_eq!(a.options.len(), 3, "the float option is left aside");
        assert!(a.options["threads"].admits(&json!(4)));
        assert!(!a.options["threads"].admits(&json!(65)));
        assert!(!a.options["threads"].admits(&json!("4")));
        assert!(a.options["style"].admits(&json!("wild")));
        assert!(!a.options["style"].admits(&json!("mad")));
        assert!(a.options["book"].admits(&json!(true)));
    }

    #[test]
    fn absent_pairings_means_every_pairing_and_malformed_ones_none() {
        let a = Announcement::read(
            json!({"version": 1, "rules": {"sashite.sanki.kernel/1": {}}})
                .as_object()
                .unwrap(),
        );
        assert!(a.plays("CC") && a.plays("JW"));
        let bad = Announcement::read(
            json!({"version": 1, "rules": {"sashite.sanki.kernel/1": {"pairings": "all"}}})
                .as_object()
                .unwrap(),
        );
        assert!(!bad.plays("WW"));
        let wrong_version = Announcement::read(
            json!({"version": 7, "rules": {"sashite.sanki.kernel/1": {}}})
                .as_object()
                .unwrap(),
        );
        assert_eq!(
            wrong_version
                .gaps(&BTreeSet::new(), &BTreeMap::new(), None)
                .len(),
            1
        );
    }

    #[test]
    fn gaps_name_what_is_missing() {
        let a = Announcement::read(&hello());
        let needs: BTreeSet<String> = ["WW", "CC"].iter().map(|s| (*s).to_owned()).collect();
        let mut options = BTreeMap::new();
        options.insert("threads".to_owned(), json!(200));
        options.insert("hash".to_owned(), json!(16));
        let gaps = a.gaps(&needs, &options, Some(3000));
        assert_eq!(gaps.len(), 4, "{gaps:?}");
        assert!(gaps.iter().any(|g| g.contains("CC")));
        assert!(gaps.iter().any(|g| g.contains("hash")));
        assert!(gaps.iter().any(|g| g.contains("threads = 200")));
        assert!(gaps.iter().any(|g| g.contains("strength 3000")));
        // A fitting configuration has no gap.
        let mut fine = BTreeMap::new();
        fine.insert("threads".to_owned(), json!(2));
        let ww: BTreeSet<String> = ["WW".to_owned()].into_iter().collect();
        assert!(a.gaps(&ww, &fine, Some(1500)).is_empty());
        // No common version, no rules.
        let none = Announcement::read(json!({"versions": [2]}).as_object().unwrap());
        assert_eq!(none.gaps(&ww, &BTreeMap::new(), None).len(), 2);
    }
}
