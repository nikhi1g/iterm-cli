//! `iterm content get|select|copy|mark|open` and `content scroll --to mark:NAME`: a pane's selection and
//! marks, the clipboard, and cmd-click, run by the app (it owns the grid). Rows are absolute, as for `pane read
//! --range` (0: the oldest history line); columns are cells.

use crate::app::wire::{ContentAct, Grab};
use crate::app::Op;

use crate::exec::Exec;
use crate::grammar::{Request, Verb};
use crate::out::{from_error, ok, planned, Error, Obj, Result};

fn num<T: std::str::FromStr>(s: &str, what: &str) -> Result<T> {
    s.trim().parse().map_err(|_| Error::invalid(format!("{what}: '{s}' is not a number")))
}

fn cell(s: &str) -> Result<(u64, u32)> {
    let (r, c) = s.split_once(':').ok_or_else(|| Error::invalid(format!("'{s}' is ROW:COL")))?;
    Ok((num(r, "row")?, num(c, "col")?))
}

/// `--range r1:c1-r2:c2` (cells) or `A:B` (rows A to B, end exclusive) | `--word R:C` | `--line R` | `--match RE`.
pub fn grab(r: &Request) -> Result<Option<Grab>> {
    if let Some(x) = r.p.one("--range") {
        return Ok(Some(match x.split_once('-') {
            Some((a, b)) => Grab::Range(cell(a)?, cell(b)?),
            None => {
                let (a, b) = x.split_once(':').ok_or_else(|| Error::invalid("--range r1:c1-r2:c2 | A:B"))?;
                let (a, b): (u64, u64) = (num(a, "--range")?, num(b, "--range")?);
                if b <= a {
                    return Err(Error::invalid("--range A:B with B > A (rows, end exclusive)"));
                }
                Grab::Range((a, 0), (b - 1, u32::MAX))
            }
        }));
    }
    if let Some(x) = r.p.one("--word") {
        let (row, col) = cell(x)?;
        return Ok(Some(Grab::Word(row, col)));
    }
    if let Some(x) = r.p.one("--line") {
        return Ok(Some(Grab::Line(num(x, "--line")?)));
    }
    if let Some(m) = r.p.one("--match") {
        regex::Regex::new(m).map_err(|e| Error::invalid(format!("--match: {e}")))?;
        return Ok(Some(Grab::Match(m.to_string())));
    }
    Ok(None)
}

/// The request as the app's operation.
pub fn act(r: &Request) -> Result<ContentAct> {
    Ok(match r.verb {
        Verb::Get => ContentAct::Get,
        Verb::Select => ContentAct::Select(grab(r)?.ok_or_else(|| Error::invalid("content select --range r1:c1-r2:c2 | --word R:C | --line R | --match RE"))?),
        Verb::Copy => ContentAct::Copy(grab(r)?),
        Verb::Find => ContentAct::FindBar(r.p.one("--match").ok_or_else(|| Error::invalid("content find --match TEXT --show"))?.to_string()),
        Verb::Open => ContentAct::Open(r.p.one("--match").ok_or_else(|| Error::invalid("content open --match RE"))?.to_string()),
        Verb::Mark => {
            // `content mark NAME` (the words are its name) or `content mark SEL --name N`
            let name = r.p.one("--name").map(str::to_string).or_else(|| r.sel.is_none().then(|| r.words.first().cloned()).flatten());
            ContentAct::Mark { row: r.p.one("--line").map(|l| num(l, "--line")).transpose()?, name }
        }
        Verb::Scroll => ContentAct::Jump(r.p.one("--to").and_then(|t| t.strip_prefix("mark:")).unwrap_or("").to_string()),
        vb => return Err(Error::invalid(format!("content {vb} is not a content operation"))),
    })
}

/// The app's reply lines as one row.
pub fn reply(pane: &str, verb: Verb, lines: &[String]) -> Obj {
    let mut row = crate::obj! {"pane" => pane};
    let first = lines.first().map(String::as_str).unwrap_or("");
    match verb {
        Verb::Get | Verb::Select => {
            let mut f = first.splitn(3, '\t').skip(1);
            let (span, text) = (f.next().unwrap_or(""), f.next());
            if !span.is_empty() {
                row.insert("span".into(), span.into());
            }
            row.insert("selection".into(), text.map_or(serde_json::Value::Null, |t| t.into()));
            if verb == Verb::Get {
                let marks: Vec<String> = lines[1.min(lines.len())..]
                    .iter()
                    .filter_map(|l| l.strip_prefix("mark\t"))
                    .map(|m| match m.split_once('\t') {
                        Some((r, n)) if !n.is_empty() => format!("{r}:{n}"),
                        Some((r, _)) => r.to_string(),
                        None => m.to_string(),
                    })
                    .collect();
                row.insert("marks".into(), marks.into());
            }
        }
        Verb::Copy => {
            row.insert("bytes".into(), first.len().into());
        }
        Verb::Mark => {
            row.insert("row".into(), first.into());
        }
        Verb::Open => {
            row.insert("url".into(), first.into());
        }
        _ => {}
    }
    let code = match verb {
        Verb::Get => "content",
        Verb::Select => "selected",
        Verb::Copy => "copied",
        Verb::Mark => "marked",
        Verb::Open => "opened",
        Verb::Find => "shown",
        _ => "scrolled",
    };
    ok(code, row)
}

pub fn run(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    let a = act(r)?;
    let (s, mut v) = x.view()?;
    let targets = x.resolve(r, &s, &mut v)?;
    if targets.is_empty() {
        return Err(Error::invalid(format!("content {}: the selector names no pane", r.verb)));
    }
    if r.verb != Verb::Get && !r.apply {
        return Ok(targets.iter().map(|i| planned(r.verb.name(), crate::obj! {"pane" => v.handle(*i), "op" => format!("{a:?}")})).collect());
    }
    let mut out = Vec::with_capacity(targets.len());
    for &i in &targets {
        let h = v.handle(i).to_string();
        out.push(match x.conn.call(Op::Content { pane: v.id(i), act: a.clone() }) {
            Ok(d) => reply(&h, r.verb, &d.lines),
            Err(e) => from_error(&e, crate::obj! {"pane" => h}),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::{parse, Noun};

    fn req(w: &[&str]) -> Request {
        parse(Noun::Content, &w.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn grabs_and_acts_parse() {
        assert_eq!(act(&req(&["select", "p4", "--range", "3:0-4:10"])).unwrap(), ContentAct::Select(Grab::Range((3, 0), (4, 10))));
        assert_eq!(act(&req(&["select", "--word", "7:2"])).unwrap(), ContentAct::Select(Grab::Word(7, 2)));
        assert_eq!(act(&req(&["select", "--line", "9"])).unwrap(), ContentAct::Select(Grab::Line(9)));
        assert_eq!(act(&req(&["copy", "--range", "2:5"])).unwrap(), ContentAct::Copy(Some(Grab::Range((2, 0), (4, u32::MAX)))));
        assert_eq!(act(&req(&["copy"])).unwrap(), ContentAct::Copy(None));
        assert_eq!(act(&req(&["open", "--match", "https://\\S+"])).unwrap(), ContentAct::Open("https://\\S+".into()));
        assert_eq!(act(&req(&["mark", "build"])).unwrap(), ContentAct::Mark { row: None, name: Some("build".into()) });
        assert_eq!(act(&req(&["mark", "p4", "--name", "x", "--line", "3"])).unwrap(), ContentAct::Mark { row: Some(3), name: Some("x".into()) });
        assert_eq!(act(&req(&["scroll", "--to", "mark", "build"])).unwrap(), ContentAct::Jump("build".into()));
        assert!(act(&req(&["select"])).is_err());
        assert!(act(&req(&["select", "--match", "("])).is_err());
        assert!(act(&req(&["copy", "--range", "5:2"])).is_err());
    }

    #[test]
    fn replies_are_one_row() {
        let g = reply("p4", Verb::Get, &["sel\t\thello".into(), "mark\t3\t".into(), "mark\t9\tbuild".into()]);
        assert_eq!(crate::out::line(&g), "ok content pane=p4 selection=hello marks=3,9:build");
        let g = reply("p4", Verb::Get, &["sel".into()]);
        assert_eq!(g["selection"], serde_json::Value::Null);
        let s = reply("p4", Verb::Select, &["sel\t1:0-1:4\thello".into()]);
        assert_eq!(crate::out::line(&s), "ok selected pane=p4 span=1:0-1:4 selection=hello");
    }
}
