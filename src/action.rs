//! `iterm action list|get|run`: every key-map action is a command. The list comes from the app (its key
//! map, overrides included, is the one source of truth); `run` has the app press the key with the target pane
//! focused, keeping the human's focus unless the action is a focus action.

use crate::app::Op;

use crate::exec::Exec;
use crate::grammar::{self, Kind, Request, Verb};
use crate::out::{ok, planned, Error, Obj, Result};

/// One "name\tkey\tdescription" line as a row.
pub fn row(line: &str) -> Obj {
    let mut f = line.splitn(3, '\t');
    let (name, key, what) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
    ok("action", crate::obj! {"name" => name, "key" => key, "does" => what})
}

/// Closing actions follow the close rail: a live agent among the panes they close needs --force.
fn closes(name: &str) -> Option<Kind> {
    match name {
        "close_pane" => Some(Kind::Pane),
        "close_vtab" => Some(Kind::VTab),
        _ => None,
    }
}

pub fn run(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    let name = r.words.join(" ");
    match r.verb {
        Verb::List | Verb::Get => {
            if r.verb == Verb::Get && name.is_empty() {
                return Err(Error::invalid("action get NAME (action list names them)"));
            }
            let d = x.conn.call(Op::Actions)?;
            // `vtab 3` is the `vtab N` row
            let base = name.split(' ').next().unwrap_or("");
            let rows: Vec<Obj> = d
                .lines
                .iter()
                .filter(|l| r.verb == Verb::List || l.split('\t').next().is_some_and(|n| n == name || n.split(' ').next() == Some(base) && n.ends_with(" N")))
                .map(|l| row(l))
                .collect();
            if rows.is_empty() {
                return Err(Error::invalid(format!("no action {name:?}; iterm action list")));
            }
            Ok(rows)
        }
        Verb::Run => {
            if name.is_empty() {
                return Err(Error::invalid("action run NAME [--in PANE]"));
            }
            let (s, v) = x.view()?;
            let pane = match &r.within {
                Some(e) => Some(x.one_node(&v, e, Kind::Pane, "--in")?),
                None => v.m.me.or_else(|| v.m.focused.iter().copied().find(|i| v.m.nodes[*i].kind == Kind::Pane)),
            };
            if let (Some(k), Some(p)) = (closes(&name), pane) {
                let target = if k == Kind::Pane { p } else { v.m.up(p, Kind::VTab).unwrap_or(p) };
                x.may_delete(&s, &v, &[target], r.force)?;
            }
            let id = pane.map(|p| v.handle(p).to_string());
            if !r.apply {
                return Ok(vec![planned("action", crate::obj! {"name" => name, "in" => id})]);
            }
            x.conn.call(Op::Action { name: name.clone(), pane: pane.map(|p| v.id(p)) })?;
            Ok(vec![ok("ran", crate::obj! {"action" => name, "in" => id})])
        }
        vb => Err(Error::invalid(format!("action {vb}: {}", grammar::verbs(grammar::Noun::Action).iter().map(|(v, _)| v.name()).collect::<Vec<_>>().join(" ")))),
    }
}

/// `app reopen`: the last closed pane, tab or vertical tab, back in its place (⇧⌘T); the rows are what came back.
pub fn reopen(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    if !r.apply {
        return Ok(vec![planned("reopen", crate::obj! {})]);
    }
    let (before, _) = x.view()?;
    let had: std::collections::HashSet<_> = before.nodes.iter().map(|n| n.id).collect();
    x.conn.call(Op::Action { name: "reopen".into(), pane: None })?;
    let (s, v) = x.view()?;
    let back: Vec<Obj> = (0..s.nodes.len()).filter(|i| !had.contains(&s.nodes[*i].id)).map(|i| ok("reopened", crate::obj! {"node" => s.nodes[i].kind.name(), "id" => v.handle(i)})).collect();
    if back.is_empty() {
        return Err(Error::invalid("nothing to reopen (no pane, tab or vertical tab closed in this run)"));
    }
    Ok(back)
}

/// `on|off|toggle` against the state now: whether to press the key.
pub fn flip(flag: &str, value: &str, now: bool) -> Result<bool> {
    match value {
        "on" => Ok(!now),
        "off" => Ok(now),
        "toggle" | "" => Ok(true),
        _ => Err(Error::invalid(format!("{flag} on|off|toggle"))),
    }
}

/// `pane set SEL --zoom on|off|toggle` (⇧⌘↩) and `window set SEL --sidebar on|off|toggle` (⌥S): the key map's
/// toggles, pressed only when the state (the snapshot's `zoom` and `sidebar` vars) differs from the one asked for.
pub fn toggles(x: &mut Exec<'_>, r: &Request, s: &crate::app::Snapshot, v: &crate::view::View, targets: &[usize]) -> Result<Vec<Obj>> {
    // one zoom per tab: each press changes the tab's zoom, so a second pane would be judged from a stale state
    if r.p.given("--zoom") {
        let mut tabs: Vec<usize> = targets.iter().filter_map(|&i| v.m.up(i, Kind::Tab)).collect();
        let n = tabs.len();
        tabs.sort_unstable();
        tabs.dedup();
        if tabs.len() < n {
            return Err(Error::invalid("--zoom: one pane per tab (a tab maximizes one pane at a time)"));
        }
    }
    let mut rows = Vec::new();
    for &i in targets {
        let (flag, action, now, pane) = match s.nodes[i].kind {
            crate::app::Kind::Pane => {
                let Some(z) = r.p.one("--zoom") else { continue };
                let tab = v.m.up(i, Kind::Tab);
                let now = tab.and_then(|t| s.nodes[t].var("zoom")).is_some_and(|z| z == v.handle(i));
                let alone = tab.and_then(|t| s.nodes[t].layout.as_ref()).is_none_or(|l| matches!(l, crate::app::Layout::Pane(_)));
                if alone && !now && z != "off" {
                    return Err(Error::invalid(format!("{} is alone in its tab: nothing to maximize it over", v.handle(i))));
                }
                (("--zoom", z), "zoom", now, i)
            }
            crate::app::Kind::Window => {
                let Some(b) = r.p.one("--sidebar") else { continue };
                let now = s.nodes[i].var("sidebar") != Some("off");
                // the window's focused pane: the key acts on the window it is pressed in
                let pane = (0..s.nodes.len()).find(|p| s.nodes[*p].kind == crate::app::Kind::Pane && v.m.is_under(*p, i)).ok_or_else(|| Error::invalid("that window has no pane"))?;
                let pane = s.focus.last().and_then(|f| v.m.by_raw(&f.0.to_string())).filter(|f| v.m.is_under(*f, i)).unwrap_or(pane);
                (("--sidebar", b), "sidebar", now, pane)
            }
            _ => continue,
        };
        let press = flip(flag.0, flag.1, now)?;
        let after = if press { !now } else { now };
        let what = if flag.0 == "--zoom" { "zoom" } else { "sidebar" };
        let row = crate::obj! {"id" => v.handle(i), what => if after { "on" } else { "off" }};
        if !r.apply {
            rows.push(planned("set", row));
            continue;
        }
        if press {
            x.conn.call(Op::Action { name: action.into(), pane: Some(v.id(pane)) })?;
        }
        rows.push(ok("set", row));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_and_the_close_rail() {
        let r = super::row("zoom\tshift+cmd+return\tMaximize the pane over its tab, or restore");
        assert_eq!(crate::out::line(&r), "ok action name=zoom key=shift+cmd+return does=\"Maximize the pane over its tab, or restore\"");
        let r = super::row("help\t\tThis guide");
        assert_eq!(r["key"], "");
        assert_eq!(super::closes("close_pane"), Some(crate::grammar::Kind::Pane));
        assert_eq!(super::closes("zoom"), None);
        let p = crate::grammar::parse(crate::grammar::Noun::Action, &["run".into(), "vtab".into(), "3".into(), "--in".into(), "p4".into()]).unwrap();
        assert_eq!(p.words.join(" "), "vtab 3");
        assert!(p.within.is_some());
    }
}
