//! `<noun> watch [SEL] [--until COND] [--timeout S] [--kind K…] [--since SEQ]` and `<noun> wait`
//! (`watch --until`, default `exit`): one subscription to the app's pushed events, blocking on the
//! socket (no polling, no CPU while nothing happens). Each event is one row, streamed as it comes
//! (NDJSON with `--json`). A watcher that falls behind gets one `gap` row; `--since SEQ` replays
//! from the app's ring.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::app::model::handle;
use crate::app::wire::Lines;
use crate::app::{Event, Id, Kind as PKind, Op, Pushed, Snapshot};
use serde_json::json;

use crate::exec::Exec;
use crate::grammar::{Kind, Noun, Request, Verb};
use crate::out::{from_error, ok, Error, Obj, Result};
use crate::view::{self, View};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cmp {
    Eq,
    Ge,
    Le,
}

/// What `--until` waits for, typed.
#[derive(Debug, Clone)]
pub enum Cond {
    /// Every selected node is gone.
    Gone,
    /// A new node appeared under the selection.
    New,
    /// Every selected pane has exited (or closed).
    Exit,
    /// Every selected pane is in one of these states (read from its screen when it goes quiet).
    State(Vec<String>),
    /// The selector names this many nodes.
    Count(Cmp, usize),
    /// A selected pane's screen matches.
    Screen(regex::Regex),
    /// One event of this name (`bell`, `focus`, `title`, …).
    Event(String),
    Change,
}

const EVENTS: [&str; 15] = ["created", "closed", "exited", "focus", "renamed", "title", "cwd", "var", "layout", "moved", "bell", "prompt", "quiet", "notify", "ui"];

pub fn parse_cond(s: &str) -> Result<Cond> {
    let s = s.trim();
    let bad = || Error::invalid(format!("--until {s}: gone | new | exit | ready | idle | busy | prompt | done | state=S[,S] | count>=N | count=N | count<=N | screen~RE | change | an event ({})", EVENTS.join(" ")));
    Ok(match s {
        "gone" => Cond::Gone,
        "new" => Cond::New,
        "exit" | "exited" => Cond::Exit,
        // the app's agent states (agent.state); a finished agent or an exited pane is done
        "done" => Cond::State(vec!["done".into(), "failed".into(), "exited".into()]),
        // ready: at its input box (fresh or after a turn); idle: a turn finished
        "ready" => Cond::State(vec!["ready".into(), "idle".into()]),
        "idle" => Cond::State(vec!["idle".into()]),
        "busy" => Cond::State(vec!["busy".into()]),
        "prompt" => Cond::State(vec!["prompt".into()]),
        "change" | "any" => Cond::Change,
        _ if EVENTS.contains(&s) => Cond::Event(s.to_string()),
        _ => {
            if let Some(v) = s.strip_prefix("state=") {
                Cond::State(v.split(',').map(|x| x.trim().to_ascii_lowercase()).filter(|x| !x.is_empty()).collect())
            } else if let Some(re) = s.strip_prefix("screen~") {
                Cond::Screen(regex::Regex::new(re).map_err(|e| Error::invalid(format!("--until screen~: {e}")))?)
            } else if let Some(rest) = s.strip_prefix("count") {
                let (cmp, n) = if let Some(n) = rest.strip_prefix(">=") {
                    (Cmp::Ge, n)
                } else if let Some(n) = rest.strip_prefix("<=") {
                    (Cmp::Le, n)
                } else if let Some(n) = rest.strip_prefix('=') {
                    (Cmp::Eq, n)
                } else {
                    return Err(bad());
                };
                Cond::Count(cmp, n.trim().parse().map_err(|_| bad())?)
            } else {
                return Err(bad());
            }
        }
    })
}

pub fn timeout_of(r: &Request) -> Result<Option<Duration>> {
    r.p.duration("--timeout").map_err(Error::invalid)
}

/// An event as a row.
fn row(p: &Pushed, kinds: &HashMap<Id, PKind>) -> Obj {
    let h = |id: &Id| kinds.get(id).map(|k| handle(*k, *id)).unwrap_or_else(|| id.0.to_string());
    let mut o = crate::obj! {"seq" => p.seq, "ms" => p.ms, "event" => p.ev.name()};
    if let Some(id) = p.ev.subject() {
        o.insert("id".into(), json!(h(&id)));
    }
    match &p.ev {
        Event::Created { parent, .. } | Event::Moved { parent, .. } => {
            o.insert("parent".into(), json!(h(parent)));
        }
        Event::Exited { status, .. } => {
            o.insert("exit".into(), json!(status));
        }
        Event::Renamed { name, .. } => {
            o.insert("name".into(), json!(name));
        }
        Event::Title { title, .. } => {
            o.insert("title".into(), json!(title));
        }
        Event::Cwd { cwd, .. } => {
            o.insert("cwd".into(), json!(cwd));
        }
        Event::Var { key, value, .. } => {
            o.insert("key".into(), json!(key));
            o.insert("value".into(), json!(value));
        }
        Event::Prompt { mark, status, .. } => {
            o.insert("mark".into(), json!(mark.to_string()));
            o.insert("status".into(), json!(status));
        }
        Event::Notify { title, body, .. } => {
            o.insert("title".into(), json!(title));
            o.insert("body".into(), json!(body));
        }
        Event::Gap { missed } => {
            o.insert("missed".into(), json!(missed));
        }
        Event::Ui { window } => {
            o.insert("window".into(), json!(window));
        }
        _ => {}
    }
    ok("event", o)
}

/// What a watch has seen of the nodes it watches: the selection and everything under it (it grows with created
/// nodes), their kinds (for handles), and which of them closed or exited.
struct Watched {
    /// The selected nodes.
    picked: Vec<Id>,
    /// The selected panes, and the panes under the selected containers.
    panes: HashSet<Id>,
    /// The selection and everything under it.
    set: HashSet<Id>,
    kinds: HashMap<Id, PKind>,
    gone: HashSet<Id>,
    exited: HashSet<Id>,
}

impl Watched {
    fn new(snap: &Snapshot, v: &View, picked: &[usize]) -> Watched {
        let mut set: HashSet<Id> = HashSet::new();
        for &i in picked {
            set.extend(v.m.subtree(i).map(|j| v.id(j)));
        }
        let panes: HashSet<Id> = picked.iter().flat_map(|i| v.m.project(*i, Some(Kind::Pane))).map(|i| v.id(i)).collect();
        let exited = snap.nodes.iter().filter(|n| n.pane.as_ref().is_some_and(|p| p.exit.is_some()) && panes.contains(&n.id)).map(|n| n.id).collect();
        Watched {
            picked: picked.iter().map(|i| v.id(*i)).collect(),
            panes,
            set,
            kinds: snap.nodes.iter().map(|n| (n.id, n.kind)).collect(),
            gone: HashSet::new(),
            exited,
        }
    }

    /// Take in one event: a node created under the set joins it; a close or exit in the set is recorded.
    /// Whether the event concerns the set (`whole`: every event does).
    fn take(&mut self, ev: &Event, whole: bool) -> bool {
        if let Event::Created { id, kind, parent } = ev {
            self.kinds.insert(*id, *kind);
            if self.set.contains(parent) {
                self.set.insert(*id);
            }
        }
        let relevant = whole || matches!(ev, Event::Gap { .. }) || ev.subject().is_some_and(|s| self.set.contains(&s));
        if relevant {
            match ev {
                Event::Closed { id, .. } => {
                    self.gone.insert(*id);
                }
                Event::Exited { pane, .. } => {
                    self.exited.insert(*pane);
                }
                _ => {}
            }
        }
        relevant
    }

    /// After a gap (dropped events), catch up from the present: nodes now under the set join it, exited panes
    /// and nodes no longer there are recorded. Whether a node is new.
    fn catch_up(&mut self, s: &Snapshot) -> bool {
        let mut under = vec![false; s.nodes.len()];
        let mut new = false;
        for (i, n) in s.nodes.iter().enumerate() {
            // tree order: a parent comes before its children
            under[i] = self.set.contains(&n.id) || n.parent.is_some_and(|p| under.get(p as usize).copied().unwrap_or(false));
            if under[i] && !self.kinds.contains_key(&n.id) {
                new = true;
                self.set.insert(n.id);
                self.kinds.insert(n.id, n.kind);
            }
            if self.panes.contains(&n.id) && n.pane.as_ref().is_some_and(|x| x.exit.is_some()) {
                self.exited.insert(n.id);
            }
        }
        let live: HashSet<Id> = s.nodes.iter().map(|n| n.id).collect();
        self.gone.extend(self.set.iter().filter(|i| !live.contains(i)));
        new
    }

    /// `--until gone`: every selected node closed.
    fn all_gone(&self) -> bool {
        self.picked.iter().all(|i| self.gone.contains(i))
    }

    /// `--until exit`: every watched pane exited or closed.
    fn all_exited(&self) -> bool {
        self.panes.iter().all(|i| self.exited.contains(i) || self.gone.contains(i))
    }
}

impl Exec<'_> {
    /// `<noun> watch|wait`: stream the events of the selection (and what is under it) as rows until `--until`
    /// is met (one `until` row), or the timeout (a timeout row; a plain watch ends with `watched events=N`).
    pub fn watch(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let until = match (r.p.one("--until"), r.verb) {
            (Some(u), _) => Some(parse_cond(u)?),
            (None, Verb::Wait) => Some(Cond::Exit),
            (None, _) => None,
        };
        let kinds_filter: Vec<String> = r.p.many("--kind");
        let since = r.p.one("--since").map(|x| x.parse::<u64>().map_err(|_| Error::invalid("--since SEQ"))).transpose()?;
        let timeout = timeout_of(r)?;
        let deadline = timeout.map(|t| Instant::now() + t);
        let (mut w, snap) = self.conn.watch(since, timeout)?;
        let me = self.conn.env().me;
        let mut v = View::new(&snap, me, &self.binds);
        // with no selection, a plain watch sees every event
        let whole = r.noun == Noun::App || (r.sel.is_none() && !r.all && r.within.is_none() && r.wheres.is_empty() && matches!(r.verb, Verb::Watch));
        let picked = if r.noun == Noun::App { vec![0] } else { self.resolve(r, &snap, &mut v)? };
        let mut seen = Watched::new(&snap, &v, &picked);
        if until.is_some() && seen.picked.is_empty() && !matches!(until, Some(Cond::Count(..)) | Some(Cond::New)) {
            return Err(Error::invalid(format!("{} {}: the selector names nothing", r.noun, r.verb)));
        }
        // conditions already true end at once
        if let Some(c) = &until {
            if let Some(rows) = self.met_now(c, r, &snap, &seen.panes, &seen.exited)? {
                return Ok(rows);
            }
        }
        let mut shown_n = 0usize;
        loop {
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left.is_some_and(|l| l.is_zero()) {
                break;
            }
            w.set_deadline(left)?;
            let Some(p) = w.next()? else { break };
            if !seen.take(&p.ev, whole) {
                continue;
            }
            let ev_row = row(&p, &seen.kinds);
            let Some(c) = &until else {
                if kinds_filter.is_empty() || kinds_filter.iter().any(|k| k == p.ev.name()) {
                    shown_n += 1;
                    (self.sink)(&ev_row);
                }
                continue;
            };
            if self.met(c, r, &p.ev, &mut seen)? {
                let mut o = ev_row;
                o.insert("code".into(), json!("until"));
                return Ok(vec![o]);
            }
        }
        if until.is_some() {
            return Ok(vec![from_error(&Error::Timeout(format!("--until {} not met", r.p.one("--until").unwrap_or("exit"))), crate::obj! {})]);
        }
        Ok(vec![ok("watched", crate::obj! {"events" => shown_n})])
    }

    /// Whether `--until c` is met after event `ev`. States, screens and counts are read from the present when an
    /// event may have changed them; after a gap, gone, exit and new are too (it may have dropped the very event).
    fn met(&mut self, c: &Cond, r: &Request, ev: &Event, seen: &mut Watched) -> Result<bool> {
        let gap = matches!(ev, Event::Gap { .. });
        Ok(match c {
            Cond::Gone | Cond::Exit | Cond::New if gap => {
                self.conn.invalidate();
                let new = seen.catch_up(self.conn.snapshot()?);
                match c {
                    Cond::Gone => seen.all_gone(),
                    Cond::Exit => seen.all_exited(),
                    _ => new,
                }
            }
            Cond::Change => true,
            Cond::Event(name) => ev.name() == name,
            Cond::New => matches!(ev, Event::Created { .. }),
            Cond::Gone => seen.all_gone(),
            Cond::Exit => seen.all_exited(),
            Cond::State(_) | Cond::Screen(_) if matches!(ev, Event::Var { .. } | Event::Quiet { .. } | Event::Prompt { .. } | Event::Exited { .. } | Event::Gap { .. }) => self.met_in_present(c, r, seen)?,
            Cond::Count(..) if matches!(ev, Event::Created { .. } | Event::Closed { .. } | Event::Gap { .. }) => self.met_in_present(c, r, seen)?,
            Cond::State(_) | Cond::Screen(_) | Cond::Count(..) => false,
        })
    }

    /// [`Exec::met_now`] against a fresh snapshot.
    fn met_in_present(&mut self, c: &Cond, r: &Request, seen: &Watched) -> Result<bool> {
        self.conn.invalidate();
        let s = self.conn.snapshot()?.clone();
        Ok(self.met_now(c, r, &s, &seen.panes, &seen.exited)?.is_some())
    }

    /// The condition, checked against the present (screen reads for states and screens).
    fn met_now(&mut self, c: &Cond, r: &Request, s: &Snapshot, panes: &HashSet<Id>, exited: &HashSet<Id>) -> Result<Option<Vec<Obj>>> {
        let done = |code: &str, o: Obj| Some(vec![ok(code, o)]);
        Ok(match c {
            Cond::Exit => {
                let here: HashSet<Id> = s.nodes.iter().map(|n| n.id).collect();
                let live = panes.iter().any(|p| !exited.contains(p) && here.contains(p));
                (!live).then(|| vec![ok("until", crate::obj! {"until" => "exit"})])
            }
            Cond::State(want) => {
                let mut v = View::new(s, self.conn.env().me, &self.binds);
                // an exited or closed pane counts as "exited"
                let finished = want.iter().any(|w| w == "exited");
                let idx: Vec<usize> = panes.iter().filter_map(|p| v.m.by_raw(&p.0.to_string())).collect();
                if idx.is_empty() {
                    return Ok(finished.then(|| vec![ok("until", crate::obj! {"until" => want.join(",")})]));
                }
                self.observe(s, &mut v, &idx);
                let all = idx.iter().all(|i| {
                    let gone = s.nodes[*i].pane.as_ref().is_some_and(|x| x.exit.is_some()) || exited.contains(&v.id(*i));
                    (finished && gone) || v.m.nodes[*i].fields.get("state").is_some_and(|st| want.contains(st))
                });
                if all { done("until", crate::obj! {"until" => want.join(","), "panes" => idx.len()}) } else { None }
            }
            Cond::Screen(re) => {
                for p in panes {
                    if let Ok(d) = self.conn.call(Op::Read { pane: *p, lines: Lines::Screen }) {
                        if let Some(l) = d.lines.iter().find(|l| re.is_match(l)) {
                            return Ok(done("until", crate::obj! {"id" => view::pane_handle(*p), "matched" => l}));
                        }
                    }
                }
                None
            }
            Cond::Count(cmp, n) => {
                let mut v = View::new(s, self.conn.env().me, &self.binds);
                let k = self.resolve(r, s, &mut v)?.len();
                let ok_ = match cmp {
                    Cmp::Eq => k == *n,
                    Cmp::Ge => k >= *n,
                    Cmp::Le => k <= *n,
                };
                if ok_ { done("until", crate::obj! {"count" => k}) } else { None }
            }
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Node, PaneInfo};

    fn node(id: u64, kind: PKind, parent: Option<u32>, exit: Option<i32>) -> Node {
        let pane = (kind == PKind::Pane).then(|| PaneInfo { exit, ..Default::default() });
        Node { id: Id(id), kind, parent, name: String::new(), rect: None, created: 0, vars: vec![], pane, layout: None }
    }

    #[test]
    fn a_gap_catches_up_on_new_exited_and_gone_nodes() {
        let (tab, p1, p2, p3) = (Id(10), Id(11), Id(12), Id(13));
        let mut w = Watched {
            picked: vec![tab],
            panes: [p1, p3].into(),
            set: [tab, p1, p3].into(),
            kinds: [(tab, PKind::Tab), (p1, PKind::Pane), (p3, PKind::Pane)].into(),
            gone: HashSet::new(),
            exited: HashSet::new(),
        };
        assert!(!w.all_exited());
        // after the gap: p1 exited, p2 opened in the tab, p3 closed
        let s = Snapshot {
            nodes: vec![node(1, PKind::App, None, None), node(10, PKind::Tab, Some(0), None), node(11, PKind::Pane, Some(1), Some(0)), node(12, PKind::Pane, Some(1), None)],
            ..Default::default()
        };
        assert!(w.catch_up(&s), "p2 is new");
        assert!(w.set.contains(&p2) && w.exited.contains(&p1) && w.gone.contains(&p3));
        assert!(w.all_exited() && !w.all_gone());
        assert!(!w.catch_up(&s), "nothing new the second time");
        // a close in the set is taken; an event outside it is not relevant
        assert!(w.take(&Event::Closed { id: tab, kind: PKind::Tab }, false) && w.all_gone());
        assert!(!w.take(&Event::Closed { id: Id(99), kind: PKind::Pane }, false));
    }
}
