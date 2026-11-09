//! A fake terminal app in process: an in-memory model (windows → vtabs → tabs →
//! split tree → panes) that answers every `Op` the way the app should, publishes a snapshot and its
//! events after each change, and lets a test set a pane's screen or end its process. The CLI runs
//! in-process (`cli::run`) against it, so every test goes through the real protocol.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use itermkit::conn::Env;
use itermkit::app::{App, Events, Hub};
use itermkit::app::wire::{Dest, Field, Lines, Side};
use itermkit::app::{Done, Event, Fail, FailCode, Id, Kind, Layout, Node, Op, PaneInfo, Rect, Snapshot};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FNode {
    pub kind: Kind,
    pub parent: Option<Id>,
    pub children: Vec<Id>,
    pub name: String,
    pub vars: BTreeMap<String, String>,
    pub created: u64,
    pub layout: Option<Layout>,
    pub screen: Vec<String>,
    /// Messages held behind a prompt (text, enter), delivered by [`World::screen`] once the prompt is gone.
    pub held: Vec<(String, bool)>,
    pub cwd: String,
    pub cmd: Option<String>,
    pub exit: Option<i32>,
    pub sent: Vec<u8>,
    pub frame: Option<Rect>,
}

#[derive(Default)]
pub struct Model {
    pub next: u64,
    pub nodes: HashMap<Id, FNode>,
    pub app: Option<Id>,
    pub focus: Option<Id>,
    pub ops: Vec<Op>,
    /// The app instance's pid in snapshots (default: this process's): another value is a restarted app.
    pub pid: Option<i32>,
    /// The dialogs `UiList` shows (default: one alert, 812); how many times it was asked.
    pub ui: Option<Vec<String>>,
    pub ui_lists: usize,
    /// A delivery starts a whole agent turn in one publish: busy, this many bell events (far past a watcher's
    /// queue, so it gets a gap), then idle.
    pub turn_burst: usize,
}

fn fail(code: FailCode, m: &str) -> Fail {
    Fail::new(code, m)
}

impl Model {
    fn id(&mut self) -> Id {
        self.next += 1;
        Id(self.next)
    }

    fn add(&mut self, kind: Kind, parent: Option<Id>, ev: &mut Vec<Event>) -> Id {
        let id = self.id();
        let created = self.next;
        self.nodes.insert(
            id,
            FNode {
                kind,
                parent,
                children: vec![],
                name: String::new(),
                vars: BTreeMap::new(),
                created,
                layout: (kind == Kind::Tab).then(|| Layout::Split { vertical: true, children: vec![] }),
                screen: vec!["$ ".into()],
                held: vec![],
                cwd: "/tmp".into(),
                cmd: None,
                exit: None,
                sent: vec![],
                frame: (kind == Kind::Window).then_some(Rect { x: 0.0, y: 25.0, w: 1200.0, h: 800.0 }),
            },
        );
        if let Some(p) = parent {
            self.nodes.get_mut(&p).unwrap().children.push(id);
            ev.push(Event::Created { id, kind, parent: p });
        }
        id
    }

    pub fn init() -> Model {
        let mut m = Model::default();
        let mut ev = vec![];
        let app = m.add(Kind::App, None, &mut ev);
        m.app = Some(app);
        m
    }

    fn spawn_into(&mut self, pane: Id, s: &itermkit::app::wire::Spawn) {
        let n = self.nodes.get_mut(&pane).unwrap();
        n.cwd = s.cwd.clone().unwrap_or_else(|| "/tmp".into());
        n.cmd = s.cmd.clone();
        n.name = s.name.clone().unwrap_or_default();
        for (k, v) in &s.vars {
            n.vars.insert(k.clone(), v.clone());
        }
    }

    fn new_pane_in_tab(&mut self, tab: Id, s: &itermkit::app::wire::Spawn, ev: &mut Vec<Event>) -> Id {
        let p = self.add(Kind::Pane, Some(tab), ev);
        self.spawn_into(p, s);
        self.nodes.get_mut(&tab).unwrap().layout = Some(Layout::Pane(p));
        p
    }

    fn new_window(&mut self, s: &itermkit::app::wire::Spawn, ev: &mut Vec<Event>) -> Vec<Id> {
        let app = self.app.unwrap();
        let w = self.add(Kind::Window, Some(app), ev);
        let v = self.add(Kind::VTab, Some(w), ev);
        let t = self.add(Kind::Tab, Some(v), ev);
        let p = self.new_pane_in_tab(t, s, ev);
        vec![w, v, t, p]
    }

    pub fn up(&self, mut id: Id, k: Kind) -> Option<Id> {
        loop {
            let n = self.nodes.get(&id)?;
            if n.kind == k {
                return Some(id);
            }
            id = n.parent?;
        }
    }

    fn kind_ok(&self, id: Id, k: Kind) -> Result<(), Fail> {
        match self.nodes.get(&id) {
            Some(n) if n.kind == k => Ok(()),
            _ => Err(fail(FailCode::NotFound, &format!("no {} {id}", k.name()))),
        }
    }

    /// The prompt rail for raw input (the adapter's `input::refuse_prompt`).
    fn no_prompt(&self, pane: Id) -> Result<(), Fail> {
        if itermkit::app::agent::classify(self.nodes[&pane].screen.iter().map(String::as_str)) == itermkit::app::agent::Screen::Prompt {
            return Err(fail(FailCode::Invalid, "the pane shows a permission or trust prompt: only the human answers it"));
        }
        Ok(())
    }

    fn insert(l: &mut Layout, at: Id, new: Id, vertical: bool, before: bool, ratio: f32) -> bool {
        match l {
            Layout::Pane(x) if *x == at => {
                let (a, b) = (Layout::Pane(at), Layout::Pane(new));
                let r = ratio;
                *l = Layout::Split { vertical, children: if before { vec![(r, b), (1.0 - r, a)] } else { vec![(1.0 - r, a), (r, b)] } };
                true
            }
            Layout::Pane(_) => false,
            Layout::Split { vertical: v, children } => {
                if let Some(i) = children.iter().position(|(_, c)| *c == Layout::Pane(at)) {
                    if *v == vertical {
                        let share = children[i].0;
                        children[i].0 = share * (1.0 - ratio);
                        children.insert(if before { i } else { i + 1 }, (share * ratio, Layout::Pane(new)));
                        return true;
                    }
                    return Self::insert(&mut children[i].1, at, new, vertical, before, ratio);
                }
                children.iter_mut().any(|(_, c)| Self::insert(c, at, new, vertical, before, ratio))
            }
        }
    }

    fn remove_leaf(l: Layout, id: Id) -> Option<Layout> {
        match l {
            Layout::Pane(x) if x == id => None,
            Layout::Pane(x) => Some(Layout::Pane(x)),
            Layout::Split { vertical, children } => {
                let kept: Vec<(f32, Layout)> = children.into_iter().filter_map(|(f, c)| Self::remove_leaf(c, id).map(|c| (f, c))).collect();
                match kept.len() {
                    0 => None,
                    1 => kept.into_iter().next().map(|(_, c)| c),
                    _ => {
                        let sum: f32 = kept.iter().map(|(f, _)| f).sum();
                        Some(Layout::Split { vertical, children: kept.into_iter().map(|(f, c)| (f / sum, c)).collect() })
                    }
                }
            }
        }
    }

    fn split(&mut self, at: Id, side: Side, ratio: Option<f32>, s: &itermkit::app::wire::Spawn, ev: &mut Vec<Event>) -> Result<Vec<Id>, Fail> {
        self.kind_ok(at, Kind::Pane)?;
        let tab = self.nodes[&at].parent.unwrap();
        let p = self.add(Kind::Pane, Some(tab), ev);
        self.spawn_into(p, s);
        self.place_beside(p, at, side, ratio);
        ev.push(Event::Layout { tab });
        Ok(vec![p])
    }

    fn place_beside(&mut self, p: Id, at: Id, side: Side, ratio: Option<f32>) {
        let tab = self.nodes[&at].parent.unwrap();
        let vertical = matches!(side, Side::Left | Side::Right);
        let before = matches!(side, Side::Left | Side::Up);
        let mut l = self.nodes[&tab].layout.clone().unwrap();
        Self::insert(&mut l, at, p, vertical, before, ratio.unwrap_or(0.5));
        self.nodes.get_mut(&tab).unwrap().layout = Some(l);
    }

    /// Detach a node from its parent (and a pane from its tab's layout), closing emptied parents.
    fn detach(&mut self, id: Id, ev: &mut Vec<Event>) {
        let Some(parent) = self.nodes[&id].parent else { return };
        let pn = self.nodes.get_mut(&parent).unwrap();
        pn.children.retain(|c| *c != id);
        if self.nodes[&id].kind == Kind::Pane {
            let l = self.nodes[&parent].layout.clone().unwrap();
            self.nodes.get_mut(&parent).unwrap().layout = Some(Self::remove_leaf(l, id).map(normalize).unwrap_or(Layout::Split { vertical: true, children: vec![] }));
            ev.push(Event::Layout { tab: parent });
        }
        if self.nodes[&parent].children.is_empty() && self.nodes[&parent].kind != Kind::App {
            self.close(parent, ev);
        }
    }

    pub fn close(&mut self, id: Id, ev: &mut Vec<Event>) {
        if !self.nodes.contains_key(&id) {
            return;
        }
        for c in self.nodes[&id].children.clone() {
            self.close_subtree(c, ev);
        }
        self.detach(id, ev);
        if let Some(n) = self.nodes.remove(&id) {
            ev.push(Event::Closed { id, kind: n.kind });
        }
    }

    fn close_subtree(&mut self, id: Id, ev: &mut Vec<Event>) {
        for c in self.nodes[&id].children.clone() {
            self.close_subtree(c, ev);
        }
        if let Some(n) = self.nodes.remove(&id) {
            ev.push(Event::Closed { id, kind: n.kind });
        }
    }

    fn reparent(&mut self, id: Id, to: Id, ev: &mut Vec<Event>) {
        self.detach(id, ev);
        self.nodes.get_mut(&id).unwrap().parent = Some(to);
        self.nodes.get_mut(&to).unwrap().children.push(id);
        ev.push(Event::Moved { id, parent: to });
    }

    fn rects(l: &Layout, x: f32, y: f32, w: f32, h: f32, out: &mut HashMap<Id, Rect>) {
        match l {
            Layout::Pane(id) => {
                out.insert(*id, Rect { x, y, w, h });
            }
            Layout::Split { vertical, children } => {
                let mut off = 0.0;
                for (f, c) in children {
                    if *vertical {
                        Self::rects(c, x + off * w, y, f * w, h, out);
                    } else {
                        Self::rects(c, x, y + off * h, w, f * h, out);
                    }
                    off += f;
                }
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut nodes = Vec::new();
        let mut rects = HashMap::new();
        for n in self.nodes.values() {
            if let Some(l) = &n.layout {
                Self::rects(l, 0.0, 0.0, 160.0, 48.0, &mut rects);
            }
        }
        fn walk(m: &Model, id: Id, parent: Option<u32>, rects: &HashMap<Id, Rect>, out: &mut Vec<Node>) {
            let n = &m.nodes[&id];
            let me = out.len() as u32;
            let rect = rects.get(&id).copied().or(n.frame);
            out.push(Node {
                id,
                kind: n.kind,
                parent,
                name: n.name.clone(),
                rect,
                created: n.created,
                vars: n.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                pane: (n.kind == Kind::Pane).then(|| PaneInfo {
                    pid: 1000 + id.0 as i32,
                    cwd: n.cwd.clone(),
                    title: String::new(),
                    job: n.cmd.as_deref().and_then(|c| c.split_whitespace().next()).unwrap_or("zsh").to_string(),
                    cols: rect.map(|r| r.w as u16).unwrap_or(80),
                    rows: rect.map(|r| r.h as u16).unwrap_or(24),
                    exit: n.exit,
                }),
                layout: n.layout.clone(),
            });
            for c in &n.children {
                walk(m, *c, Some(me), rects, out);
            }
        }
        walk(self, self.app.unwrap(), None, &rects, &mut nodes);
        let focus = match self.focus.filter(|f| self.nodes.contains_key(f)) {
            Some(p) => [Kind::Window, Kind::VTab, Kind::Tab, Kind::Pane].iter().filter_map(|k| self.up(p, *k)).collect(),
            None => vec![],
        };
        Snapshot { seq: 0, nodes, focus, app_version: "fake".into(), app_pid: self.pid.unwrap_or(std::process::id() as i32), broadcast: Default::default(), awake: "none".into() }
    }

    pub fn apply(&mut self, op: Op, ev: &mut Vec<Event>) -> Result<Done, Fail> {
        self.ops.push(op.clone());
        let ids = |v: Vec<Id>| Ok(Done { ids: v, lines: vec![] });
        match op {
            Op::NewWindow { spawn, .. } => ids(self.new_window(&spawn, ev)),
            Op::NewVTab { window, name, spawn, .. } => {
                let w = match window {
                    Some(w) => {
                        self.kind_ok(w, Kind::Window)?;
                        w
                    }
                    None => match self.focus.and_then(|f| self.up(f, Kind::Window)) {
                        Some(w) => w,
                        None => return ids(self.new_window(&spawn, ev)),
                    },
                };
                let v = self.add(Kind::VTab, Some(w), ev);
                self.nodes.get_mut(&v).unwrap().name = name.unwrap_or_default();
                let t = self.add(Kind::Tab, Some(v), ev);
                let p = self.new_pane_in_tab(t, &spawn, ev);
                ids(vec![v, t, p])
            }
            Op::NewTab { vtab, title, spawn, .. } => {
                let v = match vtab {
                    Some(v) => {
                        self.kind_ok(v, Kind::VTab)?;
                        v
                    }
                    None => self.focus.and_then(|f| self.up(f, Kind::VTab)).ok_or_else(|| fail(FailCode::Invalid, "no vertical tab"))?,
                };
                let t = self.add(Kind::Tab, Some(v), ev);
                self.nodes.get_mut(&t).unwrap().name = title.unwrap_or_default();
                let p = self.new_pane_in_tab(t, &spawn, ev);
                ids(vec![t, p])
            }
            Op::Split { at, side, ratio, spawn, focus } => {
                let made = self.split(at, side, ratio, &spawn, ev)?;
                // iTerm2 makes a new pane active; the adapter gives the focus back unless `focus`
                if focus {
                    self.focus = Some(made[0]);
                    ev.push(Event::Focus { id: made[0] });
                }
                ids(made)
            }
            Op::Close { ids: list } => {
                for id in list {
                    if !self.nodes.contains_key(&id) {
                        return Err(fail(FailCode::NotFound, &format!("no node {id}")));
                    }
                    self.close(id, ev);
                }
                ids(vec![])
            }
            Op::Set { id, field: Field::Buried(true) } => {
                // as iTerm2: the pane leaves its tab and the snapshot (it keeps running, out of sight)
                self.kind_ok(id, Kind::Pane)?;
                self.close(id, ev);
                ids(vec![])
            }
            Op::Set { id, field } => {
                let n = self.nodes.get_mut(&id).ok_or_else(|| fail(FailCode::NotFound, "no node"))?;
                match field {
                    Field::Name(s) => {
                        n.name = s.clone();
                        ev.push(Event::Renamed { id, name: s });
                    }
                    Field::Var(k, v) => {
                        match &v {
                            Some(x) => n.vars.insert(k.clone(), x.clone()),
                            None => n.vars.remove(&k),
                        };
                        ev.push(Event::Var { id, key: k, value: v });
                    }
                    Field::Color(c) => {
                        match c {
                            Some(c) => n.vars.insert("color".into(), c),
                            None => n.vars.remove("color"),
                        };
                    }
                    Field::Frame(r) => n.frame = Some(r),
                    Field::WindowState(st) => {
                        n.vars.insert("state".into(), format!("{st:?}").to_lowercase());
                    }
                    Field::Buried(_) => return Err(fail(FailCode::Unsupported, "a buried pane is not in the snapshot")),
                }
                ids(vec![])
            }
            Op::Select { id } => {
                let p = match self.nodes.get(&id).map(|n| n.kind) {
                    Some(Kind::Pane) => id,
                    Some(_) => {
                        let mut cur = id;
                        while self.nodes[&cur].kind != Kind::Pane {
                            cur = *self.nodes[&cur].children.first().ok_or_else(|| fail(FailCode::Invalid, "empty"))?;
                        }
                        cur
                    }
                    None => return Err(fail(FailCode::NotFound, "no node")),
                };
                self.focus = Some(p);
                ev.push(Event::Focus { id: p });
                ids(vec![])
            }
            Op::Move { id, to } => {
                let kind = self.nodes.get(&id).map(|n| n.kind).ok_or_else(|| fail(FailCode::NotFound, "no node"))?;
                match (kind, to) {
                    (Kind::Pane, Dest::Beside { at, side }) => {
                        self.kind_ok(at, Kind::Pane)?;
                        if at == id {
                            return Err(fail(FailCode::Invalid, "a pane does not move beside itself"));
                        }
                        let tab = self.nodes[&at].parent.unwrap();
                        self.reparent(id, tab, ev);
                        self.place_beside(id, at, side, None);
                    }
                    (Kind::Pane, Dest::NewTab) => {
                        let v = self.up(id, Kind::VTab).unwrap();
                        let t = self.add(Kind::Tab, Some(v), ev);
                        self.reparent(id, t, ev);
                        self.nodes.get_mut(&t).unwrap().layout = Some(Layout::Pane(id));
                    }
                    (Kind::Tab, Dest::Into(v)) => {
                        self.kind_ok(v, Kind::VTab)?;
                        self.reparent(id, v, ev);
                    }
                    (Kind::VTab, Dest::Into(w)) => {
                        self.kind_ok(w, Kind::Window)?;
                        self.reparent(id, w, ev);
                    }
                    (Kind::VTab, Dest::NewWindow) => {
                        let app = self.app.unwrap();
                        let w = self.add(Kind::Window, Some(app), ev);
                        self.reparent(id, w, ev);
                    }
                    (Kind::Tab, Dest::NewWindow | Dest::NewVTab) => {
                        let w = self.up(id, Kind::Window).unwrap();
                        let w = if matches!(self.ops.last(), Some(Op::Move { to: Dest::NewWindow, .. })) {
                            let app = self.app.unwrap();
                            self.add(Kind::Window, Some(app), ev)
                        } else {
                            w
                        };
                        let v = self.add(Kind::VTab, Some(w), ev);
                        self.reparent(id, v, ev);
                    }
                    _ => return Err(fail(FailCode::Unsupported, "that move")),
                }
                ids(vec![])
            }
            Op::Order { ids: list } => {
                let parent = list.first().and_then(|i| self.nodes.get(i)).and_then(|n| n.parent).ok_or_else(|| fail(FailCode::NotFound, "no node"))?;
                if list.iter().any(|i| self.nodes.get(i).and_then(|n| n.parent) != Some(parent)) {
                    return Err(fail(FailCode::Invalid, "siblings only"));
                }
                let p = self.nodes.get_mut(&parent).unwrap();
                let rest: Vec<Id> = p.children.iter().copied().filter(|c| !list.contains(c)).collect();
                p.children = list.into_iter().chain(rest).collect();
                ids(vec![])
            }
            Op::SetLayout { tab, layout } => {
                self.kind_ok(tab, Kind::Tab)?;
                let (mut a, mut b) = (vec![], vec![]);
                layout.panes(&mut a);
                self.nodes[&tab].layout.as_ref().unwrap().panes(&mut b);
                a.sort();
                b.sort();
                if a != b {
                    return Err(fail(FailCode::Invalid, "a layout must hold exactly the tab's panes"));
                }
                // iTerm2 keeps its own rules whatever it is sent (the adapter's `Plan::normalize`)
                self.nodes.get_mut(&tab).unwrap().layout = Some(normalize(layout));
                ev.push(Event::Layout { tab });
                ids(vec![])
            }
            Op::Merge { into, from } => {
                for w in from {
                    for v in self.nodes[&w].children.clone() {
                        self.reparent(v, into, ev);
                    }
                }
                ids(vec![])
            }
            Op::Send { pane, bytes, .. } => {
                self.kind_ok(pane, Kind::Pane)?;
                self.no_prompt(pane)?;
                let n = self.nodes.get_mut(&pane).unwrap();
                n.sent.extend_from_slice(&bytes);
                ids(vec![])
            }
            // the app's delivery engine: the text, then Enter, as one delivery
            Op::Deliver { pane, text, enter } => {
                self.kind_ok(pane, Kind::Pane)?;
                // the rail: a pane showing a permission or trust prompt gets nothing; its message is held, in order
                let prompt = itermkit::app::agent::classify(self.nodes[&pane].screen.iter().map(String::as_str)) == itermkit::app::agent::Screen::Prompt;
                let n = self.nodes.get_mut(&pane).unwrap();
                if prompt || !n.held.is_empty() {
                    n.held.push((text, enter));
                    return Ok(Done { ids: vec![pane], lines: vec!["queued reason=prompt".into()] });
                }
                n.sent.extend_from_slice(text.as_bytes());
                if enter {
                    n.sent.push(b'\r');
                }
                if self.turn_burst > 0 {
                    let state = |v: &str| Event::Var { id: pane, key: "agent.state".into(), value: Some(v.into()) };
                    ev.push(state("busy"));
                    ev.extend(std::iter::repeat_n(Event::Bell { pane }, self.turn_burst));
                    n.vars.insert("agent.state".into(), "idle".into());
                    ev.push(state("idle"));
                }
                Ok(Done { ids: vec![pane], lines: vec![format!("delivered bytes={}", text.len())] })
            }
            Op::Key { pane, key } => {
                self.kind_ok(pane, Kind::Pane)?;
                // the adapter's own rule: only a plain esc or ctrl-c (no shift, alt or cmd) passes a prompt
                if !itermkit::iterm2::input::cancels(key) {
                    self.no_prompt(pane)?;
                }
                let n = self.nodes.get_mut(&pane).unwrap();
                n.sent.extend_from_slice(format!("<{key:?}>").as_bytes());
                ids(vec![])
            }
            Op::Read { pane, lines } => {
                self.kind_ok(pane, Kind::Pane)?;
                let s = &self.nodes[&pane].screen;
                let out = match lines {
                    Lines::Screen => s.clone(),
                    Lines::Tail(n) => s[s.len().saturating_sub(n as usize)..].to_vec(),
                    Lines::Range(a, b) => s.get(a as usize..(b as usize).min(s.len())).unwrap_or(&[]).to_vec(),
                };
                Ok(Done { ids: vec![], lines: out })
            }
            Op::Scroll { pane, .. } | Op::Clear { pane, .. } => {
                self.kind_ok(pane, Kind::Pane)?;
                ids(vec![])
            }
            Op::Quit => ids(vec![]),
            // the app decides the targets; the fake echoes the picked panes and the mode
            Op::Broadcast { mode, add, .. } => {
                Ok(Done { ids: add, lines: vec![mode.map_or("off", |m| m.name()).into(), String::new()] })
            }
            // the app ranks; the fake echoes the query as one row, `limit` times at most
            Op::Search { query, limit } => {
                Ok(Done { ids: vec![], lines: std::iter::repeat_n(format!("Actions\t{query}\t⌘X\t"), (limit as usize).min(2)).collect() })
            }
            // the control plane: fixed answers in the app's formats
            Op::Actions => Ok(Done { ids: vec![], lines: vec!["vtab N\topt+N\tSelect vertical tab N".into(), "zoom\tshift+cmd+return\tMaximize the pane".into(), "help\t\tThis guide".into()] }),
            Op::Action { name, pane } => {
                if let Some(p) = pane {
                    self.kind_ok(p, Kind::Pane)?;
                }
                Ok(Done { ids: pane.into_iter().collect(), lines: vec![name] })
            }
            Op::WindowCells { window, cols, rows, .. } => {
                self.kind_ok(window, Kind::Window)?;
                Ok(Done { ids: vec![], lines: vec![format!("{}x{}", cols.unwrap_or(80), rows.unwrap_or(24))] })
            }
            Op::Content { pane, act } => {
                self.kind_ok(pane, Kind::Pane)?;
                use itermkit::app::wire::ContentAct as A;
                let lines = match act {
                    A::Get => vec!["sel\t\thello".into(), "mark\t2\t".into(), "mark\t5\tbuild".into()],
                    A::Select(_) => vec!["sel\t1:0-1:4\thello".into()],
                    A::Copy(_) => vec!["hello".into()],
                    A::Mark { .. } => vec!["7".into()],
                    A::Jump(_) => vec![],
                    A::Open(_) => vec!["https://example.com/".into()],
                    A::FindBar(_) => vec![],
                };
                Ok(Done { ids: vec![pane], lines })
            }
            Op::Menu { context, item } => match item {
                None => Ok(Done { ids: vec![], lines: vec![format!("Edit > Copy\tcmd+c{}", context.map_or(String::new(), |c| format!(" @{c}")))] }),
                Some(i) if i.eq_ignore_ascii_case("copy") => Ok(Done { ids: vec![], lines: vec!["Edit > Copy".into()] }),
                Some(i) => Err(fail(FailCode::Invalid, &format!("no menu item {i:?}"))),
            },
            Op::UiList => {
                self.ui_lists += 1;
                Ok(Done { ids: vec![], lines: self.ui.clone().unwrap_or_else(|| vec!["812\talert\tClose and end the running job?\tClose|Cancel".into()]) })
            }
            Op::UiAnswer { id, button } => {
                if id != 812 {
                    return Err(fail(FailCode::Invalid, "no dialog"));
                }
                Ok(Done { ids: vec![], lines: vec![button] })
            }
        }
    }
}

/// iTerm2's split-tree rules, as the adapter states them (`iterm2/shape/layout.rs`, `Plan::normalize`): no
/// one-child split, no split inside a split of the same axis (flattened, shares multiplied), shares summing to 1.
/// The fake keeps its tree this way after every change, so a test never passes on a shape iTerm2 can't hold.
pub fn normalize(l: Layout) -> Layout {
    let Layout::Split { vertical, children } = l else { return l };
    let sum: f32 = children.iter().map(|(f, _)| f.max(0.0)).sum();
    let n = children.len().max(1) as f32;
    let mut flat: Vec<(f32, Layout)> = Vec::new();
    for (f, c) in children {
        let f = if sum > 0.0 && f > 0.0 { f / sum } else { 1.0 / n };
        match normalize(c) {
            Layout::Split { vertical: v, children: inner } if v == vertical => flat.extend(inner.into_iter().map(|(g, c)| (f * g, c))),
            other => flat.push((f, other)),
        }
    }
    if flat.len() == 1 {
        return flat.pop().unwrap().1;
    }
    Layout::Split { vertical, children: flat }
}

/// A tab's split tree as text: `[a | b]` side by side, `[a / b]` stacked, panes by their number, shares in
/// parentheses rounded to hundredths.
pub fn show(l: &Layout) -> String {
    match l {
        Layout::Pane(id) => id.0.to_string(),
        Layout::Split { vertical, children } => {
            let parts: Vec<String> = children.iter().map(|(f, c)| format!("{}({:.2})", show(c), f)).collect();
            format!("[{}]", parts.join(if *vertical { " | " } else { " / " }))
        }
    }
}

#[derive(Clone)]
pub struct Fake {
    pub m: Arc<Mutex<Model>>,
    pub hub: Hub,
}

/// The fake's settings: three keys, the app's way (validate all, then apply).
pub static SETTINGS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
const SKEYS: [(&str, &str); 4] = [("font", "MesloLGS NF"), ("font_size", "14"), ("keys.new_vtab", "default"), ("profile", "Default")];

impl App for Fake {
    fn snapshot(&mut self) -> Result<Snapshot, Fail> {
        Ok(self.hub.snapshot())
    }

    fn exits(&mut self, ids: Vec<Id>) -> Result<Vec<(Id, Option<i32>)>, Fail> {
        Ok(self.hub.exits(&ids))
    }

    fn watch(&mut self, since: Option<u64>) -> Result<(Box<dyn Events>, Snapshot), Fail> {
        let (w, snap) = self.hub.watch(since);
        Ok((Box::new(w), snap))
    }

    fn call(&mut self, op: Op, wait: Option<std::time::Duration>) -> Result<Done, Fail> {
        // a pane with the var `fake.stall=MS` answers reads and deliveries that late, as the app does while its main thread is
        // stuck (the model stays free meanwhile, as the app's snapshot does)
        if let Op::Read { pane, .. } | Op::Deliver { pane, .. } = &op {
            let stall = self.m.lock().unwrap().nodes.get(pane).and_then(|n| n.vars.get("fake.stall")).and_then(|v| v.parse::<u64>().ok());
            if let Some(ms) = stall {
                let stall = std::time::Duration::from_millis(ms);
                if let Some(w) = wait.filter(|w| *w < stall) {
                    std::thread::sleep(w);
                    return Err(fail(FailCode::Internal, &format!("iTerm2 is not responding: no answer in {} s", w.as_secs_f32())));
                }
                std::thread::sleep(stall);
            }
        }
        let mut ev = vec![];
        // a held model is a main thread that never answers: give up after `wait`, as the live adapter does
        let end = wait.map(|w| std::time::Instant::now() + w);
        let mut m = loop {
            match self.m.try_lock() {
                Ok(m) => break m,
                Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if end.is_some_and(|e| std::time::Instant::now() >= e) {
                        return Err(fail(FailCode::Internal, &format!("iTerm2 is not responding: no answer in {} s", wait.unwrap_or_default().as_secs_f32())));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        };
        let read = matches!(op, Op::Read { .. } | Op::Actions | Op::UiList | Op::Menu { item: None, .. });
        let r = m.apply(op, &mut ev);
        if !read {
            let snap = m.snapshot();
            drop(m);
            self.hub.publish(snap, ev);
        }
        r
    }

    fn setting(&mut self, op: itermkit::app::wire::SettingOp) -> Result<Done, Fail> {
        use itermkit::app::wire::SettingOp as S;
        let mut st = SETTINGS.lock().unwrap();
        let known = |k: &str| SKEYS.iter().any(|(x, _)| *x == k);
        let line = |v: serde_json::Value| Ok(Done { ids: vec![], lines: vec![v.to_string()] });
        match op {
            S::Keys => Ok(Done { ids: vec![], lines: SKEYS.iter().map(|(k, d)| serde_json::json!({"key": k, "default": d}).to_string()).collect() }),
            S::Get { keys } => {
                let mut o = serde_json::Map::new();
                for (k, d) in SKEYS.iter().filter(|(k, _)| keys.is_empty() || keys.iter().any(|x| x == k)) {
                    o.insert(k.to_string(), serde_json::json!(st.get(*k).cloned().unwrap_or(d.to_string())));
                }
                // iTerm2's color presets (`presets`, `preset:NAME`) and a named profile's keys (`@NAME`)
                for k in &keys {
                    if k == "presets" {
                        o.insert(k.clone(), serde_json::json!(["Solarized Dark", "Tango Light"]));
                    } else if let Some(n) = k.strip_prefix("preset:") {
                        let bg = match n.to_lowercase().as_str() {
                            "solarized dark" => "#002b36",
                            "tango light" => "#ffffff",
                            _ => return Err(fail(FailCode::Invalid, &format!("no color preset {n:?}"))),
                        };
                        o.insert(k.clone(), serde_json::json!({"background": bg, "ansi.1": "#dc322f"}));
                    } else if let Some(n) = k.strip_prefix('@') {
                        let known = n == "Default" || st.get("profiles_seen").is_some_and(|s| s.split(',').any(|x| x == n));
                        if !known {
                            return Err(fail(FailCode::Invalid, &format!("no profile '{n}'")));
                        }
                        o.insert("Background Color".into(), serde_json::json!("#000000"));
                        o.insert("Normal Font".into(), serde_json::json!("MesloLGS NF 14"));
                    }
                }
                if keys.iter().any(|k| k == "profiles") {
                    // the profiles ever switched to (the app's copy-on-switch)
                    let mut p: Vec<String> = vec!["Default".into()];
                    p.extend(st.get("profiles_seen").map(|s| s.split(',').map(String::from).collect::<Vec<_>>()).unwrap_or_default());
                    p.sort();
                    p.dedup();
                    o.insert("profiles".into(), serde_json::json!(p));
                }
                line(serde_json::Value::Object(o))
            }
            S::Set { pairs } => {
                // profile_delete=NAME: the app's delete_profile (the one in use falls back to Default)
                if let Some((_, name)) = pairs.iter().find(|(k, _)| k == "profile_delete") {
                    let seen: Vec<String> = st.get("profiles_seen").map(|s| s.split(',').map(String::from).collect()).unwrap_or_default();
                    if name == "Default" {
                        return Err(fail(FailCode::Invalid, &format!("'{name}' is built in and can't be deleted")));
                    }
                    if !seen.contains(name) {
                        return Err(fail(FailCode::Invalid, &format!("no profile '{name}' (iterm profile list)")));
                    }
                    let rest: Vec<String> = seen.into_iter().filter(|n| n != name).collect();
                    st.insert("profiles_seen".into(), rest.join(","));
                    if st.get("profile") == Some(name) {
                        st.insert("profile".into(), "Default".into());
                    }
                    return line(serde_json::json!({"profile_delete": name, "_live": true}));
                }
                // profile_new=NAME: a new profile (a copy of the one in use), not switched to
                if let Some((_, name)) = pairs.iter().find(|(k, _)| k == "profile_new") {
                    let seen = st.get("profiles_seen").map_or(name.clone(), |s| format!("{s},{name}"));
                    st.insert("profiles_seen".into(), seen);
                    return line(serde_json::json!({"profile_new": name, "_live": true}));
                }
                // KEY@NAME: a key of another profile
                let known = |k: &str| known(k.split_once('@').map_or(k, |(k, _)| k));
                if let Some((k, _)) = pairs.iter().find(|(k, _)| !known(k)) {
                    return Err(fail(FailCode::Invalid, &format!("no setting '{k}'; closest: font")));
                }
                let mut o = serde_json::Map::new();
                for (k, v) in pairs {
                    if k == "profile" {
                        let seen = st.get("profiles_seen").map_or(v.clone(), |s| format!("{s},{v}"));
                        st.insert("profiles_seen".into(), seen);
                    }
                    st.insert(k.clone(), v.clone());
                    o.insert(k, serde_json::json!(v));
                }
                o.insert("_live".into(), serde_json::json!(true));
                line(serde_json::Value::Object(o))
            }
            S::Reset { keys } => {
                let n = if keys.is_empty() { std::mem::take(&mut *st).len() } else { keys.iter().filter(|k| st.remove(*k).is_some()).count() };
                line(serde_json::json!({"reset": n, "_live": true}))
            }
        }
    }
}

pub struct World {
    pub m: Arc<Mutex<Model>>,
    pub hub: Hub,
    pub dir: PathBuf,
}

static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn world(tag: &str) -> World {
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pt-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let m = Arc::new(Mutex::new(Model::init()));
    let hub = Hub::new(m.lock().unwrap().snapshot());
    World { m, hub, dir }
}

impl World {
    pub fn env(&self, me: Option<Id>) -> Env {
        let fake = Fake { m: self.m.clone(), hub: self.hub.clone() };
        Env {
            socket: self.dir.join("socket"),
            connect: Arc::new(move |_: &Env| Ok(Box::new(fake.clone()) as Box<dyn App>)),
            me,
            state: self.dir.join("state"),
            agent: me.is_some(),
            cwd: self.dir.clone(),
            call_timeout: itermkit::conn::CALL_WAIT,
        }
    }
    /// A human outside iTerm2 (no pane, no agent environment).
    pub fn human(&self) -> Env {
        let mut e = self.env(None);
        e.agent = false;
        e
    }
    /// Change the model directly (as the app's own UI would) and publish.
    pub fn with<T>(&self, f: impl FnOnce(&mut Model, &mut Vec<Event>) -> T) -> T {
        let mut ev = vec![];
        let mut m = self.m.lock().unwrap();
        let out = f(&mut m, &mut ev);
        let snap = m.snapshot();
        drop(m);
        self.hub.publish(snap, ev);
        out
    }
    /// A window with one pane, opened by the human: the pane's ID.
    pub fn window(&self) -> Id {
        self.with(|m, ev| {
            let ids = m.new_window(&Default::default(), ev);
            m.focus = Some(ids[3]);
            ids[3]
        })
    }
    pub fn screen(&self, pane: Id, lines: &[&str]) {
        self.with(|m, _| {
            let n = m.nodes.get_mut(&pane).unwrap();
            n.screen = lines.iter().map(|s| s.to_string()).collect();
            // the app's drainer: held messages land, in order, once no prompt shows
            if itermkit::app::agent::classify(n.screen.iter().map(String::as_str)) != itermkit::app::agent::Screen::Prompt {
                for (text, enter) in std::mem::take(&mut n.held) {
                    n.sent.extend_from_slice(text.as_bytes());
                    if enter {
                        n.sent.push(b'\r');
                    }
                }
            }
        });
    }
    /// The pane's process ends with `status` (the app reports it, then closes the pane).
    pub fn exit(&self, pane: Id, status: i32) {
        self.with(|m, ev| {
            m.nodes.get_mut(&pane).unwrap().exit = Some(status);
            ev.push(Event::Exited { pane, status });
        });
        self.with(|m, ev| m.close(pane, ev));
    }
    pub fn node(&self, id: Id) -> Option<FNode> {
        self.m.lock().unwrap().nodes.get(&id).cloned()
    }
    /// The split tree of `pane`'s tab, as [`show`] writes it.
    pub fn shape_of(&self, pane: Id) -> String {
        let m = self.m.lock().unwrap();
        let tab = m.nodes[&pane].parent.unwrap();
        show(m.nodes[&tab].layout.as_ref().unwrap())
    }
    /// The panes in `pane`'s tab, in reading order.
    pub fn row(&self, pane: Id) -> Vec<Id> {
        let m = self.m.lock().unwrap();
        let tab = m.nodes[&pane].parent.unwrap();
        let mut v = Vec::new();
        m.nodes[&tab].layout.as_ref().unwrap().panes(&mut v);
        v
    }
    pub fn panes(&self) -> Vec<Id> {
        let m = self.m.lock().unwrap();
        let mut v: Vec<Id> = m.nodes.iter().filter(|(_, n)| n.kind == Kind::Pane).map(|(i, _)| *i).collect();
        v.sort();
        v
    }
}

/// Runs `f` on its own thread and waits at most `secs` for it (None: it hung): a test of a hang must not hang.
pub fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(secs)).ok()
}

/// Run `iterm ARGS…` in-process: (exit code, rows as JSON).
pub fn run(env: Env, args: &str) -> (i32, Vec<Value>) {
    run_in(env, args, "")
}

pub fn run_in(env: Env, args: &str, stdin: &str) -> (i32, Vec<Value>) {
    let mut argv = itermkit::cli::shell_words(args).unwrap();
    argv.push("--json".into());
    let mut out = Vec::new();
    let code = itermkit::cli::run(&argv, env, &mut stdin.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    // the final rows are one JSON array; streamed rows (watch) are one object per line before it
    let mut rows = Vec::new();
    for line in text.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(Value::Array(a)) => rows.extend(a),
            Ok(v) => rows.push(v),
            Err(_) => rows.push(Value::String(line.to_string())),
        }
    }
    (code, rows)
}

pub fn text(env: Env, args: &str) -> (i32, String) {
    let argv = itermkit::cli::shell_words(args).unwrap();
    let mut out = Vec::new();
    let code = itermkit::cli::run(&argv, env, &mut &b""[..], &mut out);
    (code, String::from_utf8(out).unwrap())
}

pub fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

pub fn codes(rows: &[Value]) -> Vec<String> {
    rows.iter().map(|r| format!("{} {}", s(r, "status"), s(r, "code"))).collect()
}

pub fn pid(v: &Value) -> Id {
    itermkit::app::model::parse_handle(s(v, "id")).unwrap().1
}
