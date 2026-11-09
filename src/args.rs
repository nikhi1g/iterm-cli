//! A small table-driven argument parser with argparse's surface: `--opt value`, `--opt=value`,
//! one-or-more values (`--pane A B`), flags, positionals, unique-prefix abbreviations of long
//! options, and `--json`/`--apply` anywhere.

use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Takes {
    Flag,
    One,
    Many,
    /// Like Many, but a repeated option adds to the values (`--task A --task B`).
    Extend,
    /// A flag that may carry an inline value (`--worktree` or `--worktree=main`); never takes
    /// the next word, so a positional after it stays positional.
    FlagOrValue,
}

#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub name: &'static str,
    pub takes: Takes,
}

pub const fn flag(name: &'static str) -> Opt {
    Opt {
        name,
        takes: Takes::Flag,
    }
}
pub const fn one(name: &'static str) -> Opt {
    Opt {
        name,
        takes: Takes::One,
    }
}
pub const fn many(name: &'static str) -> Opt {
    Opt {
        name,
        takes: Takes::Many,
    }
}
pub const fn extend(name: &'static str) -> Opt {
    Opt {
        name,
        takes: Takes::Extend,
    }
}
pub const fn flag_or_value(name: &'static str) -> Opt {
    Opt {
        name,
        takes: Takes::FlagOrValue,
    }
}

#[derive(Debug, Default, Clone)]
pub struct Parsed {
    values: HashMap<&'static str, Vec<String>>,
    flags: Vec<&'static str>,
    pub positional: Vec<String>,
}

impl Parsed {
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }
    pub fn given(&self, name: &str) -> bool {
        self.values.contains_key(name) || self.has(name)
    }
    pub fn one(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }
    pub fn many(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }
    pub fn int(&self, name: &str) -> Result<Option<i64>, String> {
        match self.one(name) {
            None => Ok(None),
            Some(v) => v
                .parse::<i64>()
                .map(Some)
                .map_err(|_| format!("argument {name}: invalid int value: '{v}'")),
        }
    }
    /// `--timeout S`: see [`duration`].
    pub fn duration(&self, name: &str) -> Result<Option<Duration>, String> {
        self.one(name).map(|v| duration(name, v)).transpose()
    }
}

/// The longest wait a duration can mean, about 31 years ("forever"): longer ones are clamped to it, since the clock
/// and the kernel's socket timeouts overflow past it.
pub const MAX_WAIT_SECS: f64 = 1e9;

/// Seconds (`2`, `0.5`) as a duration: finite and not negative; 0 checks once and never waits. Anything else is a
/// usage error: never a panic (`Duration::from_secs_f64` on a negative or NaN) or a silent "forever".
pub fn duration(name: &str, v: &str) -> Result<Duration, String> {
    match v.trim().parse::<f64>() {
        Ok(x) if x.is_finite() && x >= 0.0 => Ok(Duration::from_secs_f64(x.min(MAX_WAIT_SECS))),
        _ => Err(format!("argument {name}: '{v}' is not a duration (seconds, 0 or more)")),
    }
}

fn looks_negative_number(s: &str) -> bool {
    s.strip_prefix('-')
        .map(|r| !r.is_empty() && r.parse::<f64>().is_ok())
        .unwrap_or(false)
}

fn is_option(s: &str) -> bool {
    s.starts_with('-') && s.len() > 1 && !looks_negative_number(s)
}

/// Parse `args` against `opts` (plus the global --json and --apply), collecting at most
/// `max_pos` positionals.
pub fn parse(args: &[String], opts: &[Opt], max_pos: usize) -> Result<Parsed, String> {
    let mut all: Vec<Opt> = opts.to_vec();
    // --apply is accepted and ignored (writes run; --plan is the dry run)
    for g in [flag("--json"), flag("--apply"), flag("--plan")] {
        if !all.iter().any(|o| o.name == g.name) {
            all.push(g);
        }
    }
    let mut out = Parsed::default();
    let mut i = 0;
    let mut only_positional = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if only_positional || !is_option(arg) {
            if arg == "--" && !only_positional {
                only_positional = true;
                continue;
            }
            if out.positional.len() >= max_pos {
                return Err(format!("unrecognized arguments: {arg}"));
            }
            out.positional.push(arg.clone());
            continue;
        }
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let opt = match all.iter().find(|o| o.name == key) {
            Some(o) => *o,
            None => {
                let hits: Vec<&Opt> = all
                    .iter()
                    .filter(|o| key.starts_with("--") && o.name.starts_with(&key))
                    .collect();
                match hits.len() {
                    1 => *hits[0],
                    0 => return Err(format!("unrecognized arguments: {arg}")),
                    _ => {
                        let names: Vec<&str> = hits.iter().map(|o| o.name).collect();
                        return Err(format!(
                            "ambiguous option: {key} could match {}",
                            names.join(", ")
                        ));
                    }
                }
            }
        };
        match opt.takes {
            Takes::FlagOrValue => {
                if let Some(v) = inline {
                    out.values.entry(opt.name).or_default().splice(.., [v]);
                }
                out.flags.push(opt.name);
            }
            Takes::Flag => {
                if inline.is_some() {
                    return Err(format!(
                        "argument {}: ignored explicit argument '{}'",
                        opt.name,
                        inline.unwrap_or_default()
                    ));
                }
                out.flags.push(opt.name);
            }
            Takes::One => {
                let value = match inline {
                    Some(v) => v,
                    None => {
                        if i >= args.len() || is_option(&args[i]) {
                            return Err(format!("argument {}: expected one argument", opt.name));
                        }
                        i += 1;
                        args[i - 1].clone()
                    }
                };
                out.values.entry(opt.name).or_default().splice(.., [value]);
            }
            Takes::Many | Takes::Extend => {
                let mut values = Vec::new();
                if let Some(v) = inline {
                    values.push(v);
                }
                while i < args.len() && !is_option(&args[i]) {
                    values.push(args[i].clone());
                    i += 1;
                }
                if values.is_empty() {
                    return Err(format!(
                        "argument {}: expected at least one argument",
                        opt.name
                    ));
                }
                if opt.takes == Takes::Extend {
                    out.values.entry(opt.name).or_default().extend(values);
                } else {
                    out.values.insert(opt.name, values);
                }
            }
        }
    }
    Ok(out)
}

/// Flatten `--pane A B,C` into [A, B, C], dropping blanks and duplicates.
pub fn split_panes(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values {
        for x in v.split(',') {
            let x = x.trim();
            if !x.is_empty() && !out.iter().any(|o| o == x) {
                out.push(x.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn options_values_and_abbreviations() {
        let opts = [
            many("--pane"),
            one("--text"),
            flag("--enter"),
            one("--timeout"),
        ];
        let p = parse(
            &s(&[
                "--pane",
                "A",
                "B,C",
                "--text=hi there",
                "--ent",
                "--apply",
                "--timeout",
                "-1",
            ]),
            &opts,
            0,
        )
        .unwrap();
        assert_eq!(p.many("--pane"), s(&["A", "B,C"]));
        assert_eq!(split_panes(&p.many("--pane")), s(&["A", "B", "C"]));
        assert_eq!(p.one("--text"), Some("hi there"));
        assert!(p.has("--enter") && p.has("--apply") && !p.has("--json"));
        assert!(p.duration("--timeout").unwrap_err().contains("not a duration"), "-1 is a value, and refused");
        assert!(parse(&s(&["--nope"]), &opts, 0).is_err());
        assert!(parse(&s(&["--text"]), &opts, 0).is_err());
        assert!(parse(&s(&["extra"]), &opts, 0).is_err());
        let p = parse(&s(&["done", "all good", "--json"]), &[], 2).unwrap();
        assert_eq!(p.positional, s(&["done", "all good"]));
        let p = parse(
            &s(&["--task", "a", "b", "--text", "x", "--task", "c"]),
            &[extend("--task"), one("--text")],
            0,
        )
        .unwrap();
        assert_eq!(p.many("--task"), s(&["a", "b", "c"]));
        let amb = parse(&s(&["--t", "x"]), &[one("--text"), one("--timeout")], 0).unwrap_err();
        assert!(amb.contains("ambiguous"), "{amb}");
    }

    #[test]
    fn durations_are_validated_never_a_panic_or_forever() {
        // regression (2026-10-29 audit A5): -1, nan and inf panicked in Duration::from_secs_f64; abc meant "forever"
        for bad in ["-1", "-0.5", "nan", "NaN", "inf", "-inf", "abc", "", "1s"] {
            assert!(duration("--timeout", bad).is_err(), "{bad:?}");
        }
        assert_eq!(duration("--timeout", "0"), Ok(Duration::ZERO));
        assert_eq!(duration("--timeout", "-0"), Ok(Duration::ZERO));
        assert_eq!(duration("--wait", "2.5"), Ok(Duration::from_millis(2500)));
        assert_eq!(duration("--wait", " 3 "), Ok(Duration::from_secs(3)));
        // huge is "forever", clamped where the clock can still add it
        let big = duration("--timeout", "1e30").unwrap();
        assert_eq!(big, Duration::from_secs_f64(MAX_WAIT_SECS));
        let _ = std::time::Instant::now() + big;
    }
}
