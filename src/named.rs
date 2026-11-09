//! Named things, kept by the CLI (`Env::state`, one private JSON file per name):
//! - `group`: named pane sets. Membership lives on the panes (their `groups` variable, the one `pane set
//!   --group` writes and `--group G` selects by); the store keeps each group's kind (`label`, or `broadcast`:
//!   its members also receive broadcast input).
//! - `layout`: a tab's arrangement (split tree and fractions; each pane's folder, command or agent kind, name;
//!   the tab's title and color when the snapshot carries them), recreated in a new tab.
//! - `snippet`: named text, delivered through the app's delivery engine (`pane send`).
//! - `script`: `batch` files by name (`STATE/scripts/NAME[.batch]`).
//! - `preset`: color schemes: iTerm2's color presets and the profiles; `import` makes an `.itermcolors` file a
//!   profile; `profile set --set preset=NAME` applies one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::app::wire::{Field, SettingOp, Side};
use crate::app::{Id, Layout, Op, Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::exec::{Exec, Item};
use crate::grammar::{self, Kind, Noun, Request, Verb};
use crate::out::{ok, planned, Error, Obj, Result};
use crate::select::{self, Ctx};
use crate::store;
use crate::view::{self, View};

/// A color key as the CLI names it: `Ansi 1 Color` → `ansi.1`, `Cursor Text Color` → `cursor_text`.
pub fn color_name(key: &str) -> String {
    let k = key.trim();
    if let Some(n) = k.strip_prefix("Ansi ").and_then(|r| r.strip_suffix(" Color")) {
        return format!("ansi.{n}");
    }
    k.trim_end_matches(" Color").to_lowercase().replace(' ', "_")
}

/// Dark or light, by the background's luminance (`#rrggbb`).
pub fn shade(background: &str) -> Option<&'static str> {
    let h = background.strip_prefix('#').filter(|h| h.len() == 6)?;
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(f64::from);
    let l = 0.2126 * c(0)? + 0.7152 * c(2)? + 0.0722 * c(4)?;
    Some(if l < 128.0 { "dark" } else { "light" })
}

/// Scripts running scripts: how deep (a script that runs itself stops here).
static DEPTH: AtomicUsize = AtomicUsize::new(0);
const MAX_DEPTH: usize = 8;
/// The most text a snippet keeps.
const SNIPPET_LIMIT: usize = 1 << 20;

/// A saved split tree: leaves index the layout's panes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Tree {
    P(usize),
    S { v: bool, c: Vec<(f32, Tree)> },
}

impl Tree {
    pub fn of(l: &Layout, ids: &mut Vec<Id>) -> Tree {
        match l {
            Layout::Pane(id) => {
                ids.push(*id);
                Tree::P(ids.len() - 1)
            }
            Layout::Split { vertical, children } => Tree::S { v: *vertical, c: children.iter().map(|(f, c)| (*f, Tree::of(c, ids))).collect() },
        }
    }
    /// The tree over new panes; None when a leaf has no pane.
    pub fn layout(&self, ids: &[Id]) -> Option<Layout> {
        Some(match self {
            Tree::P(i) => Layout::Pane(*ids.get(*i)?),
            Tree::S { v, c } => Layout::Split { vertical: *v, children: c.iter().map(|(f, t)| Some((*f, t.layout(ids)?))).collect::<Option<_>>()? },
        })
    }
    /// `v(0.5:0,0.5:h(0.5:1,0.5:2))`: v side by side, h stacked.
    pub fn text(&self) -> String {
        match self {
            Tree::P(i) => i.to_string(),
            Tree::S { v, c } => {
                let inner: Vec<String> = c.iter().map(|(f, t)| format!("{:.2}:{}", f, t.text())).collect();
                format!("{}({})", if *v { "v" } else { "h" }, inner.join(","))
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedPane {
    pub cwd: String,
    pub kind: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedLayout {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    pub tree: Tree,
    pub panes: Vec<SavedPane>,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Group {
    kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Snippet {
    text: String,
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    store::read_json(path).and_then(|v| serde_json::from_value(v).ok())
}

fn write<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let text = serde_json::to_vec(v).map_err(|e| Error::failed(e.to_string()))?;
    store::atomic_write(path, &text)
}

/// The names stored in `dir` (`NAME.json`), sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(String::from)).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn groups_of(s: &Snapshot, i: usize) -> Vec<String> {
    s.nodes[i].var(view::V_GROUPS).unwrap_or("").split(',').filter(|g| !g.is_empty()).map(String::from).collect()
}

/// The panes a group change adds to and removes from broadcast input: a broadcast group's members receive it.
/// `was`/`is`: the group is a broadcast group before and after; `members` before, `after` after.
fn broadcast_change(was: bool, is: bool, members: &[usize], add: &[usize], remove: &[usize], after: &[usize]) -> (Vec<usize>, Vec<usize>) {
    match (was, is) {
        // already broadcasting: only the change; newly a broadcast group: every member
        (true, true) => (add.to_vec(), remove.to_vec()),
        (false, true) => (after.to_vec(), remove.to_vec()),
        (true, false) => (vec![], members.to_vec()),
        (false, false) => (vec![], vec![]),
    }
}

impl Exec<'_> {
    fn named_dir(&self, what: &str) -> Result<PathBuf> {
        store::dir(&self.conn.env().state, &[what])
    }

    /// The request's NAME (first word), a plain name.
    fn name_of<'r>(&self, r: &'r Request, usage: &str) -> Result<&'r str> {
        let n = r.words.first().map(String::as_str).ok_or_else(|| Error::invalid(format!("{} {} {usage}", r.noun, r.verb)))?;
        if !grammar::is_slug(n) {
            return Err(Error::invalid(format!("name '{n}': 1–64 letters, digits, _ and -")));
        }
        Ok(n)
    }

    /// Panes a selector names (containers count as their panes).
    fn panes_of(&self, v: &View, sel: &str) -> Result<Vec<usize>> {
        let e = grammar::selector(sel).map_err(|e| Error::invalid(e.to_string()))?;
        let found = select::eval(&v.m, &e, None, &Ctx::default()).map_err(Error::Invalid)?;
        let mut out: Vec<usize> = found.into_iter().flat_map(|i| v.m.project(i, Some(Kind::Pane))).collect();
        out.sort_unstable();
        out.dedup();
        if out.is_empty() {
            return Err(Error::invalid(format!("'{sel}' names no pane")));
        }
        Ok(out)
    }

    pub fn named(&mut self, r: &Request) -> Result<Vec<Obj>> {
        match r.noun {
            Noun::Group => self.group(r),
            Noun::Layout => self.layout(r),
            Noun::Snippet => self.snippet(r),
            Noun::Script => self.script(r),
            Noun::Preset => self.preset(r),
            n => Err(Error::invalid(format!("{n} {} is n/a", r.verb))),
        }
    }

    // ---- groups -------------------------------------------------------------------------------

    fn group(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let dir = self.named_dir("groups")?;
        let (s, v) = self.view()?;
        let row = |name: &str, kind: &str, members: &[usize]| {
            ok("group", crate::obj! {"name" => name, "kind" => kind, "members" => members.iter().map(|i| v.handle(*i)).collect::<Vec<_>>()})
        };
        let members = |name: &str| -> Vec<usize> { (0..s.nodes.len()).filter(|i| groups_of(&s, *i).iter().any(|g| g == name)).collect() };
        let stored = |name: &str| read::<Group>(&dir.join(format!("{name}.json")));
        match r.verb {
            Verb::List => {
                let mut all = names(&dir);
                for i in 0..s.nodes.len() {
                    all.extend(groups_of(&s, i));
                }
                all.sort();
                all.dedup();
                Ok(all.iter().map(|g| row(g, &stored(g).map(|x| x.kind).unwrap_or_else(|| "label".into()), &members(g))).collect())
            }
            Verb::Get => {
                let name = self.name_of(r, "NAME")?;
                let m = members(name);
                match stored(name) {
                    None if m.is_empty() => Err(Error::invalid(format!("no group {name}; make one: iterm group new {name} --add SEL"))),
                    g => Ok(vec![row(name, &g.map(|x| x.kind).unwrap_or_else(|| "label".into()), &m)]),
                }
            }
            Verb::New | Verb::Set | Verb::Delete => self.group_change(r, &dir, &s, &v),
            verb => Err(Error::invalid(format!("group {verb} is n/a"))),
        }
    }

    /// `group new|set|delete NAME [--add SEL…] [--remove SEL…] [--kind label|broadcast]`: membership written on
    /// the panes, the kind in the store; a broadcast group's members receive broadcast input (adding switches
    /// broadcast to `pick`).
    fn group_change(&mut self, r: &Request, dir: &Path, s: &Snapshot, v: &View) -> Result<Vec<Obj>> {
        let name = self.name_of(r, if r.verb == Verb::Set { "NAME --add SEL | --remove SEL" } else { "NAME" })?.to_string();
        let path = dir.join(format!("{name}.json"));
        let old = read::<Group>(&path);
        let m: Vec<usize> = (0..s.nodes.len()).filter(|i| groups_of(s, *i).contains(&name)).collect();
        if r.verb == Verb::New && (old.is_some() || !m.is_empty()) {
            return Err(Error::invalid(format!("group {name} exists; change it with: iterm group set {name} --add SEL")));
        }
        if r.verb != Verb::New && old.is_none() && m.is_empty() {
            return Err(Error::invalid(format!("no group {name}; make one: iterm group new {name} --add SEL")));
        }
        let kind = match r.p.many("--kind").first().map(String::as_str) {
            None => old.as_ref().map(|g| g.kind.clone()).unwrap_or_else(|| "label".into()),
            Some(k @ ("label" | "broadcast")) => k.to_string(),
            Some(k) => return Err(Error::invalid(format!("--kind {k}: label|broadcast"))),
        };
        let mut add = Vec::new();
        let mut remove = Vec::new();
        if r.verb == Verb::Delete {
            remove = m.clone();
        } else {
            for sel in r.p.many("--add") {
                add.extend(self.panes_of(v, &sel)?);
            }
            for sel in r.p.many("--remove") {
                remove.extend(self.panes_of(v, &sel)?);
            }
            if r.verb == Verb::Set && add.is_empty() && remove.is_empty() && r.p.many("--kind").is_empty() {
                return Err(Error::invalid("group set NAME --add SEL | --remove SEL | --kind label|broadcast"));
            }
        }
        add.retain(|i| !m.contains(i));
        remove.retain(|i| m.contains(i));
        let after: Vec<usize> = m.iter().chain(add.iter()).filter(|i| !remove.contains(i)).copied().collect();
        let handles = |ix: &[usize]| ix.iter().map(|i| v.handle(*i)).collect::<Vec<_>>();
        if !r.apply {
            let code = match r.verb {
                Verb::New => "created",
                Verb::Delete => "deleted",
                _ => "set",
            };
            return Ok(vec![planned(code, crate::obj! {"name" => name, "kind" => kind, "members" => handles(&after)})]);
        }
        for &i in add.iter().chain(remove.iter()) {
            let mut g = groups_of(s, i);
            g.retain(|x| *x != name);
            if add.contains(&i) {
                g.push(name.clone());
            }
            self.conn.call(Op::Set { id: v.id(i), field: Field::Var(view::V_GROUPS.into(), (!g.is_empty()).then(|| g.join(","))) })?;
        }
        let was = old.as_ref().is_some_and(|g| g.kind == "broadcast");
        let is = kind == "broadcast" && r.verb != Verb::Delete;
        let (b_add, b_remove) = broadcast_change(was, is, &m, &add, &remove, &after);
        if !b_add.is_empty() || !b_remove.is_empty() {
            let ids = |ix: Vec<usize>| ix.into_iter().map(|i| v.id(i)).collect();
            self.conn.call(Op::Broadcast { mode: None, window: None, add: ids(b_add), remove: ids(b_remove) })?;
        }
        if r.verb == Verb::Delete {
            let _ = std::fs::remove_file(&path);
            return Ok(vec![ok("deleted", crate::obj! {"name" => name, "members" => m.len()})]);
        }
        write(&path, &Group { kind: kind.clone() })?;
        let code = if r.verb == Verb::New { "created" } else { "set" };
        Ok(vec![ok(code, crate::obj! {"name" => name, "kind" => kind, "members" => handles(&after)})])
    }

    // ---- layouts ------------------------------------------------------------------------------

    /// What a pane runs: an agent's base command (`agent.cmd`), else the command the CLI opened it with (the
    /// creator's ledger); a pane the human opened runs the shell.
    fn cmd_of(&self, s: &Snapshot, i: usize) -> Option<String> {
        let n = &s.nodes[i];
        if let Some(c) = n.var(view::V_CMD) {
            return Some(c.to_string());
        }
        let parent = match n.var(view::V_PARENT)? {
            view::OUTSIDE => "outside".to_string(),
            p => p.to_string(),
        };
        self.ledger(&parent).into_iter().rev().find(|x| x.pane == n.id.0 && (x.app == 0 || x.app == s.app_pid)).and_then(|x| x.cmd)
    }

    fn layout(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let dir = self.named_dir("layouts")?;
        let file = |n: &str| dir.join(format!("{n}.json"));
        match r.verb {
            Verb::List => Ok(names(&dir)
                .iter()
                .filter_map(|n| read::<SavedLayout>(&file(n)).map(|l| (n, l)))
                .map(|(n, l)| ok("layout", crate::obj! {"name" => n, "panes" => l.panes.len(), "title" => l.title, "tree" => l.tree.text(), "at" => l.at}))
                .collect()),
            Verb::Get => {
                let name = self.name_of(r, "NAME")?;
                let l = read::<SavedLayout>(&file(name)).ok_or_else(|| Error::invalid(format!("no layout {name}; iterm layout list")))?;
                let mut rows = vec![ok("layout", crate::obj! {"name" => name, "panes" => l.panes.len(), "title" => l.title, "color" => l.color, "tree" => l.tree.text(), "at" => l.at})];
                for (k, p) in l.panes.iter().enumerate() {
                    rows.push(ok("pane", crate::obj! {"i" => k, "kind" => p.kind, "name" => p.name, "cwd" => p.cwd, "cmd" => p.cmd, "model" => p.model, "effort" => p.effort}));
                }
                Ok(rows)
            }
            Verb::Delete => {
                let name = self.name_of(r, "NAME")?;
                if !file(name).exists() {
                    return Err(Error::invalid(format!("no layout {name}; iterm layout list")));
                }
                if !r.apply {
                    return Ok(vec![planned("deleted", crate::obj! {"name" => name})]);
                }
                std::fs::remove_file(file(name)).map_err(|e| Error::failed(e.to_string()))?;
                Ok(vec![ok("deleted", crate::obj! {"name" => name})])
            }
            Verb::New => self.layout_save(r, &dir),
            Verb::Run => self.layout_run(r, &dir),
            verb => Err(Error::invalid(format!("layout {verb} is n/a"))),
        }
    }

    /// `layout new NAME [--in TAB] [--force]`: save a tab (`--in TAB` or anything in it, else the caller's, else
    /// the focused one): its split tree, each pane's folder, command or agent, and name, its title and color.
    fn layout_save(&mut self, r: &Request, dir: &Path) -> Result<Vec<Obj>> {
        let name = self.name_of(r, "NAME [--in TAB]")?.to_string();
        let file = dir.join(format!("{name}.json"));
        if file.exists() && !r.force {
            return Err(Error::invalid(format!("layout {name} exists; replace it with --force, or iterm layout delete {name}")));
        }
        let (s, v) = self.view()?;
        let tab = match &r.place.into {
            Some(e) => {
                let f = select::eval(&v.m, e, None, &Ctx::default()).map_err(Error::Invalid)?;
                f.first().and_then(|i| v.m.project(*i, Some(Kind::Tab)).first().copied()).ok_or_else(|| Error::invalid("--in names no tab"))?
            }
            None => v
                .m
                .me
                .and_then(|m| v.m.up(m, Kind::Tab))
                .or_else(|| v.m.focused.iter().find(|i| v.m.nodes[**i].kind == Kind::Tab).copied())
                .ok_or_else(|| Error::invalid("layout new NAME --in TAB (you have no pane here)"))?,
        };
        let l = s.nodes[tab].layout.as_ref().ok_or_else(|| Error::invalid(format!("{} has no panes", v.handle(tab))))?;
        let mut ids = Vec::new();
        let tree = Tree::of(l, &mut ids);
        if ids.is_empty() {
            return Err(Error::invalid(format!("{} has no panes", v.handle(tab))));
        }
        let mut panes = Vec::with_capacity(ids.len());
        for id in &ids {
            let i = v.m.by_raw(&id.0.to_string()).ok_or_else(|| Error::failed(format!("p{} is not in the snapshot", id.0)))?;
            panes.push(self.saved_pane(&s, i));
        }
        let t = &s.nodes[tab];
        // the tab's color: the snapshot carries it as the var `color` (#rrggbb)
        let saved = SavedLayout { title: (!t.name.is_empty()).then(|| t.name.clone()), color: t.var("color").map(String::from), tree, panes, at: store::now() };
        let row = crate::obj! {"name" => name, "tab" => v.handle(tab), "panes" => saved.panes.len(), "tree" => saved.tree.text()};
        if !r.apply {
            return Ok(vec![planned("saved", row)]);
        }
        write(&file, &saved)?;
        Ok(vec![ok("saved", row)])
    }

    /// One pane as a layout keeps it.
    fn saved_pane(&self, s: &Snapshot, i: usize) -> SavedPane {
        let n = &s.nodes[i];
        SavedPane {
            cwd: n.pane.as_ref().map(|p| p.cwd.clone()).unwrap_or_default(),
            kind: n.var(view::V_KIND).unwrap_or("shell").to_string(),
            cmd: self.cmd_of(s, i),
            name: n.var(view::V_NAME).map(String::from).or_else(|| (!n.name.is_empty()).then(|| n.name.clone())),
            model: n.var(view::V_MODEL).map(String::from),
            effort: n.var(view::V_EFFORT).map(String::from),
        }
    }

    /// `layout run NAME [--in VTAB]`: recreate it in a new tab, in `--in VTAB` (or the vertical tab of what it
    /// names), else the caller's: every pane opened in a row, then the saved tree and color set over them. The
    /// first pane that fails to open stops it.
    fn layout_run(&mut self, r: &Request, dir: &Path) -> Result<Vec<Obj>> {
        let name = self.name_of(r, "NAME [--in VTAB]")?.to_string();
        let l = read::<SavedLayout>(&dir.join(format!("{name}.json"))).ok_or_else(|| Error::invalid(format!("no layout {name}; iterm layout list")))?;
        let (_, v) = self.view()?;
        let vtab = match &r.within {
            Some(e) => {
                let f = select::eval(&v.m, e, None, &Ctx::default()).map_err(Error::Invalid)?;
                Some(f.first().and_then(|i| v.m.project(*i, Some(Kind::VTab)).first().copied()).ok_or_else(|| Error::invalid("--in names no vertical tab"))?)
            }
            None => v.m.me.and_then(|m| v.m.up(m, Kind::VTab)),
        };
        if !r.apply {
            return Ok(vec![planned("layout", crate::obj! {"name" => name, "panes" => l.panes.len(), "in" => vtab.map(|i| v.handle(i).to_string())})]);
        }
        let mut rows = Vec::new();
        let mut made: Vec<Id> = Vec::new();
        for (k, p) in l.panes.iter().enumerate() {
            let agent = view::is_agent_kind(Some(&p.kind));
            let it = Item { name: p.name.clone(), cwd: p.cwd.clone(), task: None, cmd: p.cmd.clone(), model: p.model.clone(), effort: p.effort.clone(), worktree: None, title: None };
            let spawn = self.spawn_for(r, &it, agent)?;
            let op = match made.last() {
                None => Op::NewTab { vtab: vtab.map(|i| v.id(i)), title: l.title.clone(), spawn, focus: r.focus },
                // any shape: the saved tree replaces it once every pane is open
                Some(prev) => Op::Split { at: *prev, side: Side::Right, ratio: None, spawn, focus: false },
            };
            let row = self.made(r, op, &it, agent);
            let pane = row.get("id").and_then(Value::as_str).and_then(crate::app::model::parse_handle).map(|(_, id)| id);
            let failed = row.get("status").and_then(Value::as_str) != Some("ok");
            rows.push(row);
            match pane {
                Some(id) if !failed => made.push(id),
                _ => {
                    rows.push(crate::out::from_error(&Error::failed(format!("pane {} of {} did not open", k + 1, l.panes.len())), crate::obj! {"layout" => name}));
                    return Ok(rows);
                }
            }
        }
        let (s, v) = self.view()?;
        let tab = v.m.by_raw(&made[0].0.to_string()).and_then(|i| v.m.up(i, Kind::Tab)).ok_or_else(|| Error::failed("the new tab is gone"))?;
        if made.len() > 1 {
            let layout = l.tree.layout(&made).ok_or_else(|| Error::failed(format!("layout {name}: its tree names a pane it doesn't have")))?;
            self.conn.call(Op::SetLayout { tab: s.nodes[tab].id, layout })?;
        }
        if let Some(c) = &l.color {
            let _ = self.conn.call(Op::Set { id: s.nodes[tab].id, field: Field::Color(Some(c.clone())) });
        }
        rows.push(ok("layout", crate::obj! {"name" => name, "tab" => v.handle(tab), "panes" => made.len()}));
        Ok(rows)
    }

    // ---- snippets -----------------------------------------------------------------------------

    fn snippet(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let dir = self.named_dir("snippets")?;
        let file = |n: &str| dir.join(format!("{n}.json"));
        let load = |n: &str| read::<Snippet>(&file(n)).ok_or_else(|| Error::invalid(format!("no snippet {n}; iterm snippet list")));
        let head = |t: &str| t.lines().next().unwrap_or("").chars().take(40).collect::<String>();
        match r.verb {
            Verb::List => Ok(names(&dir)
                .iter()
                .filter_map(|n| read::<Snippet>(&file(n)).map(|x| ok("snippet", crate::obj! {"name" => n, "bytes" => x.text.len(), "head" => head(&x.text)})))
                .collect()),
            Verb::Get => {
                let name = self.name_of(r, "NAME")?;
                let x = load(name)?;
                Ok(vec![ok("snippet", crate::obj! {"name" => name, "bytes" => x.text.len(), "value" => x.text})])
            }
            Verb::New => {
                let name = self.name_of(r, "NAME --text T | --file F")?;
                let text = match (r.p.one("--text"), r.p.one("--file")) {
                    (Some(t), _) => t.to_string(),
                    (None, Some(f)) => std::fs::read_to_string(self.conn.env().cwd.join(f)).map_err(|e| Error::invalid(format!("{f}: {e}")))?,
                    _ => return Err(Error::invalid("snippet new NAME --text T | --file F")),
                };
                if text.is_empty() || text.len() > SNIPPET_LIMIT {
                    return Err(Error::invalid(format!("a snippet holds 1 byte to {} KiB", SNIPPET_LIMIT / 1024)));
                }
                if file(name).exists() && !r.force {
                    return Err(Error::invalid(format!("snippet {name} exists; replace it with --force, or iterm snippet delete {name}")));
                }
                let row = crate::obj! {"name" => name, "bytes" => text.len()};
                if !r.apply {
                    return Ok(vec![planned("created", row)]);
                }
                write(&file(name), &Snippet { text })?;
                Ok(vec![ok("created", row)])
            }
            Verb::Delete => {
                let name = self.name_of(r, "NAME")?;
                load(name)?;
                if !r.apply {
                    return Ok(vec![planned("deleted", crate::obj! {"name" => name})]);
                }
                std::fs::remove_file(file(name)).map_err(|e| Error::failed(e.to_string()))?;
                Ok(vec![ok("deleted", crate::obj! {"name" => name})])
            }
            // the delivery engine via `pane send`: whole, in order, held behind a prompt, Enter only with --enter
            Verb::Run => {
                let name = self.name_of(r, "NAME [--in PANE] [--enter]")?;
                let x = load(name)?;
                let to = r.p.one("--in").map(String::from).unwrap_or_else(|| if self.conn.env().me.is_some() { "@me".into() } else { "@focused".into() });
                let mut words: Vec<String> = ["pane", "send", to.as_str(), "--text", x.text.as_str()].iter().map(|w| w.to_string()).collect();
                for f in ["--enter", "--plan"] {
                    if r.p.has(f) {
                        words.push(f.into());
                    }
                }
                Ok(crate::cli::run_words(self, &words))
            }
            verb => Err(Error::invalid(format!("snippet {verb} is n/a"))),
        }
    }

    // ---- scripts ------------------------------------------------------------------------------

    fn script(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let dir = self.named_dir("scripts")?;
        match r.verb {
            Verb::List => {
                let mut rows: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                            .filter_map(|e| {
                                let n = e.file_name().to_str()?.to_string();
                                (!n.starts_with('.')).then(|| (n.strip_suffix(".batch").unwrap_or(&n).to_string(), e.path()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                rows.sort();
                Ok(rows
                    .into_iter()
                    .map(|(n, p)| {
                        let lines = std::fs::read_to_string(&p).map(|t| t.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).count()).unwrap_or(0);
                        ok("script", crate::obj! {"name" => n, "lines" => lines, "path" => p.display().to_string()})
                    })
                    .collect())
            }
            Verb::Run => {
                let name = self.name_of(r, "NAME (a batch file in STATE/scripts)")?;
                let path = [dir.join(format!("{name}.batch")), dir.join(name)].into_iter().find(|p| p.is_file()).ok_or_else(|| {
                    Error::invalid(format!("no script {name}: put a batch file at {}", dir.join(format!("{name}.batch")).display()))
                })?;
                let text = std::fs::read_to_string(&path).map_err(|e| Error::failed(format!("{}: {e}", path.display())))?;
                if DEPTH.fetch_add(1, Ordering::Relaxed) >= MAX_DEPTH {
                    DEPTH.fetch_sub(1, Ordering::Relaxed);
                    return Err(Error::policy("depth", format!("scripts nest at most {MAX_DEPTH} deep")));
                }
                let rows = crate::cli::run_batch(self, &text, r.p.has("--keep-going"));
                DEPTH.fetch_sub(1, Ordering::Relaxed);
                Ok(rows)
            }
            verb => Err(Error::invalid(format!("script {verb} is n/a"))),
        }
    }

    // ---- presets ------------------------------------------------------------------------------

    fn preset(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let obj = |d: crate::app::Done| -> serde_json::Map<String, Value> { d.lines.first().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_default() };
        match r.verb {
            // the app's color presets, then the profiles (themes in use or imported)
            Verb::List => {
                let grep = r.p.one("--grep").map(str::to_ascii_lowercase);
                let hit = |n: &str| grep.as_deref().is_none_or(|g| n.to_ascii_lowercase().contains(g));
                let v = obj(self.conn.setting(SettingOp::Get { keys: vec!["presets".into(), "profile".into(), "profiles".into()] })?);
                let names = |k: &str| v.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter(|n| hit(n)).map(String::from).collect::<Vec<_>>();
                let active = v.get("profile").and_then(Value::as_str).unwrap_or("Default").to_string();
                let mut rows: Vec<Obj> = names("presets").into_iter().map(|n| ok("preset", crate::obj! {"name" => n, "source" => "preset"})).collect();
                rows.extend(names("profiles").into_iter().map(|n| ok("preset", crate::obj! {"name" => n.clone(), "source" => "profile", "active" => n == active})));
                Ok(rows)
            }
            // a preset's colors, else a profile's
            Verb::Get => {
                let name = r.words.join(" ");
                if name.is_empty() {
                    return Err(Error::invalid("preset get NAME"));
                }
                let (source, colors) = match self.conn.setting(SettingOp::Get { keys: vec![format!("preset:{name}")] }) {
                    Ok(d) => ("preset", obj(d).remove(&format!("preset:{name}")).and_then(|v| v.as_object().cloned()).unwrap_or_default()),
                    Err(_) => {
                        let v = obj(self.conn.setting(SettingOp::Get { keys: vec![format!("@{name}")] }).map_err(|_| {
                            Error::invalid(format!("no preset or profile {name:?} (iterm preset list)"))
                        })?);
                        ("profile", v.into_iter().filter(|(k, c)| k.ends_with(" Color") && c.as_str().is_some_and(|c| c.starts_with('#'))).map(|(k, c)| (color_name(&k), c)).collect())
                    }
                };
                let mut row = crate::obj! {"name" => name, "source" => source};
                if let Some(k) = colors.get("background").and_then(Value::as_str).and_then(shade) {
                    row.insert("kind".into(), json!(k));
                }
                row.extend(colors);
                Ok(vec![ok("preset", row)])
            }
            // a theme file becomes a new profile named after it, switched to (the app parses the file and converts
            // its colors to sRGB, as Settings' import does)
            Verb::Import => {
                let f = r.words.first().ok_or_else(|| Error::invalid("preset import FILE (.itermcolors, Ghostty or kitty theme)"))?;
                let abs = std::fs::canonicalize(self.conn.env().cwd.join(f)).map_err(|e| Error::invalid(format!("{f}: {e}")))?;
                if !r.apply {
                    return Ok(vec![planned("imported", crate::obj! {"file" => abs.display().to_string()})]);
                }
                let d = self.conn.setting(SettingOp::Set { pairs: vec![("profile_import".into(), abs.to_string_lossy().into_owned())] })?;
                let v: Value = d.lines.first().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_default();
                Ok(vec![ok("imported", crate::obj! {"name" => v.get("profile").cloned().unwrap_or(Value::Null), "file" => abs.display().to_string()})])
            }
            verb => Err(Error::invalid(format!("preset {verb} is n/a"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_round_trip_and_presets_parse() {
        let l = Layout::Split {
            vertical: true,
            children: vec![(0.25, Layout::Pane(Id(7))), (0.75, Layout::Split { vertical: false, children: vec![(0.5, Layout::Pane(Id(3))), (0.5, Layout::Pane(Id(9)))] })],
        };
        let mut ids = Vec::new();
        let t = Tree::of(&l, &mut ids);
        assert_eq!(ids, [Id(7), Id(3), Id(9)]);
        assert_eq!(t.text(), "v(0.25:0,0.75:h(0.50:1,0.50:2))");
        let back = t.layout(&[Id(1), Id(2), Id(4)]).unwrap();
        let mut got = Vec::new();
        back.panes(&mut got);
        assert_eq!(got, [Id(1), Id(2), Id(4)]);
        assert!(t.layout(&[Id(1)]).is_none(), "a leaf without a pane");
        assert_eq!((color_name("Ansi 1 Color"), color_name("Cursor Text Color")), ("ansi.1".to_string(), "cursor_text".to_string()));
        assert_eq!((shade("#2c2c2c"), shade("#fdf6e3"), shade("2c")), (Some("dark"), Some("light"), None));
    }

    #[test]
    fn a_broadcast_group_change_moves_only_what_changed() {
        let (m, add, remove, after) = (vec![1, 2], vec![3], vec![2], vec![1, 3]);
        assert_eq!(broadcast_change(true, true, &m, &add, &remove, &after), (vec![3], vec![2]));
        assert_eq!(broadcast_change(false, true, &m, &add, &remove, &after), (vec![1, 3], vec![2]), "newly broadcast: every member");
        assert_eq!(broadcast_change(true, false, &m, &add, &remove, &after), (vec![], vec![1, 2]), "no longer: every old member out");
        assert_eq!(broadcast_change(false, false, &m, &add, &remove, &after), (vec![], vec![]));
    }
}
