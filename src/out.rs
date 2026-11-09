//! Results: every command ends in rows `{status, code, …}` (ported from iterm_api's `model.rs`).
//! Text output is one line per row (`ok node …`, nodes as `p p12* name k=v`); `--json` prints the
//! rows as a JSON array, each carrying `schema_version`. The worst status sets the exit code.

use serde_json::{json, Map, Value};

pub type Obj = Map<String, Value>;

pub const OK: &str = "ok";
pub const PLANNED: &str = "planned";
pub const REFUSED: &str = "refused";
pub const TIMEOUT: &str = "timeout";
pub const FAILED: &str = "failed";

/// `obj!{"pane" => x, "name" => y}`: a detail map in insertion order.
#[macro_export]
macro_rules! obj {
    () => { $crate::out::Obj::new() };
    ($($k:expr => $v:expr),+ $(,)?) => {{
        let mut m = $crate::out::Obj::new();
        $( m.insert(($k).to_string(), serde_json::json!($v)); )+
        m
    }};
}

/// Typed failures; each becomes one row.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Bad arguments or a selector that names nothing.
    Invalid(String),
    /// The CRUD gate or another rule refused (a code such as `not-descendant`, `needs-apply`).
    Policy { code: String, msg: String },
    Timeout(String),
    /// The app or the system failed.
    Failed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn invalid(m: impl Into<String>) -> Error {
        Error::Invalid(m.into())
    }
    pub fn policy(code: &str, m: impl Into<String>) -> Error {
        Error::Policy { code: code.into(), msg: m.into() }
    }
    pub fn failed(m: impl Into<String>) -> Error {
        Error::Failed(m.into())
    }
    pub fn timeout(m: impl Into<String>) -> Error {
        Error::Timeout(m.into())
    }
    /// The app gave no answer in time: asking it again now only waits again.
    pub fn unanswered(&self) -> bool {
        matches!(self, Error::Timeout(_)) || matches!(self, Error::Failed(m) if m.contains("not responding"))
    }
    pub fn message(&self) -> &str {
        match self {
            Error::Invalid(m) | Error::Timeout(m) | Error::Failed(m) => m,
            Error::Policy { msg, .. } => msg,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl From<crate::app::Fail> for Error {
    fn from(f: crate::app::Fail) -> Error {
        use crate::app::FailCode as C;
        match f.code {
            C::Auth => Error::policy("auth", f.msg),
            C::Unsupported => Error::policy("unsupported", f.msg),
            C::NotFound | C::Invalid => Error::Invalid(f.msg),
            C::Internal => Error::Failed(f.msg),
            C::Timeout => Error::Timeout(f.msg),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
            return Error::Timeout(e.to_string());
        }
        Error::Failed(e.to_string())
    }
}

pub fn exit_code_of(status: &str) -> i32 {
    match status {
        FAILED => 1,
        REFUSED => 2,
        TIMEOUT => 3,
        _ => 0,
    }
}

/// `{status, code, …rest}`.
pub fn ok(code: &str, rest: Obj) -> Obj {
    let mut out = obj! {"status" => OK, "code" => code};
    for (k, v) in rest {
        if k != "status" && k != "code" {
            out.insert(k, v);
        }
    }
    out
}

/// A dry run: what would happen, `(drop --plan to run it)`.
pub fn planned(code: &str, rest: Obj) -> Obj {
    let mut o = ok(code, rest);
    o.insert("status".into(), json!(PLANNED));
    o
}

pub fn from_error(err: &Error, detail: Obj) -> Obj {
    let (status, code) = match err {
        Error::Policy { code, .. } => (REFUSED, code.as_str()),
        Error::Timeout(_) => (TIMEOUT, "timeout"),
        Error::Invalid(_) | Error::Failed(_) => (FAILED, "error"),
    };
    let mut out = obj! {"status" => status, "code" => code};
    for (k, v) in detail {
        out.insert(k, v);
    }
    out.insert("reason".into(), json!(err.message()));
    out
}

/// Worst status wins: failed 1, then refused 2, then timeout 3, else 0.
pub fn exit_code(rows: &[Obj]) -> i32 {
    let has = |s: &str| rows.iter().any(|r| r.get("status").and_then(Value::as_str) == Some(s));
    for s in [FAILED, REFUSED, TIMEOUT] {
        if has(s) {
            return exit_code_of(s);
        }
    }
    0
}

fn plain_word(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c == '"' || c == '=' || c == '\\')
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        Value::Null => String::new(),
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

/// A value as `line` prints it: quoted (JSON) when it holds spaces, quotes, `=` or backslashes.
pub fn render_value(v: &Value) -> String {
    let t = scalar(v);
    if plain_word(&t) { t } else { serde_json::to_string(&Value::String(t)).unwrap_or_default() }
}

/// Keys kept for code callers but left out of the one-line form.
const HIDDEN: &[&str] = &["status", "code", "text", "reason", "lines"];
/// Rows whose `text` is what they are for (a message's body, a found line): shown last, before the reason.
const TEXT_ROWS: &[&str] = &["message", "found"];

/// `<status> <code> key=value…`: scalars only, reason last, `(drop --plan to run it)` for a dry run.
pub fn line(o: &Obj) -> String {
    let get = |k: &str| o.get(k).and_then(Value::as_str);
    let mut parts = vec![get("status").unwrap_or(FAILED).to_string(), get("code").unwrap_or("error").to_string()];
    for (k, v) in o {
        if HIDDEN.contains(&k.as_str()) || v.is_null() || v.is_object() {
            continue;
        }
        if let Value::Array(a) = v {
            if !a.iter().all(|x| x.is_string() || x.is_number() || x.is_boolean()) {
                continue;
            }
        }
        parts.push(format!("{k}={}", render_value(v)));
    }
    if let Some(t) = get("text").filter(|_| get("code").is_some_and(|c| TEXT_ROWS.contains(&c))) {
        parts.push(format!("text={}", render_value(&json!(t))));
    }
    if let Some(r) = get("reason").filter(|r| !r.is_empty()) {
        parts.push(format!("reason={}", render_value(&json!(r))));
    }
    if get("status") == Some(PLANNED) {
        parts.push("(drop --plan to run it)".into());
    }
    parts.join(" ")
}

/// Rows as text: nodes as `p p12* name k=v`, read text as its lines, everything else as `line`.
pub fn render(rows: &[Obj]) -> String {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let get = |k: &str| r.get(k).and_then(Value::as_str);
        if let (Some("ok"), Some("text")) = (get("status"), get("code")) {
            if let Some(Value::Array(lines)) = r.get("lines") {
                out.extend(lines.iter().map(scalar));
            } else if let Some(t) = get("text") {
                out.push(t.to_string());
            }
            continue;
        }
        if get("code") != Some("node") {
            out.push(line(r));
            continue;
        }
        let kind = get("node").unwrap_or("");
        let mut s = format!("{} {}", kind.get(..1).unwrap_or(""), get("id").unwrap_or(""));
        if r.get("focused") == Some(&json!(true)) {
            s.push('*');
        }
        if let Some(n) = get("name").filter(|n| !n.is_empty()) {
            s.push(' ');
            s.push_str(&if n.contains(' ') { format!("{n:?}") } else { n.to_string() });
        }
        if r.get("me") == Some(&json!(true)) {
            s.push_str(" me");
        }
        for (k, v) in r {
            if ["status", "code", "node", "id", "name", "focused", "me", "parent", "layout"].contains(&k.as_str()) {
                continue;
            }
            if v.is_null() || v.is_object() || v.as_str() == Some("") {
                continue;
            }
            s.push_str(&format!(" {k}={}", render_value(v)));
        }
        out.push(s);
    }
    out.join("\n")
}

/// `--json`: the rows as one array, each with `schema_version`.
pub fn json_rows(rows: Vec<Obj>) -> String {
    let v: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            r.insert("schema_version".into(), json!(crate::grammar::SCHEMA_VERSION));
            Value::Object(r)
        })
        .collect();
    Value::Array(v).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // regression: the adapter's silence is Timeout, but only a "not responding" Failed counted: a stuck app cost
    // up to 20 s for a list bounded at 2.5 s, and a notice that never landed read as a failure
    #[test]
    fn silence_counts_as_unanswered() {
        assert!(Error::Timeout("iTerm2 did not answer in time".into()).unanswered());
        assert!(Error::Failed("iTerm2 is not responding".into()).unanswered());
        assert!(!Error::Failed("the pane is gone".into()).unanswered());
        assert!(!Error::Invalid("x".into()).unanswered());
    }
}
