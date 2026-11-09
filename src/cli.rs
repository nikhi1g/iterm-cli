//! The command line: `iterm <noun> <verb> …`, the old commands as aliases, `batch`,
//! `children`, `help`, `coverage --json`, `version`. One connection per command (a batch shares it).

use std::collections::HashSet;
use std::io::Write;

use crate::conn::{Conn, Env};
use crate::exec::Exec;
use crate::grammar::{self, Expr, Noun};
use crate::out::{self, from_error, ok, Error, Obj};

/// Shell-like words: whitespace splits, '…' and "…" quote, backslash escapes outside '…'.
pub fn shell_words(line: &str) -> Result<Vec<String>, String> {
    let (mut out, mut cur, mut any) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                any = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("unterminated '".into()),
                    }
                }
            }
            '"' => {
                any = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err("unterminated \"".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("unterminated \"".into()),
                    }
                }
            }
            '\\' => {
                any = true;
                if let Some(x) = chars.next() {
                    cur.push(x);
                }
            }
            c if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            '#' if cur.is_empty() && !any => break,
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// The arguments as text, or the one-line error for one that isn't.
pub fn text_args(args: impl Iterator<Item = std::ffi::OsString>) -> std::result::Result<Vec<String>, String> {
    args.map(|a| a.into_string().map_err(|bad| out::render(&[from_error(&Error::invalid(format!("argument {:?} is not UTF-8", bad)), crate::obj! {})]))).collect()
}

/// An old command and its words in the v2 form (`spawn --task T` → `pane new --agent --task T`).
fn alias(words: &[String]) -> Option<Vec<String>> {
    let first = words.first()?;
    let (_, to) = grammar::ALIASES.iter().find(|(a, _)| a == first)?;
    let mut out: Vec<String> = to.split_whitespace().map(String::from).collect();
    out.extend(words[1..].iter().cloned());
    Some(out)
}

pub const USAGE_EXTRA: &str = "\
other    guide [TOPIC] (start here: what iTerm2 is, the commands, when to delegate) · batch [--file F] [--keep-going] · children [--wait] [--timeout S] · plan run FILE.json [--detach] | status [NAME] | cancel NAME · coverage --json · version · help [NOUN]";

enum Out {
    Rows(Vec<Obj>),
    Text(String),
}

/// One request line as a typed request: `agent spawn` is `agent new`; a noun with a verb is v2 (`key set`), else
/// an old command of that name (`key --key esc`, [`alias`]).
fn parse_line(words: &[String]) -> Result<grammar::Request, Error> {
    let spawn: Vec<String>;
    let words = if words.len() > 1 && words[0] == "agent" && words[1] == "spawn" {
        spawn = std::iter::once("agent".to_string()).chain(std::iter::once("new".to_string())).chain(words[2..].iter().cloned()).collect();
        &spawn[..]
    } else {
        words
    };
    let words = if grammar::is_v2(words).is_some() { words.to_vec() } else { alias(words).unwrap_or_else(|| words.to_vec()) };
    let noun = words.first().and_then(|w| Noun::parse(w)).ok_or_else(|| Error::invalid(format!("unknown command '{}'; iterm help", words.first().map(String::as_str).unwrap_or(""))))?;
    grammar::parse(noun, &words[1..]).map_err(|e| Error::invalid(e.to_string()))
}

/// `batch`: one request per line, every line parsed and checked before the first runs (`$NAME` must be bound by
/// an earlier line's `--as NAME`). A line's `--as` binds what it created. The first failed line stops the rest,
/// unless `keep_going`.
pub(crate) fn run_batch(x: &mut Exec<'_>, text: &str, keep_going: bool) -> Vec<Obj> {
    let reqs = match parse_batch(text) {
        Ok(reqs) => reqs,
        Err(row) => return vec![row],
    };
    let mut rows = Vec::new();
    for (n, r) in reqs {
        let got = x.run(&r).unwrap_or_else(|e| vec![from_error(&e, crate::obj! {"line" => n})]);
        let bad = out::exit_code(&got) != 0;
        if let Some(b) = &r.bind {
            x.binds.insert(b.clone(), x.created.clone());
        }
        rows.extend(got);
        if bad && !keep_going {
            rows.push(ok("stopped", crate::obj! {"line" => n, "reason" => "a line failed (--keep-going runs the rest)"}));
            break;
        }
    }
    rows
}

/// A batch's lines as requests, each with its line number; the first bad line is the one error row.
fn parse_batch(text: &str) -> Result<Vec<(usize, grammar::Request)>, Obj> {
    let mut reqs = Vec::new();
    let mut bound: HashSet<String> = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let at = crate::obj! {"line" => n + 1};
        let words = match shell_words(line) {
            Ok(w) if w.is_empty() => continue,
            Ok(w) => w,
            Err(e) => return Err(from_error(&Error::invalid(format!("line {}: {e}", n + 1)), at)),
        };
        let r = parse_line(&words).map_err(|e| from_error(&e, at.clone()))?;
        if let Some(u) = binds_used(&r).iter().find(|u| !bound.contains(*u)) {
            return Err(from_error(&Error::invalid(format!("line {}: ${u} is not bound by an earlier line (--as {u})", n + 1)), at));
        }
        if let Some(b) = &r.bind {
            bound.insert(b.clone());
        }
        reqs.push((n + 1, r));
    }
    Ok(reqs)
}

/// Every `$NAME` a request's selectors use.
fn binds_used(r: &grammar::Request) -> Vec<String> {
    let mut uses = Vec::new();
    for e in [&r.sel, &r.place.at, &r.place.into, &r.within].into_iter().flatten() {
        Expr::binds(e, &mut uses);
    }
    if let Some(grammar::To::Sel(e)) = &r.place.to {
        e.binds(&mut uses);
    }
    uses
}

/// Runs one command (as words) on `x`, for commands built from commands (plans).
pub fn run_words(x: &mut Exec<'_>, words: &[String]) -> Vec<Obj> {
    match parse_line(words) {
        Ok(r) => x.run(&r).unwrap_or_else(|e| vec![from_error(&e, crate::obj! {})]),
        Err(e) => vec![from_error(&e, crate::obj! {})],
    }
}

/// Runs one command line; the exit code. `env` and `out` make it testable in-process.
pub fn run(argv: &[String], env: Env, stdin: &mut dyn std::io::Read, w: &mut dyn Write) -> i32 {
    let json = argv.iter().any(|a| a == "--json");
    let words: Vec<String> = argv.iter().filter(|a| *a != "--json").cloned().collect();
    let first = words.first().map(String::as_str).unwrap_or("help");
    let mut x = Exec::new(Conn::new(env));
    x.json = json;
    // watch rows stream as they come; the sink writes them out at once, and gives `w` back after
    let sink_w = std::cell::RefCell::new(w);
    let sw = &sink_w;
    x.sink = Box::new(move |r: &Obj| {
        let mut w = sw.borrow_mut();
        let line = if json {
            let mut r = r.clone();
            r.insert("schema_version".into(), serde_json::json!(grammar::SCHEMA_VERSION));
            serde_json::Value::Object(r).to_string()
        } else {
            out::render(std::slice::from_ref(r))
        };
        let _ = writeln!(w, "{line}");
        let _ = w.flush();
    });
    let result: Out = match first {
        "help" | "-h" | "--help" => Out::Text(match words.get(1).and_then(|n| Noun::parse(n)) {
            Some(n) => grammar::noun_help(n),
            None => format!("{}\n{USAGE_EXTRA}", grammar::HELP),
        }),
        // `NOUN [VERB …] --help`: the noun's help, what agents try first
        n if words.iter().skip(1).any(|a| a == "--help") && Noun::parse(n).is_some() => Out::Text(grammar::noun_help(Noun::parse(n).expect("checked"))),
        "version" | "--version" => Out::Text(format!("iterm {} (schema {})", env!("CARGO_PKG_VERSION"), grammar::SCHEMA_VERSION)),
        "coverage" => Out::Text(grammar::coverage().to_string()),
        "guide" => match crate::guide::guide(words.get(1).map(String::as_str)) {
            Ok(t) => Out::Text(t),
            Err(e) => Out::Rows(vec![from_error(&Error::invalid(e), crate::obj! {})]),
        },
        "batch" => Out::Rows(batch(&mut x, &words[1..], stdin)),
        "plan" => Out::Rows(crate::plan::run(&mut x, &words[1..])),
        "children" => Out::Rows(children(&mut x, &words[1..])),
        _ => Out::Rows(run_words(&mut x, &words)),
    };
    drop(x);
    let w = sink_w.into_inner();
    match result {
        Out::Text(t) => {
            let _ = writeln!(w, "{t}");
            0
        }
        Out::Rows(rows) => {
            let code = out::exit_code(&rows);
            let text = if json { out::json_rows(rows) } else { out::render(&rows) };
            if !text.is_empty() {
                let _ = writeln!(w, "{text}");
            }
            code
        }
    }
}

/// `batch [--file F] [--keep-going]`: the lines from F, else stdin.
fn batch(x: &mut Exec<'_>, args: &[String], stdin: &mut dyn std::io::Read) -> Vec<Obj> {
    let fail = |e: String| vec![from_error(&Error::invalid(e), crate::obj! {})];
    let p = match crate::args::parse(args, &[crate::args::one("--file"), crate::args::flag("--keep-going")], 0) {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let text = match p.one("--file") {
        Some(f) => std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}")),
        None => {
            let mut s = String::new();
            stdin.read_to_string(&mut s).map(|_| s).map_err(|e| e.to_string())
        }
    };
    match text {
        Ok(t) => run_batch(x, &t, p.has("--keep-going")),
        Err(e) => fail(e),
    }
}

/// `children [--wait] [--timeout S]`: the caller's children, one row each (`--wait`: waits on them).
fn children(x: &mut Exec<'_>, args: &[String]) -> Vec<Obj> {
    let opts = [crate::args::flag("--wait"), crate::args::one("--timeout")];
    match crate::args::parse(args, &opts, 0).and_then(|p| p.duration("--timeout").map(|t| (p.has("--wait"), t))) {
        Err(e) => vec![from_error(&Error::invalid(e), crate::obj! {})],
        Ok((wait, t)) => x.children(wait, t, None, false).unwrap_or_else(|e| vec![from_error(&e, crate::obj! {})]),
    }
}
