// SPDX-License-Identifier: Apache-2.0
//! The wire: SEI's JSON Lines (§5, §6, §9), read and written by the host.
//!
//! A request is `{"id", "op", …}`; an event is `{"ev", "re"?, …}`. The host
//! writes strictly and reads **tolerantly** (§6.4): an unknown `ev` is
//! ignored, unknown fields are ignored, an unknown `code` is `internal`. What
//! it does not tolerate is a line that is not an event at all, or an event
//! that breaks the execution model — those are protocol violations (§11),
//! which the runtime treats as an engine failure.

use serde_json::{Map, Value};

/// The largest line the host reads from an engine, in bytes. SEI §5 asks a
/// receiver to accept at least 1 MiB; beyond this bound the line is a
/// violation (an engine flooding its output).
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// An event's error `code` (§9.4), with the fallback of §9.5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Request malformed, out of domain, or not negotiated; an envelope error.
    Invalid,
    /// Illegal or non-canonical position or Move.
    Illegal,
    /// Rules or a pairing the engine does not implement.
    Unsupported,
    /// Engine failure; fatal without `re`.
    Internal,
}

impl Code {
    /// The code a `code` string names; an unknown one is `internal` (§9.5).
    #[must_use]
    pub fn parse(code: Option<&str>) -> Self {
        match code {
            Some("invalid") => Self::Invalid,
            Some("illegal") => Self::Illegal,
            Some("unsupported") => Self::Unsupported,
            _ => Self::Internal,
        }
    }

    /// The code's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Illegal => "illegal",
            Self::Unsupported => "unsupported",
            Self::Internal => "internal",
        }
    }
}

/// An attached or fatal error (§9.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    /// The code.
    pub code: Code,
    /// The JSON Pointer, when given.
    pub path: Option<String>,
    /// The engine's message, when given — for logs only (§9.4).
    pub message: Option<String>,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code.as_str())?;
        if let Some(path) = &self.path {
            write!(f, " at {}", escape(path))?;
        }
        if let Some(message) = &self.message {
            write!(f, ": {}", escape(message))?;
        }
        Ok(())
    }
}

/// What an event is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A terminal `done`, with its fields (`re` and `ev` removed).
    Done(Map<String, Value>),
    /// A provisional `info`, with its fields.
    Info(Map<String, Value>),
    /// An `error`.
    Error(EngineError),
    /// An `ev` this host does not know: ignored (§9.5).
    Unknown,
}

/// An event as read from the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The request it is attached to; `None` on a fatal error.
    pub re: Option<u64>,
    /// What it is.
    pub kind: Kind,
}

impl Event {
    /// Whether the event ends its request (§7, rule 4).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.kind, Kind::Done(_) | Kind::Error(_))
    }
}

/// Reads one line as an event.
///
/// # Errors
///
/// A description of the violation: the line is not a JSON object, has no
/// string `ev`, or its `re` is not a non-negative integer.
pub fn read_event(line: &str) -> Result<Event, String> {
    let value: Value =
        serde_json::from_str(line.trim_end_matches('\r')).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Object(mut fields) = value else {
        return Err("not a JSON object".to_owned());
    };
    let Some(ev) = fields.remove("ev") else {
        return Err("no `ev`".to_owned());
    };
    let Value::String(ev) = ev else {
        return Err("`ev` is not a string".to_owned());
    };
    let re = match fields.remove("re") {
        None => None,
        Some(Value::Number(n)) => Some(n.as_u64().ok_or("`re` is not a non-negative integer")?),
        Some(_) => return Err("`re` is not an integer".to_owned()),
    };
    let kind = match ev.as_str() {
        "done" => Kind::Done(fields),
        "info" => Kind::Info(fields),
        "error" => Kind::Error(EngineError {
            code: Code::parse(fields.get("code").and_then(Value::as_str)),
            path: fields
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_owned),
            message: fields
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        _ => Kind::Unknown,
    };
    Ok(Event { re, kind })
}

/// Writes a request line: `id` and `op` first, then `fields`.
#[must_use]
pub fn request_line(id: u64, op: &str, fields: Map<String, Value>) -> String {
    let mut object = Map::with_capacity(fields.len().saturating_add(2));
    object.insert("id".to_owned(), Value::from(id));
    object.insert("op".to_owned(), Value::from(op));
    object.extend(fields);
    let mut line = Value::Object(object).to_string();
    line.push('\n');
    line
}

/// A string from the engine, made safe for a log line (SEI §11: escape
/// every string coming from the engine): control characters escaped,
/// length bounded.
#[must_use]
pub fn escape(text: &str) -> String {
    const MAX: usize = 512;
    let mut out = String::with_capacity(text.len().min(MAX));
    for c in text.chars() {
        if out.len() >= MAX {
            out.push('…');
            break;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
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
    fn reads_the_three_events_and_ignores_the_unknown() {
        let done = read_event(r#"{"re":3,"ev":"done","best":"e2-e4"}"#).unwrap();
        assert_eq!(done.re, Some(3));
        assert!(done.is_terminal());
        assert!(matches!(&done.kind, Kind::Done(f) if f["best"] == "e2-e4"));
        let info = read_event("{\"re\":3,\"ev\":\"info\",\"depth\":1}\r").unwrap();
        assert!(!info.is_terminal());
        let error =
            read_event(r#"{"ev":"error","code":"weird","path":"/x","message":"m"}"#).unwrap();
        assert_eq!(error.re, None);
        assert!(matches!(
            error.kind,
            Kind::Error(EngineError {
                code: Code::Internal,
                ..
            })
        ));
        let unknown = read_event(r#"{"re":1,"ev":"progress"}"#).unwrap();
        assert_eq!(unknown.kind, Kind::Unknown);
    }

    #[test]
    fn violations() {
        assert!(read_event("not json").is_err());
        assert!(read_event("[]").is_err());
        assert!(read_event(r#"{"re":1}"#).is_err());
        assert!(read_event(r#"{"re":-1,"ev":"done"}"#).is_err());
        assert!(read_event(r#"{"re":"1","ev":"done"}"#).is_err());
    }

    #[test]
    fn writes_a_request_with_id_and_op_first() {
        let mut fields = Map::new();
        fields.insert("versions".to_owned(), serde_json::json!([1]));
        assert_eq!(
            request_line(1, "hello", fields),
            "{\"id\":1,\"op\":\"hello\",\"versions\":[1]}\n"
        );
    }

    #[test]
    fn escapes_what_the_engine_says() {
        assert_eq!(escape("ok\n\u{1b}[31m"), "ok\\n\\u{1b}[31m");
        assert!(escape(&"x".repeat(2000)).len() < 600);
    }
}
