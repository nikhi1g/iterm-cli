//! The dispatcher: [`Exec::run`] takes one parsed [`Request`] and answers it with rows.
//!
//! It owns, top to bottom: routing a request to the module that answers its noun (`tree`, `ui`, `content`,
//! `named`, `agents`, `watch`, …); selector resolution against one snapshot ([`Exec::resolve`]); the reads
//! (`get`, `read`, `find`, `list`); the creates (new windows, tabs and panes, and what each new pane runs:
//! [`Item`], [`Exec::items`], [`Exec::spawn_for`]); and the writes (`set`, `move`, `order`, `resize`, `send`,
//! `key`, `run`, `close`). `--plan` is the dry run of any write; the one close rail (a live agent needs
//! `--force`) is [`Exec::may_delete`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::app::wire::Key;
use crate::app::wire::{Dest, Field, Lines, ScrollTo, Side, Spawn, WinState};
use crate::app::{Id, Layout, Op, Rect, Snapshot};
use serde_json::{json, Value};

use crate::agent;
use crate::conn::Conn;
use crate::geometry;
use crate::grammar::{self, Atom, By, Class, Dir, Expr, Kind, Noun, Request, To, Verb};
use crate::out::{from_error, ok, planned, Error, Obj, Result};
use crate::select::{self, Ctx};
use crate::view::{self, View};

pub struct Exec<'a> {
    pub conn: Conn,
    /// Batch bindings (`--as NAME`): name → raw IDs.
    pub binds: HashMap<String, Vec<String>>,
    /// Raw IDs the last request created (what `--as` binds).
    pub created: Vec<String>,
    /// Where streamed rows go (watch events), as they come.
    pub sink: Box<dyn FnMut(&Obj) + 'a>,
    /// The caller asked for `--json` (a command whose text and JSON forms differ, `tree`, reads it).
    pub json: bool,
    /// Colors this run gave new agents (the snapshot it read predates them).
    pub colored: Vec<String>,
}

pub(crate) fn side_of(d: Dir) -> Side {
    match d {
        Dir::Left => Side::Left,
        Dir::Right => Side::Right,
        Dir::Up => Side::Up,
        Dir::Down => Side::Down,
    }
}

/// How long one screen read for a pane's state may take, and all of them in one command (`observe`).
pub const OBSERVE_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
pub const OBSERVE_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);

/// `{i}` and `{n}` in a value.
pub(crate) fn tpl(s: &str, i: usize, n: usize) -> String {
    s.replace("{i}", &i.to_string()).replace("{n}", &n.to_string())
}

/// Whether the request selects by state (`--where state=…`, `@idle`): every pane's observed state.
fn state_selector(r: &Request) -> bool {
    fn scan(e: &Expr) -> bool {
        match e {
            Expr::Or(v) | Expr::And(v) => v.iter().any(scan),
            Expr::Not(x) => scan(x),
            Expr::Atom(Atom::State(s)) => !matches!(s.as_str(), "focused" | "active"),
            Expr::Atom(Atom::Field(k, _)) => k == "state",
            Expr::Atom(_) => false,
        }
    }
    r.sel.as_ref().is_some_and(scan) || r.wheres.iter().any(|(k, _)| k == "state")
}

/// `agent list|get` without a state selector: only agent panes' states are shown.
fn agents_state(r: &Request) -> bool {
    r.noun == Noun::Agent && matches!(r.verb, Verb::List | Verb::Get)
}

/// Where an agent goes in its spawner's dock: the pane to split, the side, and the spawner's agents in that tab.
struct Docked {
    at: Id,
    side: Dir,
    kids: Vec<Id>,
}

/// A pane the app or the CLI knows runs an agent (its state is a variable, or its kind is an agent's).
fn agentish(n: &crate::app::Node) -> bool {
    n.var(view::V_STATE).is_some() || n.var(view::V_PROVIDER).is_some() || view::is_agent_kind(n.var(view::V_KIND)) || n.var(view::V_CMD).is_some()
}

impl<'a> Exec<'a> {
    pub fn new(conn: Conn) -> Exec<'a> {
        Exec { conn, binds: HashMap::new(), created: Vec::new(), sink: Box::new(|_| {}), json: false, colored: Vec::new() }
    }

    /// The snapshot and the view built from it.
    pub fn view(&mut self) -> Result<(Snapshot, View)> {
        let me = self.conn.env().me;
        let s = self.conn.snapshot()?.clone();
        let v = View::new(&s, me, &self.binds);
        Ok((s, v))
    }

    /// Fill in `state` for panes: EXITED from the snapshot, else classified from the screen. The screen reads
    /// are bounded: each waits at most `OBSERVE_WAIT`, all of them `OBSERVE_BUDGET`, and after one goes unanswered
    /// (the app's main thread is stuck) none is asked again; the panes not read show `unknown`. A list never waits
    /// on the main thread once per pane.
    pub fn observe(&mut self, s: &Snapshot, v: &mut View, panes: &[usize]) {
        let until = Instant::now() + OBSERVE_BUDGET;
        let mut stuck = false;
        for &i in panes {
            if v.m.nodes[i].kind != Kind::Pane || v.m.nodes[i].fields.contains_key("state") {
                continue;
            }
            // the app's agent table first (event-driven); a pane it doesn't track is read from its screen
            let st = match s.nodes[i].var(view::V_STATE) {
                Some(st) => st.to_string(),
                None => {
                    let claude = view::is_agent_kind(s.nodes[i].var(view::V_KIND));
                    let left = until.saturating_duration_since(Instant::now()).min(OBSERVE_WAIT);
                    self.screen_state(v.id(i), claude, left, &mut stuck)
                }
            };
            if let Some(p) = s.nodes[i].var(view::V_PROVIDER) {
                v.m.nodes[i].fields.insert("provider".into(), p.into());
            }
            v.m.nodes[i].fields.insert("state".into(), st);
        }
    }

    /// A pane's state classified from its screen, read within `left`. `unknown` when there is no time left, or
    /// once a read has gone unanswered (`stuck`: the app's main thread is busy, so no other pane is asked).
    fn screen_state(&mut self, pane: Id, claude: bool, left: Duration, stuck: &mut bool) -> String {
        if *stuck || left.is_zero() {
            return "unknown".into();
        }
        let legacy = match self.conn.call_within(Op::Read { pane, lines: Lines::Screen }, left) {
            Ok(d) => agent::classify(&d.lines, claude),
            Err(e) => {
                // a pane that is gone answers at once; a stuck main thread does not
                *stuck = e.unanswered();
                "UNKNOWN"
            }
        };
        match legacy {
            agent::MENU | agent::TRUST => "prompt".into(),
            other => other.to_ascii_lowercase(),
        }
    }

    /// Answer one request. In order: `--plan` of a write (nothing runs); the nouns other modules answer whole;
    /// the verbs answered without a node selection (`agent msg`, `app quit`, …); then, against one snapshot,
    /// a create, or the selected nodes read or written.
    pub fn run(&mut self, r: &Request) -> Result<Vec<Obj>> {
        self.created.clear();
        // --plan is a dry run everywhere: no write starts, in or out of the caller's scope
        if r.p.has("--plan") && grammar::class_of(r.noun, r.verb) != grammar::Class::Read {
            // land's dry run checks every refusal and names what each landing would take
            if (r.noun, r.verb) == (Noun::Agent, Verb::Land) {
                return self.land(r);
            }
            let mut row = crate::obj! {"noun" => r.noun.to_string(), "args" => r.rest.clone()};
            // a new pane or agent: how many it makes, and (an agent with no placement) where its dock puts it
            if matches!(r.noun, Noun::Pane | Noun::Agent) && r.verb == Verb::New {
                if let Ok((s, v)) = self.view() {
                    let agent = r.p.has("--agent") || r.noun == Noun::Agent;
                    // the same checks a real run makes, without making worktrees
                    let mut dry = r.clone();
                    dry.multi.worktree = None;
                    row.insert("items".into(), json!(self.items(&dry, &s, &v, agent)?.len()));
                }
                if let Some((beside, side)) = self.dock_plan(r) {
                    row.insert("beside".into(), json!(beside));
                    row.insert("side".into(), json!(side));
                }
            }
            return Ok(vec![crate::out::planned(&r.verb.to_string(), row)]);
        }
        if grammar::slot_only(r.noun, r.verb) {
            return Err(Error::policy("unsupported", format!("{} {} is not wired in iTerm2 yet", r.noun, r.verb)));
        }
        if matches!(r.noun, Noun::Group | Noun::Layout | Noun::Snippet | Noun::Script | Noun::Preset) {
            return self.named(r);
        }
        if r.noun == Noun::Broadcast {
            return self.broadcast(r);
        }
        // the control plane: layout, key-map actions, dialogs, menus, selection and marks
        match (r.noun, r.verb) {
            (Noun::Tree, _) => return crate::tree::run(self, r),
            (Noun::App, Verb::Reopen) => return crate::action::reopen(self, r),
            (Noun::Action, _) => return crate::action::run(self, r),
            (Noun::Ui, _) | (Noun::App, Verb::Launch | Verb::Menu) => return crate::ui::run(self, r),
            (Noun::Content, Verb::Get | Verb::Select | Verb::Copy | Verb::Mark | Verb::Open) => return crate::content::run(self, r),
            (Noun::Content, Verb::Find) if r.p.has("--show") => return crate::content::run(self, r),
            (Noun::Content, Verb::Scroll) if r.p.one("--to").is_some_and(|t| t.starts_with("mark:")) => return crate::content::run(self, r),
            _ => {}
        }
        if r.noun == Noun::App && r.verb == Verb::Find {
            return self.search(r);
        }
        if matches!(r.verb, Verb::Watch | Verb::Wait) && r.noun != Noun::Task {
            return self.watch(r);
        }
        match r.noun {
            Noun::Task => return self.task(r),
            Noun::Setting | Noun::Profile | Noun::Key => return self.settings(r),
            Noun::Agent
                if matches!(
                    r.verb,
                    Verb::Run | Verb::Msg | Verb::Inbox | Verb::Checkpoint | Verb::Merge | Verb::Pause | Verb::Resume | Verb::WtRemove | Verb::Adopt | Verb::Land | Verb::Unland | Verb::Landed
                ) =>
            {
                return self.agent_verb(r);
            }
            Noun::Agent if r.verb == Verb::List && r.p.has("--paused") => return self.paused(),
            Noun::Agent if r.verb == Verb::List && r.p.has("--history") => return self.subagent_history(r),
            Noun::Pane if r.verb == Verb::Lend => return self.lend(r),
            Noun::App if r.verb == Verb::Quit => {
                if !r.apply {
                    return Ok(vec![planned("quit", crate::obj! {})]);
                }
                // the close rail, app-wide: quitting with live agents takes --force
                if !r.force {
                    let s = self.conn.snapshot()?.clone();
                    let live: Vec<String> = s
                        .nodes
                        .iter()
                        .filter(|n| n.pane.as_ref().is_some_and(|p| p.exit.is_none()))
                        .filter(|n| crate::agents::provider(n).is_some())
                        .filter(|n| !n.var(view::V_STATE).and_then(crate::app::agent::State::parse).is_some_and(|x| x.finished()))
                        .map(|n| view::pane_handle(n.id))
                        .collect();
                    if !live.is_empty() {
                        return Err(Error::policy("live-agent", format!("live agents ({}); pass --force to quit anyway", live.join(","))));
                    }
                }
                // the app may exit before its reply: the closed connection is the answer
                match self.conn.call(Op::Quit) {
                    Ok(_) => {}
                    Err(Error::Failed(m)) if m.starts_with("iTerm2 connection") => {}
                    Err(e) => return Err(e),
                }
                return Ok(vec![ok("quit", crate::obj! {})]);
            }
            _ => {}
        }
        let (s, mut v) = self.view()?;
        if r.class() == Class::Create {
            return self.create(r, &s, &v);
        }
        let targets = self.resolve(r, &s, &mut v)?;
        if targets.is_empty() && r.class() != Class::Read {
            return Err(Error::invalid(format!("{} {}: the selector names nothing", r.noun, r.verb)));
        }
        match r.class() {
            Class::Read => return self.read_verb(r, &s, &mut v, &targets),
            // open model: clearing any pane's screen runs; --plan is the dry run
            Class::Delete if r.noun == Noun::Content => {
                if !r.apply {
                    return Ok(self.plans(r, &v, &targets));
                }
            }
            Class::Delete => {
                // work not landed first: it names the command that takes the work home, which --force skips
                self.refuse_unlanded(&s, &v, &targets, r.force)?;
                self.may_delete(&s, &v, &targets, r.force)?;
                if !r.apply {
                    return Ok(self.plans(r, &v, &targets));
                }
            }
            _ => {
                if !r.apply && !v.inside(&s, &targets, false) {
                    return Ok(self.plans(r, &v, &targets));
                }
            }
        }
        self.write_verb(r, &s, &v, &targets)
    }

    /// Dry-run rows: what the request would act on.
    fn plans(&self, r: &Request, v: &View, targets: &[usize]) -> Vec<Obj> {
        targets.iter().map(|i| planned(r.verb.name(), crate::obj! {"node" => v.m.nodes[*i].kind.letter(), "id" => v.handle(*i)})).collect()
    }

    /// `agent list --history [N] [--all | --cwd D]`: Claude Code's own sub-agents, newest first (what
    /// `agent resume agent-<id>` takes), under your project (the repository you run in, else the folder).
    fn subagent_history(&mut self, r: &Request) -> Result<Vec<Obj>> {
        // `--history=N`, or `--history N` (the number then reads as the selector word)
        let n = r.p.one("--history").filter(|x| !x.is_empty()).map(str::to_string).or_else(|| r.p.positional.first().filter(|w| w.bytes().all(|b| b.is_ascii_digit())).cloned());
        let n = n.map(|x| x.parse::<usize>().map_err(|_| Error::invalid("--history N"))).transpose()?.unwrap_or(10);
        let here = self.conn.env().cwd.clone();
        let under = match r.p.one("--cwd") {
            _ if r.all => None,
            Some(c) => Some(here.join(c)),
            None => Some(git_top(&here).unwrap_or(here)),
        };
        Ok(crate::subagent::history(n.min(500), under.as_deref())
            .into_iter()
            .map(|s| {
                let age = s.modified.and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()).unwrap_or(0);
                ok("subagent", crate::obj! {"id" => format!("agent-{}", s.id), "name" => s.name, "type" => s.agent_type, "session" => s.parent_session, "age_s" => age, "cwd" => s.cwd})
            })
            .collect())
    }

    // ---- selection --------------------------------------------------------------------------

    pub fn resolve(&mut self, r: &Request, s: &Snapshot, v: &mut View) -> Result<Vec<usize>> {
        let kind = r.of.or(r.noun.kind());
        let every = state_selector(r);
        if every || agents_state(r) {
            // agent list|get reads no shell's screen (each read is a main-thread hop: ~8 ms apiece); a state
            // selector still observes every pane
            let only_agents = !every;
            let panes: Vec<usize> = (0..v.m.nodes.len()).filter(|i| v.m.nodes[*i].kind == Kind::Pane && (!only_agents || agentish(&s.nodes[*i]))).collect();
            self.observe(s, v, &panes);
        }
        let m = &v.m;
        let ctx = Ctx {
            within: match &r.within {
                Some(e) => Some(select::eval(m, e, None, &Ctx::default()).map_err(Error::Invalid)?),
                None => None,
            },
        };
        let all = || select::eval(m, &Expr::Atom(Atom::All), kind, &ctx).map_err(Error::Invalid);
        if kind == Some(Kind::App) {
            return Ok(vec![0]);
        }
        let mut out = match (&r.sel, r.all) {
            (Some(e), _) => select::eval(m, e, kind, &ctx).map_err(Error::Invalid)?,
            (None, true) => all()?,
            (None, false)
                if matches!(r.verb, Verb::List | Verb::Find | Verb::Watch | Verb::Wait)
                    || r.within.is_some()
                    || !r.wheres.is_empty()
                    || r.group.is_some() =>
            {
                all()?
            }
            (None, false) => {
                let me = m.me.ok_or_else(|| {
                    Error::invalid(format!("{} {} needs a selector (you have no pane identity here)", r.noun, r.verb))
                })?;
                m.project(me, kind)
            }
        };
        if let Some(w) = &ctx.within {
            let under = m.covered(w, true);
            out.retain(|i| under[*i]);
        }
        for (k, mat) in &r.wheres {
            let keep = select::eval(m, &Expr::Atom(Atom::Field(k.clone(), mat.clone())), kind, &Ctx::default()).map_err(Error::Invalid)?;
            out.retain(|i| keep.binary_search(i).is_ok());
        }
        if let Some(g) = &r.group {
            let e = Expr::Atom(Atom::Field("groups".into(), grammar::Match::Eq(g.clone())));
            let keep = select::eval(m, &e, Some(Kind::Pane), &Ctx::default()).map_err(Error::Invalid)?;
            out.retain(|i| m.any_under(&keep, *i));
        }
        if r.noun == Noun::Agent {
            out.retain(|i| view::is_agent_kind(crate::agents::provider(&s.nodes[*i])));
        }
        Ok(out)
    }

    /// `search QUERY` (`app find`): the app's unified search, one row per result in its order.
    fn search(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let prefix = match r.p.one("--kind") {
            None => "",
            Some("tabs" | "panes") => "@",
            Some("actions") => ">",
            Some("settings") => ",",
            Some("themes") => "#",
            Some("cli") => "$",
            Some(_) => return Err(Error::invalid("--kind tabs|actions|settings|themes|cli")),
        };
        let limit = match r.p.one("--limit") {
            Some(n) => n.parse().map_err(|_| Error::invalid("--limit N"))?,
            None => 20,
        };
        let query = format!("{prefix}{}", r.words.join(" "));
        let done = self.conn.call(Op::Search { query, limit })?;
        Ok(done
            .lines
            .iter()
            .map(|l| {
                let mut f = l.splitn(4, '\t');
                let (group, name, detail, id) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
                ok("found", crate::obj! {"group" => group, "name" => name, "detail" => detail, "id" => id})
            })
            .collect())
    }

    /// `broadcast get` and `broadcast set`: what typing in the focused pane is copied to.
    fn broadcast(&mut self, r: &Request) -> Result<Vec<Obj>> {
        use crate::app::BroadcastMode;
        let mut op = Op::Broadcast { mode: None, window: None, add: Vec::new(), remove: Vec::new() };
        if r.verb == Verb::Set {
            let (_, v) = self.view()?;
            let panes = |flag: &str| -> Result<Vec<Id>> {
                let mut out = Vec::new();
                for s in r.p.many(flag) {
                    let e = grammar::selector(&s).map_err(|e| Error::invalid(e.to_string()))?;
                    let found = select::eval(&v.m, &e, Some(Kind::Pane), &Ctx::default()).map_err(Error::Invalid)?;
                    if found.is_empty() {
                        return Err(Error::invalid(format!("{flag} {s} names no pane")));
                    }
                    out.extend(found.into_iter().map(|i| v.id(i)));
                }
                Ok(out)
            };
            let mode = match r.p.one("--mode") {
                Some(m) => Some(BroadcastMode::parse(m).ok_or_else(|| Error::invalid("--mode vtab|tab-agents|window|tab|all|pick|off"))?),
                None => None,
            };
            let window = match r.p.one("--in") {
                Some(s) => {
                    let e = grammar::selector(s).map_err(|e| Error::invalid(e.to_string()))?;
                    Some(v.id(self.one_node(&v, &e, Kind::Window, "--in")?))
                }
                None => None,
            };
            let (add, remove) = (panes("--add")?, panes("--remove")?);
            if mode.is_none() && add.is_empty() && remove.is_empty() {
                return Err(Error::invalid("broadcast set --mode vtab|tab-agents|window|tab|all|pick|off [--in WINDOW] | --add SEL | --remove SEL"));
            }
            op = Op::Broadcast { mode, window, add, remove };
            // it redirects the human's typing: never without --apply
            if !r.apply {
                return Ok(vec![planned("broadcast", crate::obj! {"op" => format!("{op:?}")})]);
            }
        } else if r.verb != Verb::Get {
            return Err(Error::invalid("broadcast get | broadcast set …"));
        }
        let done = self.conn.call(op)?;
        let mode = done.lines.first().cloned().unwrap_or_default();
        let window = done.lines.get(1).filter(|w| !w.is_empty()).map(|w| format!("w{w}"));
        let targets: Vec<String> = done.ids.iter().map(|i| format!("p{i}")).collect();
        Ok(vec![ok("broadcast", crate::obj! {"mode" => mode, "window" => window, "targets" => targets})])
    }

    pub fn one_node(&self, v: &View, e: &Expr, kind: Kind, what: &str) -> Result<usize> {
        let found = select::eval(&v.m, e, Some(kind), &Ctx::default()).map_err(Error::Invalid)?;
        match found.len() {
            1 => Ok(found[0]),
            0 => Err(Error::invalid(format!("{what} names no {}", select::kind_name(Some(kind))))),
            n => Err(Error::invalid(format!("{what} names {n} {}s; pass one", select::kind_name(Some(kind))))),
        }
    }

    /// Each pane strictly mine, and no live agent among them unless `force`.
    pub(crate) fn may_delete(&self, s: &Snapshot, v: &View, nodes: &[usize], force: bool) -> Result<()> {
        for n in nodes {
            for p in v.m.project(*n, Some(Kind::Pane)) {
                if v.m.me == Some(p) && !force {
                    return Err(Error::policy("self", "that's your own pane: exit instead, or --force"));
                }
                // the one close rail: a live agent needs --force
                let node = &s.nodes[p];
                let agent = view::is_agent_kind(node.var(view::V_KIND)) || node.var(view::V_PROVIDER).is_some();
                let finished = node.var(view::V_STATE).and_then(crate::app::agent::State::parse).is_some_and(|x| x.finished());
                let live = agent && !finished && node.pane.as_ref().is_some_and(|x| x.exit.is_none());
                if live && !force {
                    return Err(Error::policy(
                        "live-agent",
                        format!("{} is a live agent ({}); pass --force to close it anyway", v.handle(p), node.name),
                    ));
                }
            }
        }
        Ok(())
    }

    // ---- reads --------------------------------------------------------------------------------

    fn read_verb(&mut self, r: &Request, s: &Snapshot, v: &mut View, targets: &[usize]) -> Result<Vec<Obj>> {
        match (r.noun, r.verb) {
            (Noun::App, Verb::Get) => {
                let count = |k: Kind| v.m.nodes.iter().filter(|n| n.kind == k).count();
                Ok(vec![ok(
                    "app",
                    crate::obj! {"app" => "iterm", "version" => s.app_version, "pid" => s.app_pid,
                    "windows" => count(Kind::Window), "vtabs" => count(Kind::VTab), "tabs" => count(Kind::Tab), "panes" => count(Kind::Pane),
                    "focused" => v.m.focused.iter().map(|i| v.handle(*i).to_string()).collect::<Vec<_>>(),
                    "me" => v.m.me.map(|i| v.handle(i).to_string()),
                    "awake" => s.awake.clone(), "socket" => self.conn.env().socket.display().to_string(), "schema_version" => grammar::SCHEMA_VERSION},
                )])
            }
            (Noun::Pane | Noun::Content | Noun::Agent, Verb::Read) => {
                let lines = lines_of(r)?;
                Ok(targets.iter().map(|&i| self.read_text(s, v, i, lines)).collect())
            }
            (Noun::Pane | Noun::Content, Verb::Find) => self.find_text(r, v, targets),
            (Noun::Pane | Noun::Agent, Verb::Get) => Ok(targets.iter().map(|&i| pane_row(s, v, i)).collect()),
            (Noun::Tab, Verb::Get) => Ok(targets
                .iter()
                .map(|i| {
                    let mut row = v.row(s, *i);
                    if let Some(l) = &s.nodes[*i].layout {
                        row.insert("layout".into(), layout_json(l));
                    }
                    row
                })
                .collect()),
            (_, Verb::List | Verb::Get | Verb::Find) => Ok(targets.iter().map(|i| v.row(s, *i)).collect()),
            (n, vb) => Err(Error::invalid(format!("{n} {vb} has no reader"))),
        }
    }

    /// `pane read`: one pane's text, its blank bottom rows dropped; a failed read is that pane's error row.
    fn read_text(&mut self, s: &Snapshot, v: &View, i: usize, lines: Lines) -> Obj {
        // --tail N counts text lines: ask for N more than the screen, whose unused rows below the cursor are
        // dropped first
        let rows = s.nodes[i].pane.as_ref().map_or(0, |p| p.rows as u32);
        let ask = match lines {
            Lines::Tail(n) => Lines::Tail(n.saturating_add(rows)),
            l => l,
        };
        match self.conn.call(Op::Read { pane: v.id(i), lines: ask }) {
            Ok(mut d) => {
                trim_read(&mut d.lines, lines);
                ok("text", crate::obj! {"pane" => v.handle(i), "lines" => d.lines})
            }
            Err(e) => from_error(&e, crate::obj! {"pane" => v.handle(i)}),
        }
    }

    /// `pane find --match RE [--tail N]`: the matching lines of each pane's last N (500), at most 50 in all;
    /// `line` counts up from the bottom.
    fn find_text(&mut self, r: &Request, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let re = regex::Regex::new(r.p.one("--match").ok_or_else(|| Error::invalid("find needs --match RE"))?)
            .map_err(|e| Error::invalid(format!("--match: {e}")))?;
        let tail: u32 = r.p.one("--tail").and_then(|x| x.parse().ok()).unwrap_or(500);
        let mut out = Vec::new();
        for &i in targets {
            let d = self.conn.call(Op::Read { pane: v.id(i), lines: Lines::Tail(tail) })?;
            let n = d.lines.len();
            for (k, l) in d.lines.iter().enumerate() {
                if re.is_match(l) && out.len() < 50 {
                    out.push(ok("found", crate::obj! {"pane" => v.handle(i), "line" => n - k, "text" => l}));
                }
            }
        }
        Ok(out)
    }

    // ---- creates ------------------------------------------------------------------------------

    fn create(&mut self, r: &Request, s: &Snapshot, v: &View) -> Result<Vec<Obj>> {
        let agent = r.p.has("--agent") || r.noun == Noun::Agent;
        let rows = match r.noun {
            Noun::Pane | Noun::Agent => self.pane_new(r, s, v, agent)?,
            Noun::Window | Noun::VTab | Noun::Tab => self.container_new(r, s, v, agent)?,
            Noun::Content if r.verb == Verb::Save => self.content_save(r, s, v)?,
            n => return Err(Error::invalid(format!("{n} {} is not a create here", r.verb))),
        };
        if r.p.has("--detached") {
            self.detach(&rows);
        }
        Ok(rows)
    }

    /// `--detached`: the new panes' parent variable is cleared, so they are no one's children.
    fn detach(&mut self, rows: &[Obj]) {
        for row in rows {
            let pane = row.get("pane").or(row.get("id")).and_then(Value::as_str).and_then(crate::app::model::parse_handle);
            if let Some((_, id)) = pane {
                let _ = self.conn.call(Op::Set { id, field: Field::Var(view::V_PARENT.into(), None) });
            }
        }
    }

    /// The container a new tab or vertical tab goes in: `--in`, else the caller's own.
    fn container_for(&self, r: &Request, v: &View, kind: Kind) -> Result<Option<usize>> {
        if let Some(e) = &r.place.into {
            let found = select::eval(&v.m, e, None, &Ctx::default()).map_err(Error::Invalid)?;
            let i = *found.first().ok_or_else(|| Error::invalid("--in names nothing"))?;
            return match v.m.project(i, Some(kind)).as_slice() {
                [one] => Ok(Some(*one)),
                _ => Err(Error::invalid(format!("--in must name one {} (or a node inside one)", select::kind_name(Some(kind))))),
            };
        }
        Ok(v.m.me.and_then(|me| v.m.up(me, kind)))
    }

    fn container_new(&mut self, r: &Request, s: &Snapshot, v: &View, agent: bool) -> Result<Vec<Obj>> {
        let items = self.items(r, s, v, agent)?;
        let n = items.len();
        let into = match r.noun {
            Noun::VTab => self.container_for(r, v, Kind::Window)?,
            Noun::Tab => self.container_for(r, v, Kind::VTab)?,
            _ => None,
        };
        // `vtab new NAME`: the vertical tab's name (else --name); templates apply per item
        let name_of = |k: usize| r.words.first().map(String::as_str).or(r.p.one("--name")).map(|w| tpl(w, k + 1, n));
        let mut rows = Vec::new();
        for (k, it) in items.iter().enumerate() {
            let spawn = self.spawn_for(r, it, agent)?;
            let op = match r.noun {
                Noun::Window => Op::NewWindow { spawn, frame: None, focus: r.focus },
                Noun::VTab => Op::NewVTab {
                    window: into.map(|i| v.id(i)),
                    name: name_of(k),
                    spawn,
                    focus: r.focus,
                },
                _ => Op::NewTab { vtab: into.map(|i| v.id(i)), title: r.p.one("--title").map(|x| tpl(x, k + 1, n)), spawn, focus: r.focus },
            };
            rows.push(self.made(r, op, it, agent));
        }
        Ok(rows)
    }

    /// Run one create; the row names the nodes it made, with the new pane last.
    pub(crate) fn made(&mut self, r: &Request, op: Op, it: &Item, agent: bool) -> Obj {
        match self.conn.call(op) {
            Ok(d) => {
                self.created.extend(d.ids.iter().map(|i| i.0.to_string()));
                let pane = d.ids.last().copied();
                // the row names the node of the noun's kind (a vtab new: the vtab), and its pane
                let kind = match r.noun.kind() {
                    Some(Kind::Window) => crate::app::Kind::Window,
                    Some(Kind::VTab) => crate::app::Kind::VTab,
                    Some(Kind::Tab) => crate::app::Kind::Tab,
                    _ => crate::app::Kind::Pane,
                };
                let node = if kind == crate::app::Kind::Pane { pane } else { d.ids.first().copied() };
                let mut row = crate::obj! {"node" => kind.letter().to_string()};
                if let Some(n) = node {
                    row.insert("id".into(), json!(crate::app::model::handle(kind, n)));
                }
                if let Some(p) = pane.filter(|p| Some(*p) != node) {
                    row.insert("pane".into(), json!(view::pane_handle(p)));
                }
                if d.ids.len() > 1 {
                    row.insert("created".into(), json!(d.ids.iter().map(|i| i.0.to_string()).collect::<Vec<_>>()));
                }
                if let Some(nm) = &it.name {
                    row.insert("name".into(), json!(nm));
                }
                row.insert("cwd".into(), json!(it.cwd));
                // every pane the CLI makes is a child in the ledger (children, exits, worktrees)
                if let Some(p) = pane {
                    self.ledger_add(p, it, agent);
                }
                if agent {
                    row.insert("kind".into(), json!(it.kind(agent)));
                    if it.name.is_some() && it.kind(agent) == "claude" {
                        row.insert("then".into(), json!("SendMessage"));
                    }
                }
                ok("created", row)
            }
            Err(e) => from_error(&e, crate::obj! {"name" => it.name, "cwd" => it.cwd}),
        }
    }

    fn pane_new(&mut self, r: &Request, s: &Snapshot, v: &View, agent: bool) -> Result<Vec<Obj>> {
        // where: --at, else --to TAB (beside its last pane), else --in TAB, else the caller's pane
        let mut at = match &r.place.at {
            Some(e) => Some(self.one_node(v, e, Kind::Pane, "--at")?),
            None => None,
        };
        let mut new_place = false;
        match &r.place.to {
            Some(To::Sel(e)) => {
                let tab = self.one_node(v, e, Kind::Tab, "--to")?;
                at = v.m.project(tab, Some(Kind::Pane)).last().copied();
            }
            Some(_) => new_place = true,
            None => {}
        }
        if at.is_none() && !new_place {
            if let Some(e) = &r.place.into {
                let tab = self.one_node(v, e, Kind::Tab, "--in")?;
                at = v.m.project(tab, Some(Kind::Pane)).last().copied();
            }
        }
        let at = match (at, new_place) {
            (Some(a), _) => Some(a),
            (None, true) => None,
            (None, false) => Some(v.m.me.ok_or_else(|| {
                Error::invalid("pane new needs --at PANE or --to new-tab|new-window|TAB (you have no pane here)")
            })?),
        };
        let items = self.items(r, s, v, agent)?;
        let n = items.len();
        if let Some(w) = &r.geom.weights {
            if w.len() != n {
                return Err(Error::invalid(format!("--weights gives {} values for {n} panes", w.len())));
            }
        }
        if let Some(max) = r.multi.max {
            return self.queued_new(r, s, v, items, agent, at.map(|a| v.id(a)), max);
        }
        let mut rows = self.place_items(r, s, v, &items, agent, at.map(|a| v.id(a)))?;
        // `--tile grid|columns|rows`: the tab the panes went into, all its panes tiled in order
        if let (Some(mode), Some(a)) = (r.geom.tile, at) {
            if let Ok((tab, layout)) = self.tab_layout(v.id(a)) {
                let mut ids = Vec::new();
                layout.panes(&mut ids);
                if let Some(l) = geometry::tiled(&ids, mode) {
                    self.conn.call(Op::SetLayout { tab, layout: l })?;
                    rows.push(ok("tiled", crate::obj! {"panes" => ids.len()}));
                }
            }
        }
        Ok(rows)
    }

    /// Open the items: in new tabs or windows (`--to new-…`), else as a block beside `at`: the
    /// first splits it on `--side`, each next one the block's newest pane along `--stack`, then
    /// the block gets `--weights` (equal by default).
    pub(crate) fn place_items(&mut self, r: &Request, s: &Snapshot, v: &View, items: &[Item], agent: bool, at: Option<Id>) -> Result<Vec<Obj>> {
        let n = items.len();
        let new_place = match &r.place.to {
            Some(To::Sel(_)) | None => None,
            Some(t) => Some(t),
        };
        if let (Some(to), None) = (new_place, at) {
            let mut rows = Vec::new();
            for it in items {
                let spawn = self.spawn_for(r, it, agent)?;
                let op = self.new_place_op(to, v, spawn, r.focus)?;
                rows.push(self.made(r, op, it, agent));
            }
            return Ok(rows);
        }
        let at = at.ok_or_else(|| Error::invalid("pane new needs --at PANE or --to new-tab|new-window|TAB"))?;
        // an agent spawned beside its spawner with no placement goes to the spawner's dock; a row grows rightwards
        let dock = self.dock(r, s, v, agent, at);
        let at = dock.as_ref().map_or(at, |d| d.at);
        let stack = dock.as_ref().map(|_| Dir::Right).or(r.place.stack).or(if n > 1 { Some(Dir::Down) } else { None });
        let side = dock.as_ref().map(|d| d.side).or(r.place.side).or(stack.map(|d| if d == Dir::Down { Dir::Right } else { Dir::Down }));
        let auto = side.unwrap_or_else(|| {
            // one pane, no placement: split the wider way (cells are about twice as tall as wide)
            let ix = v.m.by_raw(&at.0.to_string());
            match ix.and_then(|i| s.nodes[i].pane.as_ref()) {
                Some(p) if (p.cols as f32) * 0.5 > p.rows as f32 => Dir::Right,
                Some(_) => Dir::Down,
                None => Dir::Right,
            }
        });
        let mut rows = Vec::new();
        let mut made: Vec<Id> = Vec::new();
        for (k, it) in items.iter().enumerate() {
            let spawn = self.spawn_for(r, it, agent)?;
            let (beside, d) = match made.last() {
                Some(last) => (*last, stack.unwrap_or(Dir::Down)),
                None => (at, auto),
            };
            let ratio = if k == 0 { r.geom.ratio.map(|x| x as f32) } else { None };
            let op = Op::Split { at: beside, side: side_of(d), ratio, spawn, focus: r.focus && k == 0 };
            let row = self.made(r, op, it, agent);
            let failed = row.get("status").and_then(Value::as_str) != Some("ok");
            if let Some(id) = row.get("id").and_then(Value::as_str).and_then(crate::app::model::parse_handle) {
                made.push(id.1);
            }
            rows.push(row);
            if failed {
                for rest in &items[k + 1..] {
                    rows.push(from_error(&Error::failed(format!("item {} did not open", k + 1)), crate::obj! {"name" => rest.name}));
                }
                break;
            }
        }
        if let (Some(d), Some(last)) = (&dock, made.last()) {
            // the dock row: the spawner's agents beside the newest get equal shares of what they hold together
            let members: Vec<Id> = d.kids.iter().chain(&made).copied().collect();
            if let Err(e) = self.even_row(*last, &members) {
                rows.push(from_error(&e, crate::obj! {"layout" => "dock"}));
            }
        } else if made.len() > 1 || (r.geom.weights.is_some() && !made.is_empty()) {
            let ones = vec![1.0; made.len()];
            if let Err(e) = self.reshape(&made, r.geom.weights.as_deref().unwrap_or(&ones)) {
                rows.push(from_error(&e, crate::obj! {"layout" => "weights"}));
            }
        } else if made.len() == 1 && r.geom.ratio.is_none() && !r.p.has("--no-equalize") {
            // a split shrinks its row or column in proportion, keeping any unevenness: even it out
            if let Err(e) = self.equalize_around(made[0]) {
                rows.push(from_error(&e, crate::obj! {"layout" => "equalize"}));
            }
        }
        Ok(rows)
    }

    /// The dock ([`geometry::dock`]) when an agent is spawned beside its spawner (the caller) with no
    /// `--at/--side/--stack/--to/--in`.
    fn dock(&self, r: &Request, s: &Snapshot, v: &View, agent: bool, at: Id) -> Option<Docked> {
        let pl = &r.place;
        if !agent || pl.at.is_some() || pl.side.is_some() || pl.stack.is_some() || pl.to.is_some() || pl.into.is_some() {
            return None;
        }
        let me = v.m.me.filter(|m| v.id(*m) == at)?;
        let tab = s.nodes[me].parent?;
        let layout = s.nodes[tab as usize].layout.as_ref()?;
        let kids: Vec<Id> = v
            .m
            .agent_parent
            .iter()
            .filter(|(c, p)| **p == me && s.nodes[**c].parent == Some(tab) && agentish(&s.nodes[**c]))
            .map(|(c, _)| v.id(*c))
            .collect();
        let is_agent = |id: Id| v.m.by_raw(&id.0.to_string()).is_some_and(|i| agentish(&s.nodes[i]));
        // the wider way, as a lone split goes (cells are about twice as tall as wide); a child in its
        // parent's dock row starts its own row under itself
        let in_a_row = v.m.agent_parent.get(&me).is_some_and(|p| s.nodes[*p].parent == Some(tab));
        let wide = !in_a_row && s.nodes[me].pane.as_ref().is_none_or(|p| (p.cols as f32) * 0.5 > p.rows as f32);
        let (pane, right) = geometry::dock(layout, at, &kids, &is_agent, wide);
        Some(Docked { at: pane, side: if right { Dir::Right } else { Dir::Down }, kids })
    }

    /// `--plan` of an agent spawn with no placement: the pane its dock splits, and the side.
    fn dock_plan(&mut self, r: &Request) -> Option<(String, &'static str)> {
        let (s, v) = self.view().ok()?;
        let me = v.m.me?;
        let d = self.dock(r, &s, &v, r.p.has("--agent") || r.noun == Noun::Agent, v.id(me))?;
        let beside = v.handle(v.m.by_raw(&d.at.0.to_string())?).to_string();
        Some((beside, if d.side == Dir::Right { "right" } else { "down" }))
    }

    /// Equal shares for the `members` that share the split holding `pane` (its dock row).
    fn even_row(&mut self, pane: Id, members: &[Id]) -> Result<()> {
        let s = self.conn.snapshot()?.clone();
        let ix = crate::app::model::Index::new(&s);
        let p = ix.get(pane).ok_or_else(|| Error::failed("the new pane is gone"))?;
        let tab = s.nodes[p].parent.map(|t| t as usize).ok_or_else(|| Error::failed("a pane without a tab"))?;
        let mut layout = s.nodes[tab].layout.clone().ok_or_else(|| Error::failed("the tab has no layout"))?;
        let row = geometry::row_of(&layout, pane, members);
        if row.len() > 1 {
            geometry::set_weights(&mut layout, &row, &vec![1.0; row.len()]).map_err(Error::Invalid)?;
            self.conn.call(Op::SetLayout { tab: s.nodes[tab].id, layout })?;
        }
        Ok(())
    }

    pub(crate) fn new_place_op(&self, to: &To, v: &View, spawn: Spawn, focus: bool) -> Result<Op> {
        let me = v.m.me;
        Ok(match to {
            To::NewWindow => Op::NewWindow { spawn, frame: None, focus },
            To::NewVTab => Op::NewVTab { window: me.and_then(|m| v.m.up(m, Kind::Window)).map(|i| v.id(i)), name: None, spawn, focus },
            To::NewTab => Op::NewTab { vtab: me.and_then(|m| v.m.up(m, Kind::VTab)).map(|i| v.id(i)), title: None, spawn, focus },
            To::Sel(_) => return Err(Error::invalid("--to SEL places beside a tab's last pane")),
        })
    }

    /// The tab holding `pane` and its split tree, from a fresh snapshot (the pane was just made).
    fn tab_layout(&mut self, pane: Id) -> Result<(Id, Layout)> {
        let s = self.conn.snapshot()?;
        let p = crate::app::model::Index::new(s).get(pane).ok_or_else(|| Error::failed("the new pane is gone"))?;
        let tab = s.nodes[p].parent.map(|t| t as usize).ok_or_else(|| Error::failed("a pane without a tab"))?;
        let layout = s.nodes[tab].layout.clone().ok_or_else(|| Error::failed("the tab has no layout"))?;
        Ok((s.nodes[tab].id, layout))
    }

    /// Give panes that share one split these weights.
    fn reshape(&mut self, panes: &[Id], weights: &[f64]) -> Result<()> {
        let (tab, mut layout) = self.tab_layout(panes[0])?;
        geometry::set_weights(&mut layout, panes, weights).map_err(Error::Invalid)?;
        self.conn.call(Op::SetLayout { tab, layout })?;
        Ok(())
    }

    /// Even out the row or column a new pane split into ([`geometry::equalize_around`]).
    fn equalize_around(&mut self, pane: Id) -> Result<()> {
        let (tab, mut layout) = self.tab_layout(pane)?;
        if geometry::equalize_around(&mut layout, pane) {
            self.conn.call(Op::SetLayout { tab, layout })?;
        }
        Ok(())
    }

    fn content_save(&mut self, r: &Request, s: &Snapshot, v: &View) -> Result<Vec<Obj>> {
        let to = r.p.one("--to").ok_or_else(|| Error::invalid("content save --to FILE"))?;
        let pane = match &r.sel {
            Some(e) => self.one_node(v, e, Kind::Pane, "content save")?,
            None => v.m.me.ok_or_else(|| Error::invalid("content save PANE --to FILE"))?,
        };
        if !r.apply && !v.mine(s, pane, false) {
            return Ok(vec![planned("saved", crate::obj! {"pane" => v.handle(pane), "to" => to})]);
        }
        let lines = match r.p.one("--tail") {
            Some(t) => Lines::Tail(t.parse().map_err(|_| Error::invalid("--tail N"))?),
            None => Lines::Tail(u32::MAX),
        };
        let d = self.conn.call(Op::Read { pane: v.id(pane), lines })?;
        let path = self.conn.env().cwd.join(to);
        let mut text = d.lines.join("\n");
        text.push('\n');
        crate::store::atomic_write(&path, text.as_bytes())?;
        Ok(vec![ok("saved", crate::obj! {"pane" => v.handle(pane), "to" => path.display().to_string(), "lines" => d.lines.len()})])
    }

    // ---- writes -------------------------------------------------------------------------------

    fn write_verb(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        use Noun as N;
        use Verb as V;
        let row = |code: &str, i: usize| ok(code, crate::obj! {"id" => v.handle(i)});
        let each = |this: &mut Exec<'_>, code: &str, f: &dyn Fn(usize) -> Op| -> Vec<Obj> {
            targets
                .iter()
                .map(|i| match this.conn.call(f(*i)) {
                    Ok(_) => row(code, *i),
                    Err(e) => from_error(&e, crate::obj! {"id" => v.handle(*i)}),
                })
                .collect()
        };
        match (r.noun, r.verb) {
            (_, V::Close) => {
                let ids: Vec<Id> = targets.iter().map(|i| v.id(*i)).collect();
                self.conn.call(Op::Close { ids })?;
                Ok(targets.iter().map(|i| row("closed", *i)).collect())
            }
            (_, V::Select) => {
                let i = *targets.last().expect("non-empty");
                self.conn.call(Op::Select { id: v.id(i) })?;
                Ok(vec![row("selected", i)])
            }
            (N::Window, V::Set) => self.window_set(r, s, v, targets),
            (N::Window, V::Merge) => self.window_merge(r, v, targets),
            (N::VTab, V::Set) | (N::Tab, V::Set) | (N::Pane | N::Agent, V::Set) => self.node_set(r, s, v, targets),
            // a tab onto a pane, as dropped on its half: the tab's panes split in beside it
            (N::Tab, V::Move) if r.place.at.is_some() => {
                let dest = self.pane_dest(r, v)?;
                Ok(each(self, "moved", &|i| Op::Move { id: v.id(i), to: dest.clone() }))
            }
            (N::VTab | N::Tab, V::Move) => self.container_move(r, v, targets),
            (N::Pane | N::Agent, V::Move) => {
                let dest = self.pane_dest(r, v)?;
                Ok(each(self, "moved", &|i| Op::Move { id: v.id(i), to: dest.clone() }))
            }
            (_, V::Order) => self.order(r, v),
            (N::Pane | N::Agent, V::Resize) => self.pane_resize(r, s, v, targets),
            (N::Pane | N::Agent, V::Send) => self.send(r, s, v, targets),
            (N::Pane | N::Agent, V::Key) => self.pane_key(r, s, v, targets),
            (N::Pane, V::Run) => self.pane_run(r, s, v, targets),
            (N::Content, V::Scroll) => {
                let to = scroll_to(r)?;
                Ok(each(self, "scrolled", &|i| Op::Scroll { pane: v.id(i), to }))
            }
            (N::Content, V::Clear) => {
                let sb = r.p.has("--scrollback");
                Ok(each(self, "cleared", &|i| Op::Clear { pane: v.id(i), scrollback: sb }))
            }
            (n, vb) => Err(Error::invalid(format!("{n} {vb} is not implemented"))),
        }
    }

    /// `window merge SEL|--all [--into W]`: every tab of the other windows into one (the first selected).
    fn window_merge(&mut self, r: &Request, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let into = match r.p.one("--into") {
            Some(x) => self.one_node(v, &grammar::selector(x).map_err(|e| Error::invalid(e.to_string()))?, Kind::Window, "--into")?,
            None => targets[0],
        };
        let from: Vec<Id> = if r.all { v.m.project(0, Some(Kind::Window)) } else { targets.to_vec() }
            .into_iter()
            .filter(|w| *w != into)
            .map(|w| v.id(w))
            .collect();
        if from.is_empty() {
            return Err(Error::invalid("window merge: name the windows to merge (or --all)"));
        }
        self.conn.call(Op::Merge { into: v.id(into), from: from.clone() })?;
        Ok(vec![ok("merged", crate::obj! {"into" => v.handle(into), "windows" => from.len()})])
    }

    /// Where `pane move` (or `tab move --at`) puts what it moves: beside `--at PANE` on `--side` (right), beside
    /// the last pane of `--to TAB`, or into a new tab, vertical tab or window.
    fn pane_dest(&self, r: &Request, v: &View) -> Result<Dest> {
        let beside = |at: usize| Dest::Beside { at: v.id(at), side: side_of(r.place.side.unwrap_or(Dir::Right)) };
        Ok(match (&r.place.at, &r.place.to) {
            (Some(e), _) => beside(self.one_node(v, e, Kind::Pane, "--at")?),
            (None, Some(To::NewTab)) => Dest::NewTab,
            (None, Some(To::NewVTab)) => Dest::NewVTab,
            (None, Some(To::NewWindow)) => Dest::NewWindow,
            (None, Some(To::Sel(e))) => {
                let tab = self.one_node(v, e, Kind::Tab, "--to")?;
                beside(*v.m.project(tab, Some(Kind::Pane)).last().ok_or_else(|| Error::invalid("that tab has no pane"))?)
            }
            (None, None) => return Err(Error::invalid("pane move --at PANE --side S | --to new-tab|new-vtab|new-window|TAB")),
        })
    }

    /// `<noun> order SEL…`: the nodes in the order the words give, each word resolved on its own.
    fn order(&mut self, r: &Request, v: &View) -> Result<Vec<Obj>> {
        let kind = r.noun.kind().unwrap_or(Kind::Pane);
        let mut ids = Vec::new();
        for w in &r.words {
            let e = grammar::selector(w).map_err(|e| Error::invalid(e.to_string()))?;
            for i in select::eval(&v.m, &e, Some(kind), &Ctx::default()).map_err(Error::Invalid)? {
                if !ids.contains(&v.id(i)) {
                    ids.push(v.id(i));
                }
            }
        }
        let n = ids.len();
        self.conn.call(Op::Order { ids })?;
        Ok(vec![ok("ordered", crate::obj! {"count" => n})])
    }

    /// `pane key --key 'K …'`: the keys pressed in each pane in turn; a pane stops at its first failed key.
    /// An exited pane refuses keys, except a cancel (Esc, Ctrl-C) or with `--force`.
    fn pane_key(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let spec = r.p.one("--key").or(r.words.last().map(String::as_str)).ok_or_else(|| Error::invalid("pane key --key K"))?;
        let keys = parse_keys(spec)?;
        if !is_cancel(&keys) && !r.force {
            self.typing_allowed(s, v, targets)?;
        }
        let mut out = Vec::new();
        for &i in targets {
            let pressed = keys.iter().try_for_each(|k| self.conn.call(Op::Key { pane: v.id(i), key: *k }).map(drop));
            out.push(match pressed {
                Ok(()) => ok("keyed", crate::obj! {"id" => v.handle(i)}),
                Err(e) => from_error(&e, crate::obj! {"id" => v.handle(i)}),
            });
        }
        Ok(out)
    }

    fn window_set(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let mut out = if r.p.given("--sidebar") { crate::action::toggles(self, r, s, v, targets)? } else { Vec::new() };
        if let Some(t) = r.p.one("--title") {
            for &i in targets {
                self.conn.call(Op::Set { id: v.id(i), field: Field::Name(t.to_string()) })?;
                out.push(ok("set", crate::obj! {"id" => v.handle(i), "title" => t}));
            }
        }
        if let Some(st) = r.p.one("--state") {
            let want = match st {
                "normal" => WinState::Normal,
                "zoomed" => WinState::Zoomed,
                "minimized" => WinState::Minimized,
                "fullscreen" => WinState::FullScreen,
                _ => return Err(Error::invalid("--state normal|zoomed|minimized|fullscreen")),
            };
            for &i in targets {
                self.conn.call(Op::Set { id: v.id(i), field: Field::WindowState(want) })?;
                out.push(ok("set", crate::obj! {"id" => v.handle(i), "state" => st}));
            }
        }
        let frames: Vec<geometry::Frame> = if let Some(f) = r.geom.frame {
            vec![f; targets.len()]
        } else if let Some(mode) = r.geom.tile {
            // window frames are in points, as the tile is
            let area = screen_area(s, targets);
            geometry::tile(area, targets.len(), mode, r.geom.weights.as_deref())
        } else if r.geom.cols.is_some() || r.geom.rows.is_some() {
            let cells = |n: Option<i64>, what: &str| n.map(|c| u16::try_from(c).ok().filter(|c| (1..=2000).contains(c)).ok_or_else(|| Error::invalid(format!("--{what} 1–2000")))).transpose();
            let (cols, rows) = (cells(r.geom.cols, "cols")?, cells(r.geom.rows, "rows")?);
            for &i in targets {
                let d = self.conn.call(Op::WindowCells { window: v.id(i), pane: None, cols, rows })?;
                out.push(ok("sized", crate::obj! {"id" => v.handle(i), "pane" => d.ids.first().map(|p| view::pane_handle(*p)), "size" => d.lines.first()}));
            }
            vec![]
        } else {
            vec![]
        };
        for (k, f) in frames.iter().enumerate() {
            let i = targets[k];
            let rect = Rect { x: f.x as f32, y: f.y as f32, w: f.width as f32, h: f.height as f32 };
            self.conn.call(Op::Set { id: v.id(i), field: Field::Frame(rect) })?;
            out.push(ok("framed", crate::obj! {"id" => v.handle(i), "frame" => format!("{},{},{},{}", f.x, f.y, f.width, f.height)}));
        }
        if out.is_empty() {
            return Err(Error::invalid("window set --title T | --state normal|zoomed|minimized|fullscreen | --frame x,y,w,h | --tile columns|rows|grid | --cols N --rows N | --sidebar on|off|toggle"));
        }
        Ok(out)
    }

    /// `vtab|tab|pane set`: `--name`, `--color` and `--set k=v` on each node, and the verbs' own extras
    /// (`--zoom`, `--group`, a tab's `--tile`/`--equalize`). A live agent's session takes a new name and color
    /// too, typed as its own `/rename` and `/color` once its input box is drawn.
    fn node_set(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let fields = set_fields(r)?;
        let mut rows = if r.p.given("--zoom") && matches!(r.noun, Noun::Pane | Noun::Agent) { crate::action::toggles(self, r, s, v, targets)? } else { Vec::new() };
        if let Some(g) = r.p.one("--group") {
            rows.extend(self.group_add(s, v, targets, g)?);
        }
        if r.noun == Noun::Tab {
            rows.extend(self.tab_arrange(r, s, v, targets)?);
        }
        if fields.is_empty() && rows.is_empty() {
            return Err(Error::invalid(format!("{} set --name N | --color C | --group G | --set k=v{}", r.noun, match r.noun { Noun::Tab => " | --equalize", Noun::Pane | Noun::Agent => " | --zoom on|off|toggle | --bury", _ => "" })));
        }
        let name = fields.iter().find_map(|(_, f)| if let Field::Name(n) = f { Some(n.trim()) } else { None }).filter(|n| !n.is_empty());
        for &i in targets {
            let sess = Session::of(&s.nodes[i]);
            for (what, f) in &fields {
                rows.push(match (f, sess.provider) {
                    // a Claude agent's color is its own session color (`/color`); any other pane's is its tab color
                    (Field::Color(c), "claude") => self.claude_color(v, i, c.as_deref(), &sess),
                    _ => match self.conn.call(Op::Set { id: v.id(i), field: f.clone() }) {
                        Ok(_) => ok("set", crate::obj! {"id" => v.handle(i), "field" => what}),
                        Err(e) => from_error(&e, crate::obj! {"id" => v.handle(i), "field" => what}),
                    },
                });
            }
            if let (Some(nm), crate::app::Kind::Pane, false) = (name, s.nodes[i].kind, r.p.has("--no-session")) {
                rows.extend(self.rename_session(s, v, i, nm, &sess));
            }
        }
        Ok(rows)
    }

    /// `--group G`: each node joins group G (its `groups` variable), keeping the groups it is in.
    fn group_add(&mut self, s: &Snapshot, v: &View, targets: &[usize], g: &str) -> Result<Vec<Obj>> {
        let mut rows = Vec::new();
        for &i in targets {
            let mut groups: Vec<String> = s.nodes[i].var(view::V_GROUPS).unwrap_or("").split(',').filter(|x| !x.is_empty()).map(String::from).collect();
            if !groups.iter().any(|x| x == g) {
                groups.push(g.to_string());
            }
            self.conn.call(Op::Set { id: v.id(i), field: Field::Var(view::V_GROUPS.into(), Some(groups.join(","))) })?;
            rows.push(ok("set", crate::obj! {"id" => v.handle(i), "groups" => groups.join(",")}));
        }
        Ok(rows)
    }

    /// `tab set --tile grid|columns|rows` (the tab's panes, in their order) and `--equalize`.
    fn tab_arrange(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let mut rows = Vec::new();
        if let Some(mode) = r.geom.tile {
            for &i in targets {
                let mut ids = Vec::new();
                if let Some(l) = &s.nodes[i].layout {
                    l.panes(&mut ids);
                }
                if let Some(l) = geometry::tiled(&ids, mode) {
                    self.conn.call(Op::SetLayout { tab: v.id(i), layout: l })?;
                    rows.push(ok("tiled", crate::obj! {"id" => v.handle(i), "panes" => ids.len()}));
                }
            }
        }
        if r.geom.equalize {
            for &i in targets {
                if let Some(mut l) = s.nodes[i].layout.clone() {
                    geometry::equalize(&mut l);
                    self.conn.call(Op::SetLayout { tab: v.id(i), layout: l })?;
                    rows.push(ok("equalized", crate::obj! {"id" => v.handle(i)}));
                }
            }
        }
        Ok(rows)
    }

    /// A Claude agent's `--color`: the pane's color variable, and `/color` typed into the session once it is
    /// typable (else a `later` row with the command to run then).
    fn claude_color(&mut self, v: &View, i: usize, color: Option<&str>, sess: &Session) -> Obj {
        let (line, hex) = agent::claude_color_line(color);
        let _ = self.conn.call(Op::Set { id: v.id(i), field: Field::Var("color".into(), hex.map(str::to_string)) });
        if !sess.typable {
            return ok("session", crate::obj! {"id" => v.handle(i), "color" => "later", "state" => sess.state, "hint" => format!("the session is not ready; run pane set {} --color C once it is", v.handle(i))});
        }
        match self.conn.call(Op::Deliver { pane: v.id(i), text: line.clone(), enter: true }) {
            Ok(d) => ok("session", crate::obj! {"id" => v.handle(i), "color" => line.trim_start_matches("/color ").to_string(), "delivery" => d.lines.first().cloned().unwrap_or_default()}),
            Err(e) => from_error(&e, crate::obj! {"id" => v.handle(i), "field" => "color"}),
        }
    }

    /// A pane's new name, carried to its agent: the agent's own name (messages and rows say who it is now, not
    /// who it was spawned as) and, for a live agent, its session's (`/rename`, typed once it is typable).
    fn rename_session(&mut self, s: &Snapshot, v: &View, i: usize, nm: &str, sess: &Session) -> Option<Obj> {
        if s.nodes[i].var(view::V_NAME).is_some_and(|old| old != nm) {
            let _ = self.conn.call(Op::Set { id: v.id(i), field: Field::Var(view::V_NAME.into(), Some(nm.to_string())) });
            if let Some(parent) = s.nodes[i].var(view::V_PARENT) {
                self.ledger_rename(parent, v.id(i), nm);
            }
        }
        // only a live agent: one that quit back to its shell would get `/rename` typed at the prompt
        if !sess.provider.is_empty() && !sess.typable {
            let state = if sess.state.is_empty() { "unknown" } else { sess.state };
            return Some(ok("session", crate::obj! {"id" => v.handle(i), "renamed" => "later", "state" => sess.state, "hint" => format!("the session is not ready ({state}); run pane set {} --name {nm} once it is", v.handle(i))}));
        }
        let line = agent::rename_line(sess.provider, nm)?;
        Some(match self.conn.call(Op::Deliver { pane: v.id(i), text: line, enter: true }) {
            Ok(d) => ok("session", crate::obj! {"id" => v.handle(i), "renamed" => nm, "delivery" => d.lines.first().cloned().unwrap_or_default()}),
            Err(e) => from_error(&e, crate::obj! {"id" => v.handle(i), "field" => "session"}),
        })
    }

    /// vtab/tab move: `--to new-window|new-vtab|W|V`, optionally partitioned (`--groups N`,
    /// `--by K`): each part goes to its own new window.
    fn container_move(&mut self, r: &Request, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let parts: Vec<Vec<usize>> = match (r.multi.groups, r.multi.by) {
            (Some(n), _) => select::parts(targets, n),
            (None, Some(by)) => group_by(targets, |i| self.by_key(v, i, by)),
            (None, None) => vec![targets.to_vec()],
        };
        let to = r.place.to.as_ref().ok_or_else(|| Error::invalid(format!("{} move --to new-window|new-vtab|SEL", r.noun)))?;
        if parts.len() > 1 && !matches!(to, To::NewWindow) {
            return Err(Error::invalid("--groups/--by send each part to its own new window: --to new-window"));
        }
        let mut rows = Vec::new();
        for part in parts {
            let mut dest: Option<Dest> = None;
            for (k, &i) in part.iter().enumerate() {
                let d = match (&dest, to) {
                    (Some(d), _) => d.clone(),
                    (None, To::NewWindow) => Dest::NewWindow,
                    (None, To::NewVTab) if r.noun == Noun::Tab => Dest::NewVTab,
                    (None, To::NewVTab) => return Err(Error::invalid("a vertical tab moves --to new-window or a window")),
                    (None, To::NewTab) => return Err(Error::invalid("--to new-tab is for panes")),
                    (None, To::Sel(e)) => {
                        let want = if r.noun == Noun::Tab { Kind::VTab } else { Kind::Window };
                        let found = select::eval(&v.m, e, None, &Ctx::default()).map_err(Error::Invalid)?;
                        let c = found.first().and_then(|x| v.m.project(*x, Some(want)).first().copied()).ok_or_else(|| Error::invalid("--to names no container"))?;
                        Dest::Into(v.id(c))
                    }
                };
                self.conn.call(Op::Move { id: v.id(i), to: d.clone() })?;
                rows.push(ok("moved", crate::obj! {"id" => v.handle(i)}));
                // the rest of this part follows the first into the container it now has
                if k == 0 && dest.is_none() {
                    let want = if r.noun == Noun::Tab { crate::app::Kind::VTab } else { crate::app::Kind::Window };
                    dest = self.container_of(v.id(i), want)?.map(Dest::Into);
                }
            }
        }
        Ok(rows)
    }

    /// The container of kind `want` that `id` is in now (a fresh snapshot: it just moved).
    fn container_of(&mut self, id: Id, want: crate::app::Kind) -> Result<Option<Id>> {
        let s = self.conn.snapshot()?;
        let parent = crate::app::model::Index::new(s).get(id).and_then(|j| s.nodes[j].parent).map(|p| &s.nodes[p as usize]);
        Ok(parent.filter(|p| p.kind == want).map(|p| p.id))
    }

    /// The `--by K` key of a node: its first pane's folder, project (the repository holding it), kind or groups.
    fn by_key(&self, v: &View, i: usize, by: By) -> String {
        let panes = v.m.project(i, Some(Kind::Pane));
        let first = panes.first().map(|p| &v.m.nodes[*p]);
        let f = |k: &str| first.and_then(|n| n.fields.get(k)).cloned().unwrap_or_default();
        match by {
            By::Cwd => f("cwd"),
            By::Project => {
                let cwd = f("cwd");
                git_top(Path::new(&cwd)).map(|p| p.display().to_string()).unwrap_or(cwd)
            }
            By::Kind => f("kind"),
            By::Group => f("groups"),
        }
    }

    fn pane_resize(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let tab = v.m.up(targets[0], Kind::Tab).ok_or_else(|| Error::invalid("a pane without a tab"))?;
        if targets.iter().any(|t| v.m.up(*t, Kind::Tab) != Some(tab)) {
            return Err(Error::invalid("pane resize: the panes must share one tab"));
        }
        let mut layout: Layout = s.nodes[tab].layout.clone().ok_or_else(|| Error::failed("the tab has no layout"))?;
        let ids: Vec<Id> = targets.iter().map(|i| v.id(*i)).collect();
        if let Some(w) = &r.geom.weights {
            geometry::set_weights(&mut layout, &ids, w).map_err(Error::Invalid)?;
        } else if let Some(x) = r.geom.ratio {
            geometry::set_ratio(&mut layout, ids[0], x).map_err(Error::Invalid)?;
        } else if r.geom.equalize {
            geometry::equalize(&mut layout);
        } else {
            return Err(Error::invalid("pane resize --ratio R | --weights a,b,… | --equalize"));
        }
        self.conn.call(Op::SetLayout { tab: v.id(tab), layout })?;
        Ok(vec![ok("resized", crate::obj! {"tab" => v.handle(tab), "panes" => ids.len()})])
    }

    /// Keys and commands go to live panes. (Text never answers a permission or trust prompt: the app's delivery
    /// engine holds it until the prompt is gone.)
    fn typing_allowed(&mut self, s: &Snapshot, v: &View, targets: &[usize]) -> Result<()> {
        for &i in targets {
            if s.nodes[i].pane.as_ref().is_some_and(|p| p.exit.is_some()) {
                return Err(Error::policy("exited", format!("{} has exited", v.handle(i))));
            }
        }
        Ok(())
    }

    /// `pane send --text T [--enter [--wait]] | --path F`.
    pub(crate) fn send(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        if let Some(t) = r.p.one("--text") {
            return self.deliver(r, s, v, targets, t);
        }
        // a dropped file: its quoted path and a space, as the drop inserts it
        let path = r.p.one("--path").ok_or_else(|| Error::invalid("pane send --text T [--enter] | --path F"))?;
        let abs = self.conn.env().cwd.join(path);
        let bytes = format!("{} ", agent::quote(&abs.to_string_lossy())).into_bytes();
        self.typing_allowed(s, v, targets)?;
        let mut out = Vec::new();
        for &i in targets {
            out.push(match self.conn.call(Op::Send { pane: v.id(i), bytes: bytes.clone(), paste: true }) {
                Ok(_) => ok("sent", crate::obj! {"id" => v.handle(i), "bytes" => bytes.len()}),
                Err(e) => from_error(&e, crate::obj! {"id" => v.handle(i)}),
            });
        }
        Ok(out)
    }

    /// Text goes through the app's delivery engine: whole, in order, Enter after it shows, held behind prompts.
    /// `--wait` (with `--enter`): until the turn the message starts is over in every pane, by one deadline.
    fn deliver(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize], text: &str) -> Result<Vec<Obj>> {
        self.typing_allowed(s, v, targets)?;
        let enter = r.p.has("--enter");
        let wait = enter && r.p.has("--wait");
        let timeout = crate::watch::timeout_of(r)?.unwrap_or(Duration::from_secs(600));
        let deadline = Instant::now() + timeout;
        // subscribed before the first delivery, so no state change is missed
        let mut watch = if wait { Some(self.conn.watch(None, Some(timeout))?.0) } else { None };
        let mut out = Vec::new();
        for &i in targets {
            out.push(match self.conn.call(Op::Deliver { pane: v.id(i), text: text.to_string(), enter }) {
                Ok(d) => delivered(v.handle(i), d.lines.first().map(String::as_str).unwrap_or("delivered")),
                Err(e) => from_error(&e, crate::obj! {"id" => v.handle(i)}),
            });
        }
        if let Some(w) = watch.as_mut() {
            let ids: Vec<Id> = targets.iter().map(|&i| v.id(i)).collect();
            if !self.wait_turns(w, &ids, deadline)? {
                out.push(from_error(&Error::Timeout("the turn didn't finish".into()), crate::obj! {}));
            }
        }
        Ok(out)
    }

    /// Wait on the pushed events until every pane in `ids` is through a turn ([`Turns`]); false if `deadline`
    /// passed first.
    fn wait_turns(&mut self, w: &mut crate::conn::Watch, ids: &[Id], deadline: Instant) -> Result<bool> {
        use crate::app::Event as E;
        let mut turns = Turns::default();
        while turns.over() < ids.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            w.set_deadline(Some(left))?;
            let Some(p) = w.next()? else { return Ok(false) };
            match p.ev {
                E::Var { id, ref key, ref value } if key == view::V_STATE && ids.contains(&id) => turns.state(id, value.as_deref()),
                E::Exited { pane, .. } | E::Closed { id: pane, .. } if ids.contains(&pane) => turns.end(pane),
                // dropped events may hold the turn's busy and idle: read the states now
                E::Gap { .. } => {
                    self.conn.invalidate();
                    let s = self.conn.snapshot()?;
                    let ix = crate::app::model::Index::new(s);
                    for id in ids {
                        let n = ix.get(*id).map(|i| &s.nodes[i]);
                        let ended = n.is_none_or(|n| n.pane.as_ref().is_some_and(|p| p.exit.is_some()));
                        turns.reread(*id, ended, n.and_then(|n| n.var(view::V_STATE)));
                    }
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// `pane run --cmd C [--until RE] [--timeout S]`: type the command, then wait (pushed events,
    /// no polling) until the screen matches RE, or the shell's next prompt mark.
    fn pane_run(&mut self, r: &Request, s: &Snapshot, v: &View, targets: &[usize]) -> Result<Vec<Obj>> {
        let cmd = r.p.one("--cmd").ok_or_else(|| Error::invalid("pane run --cmd C [--until RE]"))?;
        let until = r.p.one("--until").map(regex::Regex::new).transpose().map_err(|e| Error::invalid(format!("--until: {e}")))?;
        let timeout = crate::watch::timeout_of(r)?.unwrap_or(std::time::Duration::from_secs(60));
        if !r.force {
            self.typing_allowed(s, v, targets)?;
        }
        let mut rows = Vec::new();
        for &i in targets {
            let pane = v.id(i);
            let (mut w, _) = self.conn.watch(None, Some(timeout))?;
            let mut b = cmd.as_bytes().to_vec();
            b.push(b'\r');
            self.conn.call(Op::Send { pane, bytes: b, paste: false })?;
            let deadline = std::time::Instant::now() + timeout;
            let mut matched = None;
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                w.set_deadline(Some(left))?;
                let Some(p) = w.next()? else { break };
                use crate::app::Event as E;
                let mine = p.ev.subject() == Some(pane);
                let check = mine && matches!(p.ev, E::Quiet { .. } | E::Prompt { .. } | E::Exited { .. });
                if !check {
                    continue;
                }
                if let Some(re) = &until {
                    let d = self.conn.call(Op::Read { pane, lines: Lines::Screen })?;
                    if let Some(l) = d.lines.iter().find(|l| re.is_match(l)) {
                        matched = Some(l.clone());
                        break;
                    }
                } else if matches!(p.ev, E::Prompt { mark: 'D' | 'A', .. } | E::Quiet { .. }) {
                    matched = Some(String::new());
                    break;
                }
                if matches!(p.ev, E::Exited { .. }) {
                    break;
                }
            }
            rows.push(match matched {
                Some(m) => ok("ran", crate::obj! {"id" => v.handle(i), "matched" => (!m.is_empty()).then_some(m)}),
                None => from_error(&Error::Timeout(format!("{} did not reach --until within {:?}", v.handle(i), timeout)), crate::obj! {"id" => v.handle(i)}),
            });
        }
        Ok(rows)
    }

    // ---- items --------------------------------------------------------------------------------

    /// What each new pane gets, one item per `--count`/`--task`/task-file item/`--each` row.
    pub(crate) fn items(&mut self, r: &Request, s: &Snapshot, v: &View, agent: bool) -> Result<Vec<Item>> {
        let mut tasks: Vec<String> = r.multi.tasks.iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        let base = self.conn.env().cwd.clone();
        let mut file_items: Vec<String> = Vec::new();
        let read = |f: &str| std::fs::read_to_string(base.join(f)).map_err(|e| Error::invalid(format!("{f}: {e}")));
        for f in &r.multi.task_files {
            // `{i}`/`{n}` in the path: one file per item, as `--task` takes them
            if f.contains("{i}") || f.contains("{n}") {
                let n = r.multi.count.ok_or_else(|| Error::invalid(format!("--task-file {f}: {{i}} needs --count N")))?;
                for i in 1..=n {
                    file_items.push(read(&tpl(f, i, n))?.trim().to_string());
                }
                continue;
            }
            let text = read(f)?;
            // a file is one task; `--split` makes one per `## ` section (else top-level list item)
            file_items.extend(if r.p.has("--split") { agent::split_tasks(&text) } else { vec![text.trim().to_string()] });
        }
        let rows = match &r.multi.each {
            Some(f) => each_rows(&base.join(f))?,
            None => vec![],
        };
        let count = r.multi.count.unwrap_or(0);
        let cwds = r.p.many("--cwd");
        // --task T… then the files' items, one agent each; a single task, from either, goes to every item
        let from_file = file_items.len();
        if from_file > 1 && count > 0 && count != from_file + tasks.len() {
            return Err(Error::invalid(format!("the task file has {from_file} items for --count {count}: one item each, or one item for all")));
        }
        tasks.extend(file_items);
        let n = count.max(tasks.len()).max(rows.len()).max(cwds.len()).max(1);
        if tasks.len() > 1 && tasks.len() != n {
            return Err(Error::invalid(format!("{} tasks for {n} items: one task each, or one for all", tasks.len())));
        }
        if n > 32 {
            return Err(Error::invalid("at most 32 items at once"));
        }
        // the default folder: the caller's pane's, else where the CLI runs
        let scope = v.m.me.and_then(|m| s.nodes[m].pane.as_ref()).map(|p| p.cwd.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| base.display().to_string());
        let repo = match &r.multi.worktree {
            Some(_) => Some(git_top(Path::new(cwds.first().map(String::as_str).unwrap_or(&scope))).ok_or_else(|| Error::policy("not-git", format!("--worktree: {scope} is not in a Git repository")))?),
            None => None,
        };
        if r.p.one("--name").is_some_and(|x| !x.contains("{i}")) && n > 1 {
            return Err(Error::invalid("--name for several items needs {i}, or leave it out"));
        }
        let next = self.next_index();
        // the provider: claude (default), codex or omp; model names are Claude's to resolve, others' pass through
        let provider = r.p.one("--provider").map(|p| p.to_ascii_lowercase());
        if let Some(p) = &provider {
            if crate::app::agent::Provider::parse(p).is_none() {
                return Err(Error::invalid(format!("provider '{p}': claude, codex or omp")));
            }
        }
        let claude = provider.as_deref().is_none_or(|p| p == "claude");
        let resolve = |m: &str| if claude { agent::resolve_model(m) } else { Ok(m.to_string()) };
        let model = r.p.one("--model").map(resolve).transpose()?;
        let effort = r.p.one("--effort").map(agent::check_effort).transpose()?;
        let mut out = Vec::new();
        let mut taken = match &repo {
            Some(rp) => self.names_taken(s, rp),
            None => vec![],
        };
        for i in 1..=n {
            let row = rows.get(i - 1).cloned().unwrap_or_default();
            let get = |k: &str| row.get(k).cloned().filter(|x| !x.is_empty());
            let task = get("task").or_else(|| tasks.get(i - 1).or(tasks.first().filter(|_| tasks.len() == 1)).cloned()).map(|t| tpl(&t, i, n));
            let name = match (get("name").or_else(|| r.p.one("--name").map(str::to_string)), &repo) {
                (Some(x), _) => Some(tpl(&x, i, n)),
                // a worktree is named for its feature (its branch is the history), never w<N>
                (None, Some(_)) => Some(crate::land::feature_name(task.as_deref(), &mut taken)?),
                (None, None) => agent.then(|| format!("w{}", next + i - 1)),
            };
            if let Some(nm) = &name {
                if !grammar::is_slug(nm) {
                    return Err(Error::invalid(format!("name '{nm}': 1–64 letters, digits, _ and -")));
                }
            }
            let mut worktree = None;
            let cwd = match (&repo, get("cwd")) {
                (_, Some(c)) => folder(&base, &tpl(&c, i, n))?,
                (Some(repo), None) => {
                    let nm = name.clone().unwrap_or_else(|| format!("w{i}"));
                    let path = crate::agents::worktree_create(repo, &nm, r.multi.worktree.clone().flatten().as_deref())?;
                    worktree = Some(path.display().to_string());
                    path.display().to_string()
                }
                (None, None) => match cwds.get(i - 1).or(cwds.first()) {
                    Some(c) => folder(&base, &tpl(c, i, n))?,
                    None => tpl(&scope, i, n),
                },
            };
            if task.is_some() && !agent {
                return Err(Error::invalid("--task needs --agent"));
            }
            out.push(Item {
                name,
                cwd,
                task,
                cmd: get("cmd")
                    .or_else(|| r.p.one("--cmd").map(str::to_string))
                    .map(|c| tpl(&c, i, n))
                    .or_else(|| provider.clone().filter(|_| agent)),
                model: get("model").map(|m| resolve(&m)).transpose()?.or(model.clone()),
                effort: get("effort").map(|e| agent::check_effort(&e)).transpose()?.or(effort.clone()),
                worktree,
                title: None,
            });
        }
        // writers never share a folder
        if agent && !r.p.has("--read-only") && n > 1 {
            for (k, a) in out.iter().enumerate() {
                if out[k + 1..].iter().any(|b| b.cwd == a.cwd) {
                    return Err(Error::policy("overlap", format!("two writing agents would share {}; pass --worktree, {{i}} in --cwd, or --read-only", a.cwd)));
                }
            }
        }
        Ok(out)
    }

    /// The process a new pane runs, its variables (the agent overlay) and its name.
    pub(crate) fn spawn_for(&mut self, r: &Request, it: &Item, agent: bool) -> Result<Spawn> {
        let env = self.conn.env().clone();
        let parent = env.me.map(view::pane_handle).unwrap_or_else(|| view::OUTSIDE.to_string());
        let mut vars = vec![(view::V_PARENT.to_string(), parent), (view::V_KIND.to_string(), it.kind(agent).to_string())];
        let cmd = if agent {
            let base = it.cmd.clone().unwrap_or_else(|| agent::AGENT_COMMAND.to_string());
            let task_file = it.task.as_deref().map(|t| task_file(&env.state, it, t)).transpose()?;
            vars.push((view::V_CMD.into(), base.clone()));
            for (k, val) in [(view::V_MODEL, &it.model), (view::V_EFFORT, &it.effort), (view::V_WORKTREE, &it.worktree)] {
                if let Some(x) = val {
                    vars.push((k.into(), x.clone()));
                }
            }
            if let Some(t) = &it.task {
                vars.push((view::V_TASK.into(), t.chars().take(200).collect()));
            }
            // Codex's sandbox writes only its folder; a worktree's index and refs live in the repository's git
            // dir, so without it Codex can't commit (live run 2026-10-29)
            let git_dir = it.worktree.as_deref().filter(|_| it.kind(agent) == "codex").and_then(git_common_dir);
            let base = match &git_dir {
                Some(d) if agent::plain_codex(&base) => format!("{base} --add-dir {}", agent::quote(d)),
                _ => base,
            };
            Some(agent::agent_command(&base, it.model.as_deref(), it.effort.as_deref(), it.title.as_deref().or(it.name.as_deref()), task_file.as_deref()))
        } else {
            it.cmd.clone()
        };
        if let Some(nm) = &it.name {
            vars.push((view::V_NAME.into(), nm.clone()));
        }
        if let Some(c) = self.spawn_color(r, it, agent)? {
            // this run's own, which the snapshot it read doesn't have yet
            self.colored.push(c.clone());
            vars.push(("color".into(), c));
        }
        Ok(Spawn { cwd: Some(it.cwd.clone()), cmd, env: vec![], vars, name: it.title.clone().or(it.name.clone()) })
    }

    /// A new pane's accent (the app takes the variable as the pane's color): `--color`, else for an agent one no
    /// live pane has yet, so a tab of agents reads apart. A titled agent (a resumed sub-agent) is picked by its
    /// title, so every resume of it is the same color.
    fn spawn_color(&mut self, r: &Request, it: &Item, agent: bool) -> Result<Option<String>> {
        Ok(match r.p.one("--color") {
            Some("none") => None,
            Some(c) => Some(agent::color_hex(c)?),
            None if agent => {
                let mut taken: Vec<String> = self.conn.snapshot().map(|s| s.nodes.iter().filter(|n| n.pane.as_ref().is_some_and(|p| p.exit.is_none())).filter_map(|n| n.var("color").map(str::to_string)).collect()).unwrap_or_default();
                taken.extend(self.colored.iter().cloned());
                Some(agent::auto_color(spawn_index(it.title.as_deref().or(it.name.as_deref())), &taken).to_string())
            }
            None => None,
        })
    }
}

/// The task an agent starts with, written to `STATE/launch/<pid>-<name>.task` for its launch command to read.
fn task_file(state: &Path, it: &Item, task: &str) -> Result<PathBuf> {
    let dir = crate::store::dir(state, &["launch"])?;
    let f = dir.join(format!("{}-{}.task", std::process::id(), crate::store::sanitize(it.name.as_deref().unwrap_or("agent"))));
    crate::store::atomic_write(&f, task.as_bytes())?;
    Ok(f)
}

/// A worktree's shared git dir (`git rev-parse --git-common-dir`, absolute), where its index and refs live.
fn git_common_dir(worktree: &str) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(worktree).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Which spawn color a name gets: its trailing number (w3, review-2), else a hash, so siblings differ.
fn spawn_index(name: Option<&str>) -> usize {
    let Some(n) = name else { return 0 };
    let digits: String = n.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    digits.parse().unwrap_or_else(|_| n.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize)))
}

/// One item of a multiplicity: what each new pane gets.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Item {
    pub name: Option<String>,
    pub cwd: String,
    pub task: Option<String>,
    pub cmd: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub worktree: Option<String>,
    /// The pane's and the session's name when it isn't `name` (an agent name is a slug; a resumed Claude Code
    /// sub-agent keeps its own words).
    #[serde(default)]
    pub title: Option<String>,
}

impl Item {
    pub fn kind(&self, agent: bool) -> &'static str {
        let prog = self.cmd.as_deref().and_then(|c| c.split_whitespace().next()).map(|w| w.rsplit('/').next().unwrap_or(w));
        match (agent, prog) {
            (true, None | Some("claude")) => "claude",
            (_, Some("omp")) => "omp",
            (_, Some("codex")) => "codex",
            (_, Some(_)) => "cmd",
            (false, None) => "shell",
        }
    }
}

/// A `--cwd` as the new pane gets it: `~` expanded, a relative path taken from where the command runs (not from
/// whichever session iTerm2 last had active), and refused unless the folder exists, so a command never starts
/// in the wrong project.
fn folder(base: &Path, d: &str) -> Result<String> {
    let p = match d.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => crate::conn::home().join(rest.trim_start_matches('/')),
        _ => PathBuf::from(d),
    };
    let p = if p.is_absolute() { p } else { base.join(p) };
    if !p.is_dir() {
        return Err(Error::invalid(format!("--cwd {d}: no such folder ({})", p.display())));
    }
    Ok(p.display().to_string())
}

/// `items` grouped by `key`, the groups in the order their first item comes, each in the items' order.
fn group_by(items: &[usize], mut key: impl FnMut(usize) -> String) -> Vec<Vec<usize>> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for &i in items {
        let k = key(i);
        match groups.iter_mut().find(|g| g.0 == k) {
            Some(g) => g.1.push(i),
            None => groups.push((k, vec![i])),
        }
    }
    groups.into_iter().map(|g| g.1).collect()
}

/// `--each FILE`: a JSON array of objects, or CSV with a header row.
fn each_rows(path: &Path) -> Result<Vec<HashMap<String, String>>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
    if text.trim_start().starts_with('[') {
        let v: Vec<serde_json::Map<String, Value>> = serde_json::from_str(&text).map_err(|e| Error::invalid(format!("--each: {e}")))?;
        return Ok(v
            .into_iter()
            .map(|o| o.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect())
            .collect());
    }
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let head: Vec<String> = lines.next().unwrap_or("").split(',').map(|x| x.trim().to_string()).collect();
    Ok(lines.map(|l| head.iter().cloned().zip(l.split(',').map(|x| x.trim().to_string())).collect()).collect())
}

pub(crate) fn git_top(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// `--key 'ctrl-c esc …'`: the keys, in order.
fn parse_keys(spec: &str) -> Result<Vec<Key>> {
    spec.split_whitespace()
        .map(|k| Key::parse(k).ok_or_else(|| Error::invalid(format!("--key {k}: enter, esc, tab, ctrl-c, up, f5, a, …"))))
        .collect()
}

/// Every key is Esc or Ctrl-C: a cancel, which may go to any pane.
fn is_cancel(keys: &[Key]) -> bool {
    use crate::app::wire::KeyCode;
    keys.iter().all(|k| k.code == KeyCode::Esc || (k.mods.ctrl && k.code == KeyCode::Char('c')))
}

/// `content scroll --lines N | --to top|bottom`.
fn scroll_to(r: &Request) -> Result<ScrollTo> {
    Ok(match (r.p.one("--lines"), r.p.one("--to")) {
        (Some(n), _) => ScrollTo::Lines(n.parse().map_err(|_| Error::invalid("--lines N"))?),
        (None, Some("top")) => ScrollTo::Top,
        (None, Some("bottom")) => ScrollTo::Bottom,
        _ => return Err(Error::invalid("content scroll --lines N | --to top|bottom")),
    })
}

/// Which panes' turns are over, from their agents' state changes: a turn is over once the agent has been busy
/// and is idle (or done, failed, exited) again, or its pane has ended.
#[derive(Default)]
struct Turns {
    worked: HashSet<Id>,
    done: HashSet<Id>,
}

impl Turns {
    /// A pushed state change.
    fn state(&mut self, id: Id, state: Option<&str>) {
        match state {
            Some("busy") => {
                self.worked.insert(id);
            }
            Some("idle" | "done" | "failed" | "exited") if self.worked.contains(&id) => {
                self.done.insert(id);
            }
            _ => {}
        }
    }

    /// A state read after a gap in the events: idle again counts as through (its busy may be among the dropped).
    fn reread(&mut self, id: Id, ended: bool, state: Option<&str>) {
        match state {
            _ if ended => self.end(id),
            Some("busy") => {
                self.worked.insert(id);
            }
            Some("idle" | "done" | "failed" | "exited") => self.end(id),
            _ => {}
        }
    }

    /// The pane exited or closed.
    fn end(&mut self, id: Id) {
        self.done.insert(id);
    }

    /// How many turns are over.
    fn over(&self) -> usize {
        self.done.len()
    }
}

/// `set --name N --color C --set k=v…`: the fields a `set` writes on each node, named for its rows.
fn set_fields(r: &Request) -> Result<Vec<(String, Field)>> {
    let mut fields: Vec<(String, Field)> = Vec::new();
    if let Some(nm) = r.p.one("--name").or(r.p.one("--title")) {
        if nm.chars().any(|c| (c as u32) < 32) || nm.chars().count() > 200 {
            return Err(Error::invalid("names are under 200 characters, with no control characters"));
        }
        fields.push(("name".into(), Field::Name(nm.to_string())));
    }
    if let Some(c) = r.p.one("--color") {
        fields.push(("color".into(), Field::Color(if c == "none" { None } else { Some(agent::color_hex(c)?) })));
    }
    for (k, val) in &r.payload {
        fields.push((k.clone(), Field::Var(k.clone(), (!val.is_empty()).then(|| val.clone()))));
    }
    if r.p.has("--bury") {
        fields.push(("buried".into(), Field::Buried(true)));
    }
    if r.p.has("--unbury") {
        // a buried pane leaves the snapshot, so it has no ID to name here yet
        return Err(Error::policy("unsupported", "pane set --unbury: a buried pane is not in the snapshot, so it can't be named yet; iTerm2 lists it under Session > Buried Sessions"));
    }
    Ok(fields)
}

/// The agent in a pane, as `set` sees it: its state, its provider while it runs (else empty), and whether its
/// input box is drawn (keys sent while it starts wait in the tty and land on its first screen, a folder-trust
/// prompt included: the hard rail is never to answer one).
struct Session<'s> {
    state: &'s str,
    provider: &'s str,
    typable: bool,
}

impl<'s> Session<'s> {
    fn of(n: &'s crate::app::Node) -> Session<'s> {
        let state = n.var(view::V_STATE).unwrap_or("");
        let live = n.kind == crate::app::Kind::Pane && n.pane.as_ref().is_some_and(|p| p.exit.is_none()) && !matches!(state, "exited" | "done" | "failed");
        let provider = if live { n.var(view::V_PROVIDER).unwrap_or("") } else { "" };
        Session { state, provider, typable: matches!(state, "ready" | "idle" | "busy") }
    }
}

/// What `pane read` reads: `--tail N`, else `--range A:B`, else the screen.
fn lines_of(r: &Request) -> Result<Lines> {
    let range = || Error::invalid("--range A:B");
    Ok(match (r.p.one("--tail"), r.p.one("--range")) {
        (Some(t), _) => Lines::Tail(t.parse().map_err(|_| Error::invalid("--tail N"))?),
        (None, Some(x)) => {
            let (a, b) = x.split_once(':').ok_or_else(range)?;
            Lines::Range(a.trim().parse().map_err(|_| range())?, b.trim().parse().map_err(|_| range())?)
        }
        _ => Lines::Screen,
    })
}

/// A read's lines as shown: blank lines at the bottom dropped, then (`--tail N`) only the last N.
fn trim_read(lines: &mut Vec<String>, asked: Lines) {
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if let Lines::Tail(n) = asked {
        let cut = lines.len().saturating_sub(n as usize);
        lines.drain(..cut);
    }
}

/// `pane get`: the node's row, plus its lend, its task, and a Claude agent's usage (from Claude's own session
/// records, read now, never polled).
fn pane_row(s: &Snapshot, v: &View, i: usize) -> Obj {
    let mut row = v.row(s, i);
    let n = &s.nodes[i];
    if let Some(l) = n.var(view::V_LEND) {
        row.insert("lend".into(), serde_json::from_str(l).unwrap_or(json!(l)));
    }
    if let Some(t) = n.var(view::V_TASK) {
        row.insert("task".into(), json!(t));
    }
    if crate::agents::provider(n) == Some("claude") {
        if let Some(u) = n.pane.as_ref().and_then(|p| crate::claude::usage(p.pid)) {
            row.insert("tokens".into(), json!(u.tokens));
            row.insert("ctx".into(), json!(format!("{}%", u.percent())));
            if let Some(m) = u.model {
                row.insert("model".into(), json!(m));
            }
        }
    }
    row
}

fn layout_json(l: &Layout) -> Value {
    match l {
        Layout::Pane(id) => json!(view::pane_handle(*id)),
        Layout::Split { vertical, children } => json!({
            "split": if *vertical { "vertical" } else { "horizontal" },
            "fractions": children.iter().map(|(f, _)| (*f as f64 * 10000.0).round() / 10000.0).collect::<Vec<_>>(),
            "children": children.iter().map(|(_, c)| layout_json(c)).collect::<Vec<_>>(),
        }),
    }
}

/// The area windows tile: the bounding box of the selected windows' frames (1440×900 when none has one).
///
/// Not yet the display's: the snapshot has no screens, so `--screen` is ignored. When the app reports screens,
/// tile the visible frame of the screen the first window is on.
fn screen_area(s: &Snapshot, targets: &[usize]) -> geometry::Frame {
    let rects: Vec<Rect> = targets.iter().filter_map(|i| s.nodes[*i].rect).collect();
    if rects.is_empty() {
        return geometry::Frame { x: 0.0, y: 0.0, width: 1440.0, height: 900.0 };
    }
    let x0 = rects.iter().map(|r| r.x).fold(f32::MAX, f32::min);
    let y0 = rects.iter().map(|r| r.y).fold(f32::MAX, f32::min);
    let x1 = rects.iter().map(|r| r.x + r.w).fold(f32::MIN, f32::max);
    let y1 = rects.iter().map(|r| r.y + r.h).fold(f32::MIN, f32::max);
    geometry::Frame { x: x0 as f64, y: y0 as f64, width: (x1 - x0) as f64, height: (y1 - y0) as f64 }
}

/// A delivery reply (`delivered bytes=N [wrapped=N]` or `queued reason=prompt`) as a row.
pub fn delivered(id: impl Into<String>, line: &str) -> Obj {
    let mut row = crate::obj! {"id" => id.into()};
    let mut words = line.split_whitespace();
    let status = words.next().unwrap_or("delivered");
    for w in words {
        if let Some((k, v)) = w.split_once('=') {
            row.insert(k.into(), v.parse::<u64>().map_or_else(|_| json!(v), |n| json!(n)));
        }
    }
    ok(status, row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(noun: Noun, words: &[&str]) -> Request {
        grammar::parse(noun, &words.iter().map(|w| w.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_read_drops_blank_bottom_rows_then_keeps_the_tail() {
        let mut l: Vec<String> = ["a", "b", "c", "  ", ""].map(String::from).to_vec();
        trim_read(&mut l, Lines::Tail(2));
        assert_eq!(l, ["b", "c"]);
        let mut l: Vec<String> = ["a", ""].map(String::from).to_vec();
        trim_read(&mut l, Lines::Screen);
        assert_eq!(l, ["a"]);
        assert!(matches!(lines_of(&req(Noun::Pane, &["read", "--range", "3:9"])).unwrap(), Lines::Range(3, 9)));
        assert!(matches!(lines_of(&req(Noun::Pane, &["read", "--tail", "5"])).unwrap(), Lines::Tail(5)));
        assert!(matches!(lines_of(&req(Noun::Pane, &["read"])).unwrap(), Lines::Screen));
        assert!(lines_of(&req(Noun::Pane, &["read", "--range", "3"])).is_err());
    }

    #[test]
    fn a_turn_is_over_after_busy_then_idle_or_an_end() {
        let (a, b, c) = (Id(1), Id(2), Id(3));
        let mut t = Turns::default();
        t.state(a, Some("idle")); // idle before working: the turn hasn't started
        assert_eq!(t.over(), 0);
        t.state(a, Some("busy"));
        t.state(a, Some("idle"));
        t.end(b);
        assert_eq!(t.over(), 2);
        // after a gap, idle counts as through even without the busy
        t.reread(c, false, Some("idle"));
        assert_eq!(t.over(), 3);
        let mut t = Turns::default();
        t.reread(a, false, Some("busy"));
        t.reread(b, true, Some("busy"));
        t.reread(c, false, None);
        assert_eq!(t.over(), 1);
        t.state(a, Some("done"));
        assert_eq!(t.over(), 2);
    }

    #[test]
    fn keys_parse_in_order_and_only_esc_or_ctrl_c_is_a_cancel() {
        assert_eq!(parse_keys("esc ctrl-c").unwrap().len(), 2);
        assert!(is_cancel(&parse_keys("esc ctrl-c").unwrap()));
        assert!(!is_cancel(&parse_keys("esc enter").unwrap()));
        assert!(parse_keys("nosuchkey").is_err());
    }

    #[test]
    fn groups_keep_first_seen_order() {
        let key = |i: usize| ["x", "y", "x", "z", "y"][i].to_string();
        assert_eq!(group_by(&[0, 1, 2, 3, 4], key), vec![vec![0, 2], vec![1, 4], vec![3]]);
        assert!(group_by(&[], key).is_empty());
    }
}
