//! `iterm tree [SEL]`: the whole layout from the one snapshot, no other round trip. Text: one line per
//! node, indented two spaces a level (window > vertical tab > tab > pane), `*` on the focused chain; panes carry
//! their agent's provider and state (the app's agent table) and a `~`-shortened cwd. `--json`: the same as
//! nested objects. With SEL: the selected nodes, what is under them, and the path down to them.

use crate::app::{Kind as PKind, Node, Snapshot};
use serde_json::{json, Map, Value};

use crate::exec::Exec;
use crate::grammar::{self, Request};
use crate::out::{ok, Error, Obj, Result};
use crate::select::{self, Ctx};
use crate::view::{self, View};

pub fn run(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    let me = x.conn.env().me;
    let s = x.conn.snapshot()?;
    let keep = if r.words.is_empty() {
        None
    } else {
        let v = View::new(s, me, &x.binds);
        let e = grammar::selector(&r.words.join(" ")).map_err(|e| Error::invalid(e.to_string()))?;
        let picked = select::eval(&v.m, &e, None, &Ctx::default()).map_err(Error::Invalid)?;
        if picked.is_empty() {
            return Err(Error::invalid("tree: the selector names nothing"));
        }
        Some(kept(s, &picked))
    };
    let home = std::env::var("HOME").unwrap_or_default();
    Ok(vec![if x.json { tree_json(s, keep.as_deref(), me, &home) } else { ok("text", crate::obj! {"lines" => lines(s, keep.as_deref(), me, &home)}) }])
}

/// The nodes shown for a selection: each picked node, its ancestors and its descendants. O(nodes): the
/// snapshot is pre-order, so a node's descendants follow it until the depth returns to its own.
fn kept(s: &Snapshot, picked: &[usize]) -> Vec<bool> {
    let n = s.nodes.len();
    let mut keep = vec![false; n];
    let mut under = vec![false; n];
    for &p in picked {
        under[p] = true;
        let mut up = s.nodes[p].parent;
        while let Some(i) = up {
            keep[i as usize] = true;
            up = s.nodes[i as usize].parent;
        }
    }
    for i in 0..n {
        if let Some(p) = s.nodes[i].parent {
            under[i] |= under[p as usize];
        }
        keep[i] |= under[i];
    }
    keep
}

fn short(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn word(v: &str) -> String {
    crate::out::render_value(&json!(v))
}

/// One node's line, without its indent.
fn line(n: &Node, focused: bool, me: bool, home: &str) -> String {
    let mut out = n.handle();
    if focused {
        out.push('*');
    }
    if !n.name.is_empty() {
        out.push(' ');
        out.push_str(&word(&n.name));
    }
    if me {
        out.push_str(" me");
    }
    if let Some(c) = n.var("color") {
        out.push_str(" color=");
        out.push_str(c);
    }
    if let Some(p) = &n.pane {
        if let Some(pv) = n.var(view::V_PROVIDER).or(n.var(view::V_KIND).filter(|k| view::is_agent_kind(Some(k)))) {
            out.push_str(" agent=");
            out.push_str(pv);
            if let Some(st) = n.var(view::V_STATE) {
                out.push(':');
                out.push_str(st);
            }
        }
        if let Some(a) = n.var(view::V_NAME).filter(|a| *a != n.name) {
            out.push_str(" as=");
            out.push_str(&word(a));
        }
        if !p.cwd.is_empty() {
            out.push(' ');
            out.push_str(&word(&short(&p.cwd, home)));
        }
        if let Some(x) = p.exit {
            out.push_str(&format!(" exit={x}"));
        }
    }
    out
}

/// A window's implicit vertical tab (iTerm2 reports no tab group there): not printed as a level.
pub fn implicit(n: &Node) -> bool {
    n.kind == PKind::VTab && n.var("implicit") == Some("1")
}

fn depth(s: &Snapshot, i: usize) -> usize {
    let d = match s.nodes[i].kind {
        PKind::App => 0,
        PKind::Window => 0,
        PKind::VTab => 1,
        PKind::Tab => 2,
        PKind::Pane => 3,
    };
    // under an implicit vertical tab everything moves up a level
    let mut up = s.nodes[i].parent;
    while let Some(p) = up {
        if implicit(&s.nodes[p as usize]) {
            return d - 1;
        }
        up = s.nodes[p as usize].parent;
    }
    d
}

pub fn lines(s: &Snapshot, keep: Option<&[bool]>, me: Option<crate::app::Id>, home: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(s.nodes.len());
    for (i, n) in s.nodes.iter().enumerate() {
        if n.kind == PKind::App || implicit(n) || keep.is_some_and(|k| !k[i]) {
            continue;
        }
        let mut l = "  ".repeat(depth(s, i));
        l.push_str(&line(n, s.focus.contains(&n.id), me == Some(n.id), home));
        out.push(l);
    }
    out
}

/// The tree as nested objects: windows → vtabs → tabs → panes.
pub fn tree_json(s: &Snapshot, keep: Option<&[bool]>, me: Option<crate::app::Id>, home: &str) -> Obj {
    let n = s.nodes.len();
    let mut objs: Vec<Option<Map<String, Value>>> = Vec::with_capacity(n);
    for (i, node) in s.nodes.iter().enumerate() {
        if keep.is_some_and(|k| !k[i]) && node.kind != PKind::App {
            objs.push(None);
            continue;
        }
        let mut o = crate::obj! {"id" => node.handle()};
        if !node.name.is_empty() {
            o.insert("name".into(), json!(node.name));
        }
        if s.focus.contains(&node.id) {
            o.insert("focused".into(), json!(true));
        }
        if implicit(node) {
            o.insert("implicit".into(), json!(true));
        }
        if me == Some(node.id) {
            o.insert("me".into(), json!(true));
        }
        if let Some(c) = node.var("color") {
            o.insert("color".into(), json!(c));
        }
        if let Some(p) = &node.pane {
            for (k, key) in [("provider", view::V_PROVIDER), ("state", view::V_STATE), ("agent", view::V_NAME), ("kind", view::V_KIND)] {
                if let Some(v) = node.var(key) {
                    o.insert(k.into(), json!(v));
                }
            }
            o.insert("cwd".into(), json!(short(&p.cwd, home)));
            o.insert("size".into(), json!(format!("{}x{}", p.cols, p.rows)));
            if let Some(x) = p.exit {
                o.insert("exit".into(), json!(x));
            }
        }
        objs.push(Some(o));
    }
    // children before parents: walk backwards, folding each node into its parent's list
    let key = |k: PKind| match k {
        PKind::App => "windows",
        PKind::Window => "vtabs",
        PKind::VTab => "tabs",
        _ => "panes",
    };
    for i in (1..n).rev() {
        let Some(mut o) = objs[i].take() else { continue };
        // its children were all folded in by now, last first
        if let Some(Value::Array(a)) = o.get_mut(key(s.nodes[i].kind)) {
            a.reverse();
        }
        let Some(p) = s.nodes[i].parent.map(|p| p as usize) else { continue };
        let pk = key(s.nodes[p].kind);
        if let Some(po) = objs[p].as_mut() {
            match po.entry(pk).or_insert_with(|| json!([])) {
                Value::Array(a) => a.push(Value::Object(o)),
                _ => {}
            }
        }
    }
    let mut windows = objs[0].take().and_then(|mut o| o.remove("windows")).unwrap_or_else(|| json!([]));
    if let Value::Array(a) = &mut windows {
        a.reverse();
    }
    ok("tree", crate::obj! {"windows" => windows})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Id, PaneInfo};

    fn node(id: u64, kind: PKind, parent: Option<u32>, name: &str) -> Node {
        Node { id: Id(id), kind, parent, name: name.into(), rect: None, created: id, vars: vec![], pane: None, layout: None }
    }

    fn snap() -> Snapshot {
        let mut p = node(4, PKind::Pane, Some(3), "lead");
        p.vars = vec![("agent.provider".into(), "claude".into()), ("agent.state".into(), "busy".into())];
        p.pane = Some(PaneInfo { cwd: "/Users/me/proj".into(), cols: 80, rows: 24, ..PaneInfo::default() });
        let mut q = node(5, PKind::Pane, Some(3), "");
        q.pane = Some(PaneInfo { cwd: "/tmp".into(), ..PaneInfo::default() });
        let mut t = node(3, PKind::Tab, Some(2), "");
        t.vars = vec![("color".into(), "#ff0000".into())];
        let mut v6 = node(6, PKind::VTab, Some(1), "other");
        v6.created = 6;
        Snapshot {
            nodes: vec![node(0, PKind::App, None, ""), node(1, PKind::Window, Some(0), ""), node(2, PKind::VTab, Some(1), "main"), t, p, q, v6, {
                let mut t = node(7, PKind::Tab, Some(6), "");
                t.created = 7;
                t
            }],
            focus: vec![Id(1), Id(2), Id(3), Id(4)],
            ..Snapshot::default()
        }
    }

    #[test]
    fn one_line_per_node_indented_with_focus_and_agent() {
        let s = snap();
        let l = lines(&s, None, Some(Id(5)), "/Users/me");
        assert_eq!(l, ["w1*", "  v2* main", "    t3* color=#ff0000", "      p4* lead agent=claude:busy ~/proj", "      p5 me /tmp", "  v6 other", "    t7"]);
    }

    #[test]
    fn an_implicit_vertical_tab_is_not_a_level() {
        let mut s = snap();
        s.nodes[2].vars = vec![("implicit".into(), "1".into())];
        let l = lines(&s, None, None, "/Users/me");
        assert_eq!(&l[..4], ["w1*", "  t3* color=#ff0000", "    p4* lead agent=claude:busy ~/proj", "    p5 /tmp"]);
        assert_eq!(tree_json(&s, None, None, "")["windows"][0]["vtabs"][0]["implicit"], true);
    }

    #[test]
    fn a_selection_keeps_its_path_and_subtree() {
        let s = snap();
        let k = kept(&s, &[6]);
        assert_eq!(lines(&s, Some(&k), None, ""), ["w1*", "  v6 other", "    t7"]);
        let k = kept(&s, &[5]);
        assert_eq!(lines(&s, Some(&k), None, "").len(), 4);
    }

    #[test]
    fn json_nests_windows_vtabs_tabs_panes() {
        let s = snap();
        let o = tree_json(&s, None, None, "/Users/me");
        let w = &o["windows"][0];
        assert_eq!(w["id"], "w1");
        assert_eq!(w["vtabs"][0]["tabs"][0]["panes"][0]["state"], "busy");
        assert_eq!(w["vtabs"][0]["tabs"][0]["panes"][1]["id"], "p5");
        assert_eq!(w["vtabs"][1]["tabs"][0]["id"], "t7");
    }
}
