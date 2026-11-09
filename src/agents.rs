//! Agent primitives (ported from iterm_api's `ops/queue.rs`, `mailbox.rs`, `worktree.rs`,
//! `lend.rs`): the children ledger, `--max N` queues, `children [--wait]`, `agent run`,
//! `agent msg|inbox`, the task board verbs, `agent checkpoint|merge|pause|resume|wt-remove`,
//! `agent adopt` and `pane lend`.
//!
//! The live tree is on the panes (`view.rs`); the ledger (`<state>/children/<parent>.json`) is what
//! outlives them: each child a parent launched, with what it was launched as, so a closed child's
//! exit, worktree and relaunch recipe are still known.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::app::model::parse_handle;
use crate::app::wire::Field;
use crate::app::{Event, Id, Op, Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::board::{self, Claim, Status, Team};
use crate::exec::{Exec, Item};
use crate::grammar::{self, Kind, Request, Verb};
use crate::mail::{self, Message, Since};
use crate::out::{from_error, ok, planned, Error, Obj, Result};
use crate::store::{self, Lock};
use crate::view::{self, View};

/// How long `agent msg` waits for its one-line notice to be typed in before reporting it pending.
const NOTICE_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rec {
    pub pane: u64,
    pub name: Option<String>,
    pub kind: String,
    pub cwd: String,
    #[serde(default)]
    pub cmd: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    /// The app instance (its pid) the pane belonged to: pane IDs start over with every launch, so a record
    /// from another instance names some other node.
    #[serde(default)]
    pub app: i32,
    pub at: i64,
}

/// One queued launch: the request's words (re-parsed when it starts) and its item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queued {
    pub noun: String,
    pub words: Vec<String>,
    pub item: Item,
    pub agent: bool,
    pub at: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    pub max: usize,
    pub items: Vec<Queued>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paused {
    pub name: String,
    pub rec: Rec,
    pub repo: String,
    pub branch: String,
    pub onto: String,
    pub commit: String,
    pub at: i64,
}

/// A worktree child, resolved: where it works, the checkout it lands in (`parent_top`, on `onto`), its branch.
#[derive(Debug, Clone)]
pub(crate) struct WtTarget {
    /// Its agent name (the live pane's, else the ledger's).
    pub name: String,
    pub live: Option<usize>,
    pub rec: Option<Rec>,
    pub is_parent: bool,
    pub worktree: PathBuf,
    pub exists: bool,
    pub parent_top: PathBuf,
    pub onto: String,
    pub branch: String,
}

/// The checkout a worktree was made in: `<top>` of `<top>/.worktrees/<name>`.
pub(crate) fn made_in(worktree: &Path) -> Option<&Path> {
    worktree.parent().filter(|d| d.file_name().is_some_and(|n| n == ".worktrees")).and_then(Path::parent)
}

/// Supervised kinds: they report exits; shells never end on their own.
fn supervised(kind: &str) -> bool {
    matches!(kind, "claude" | "codex" | "omp" | "cmd")
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| Error::failed(format!("git: {e}")))
}

pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(Error::failed(format!("git {}: {}", args.first().copied().unwrap_or(""), if err.is_empty() { "failed" } else { &err })));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn git_yes(dir: &Path, args: &[&str]) -> Result<bool> {
    Ok(git(dir, args)?.status.success())
}

pub(crate) fn branch_of(dir: &Path) -> Option<String> {
    git_ok(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok().filter(|b| !b.is_empty())
}

pub(crate) fn dirty(dir: &Path, untracked: bool) -> Result<bool> {
    let mode = if untracked { "--untracked-files=all" } else { "--untracked-files=no" };
    Ok(!git_ok(dir, &["status", "--porcelain", mode])?.is_empty())
}

pub(crate) fn count(dir: &Path, range: &str) -> Result<i64> {
    Ok(git_ok(dir, &["rev-list", "--count", range])?.parse().unwrap_or(0))
}

pub(crate) fn conflicts(dir: &Path) -> String {
    git_ok(dir, &["diff", "--name-only", "--diff-filter=U"]).map(|s| s.lines().collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

/// `--worktree [BASE]`: `<repo>/.worktrees/<name>` on branch `wt/<name>` from BASE (default HEAD),
/// reused when it exists; `.worktrees/` goes in the repository's local exclude file.
pub fn worktree_create(repo: &Path, name: &str, base: Option<&str>) -> Result<PathBuf> {
    if !grammar::is_slug(name) {
        return Err(Error::invalid(format!("worktree name {name:?} must be letters, digits, _ and -")));
    }
    if let Some(b) = base.filter(|b| b.starts_with('-')) {
        return Err(Error::invalid(format!("--worktree={b}: a base is a branch or commit, not an option")));
    }
    let path = repo.join(".worktrees").join(name);
    let branch = format!("wt/{name}");
    if path.join(".git").exists() {
        return Ok(path);
    }
    let exclude = PathBuf::from(git_ok(repo, &["rev-parse", "--git-path", "info/exclude"])?);
    let exclude = if exclude.is_absolute() { exclude } else { repo.join(exclude) };
    let text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !text.lines().any(|l| l.trim() == ".worktrees/" || l.trim() == ".worktrees") {
        if let Some(d) = exclude.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&exclude, format!("{text}{sep}.worktrees/\n")).map_err(|e| Error::failed(format!("{}: {e}", exclude.display())))?;
    }
    let p = path.to_string_lossy().into_owned();
    if git_yes(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])? {
        git_ok(repo, &["worktree", "add", "-q", &p, &branch])?;
    } else {
        git_ok(repo, &["worktree", "add", "-q", "-b", &branch, &p, base.unwrap_or("HEAD")])?;
    }
    Ok(path)
}

impl Exec<'_> {
    pub(crate) fn parent_key(&self) -> String {
        self.conn.env().me.map(view::pane_handle).unwrap_or_else(|| "outside".into())
    }

    fn ledger_path(&self, parent: &str) -> Result<PathBuf> {
        Ok(store::dir(&self.conn.env().state, &["children"])?.join(store::sanitize(parent) + ".json"))
    }

    pub fn ledger(&self, parent: &str) -> Vec<Rec> {
        self.ledger_path(parent).ok().and_then(|p| store::read_json(&p)).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    /// The next free index for default names (`w1`, `w2`, … per parent, per app instance: pane IDs restart
    /// with the app, so an earlier run's children don't count).
    pub fn next_index(&mut self) -> usize {
        let app = self.conn.snapshot().map(|s| s.app_pid).unwrap_or(0);
        self.ledger(&self.parent_key()).iter().filter(|r| r.app == app).count() + 1
    }

    /// Where state keyed by pane ID lives (mailboxes, task boards, queues): one folder per app instance, since
    /// pane IDs restart with the app and a new p11 must not read the last one's mail.
    // ponytail: old instances' folders are never pruned (a few KiB each); prune by app pid liveness if they pile up.
    pub fn instance_root(&mut self) -> Result<PathBuf> {
        let app = self.conn.snapshot()?.app_pid;
        store::dir(&self.conn.env().state, &["apps", &app.to_string()])
    }

    pub fn ledger_add(&mut self, pane: Id, it: &Item, agent: bool) {
        let key = self.parent_key();
        let Ok(path) = self.ledger_path(&key) else { return };
        let Ok(_held) = Lock::acquire(&path.with_extension("lock")) else { return };
        let mut recs = self.ledger(&key);
        recs.push(Rec {
            pane: pane.0,
            name: it.name.clone(),
            kind: it.kind(agent).into(),
            cwd: it.cwd.clone(),
            cmd: it.cmd.clone(),
            model: it.model.clone(),
            effort: it.effort.clone(),
            task: it.task.clone(),
            worktree: it.worktree.clone(),
            app: self.conn.snapshot().map(|s| s.app_pid).unwrap_or(0),
            at: store::now(),
        });
        if let Ok(text) = serde_json::to_string(&recs) {
            let _ = store::atomic_write(&path, text.as_bytes());
        }
    }

    /// A renamed child keeps its new name in its parent's ledger, so `agent merge|wt-remove NEW` finds it after
    /// it closed. `parent` is its `agent.parent` (`-`: spawned from outside iTerm2).
    pub fn ledger_rename(&mut self, parent: &str, pane: Id, name: &str) {
        let key = if parent == view::OUTSIDE { "outside" } else { parent };
        let Ok(path) = self.ledger_path(key) else { return };
        let Ok(_held) = Lock::acquire(&path.with_extension("lock")) else { return };
        let app = self.conn.snapshot().map(|s| s.app_pid).unwrap_or(0);
        let mut recs = self.ledger(key);
        let Some(r) = recs.iter_mut().rev().find(|r| r.pane == pane.0 && (r.app == 0 || r.app == app)) else { return };
        r.name = Some(name.to_string());
        if let Ok(text) = serde_json::to_string(&recs) {
            let _ = store::atomic_write(&path, text.as_bytes());
        }
    }

    /// The pane is lent to the caller (or to any agent), and the lend has not expired.
    pub fn lent_to_me(&self, s: &Snapshot, v: &View, pane: usize) -> bool {
        let Some(l) = s.nodes[pane].var(view::V_LEND).and_then(|l| serde_json::from_str::<Value>(l).ok()) else { return false };
        if l.get("until").and_then(Value::as_i64).is_none_or(|u| u <= store::now()) {
            return false;
        }
        match l.get("to").and_then(Value::as_str) {
            None => true,
            Some(to) => v.m.me.map(|m| v.handle(m) == to).unwrap_or(false),
        }
    }

    // ---- queues and children ------------------------------------------------------------------

    fn queue_path(&mut self) -> Result<PathBuf> {
        let root = self.instance_root()?;
        Ok(store::dir(&root, &["queues"])?.join(store::sanitize(&self.parent_key()) + ".json"))
    }

    fn read_queue(&mut self) -> Queue {
        self.queue_path().ok().and_then(|p| store::read_json(&p)).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    fn write_queue(&mut self, q: &Queue) -> Result<()> {
        let text = serde_json::to_string(q).map_err(|e| Error::failed(e.to_string()))?;
        store::atomic_write(&self.queue_path()?, text.as_bytes())
    }

    /// My live supervised children (panes in the snapshot, not exited): with `agents`, only those spawned as
    /// agents (a companion like `st -l` beside me is not one of the `--max N` running).
    fn live_children(&self, s: &Snapshot, agents: bool) -> usize {
        let me = self.parent_key();
        s.nodes
            .iter()
            .filter(|n| n.var(view::V_PARENT) == Some(if me == "outside" { view::OUTSIDE } else { me.as_str() }))
            .filter(|n| n.pane.as_ref().is_some_and(|p| p.exit.is_none()) && supervised(n.var(view::V_KIND).unwrap_or("shell")))
            .filter(|n| !agents || n.var(view::V_CMD).is_some() || view::is_agent_kind(n.var(view::V_KIND)))
            .count()
    }

    /// `--max N`: what fits beside the caller's live children now; the rest queued, in order, to
    /// start as children end (whenever the caller runs `children`, `children --wait` or another
    /// capped launch).
    pub fn queued_new(&mut self, r: &Request, s: &Snapshot, v: &View, items: Vec<Item>, agent: bool, at: Option<Id>, max: usize) -> Result<Vec<Obj>> {
        if max == 0 {
            return Err(Error::invalid("--max N needs N >= 1"));
        }
        let slots = max.saturating_sub(self.live_children(s, agent));
        let (now, later) = items.split_at(slots.min(items.len()));
        let mut rows = if now.is_empty() { vec![] } else { self.place_items(r, s, v, now, agent, at)? };
        if !later.is_empty() {
            let path = self.queue_path()?;
            let _held = Lock::acquire(&path.with_extension("lock"))?;
            let mut q = self.read_queue();
            q.max = max;
            let mut argv = vec![r.verb.name().to_string()];
            argv.extend(r.rest.iter().cloned());
            for it in later {
                q.items.push(Queued { noun: r.noun.name().into(), words: argv.clone(), item: it.clone(), agent, at: at.map(|a| a.0) });
                rows.push(ok("queued", crate::obj! {"name" => it.name, "position" => q.items.len()}));
            }
            self.write_queue(&q)?;
        }
        Ok(rows)
    }

    /// Start queued launches while there is room; the rows of what started.
    pub fn drain_queue(&mut self) -> Vec<Obj> {
        let Ok(path) = self.queue_path() else { return vec![] };
        let Ok(_held) = Lock::acquire(&path.with_extension("lock")) else { return vec![] };
        let mut q = self.read_queue();
        let mut rows = Vec::new();
        while !q.items.is_empty() {
            self.conn.invalidate();
            let Ok((s, v)) = self.view() else { break };
            if q.items.first().is_some_and(|j| self.live_children(&s, j.agent) >= q.max) {
                break;
            }
            let job = q.items.remove(0);
            let Some(noun) = grammar::Noun::parse(&job.noun) else { continue };
            let parsed = grammar::parse(noun, &job.words);
            let row = match parsed {
                Ok(r) => {
                    // the stored pane, else beside the caller
                    let at = job.at.map(Id).filter(|a| v.m.by_raw(&a.0.to_string()).is_some()).or(self.conn.env().me);
                    match self.place_items(&r, &s, &v, std::slice::from_ref(&job.item), job.agent, at) {
                        Ok(mut r) => r.pop().unwrap_or_default(),
                        Err(e) => from_error(&e, crate::obj! {"name" => job.item.name}),
                    }
                }
                Err(e) => from_error(&Error::invalid(e.to_string()), crate::obj! {"name" => job.item.name}),
            };
            rows.push(row);
        }
        let _ = self.write_queue(&q);
        rows
    }

    /// One row per child of mine: live (with its state), exited (with its status) or closed;
    /// queued launches last.
    pub fn children_rows(&mut self, s: &Snapshot, only: Option<&[u64]>) -> Result<Vec<Obj>> {
        // this app instance's records (an older one's pane IDs name other nodes now)
        let recs: Vec<Rec> = self
            .ledger(&self.parent_key())
            .into_iter()
            .filter(|r| r.app == 0 || r.app == s.app_pid)
            .filter(|r| only.is_none_or(|o| o.contains(&r.pane)))
            .collect();
        let mut v = View::new(s, self.conn.env().me, &self.binds);
        let live: Vec<usize> = recs.iter().filter_map(|r| v.m.by_raw(&r.pane.to_string())).collect();
        self.observe(s, &mut v, &live);
        let gone: Vec<Id> = recs.iter().filter(|r| v.m.by_raw(&r.pane.to_string()).is_none()).map(|r| Id(r.pane)).collect();
        let exits: std::collections::HashMap<u64, Option<i32>> = if gone.is_empty() { Default::default() } else { self.conn.exits(gone).unwrap_or_default().into_iter().map(|(id, x)| (id.0, x)).collect() };
        let mut rows = Vec::new();
        for r in &recs {
            // a live child goes by its name now (a rename moves agent.name too)
            let name = v.m.by_raw(&r.pane.to_string()).and_then(|i| s.nodes[i].var(view::V_NAME)).map(str::to_string).or(r.name.clone());
            let mut row = crate::obj! {"name" => name, "pane" => view::pane_handle(Id(r.pane)), "kind" => r.kind};
            let code = match v.m.by_raw(&r.pane.to_string()) {
                Some(i) => {
                    let st = v.m.nodes[i].fields.get("state").cloned().unwrap_or_default();
                    if let Some(x) = s.nodes[i].pane.as_ref().and_then(|p| p.exit) {
                        row.insert("exit".into(), json!(x));
                        "exited"
                    } else {
                        row.insert("state".into(), json!(st));
                        "running"
                    }
                }
                None => match exits.get(&r.pane).copied().flatten() {
                    Some(x) => {
                        row.insert("exit".into(), json!(x));
                        "exited"
                    }
                    None => "closed",
                },
            };
            if let Some(w) = &r.worktree {
                row.insert("worktree".into(), json!(w));
            }
            rows.push(ok(code, row));
        }
        if only.is_none() {
            for (k, q) in self.read_queue().items.iter().enumerate() {
                rows.push(ok("queued", crate::obj! {"name" => q.item.name, "position" => k + 1}));
            }
        }
        Ok(rows)
    }

    /// `children [--wait] [--timeout S]`: the caller's children; with --wait, block (on pushed
    /// events) until every supervised child is through: an agent's turn finished (idle, or it ended; it stays
    /// open for review and merge), a command ended; and the queue is empty, starting queued ones as room frees up.
    pub fn children(&mut self, wait: bool, timeout: Option<Duration>, only: Option<Vec<u64>>, until_idle: bool) -> Result<Vec<Obj>> {
        let mut rows = self.children_until(wait, timeout, only, until_idle)?;
        // once, on the rows that are answered: a worktree child's branch, its commits not landed, whether it landed
        self.branch_fields(&mut rows);
        Ok(rows)
    }

    fn children_until(&mut self, wait: bool, timeout: Option<Duration>, only: Option<Vec<u64>>, until_idle: bool) -> Result<Vec<Obj>> {
        let mut started = self.drain_queue();
        if !wait {
            let s = self.conn.snapshot()?.clone();
            let mut rows = started;
            rows.extend(self.children_rows(&s, only.as_deref())?);
            return Ok(rows);
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        let (mut w, _) = self.conn.watch(None, timeout)?;
        loop {
            // events moved the model on: never re-check against the cached snapshot
            self.conn.invalidate();
            let s = self.conn.snapshot()?.clone();
            let rows = self.children_rows(&s, only.as_deref())?;
            let pending = rows.iter().any(|r| {
                let code = r.get("code").and_then(Value::as_str);
                let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
                let st = r.get("state").and_then(Value::as_str);
                // its turn finished: the app's idle (or it ended); an untracked pane's old screen reading
                let waiting = matches!(st, Some("idle" | "done" | "failed" | "exited")) || st == Some(crate::agent::READY);
                let idle_ends = until_idle || view::is_agent_kind(Some(kind));
                (code == Some("running") && supervised(kind) && !(idle_ends && waiting)) || code == Some("queued")
            });
            let mine: Vec<Id> = rows.iter().filter_map(|r| r.get("pane").and_then(Value::as_str)).filter_map(parse_handle).map(|(_, id)| id).collect();
            if !pending {
                started.extend(rows);
                return Ok(started);
            }
            // block until something about the panes changes
            loop {
                let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                if left.is_some_and(|l| l.is_zero()) {
                    let mut rows = started;
                    rows.extend(self.children_rows(&s, only.as_deref())?);
                    rows.push(from_error(&Error::Timeout("children still running".into()), crate::obj! {}));
                    return Ok(rows);
                }
                w.set_deadline(left)?;
                match w.next()? {
                    Some(p) if matches!(p.ev, Event::Exited { .. } | Event::Closed { .. } | Event::Gap { .. }) => break,
                    // an agent's state arrives as its var (published just after the quiet that caused it)
                    Some(p) if matches!(p.ev, Event::Quiet { .. } | Event::Var { .. }) && p.ev.subject().is_some_and(|x| mine.contains(&x)) => break,
                    Some(_) | None => continue,
                }
            }
            started.extend(self.drain_queue());
        }
    }

    // ---- agent verbs --------------------------------------------------------------------------

    /// An agent by name or pane: a live pane first (my children's names win), else a past child
    /// in my ledger.
    pub(crate) fn find_agent(&mut self, who: &str) -> Result<(Option<usize>, Option<Rec>, Snapshot, View)> {
        let (s, v) = self.view()?;
        let recs = self.ledger(&self.parent_key());
        // a name first: default names (w1, w2, …) read as window handles too; a handle counts only as a pane
        let named: Vec<usize> = (0..s.nodes.len()).filter(|i| s.nodes[*i].var(view::V_NAME) == Some(who)).collect();
        let past = recs.iter().rev().find(|r| r.name.as_deref() == Some(who)).cloned();
        let by_pane = parse_handle(who).filter(|(k, _)| matches!(k, None | Some(crate::app::Kind::Pane))).map(|(_, id)| id);
        let by_pane = by_pane.filter(|_| named.is_empty() && past.is_none());
        let live = match by_pane.and_then(|id| v.m.by_raw(&id.0.to_string())).filter(|i| s.nodes[*i].kind == crate::app::Kind::Pane) {
            Some(i) => Some(i),
            None => named.iter().copied().find(|i| v.mine(&s, *i, true)).or(named.first().copied()),
        };
        let rec = match live {
            Some(i) => recs.iter().rev().find(|r| r.pane == v.id(i).0).cloned(),
            None => past.or_else(|| recs.iter().rev().find(|r| by_pane.is_some_and(|p| p.0 == r.pane)).cloned()),
        };
        if live.is_none() && rec.is_none() {
            return Err(Error::invalid(format!("no agent {who}: not a live pane, nor a past child of yours")));
        }
        Ok((live, rec, s, v))
    }

    pub fn agent_verb(&mut self, r: &Request) -> Result<Vec<Obj>> {
        match r.verb {
            Verb::Run => self.agent_run(r),
            Verb::Msg => self.agent_msg(r),
            Verb::Inbox => self.inbox(r),
            Verb::Adopt => {
                let who = r.words.first().ok_or_else(|| Error::invalid("agent adopt X [--to me|human]"))?;
                let (live, _, s, v) = self.find_agent(who)?;
                let i = live.ok_or_else(|| Error::invalid(format!("{who} is not running")))?;
                // only a head (no live parent) is adopted
                if v.m.agent_parent.contains_key(&i) {
                    return Err(Error::policy("has-parent", format!("{who} has a live parent; only heads are adopted")));
                }
                let to = match r.p.one("--to") {
                    Some("human") => view::OUTSIDE.to_string(),
                    _ => self.parent_key(),
                };
                let to = if to == "outside" { view::OUTSIDE.to_string() } else { to };
                let _ = s;
                self.conn.call(Op::Set { id: v.id(i), field: Field::Var(view::V_PARENT.into(), Some(to.clone())) })?;
                Ok(vec![ok("adopted", crate::obj! {"agent" => v.handle(i), "parent" => to})])
            }
            Verb::Checkpoint | Verb::Merge | Verb::Pause | Verb::Resume | Verb::WtRemove => self.worktree_verb(r),
            Verb::Land => self.land(r),
            Verb::Unland => self.unland(r),
            Verb::Landed => self.landed(),
            v => Err(Error::invalid(format!("agent {v} is not an agent verb"))),
        }
    }

    /// `agent run TASK… [--worktree] [--model M --effort E] [--max N]`: one agent per task, then
    /// wait until each is idle again (or has exited); one line per child.
    fn agent_run(&mut self, r: &Request) -> Result<Vec<Obj>> {
        // the same request as `agent new --task T…`, one agent per task
        let mut spawn = r.clone();
        spawn.verb = Verb::New;
        spawn.multi.tasks = r.words.iter().chain(r.multi.tasks.iter()).cloned().collect();
        spawn.words.clear();
        spawn.sel = None;
        let rows = self.run(&spawn)?;
        let made: Vec<u64> = rows.iter().filter_map(|x| x.get("pane").or(x.get("id")).and_then(Value::as_str)).filter_map(parse_handle).map(|(_, id)| id.0).collect();
        if made.is_empty() {
            return Ok(rows);
        }
        let timeout = crate::watch::timeout_of(r)?;
        // a Claude agent stays open when its task is done: done means idle again
        self.children(true, timeout, Some(made), true)
    }

    fn agent_msg(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let text = match (r.p.one("--text"), r.p.one("--file")) {
            (Some(t), _) => t.to_string(),
            (None, Some(f)) => std::fs::read_to_string(self.conn.env().cwd.join(f)).map_err(|e| Error::invalid(format!("{f}: {e}")))?,
            _ => return Err(Error::invalid("agent msg TO… --text T | --file F")),
        };
        if text.trim().is_empty() {
            return Err(Error::invalid("a message needs text"));
        }
        if text.len() > mail::MAIL_LIMIT {
            return Err(Error::invalid(format!("a message is limited to {} KiB; put longer work in a file or a task body", mail::MAIL_LIMIT / 1024)));
        }
        if r.words.is_empty() {
            return Err(Error::invalid("name at least one recipient"));
        }
        let me_id = self.conn.env().me.ok_or_else(|| Error::invalid("agent msg needs your pane (run it inside iTerm2)"))?;
        let (s, v) = self.view()?;
        let me = v.m.by_raw(&me_id.0.to_string()).ok_or_else(|| Error::invalid("your pane is gone"))?;
        let from_name = s.nodes[me].var(view::V_NAME).map(str::to_string);
        let root = self.instance_root()?;
        let mut seen = Vec::new();
        let mut rows = Vec::new();
        for t in &r.words {
            let res = (|| -> Result<Obj> {
                let to = match t.as_str() {
                    "@parent" => *v.m.agent_parent.get(&me).ok_or_else(|| Error::invalid("you have no live parent"))?,
                    _ => {
                        // an agent's name first (the `as=` in tree; default names w1, … are window handles too),
                        // then a selector: a pane ID, prefix or pane name
                        match (0..s.nodes.len()).find(|i| s.nodes[*i].var(view::V_NAME) == Some(t.as_str())) {
                            Some(i) => i,
                            None => {
                                let e = grammar::selector(t).map_err(|e| Error::invalid(e.to_string()))?;
                                let found = crate::select::eval(&v.m, &e, Some(Kind::Pane), &Default::default()).map_err(|_| Error::invalid(format!("no agent or pane {t}")))?;
                                *found.first().ok_or_else(|| Error::invalid(format!("no agent or pane {t}")))?
                            }
                        }
                    }
                };
                if seen.contains(&to) {
                    return Err(Error::invalid("listed twice"));
                }
                seen.push(to);
                if to == me {
                    return Err(Error::policy("self", "agent msg does not go to your own pane"));
                }
                // every message is kept in the receiver's mailbox, whatever its delivery
                let msg = Message { id: 0, at: store::now(), from: v.handle(me).into(), from_name: from_name.clone(), to: v.handle(to).into(), text: text.clone() };
                let id = mail::append(&root, v.handle(to), msg)?;
                let name = s.nodes[to].var(view::V_NAME).unwrap_or(&s.nodes[to].name).to_string();
                let state = v.m.nodes[to].fields.get("state").cloned().or_else(|| s.nodes[to].var(view::V_STATE).map(str::to_string)).unwrap_or_default();
                let mut row = crate::obj! {"to" => t, "pane" => v.handle(to), "name" => name, "who" => mail::who(Some(&name), v.handle(to)), "state" => state, "id" => id};
                // Claude to Claude: Claude's own channel (SendMessage); everything else is typed in by the app
                if provider(&s.nodes[me]) == Some("claude") && provider(&s.nodes[to]) == Some("claude") {
                    row.insert("route".into(), json!("sendmessage"));
                    // the push that never depends on the sender: a one-line notice typed in (held
                    // by the app while a prompt is up); the text itself stays in the mailbox
                    let who = mail::who(from_name.as_deref(), v.handle(me));
                    let notice = format!("[iterm mail from {who}, id {id}] read it: iterm agent inbox --mark-read");
                    // bounded: the mail is kept already, so the reply never waits long on the notice. Only what the
                    // app confirmed counts: past the wait the notice is `unconfirmed` (the app may still type it, or
                    // give it up if its main thread stays stuck), never reported as typed.
                    let (typed, notice) = match self.conn.call_within(Op::Deliver { pane: v.id(to), text: notice, enter: true }, NOTICE_WAIT) {
                        Ok(d) => {
                            let line = d.lines.first().cloned().unwrap_or_else(|| "delivered".into());
                            (line.starts_with("delivered"), line)
                        }
                        Err(e) if e.unanswered() => (false, "unconfirmed".into()),
                        Err(e) => (false, format!("failed: {}", e.message())),
                    };
                    row.insert("notice_typed".into(), json!(typed));
                    row.insert("notice".into(), json!(notice));
                    // the push itself is the sender's: SendMessage to the receiver's Claude session by this name
                    // (iterm does not speak Claude Code's cross-session socket)
                    let session = s.nodes[to].pane.as_ref().and_then(|p| crate::claude::session_name_under(p.pid));
                    row.insert("sendmessage_to".into(), json!(session));
                    return Ok(ok("mailed", row));
                }
                let d = self.conn.call(Op::Deliver { pane: v.id(to), text: text.clone(), enter: true })?;
                let mut r = crate::exec::delivered(v.handle(to), d.lines.first().map(String::as_str).unwrap_or("delivered"));
                r.extend(row);
                Ok(r)
            })();
            rows.push(res.unwrap_or_else(|e| from_error(&e, crate::obj! {"to" => t})));
        }
        Ok(rows)
    }

    /// `agent inbox [--since ID|--all] [--mark-read] [--wait [S]] | --sent | --all-panes`: the wait blocks on the
    /// mailbox file (kqueue), never polls.
    fn inbox(&mut self, r: &Request) -> Result<Vec<Obj>> {
        if r.p.has("--all-panes") {
            // the human's view: every message between any panes, oldest first, whether its receiver read it
            let root = self.instance_root()?;
            let all = mail::all(&root)?;
            let unread = all.iter().filter(|(_, read)| !read).count();
            let mut rows: Vec<Obj> = all
                .iter()
                .map(|(m, read)| {
                    let mut row = m.row();
                    row.insert("to".into(), json!(m.to));
                    row.insert("read".into(), json!(read));
                    row
                })
                .collect();
            rows.push(ok("inbox", crate::obj! {"panes" => "all", "count" => all.len(), "unread" => unread}));
            return Ok(rows);
        }
        let me = self.conn.env().me.map(view::pane_handle).ok_or_else(|| Error::invalid("agent inbox needs your pane (run it inside iTerm2)"))?;
        let since = match (r.p.one("--since"), r.p.has("--all")) {
            (Some(x), _) => Since::After(x.parse().map_err(|_| Error::invalid("--since ID"))?),
            (None, true) => Since::All,
            _ => Since::Unread,
        };
        let root = self.instance_root()?;
        if r.p.has("--sent") {
            // read receipts: each message I sent, and whether its receiver has marked it read
            let names: std::collections::HashMap<String, String> = {
                let (s, v) = self.view()?;
                (0..s.nodes.len()).filter_map(|i| s.nodes[i].var(view::V_NAME).map(|n| (v.handle(i).to_string(), n.to_string()))).collect()
            };
            let sent = mail::sent(&root, &me)?;
            let unread = sent.iter().filter(|(_, r)| !r).count();
            let mut rows: Vec<Obj> = sent
                .iter()
                .map(|(m, read)| ok("sent", crate::obj! {"id" => m.id, "at" => m.at, "to" => m.to, "who" => mail::who(names.get(&m.to).map(String::as_str), &m.to), "read" => read}))
                .collect();
            rows.push(ok("inbox", crate::obj! {"pane" => me, "sent" => sent.len(), "unread" => unread}));
            return Ok(rows);
        }
        // `--wait` (no limit), `--wait=S` or `--wait S` (inbox takes no selector: its one word is the wait)
        let value = r.p.one("--wait").or(match &r.words[..] {
            [v] => Some(v.as_str()),
            _ => None,
        });
        let wait = match (r.p.given("--wait"), value) {
            (false, _) => None,
            (true, Some(v)) => Some(Some(crate::args::duration("--wait", v).map_err(Error::invalid)?)),
            (true, None) => Some(None),
        };
        let deadline = wait.flatten().map(|d| Instant::now() + d);
        let (msgs, next) = loop {
            let dir = store::dir(&root, &["mail"])?;
            let file = mail::box_path(&root, &me)?;
            let watcher = wait.is_some().then(|| crate::fswait::Watcher::new(&[dir.as_path(), file.as_path()])).transpose().map_err(|e| Error::failed(e.to_string()))?;
            let (msgs, next) = mail::read(&root, &me, since)?;
            let Some(watcher) = watcher.filter(|_| msgs.is_empty()) else { break (msgs, next) };
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left.is_some_and(|l| l.is_zero()) || !watcher.wait(left).map_err(|e| Error::failed(e.to_string()))? {
                break (msgs, next);
            }
        };
        if r.p.has("--mark-read") && !msgs.is_empty() {
            mail::mark_read(&root, &me, next)?;
        }
        let mut rows: Vec<Obj> = msgs.iter().map(Message::row).collect();
        rows.push(ok("inbox", crate::obj! {"pane" => me, "count" => msgs.len(), "next" => next, "marked" => r.p.has("--mark-read") && !msgs.is_empty()}));
        Ok(rows)
    }

    // ---- the task board -----------------------------------------------------------------------

    pub fn task(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let (s, v) = self.view()?;
        let me = v.m.me.ok_or_else(|| Error::invalid("the task board needs your pane (run it inside iTerm2)"))?;
        // the team: the subtree of my topmost live ancestor
        let mut head = me;
        for _ in 0..64 {
            match v.m.agent_parent.get(&head) {
                Some(&p) => head = p,
                None => break,
            }
        }
        let root = self.instance_root()?;
        let live: std::collections::HashSet<String> = (0..s.nodes.len())
            .filter(|i| s.nodes[*i].pane.as_ref().is_some_and(|p| p.exit.is_none()))
            .map(|i| v.handle(i).to_string())
            .collect();
        let alive = |p: &str| live.contains(p);
        let team = Team { me: v.handle(me).into(), head: v.handle(head).into(), alive: &alive, root: &root };
        let resolve = |who: &str| -> Result<String> {
            // a name first (w1 is a window handle too), else a pane handle
            if let Some(i) = (0..s.nodes.len()).find(|i| s.nodes[*i].var(view::V_NAME) == Some(who)) {
                return Ok(v.handle(i).to_string());
            }
            match parse_handle(who) {
                Some((None | Some(crate::app::Kind::Pane), id)) => Ok(view::pane_handle(id)),
                _ => Err(Error::invalid(format!("no agent {who}"))),
            }
        };
        match r.verb {
            Verb::New => {
                let title = r.words.join(" ");
                let body = match (r.p.one("--body"), r.p.one("--body-file")) {
                    (Some(b), _) => b.to_string(),
                    (None, Some(f)) => std::fs::read_to_string(self.conn.env().cwd.join(f)).map_err(|e| Error::invalid(format!("{f}: {e}")))?,
                    _ => String::new(),
                };
                let deps: Vec<u64> = r.p.many("--after").iter().flat_map(|x| x.split(',')).map(|x| x.trim().parse().map_err(|_| Error::invalid("--after ID…"))).collect::<Result<_>>()?;
                let assignee = r.p.one("--assign").map(resolve).transpose()?;
                Ok(vec![team.new_task(&title, &body, &deps, assignee)?])
            }
            Verb::Claim => {
                let which = match r.words.first().map(String::as_str) {
                    None | Some("next") => Claim::Next,
                    Some(x) => Claim::Id(x.parse().map_err(|_| Error::invalid("task claim ID|next"))?),
                };
                Ok(vec![team.claim(which)?])
            }
            Verb::Done => {
                let id: u64 = r.words.first().and_then(|x| x.parse().ok()).ok_or_else(|| Error::invalid("task done ID [--status done|failed|blocked] \"summary\""))?;
                let summary = r.words[1..].join(" ");
                let status = Status::parse(r.p.one("--status").unwrap_or("done"))?;
                Ok(vec![team.done(id, &summary, status)?])
            }
            Verb::List => {
                let f = board::Filter { status: r.p.one("--status").map(Status::parse).transpose()?, mine: r.p.has("--mine"), ready: r.p.has("--ready") };
                team.list(&f)
            }
            Verb::Watch | Verb::Wait => {
                // wakes on board writes (kqueue on the board's directory), never polls
                let until = r.p.one("--until").unwrap_or(if r.verb == Verb::Wait { "all-done" } else { "change" }).to_string();
                let timeout = crate::watch::timeout_of(r)?;
                let deadline = timeout.map(|t| Instant::now() + t);
                let dir = store::dir(&root, &["tasks"])?;
                let file = board::board_path(&root, &team.head)?;
                let first = board::read_board(&root, &team.head)?;
                let before = serde_json::to_string(&first.tasks).unwrap_or_default();
                loop {
                    let watcher = crate::fswait::Watcher::new(&[dir.as_path(), file.as_path()]).map_err(|e| Error::failed(e.to_string()))?;
                    let b = board::read_board(&root, &team.head)?;
                    let met = match until.as_str() {
                        "change" => serde_json::to_string(&b.tasks).unwrap_or_default() != before,
                        "done" | "all-done" => b.all_final(),
                        "ready" => b.tasks.iter().any(|t| b.claimable_by(t, &team.me)),
                        "claimed" => b.tasks.iter().any(|t| t.status == Status::Claimed),
                        c => match c.strip_prefix("count>=").and_then(|n| n.parse::<usize>().ok()) {
                            Some(n) => b.tasks.len() >= n,
                            None => return Err(Error::invalid("--until change|done|all-done|ready|claimed|count>=N")),
                        },
                    };
                    if met {
                        let mut rows = team.list(&board::Filter::default())?;
                        rows.push(ok("board", crate::obj! {"until" => until, "tasks" => b.tasks.len()}));
                        return Ok(rows);
                    }
                    let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                    if left.is_some_and(|l| l.is_zero()) || !watcher.wait(left).map_err(|e| Error::failed(e.to_string()))? {
                        return Ok(vec![from_error(&Error::Timeout(format!("task board: --until {until} not met")), crate::obj! {})]);
                    }
                }
            }
            vb => Err(Error::invalid(format!("task {vb} is n/a"))),
        }
    }

    // ---- worktrees ----------------------------------------------------------------------------

    fn paused_path(&self, name: &str) -> Result<PathBuf> {
        Ok(store::dir(&self.conn.env().state, &["paused", &store::sanitize(&self.parent_key())])?.join(store::sanitize(name) + ".json"))
    }

    pub fn paused(&mut self) -> Result<Vec<Obj>> {
        let dir = store::dir(&self.conn.env().state, &["paused", &store::sanitize(&self.parent_key())])?;
        let mut list: Vec<Paused> = std::fs::read_dir(dir)
            .map(|d| d.flatten().filter_map(|e| store::read_json(&e.path())).filter_map(|v| serde_json::from_value(v).ok()).collect())
            .unwrap_or_default();
        list.sort_by(|a, b| a.at.cmp(&b.at).then(a.name.cmp(&b.name)));
        Ok(list.iter().map(|p| ok("paused", crate::obj! {"name" => p.name, "branch" => p.branch, "worktree" => p.rec.worktree, "commit" => p.commit})).collect())
    }

    /// A worktree agent by name or pane. Open by default: anyone may checkpoint, merge, pause, free or land any
    /// worktree agent (who spawned whom is recorded for tree and children, never a permission).
    pub(crate) fn wt_target(&mut self, who: &str) -> Result<(WtTarget, Snapshot, View)> {
        let (live, rec, s, v) = self.find_agent(who)?;
        let me = v.m.me;
        let parent = live.and_then(|i| v.m.agent_parent.get(&i).copied());
        let is_parent = match live {
            Some(_) => parent.is_some() && parent == me,
            None => rec.is_some(), // a past child from my own ledger
        };
        let worktree = live
            .and_then(|i| s.nodes[i].var(view::V_WORKTREE).map(PathBuf::from))
            .or_else(|| rec.as_ref().and_then(|x| x.worktree.clone()).map(PathBuf::from))
            .ok_or_else(|| Error::policy("not-worktree", format!("{who} does not work in a worktree")))?;
        // the parent's checkout: the one the worktree was made in (`<top>/.worktrees/<name>`), else its pane's
        // folder (or, outside iTerm2, the CLI's): a parent that cd'd elsewhere still merges into the right one
        let made = made_in(&worktree);
        let parent_dir = match parent.or(me) {
            _ if made.is_some_and(Path::is_dir) => made.unwrap_or(&worktree).to_path_buf(),
            Some(p) if Some(p) != live => s.nodes[p].pane.as_ref().map(|x| PathBuf::from(&x.cwd)).unwrap_or_else(|| self.conn.env().cwd.clone()),
            _ => self.conn.env().cwd.clone(),
        };
        let parent_top = crate::exec::git_top(&parent_dir).ok_or_else(|| Error::policy("parent-not-git", format!("{} is not in a Git repository", parent_dir.display())))?;
        let onto = branch_of(&parent_top).ok_or_else(|| Error::policy("parent-detached", format!("{} has no branch checked out", parent_top.display())))?;
        let exists = worktree.is_dir();
        let name = live.and_then(|i| s.nodes[i].var(view::V_NAME).map(str::to_string)).or(rec.as_ref().and_then(|x| x.name.clone())).unwrap_or(who.to_string());
        let branch = if exists { branch_of(&worktree) } else { None }.unwrap_or_else(|| format!("wt/{}", rec.as_ref().and_then(|x| x.name.clone()).unwrap_or(who.to_string())));
        if branch == onto {
            return Err(Error::policy("same-branch", format!("{who} and its parent are both on {branch}")));
        }
        let t = WtTarget { name, live, rec, is_parent, worktree, exists, parent_top, onto, branch };
        Ok((t, s, v))
    }

    fn worktree_verb(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let who = r.words.first().ok_or_else(|| Error::invalid(format!("agent {} NAME", r.verb)))?.clone();
        if r.verb == Verb::Resume {
            return self.resume(r, &who).map(|o| vec![o]);
        }
        let (t, s, v) = self.wt_target(&who)?;
        let WtTarget { live, rec, is_parent, worktree, exists, parent_top, onto, branch, .. } = t;
        let mut row = crate::obj! {"agent" => who, "branch" => branch, "onto" => onto, "worktree" => worktree.display().to_string()};
        let gated = |row: Obj, code: &str| if r.apply || is_parent || r.verb == Verb::Checkpoint { Ok(None) } else { Ok::<_, Error>(Some(planned(code, row))) };
        match r.verb {
            Verb::Checkpoint => {
                if !exists {
                    return Err(Error::invalid(format!("{who} is paused (no worktree); resume it first")));
                }
                Ok(vec![self.checkpoint(&worktree, &onto, &who, r.p.one("--message"), row, false)?])
            }
            Verb::Merge => {
                if let Some(p) = gated(row.clone(), "merged")? {
                    return Ok(vec![p]);
                }
                if exists && dirty(&worktree, true)? {
                    return Err(Error::policy("uncommitted", format!("{who} has uncommitted work; checkpoint it first")));
                }
                if dirty(&parent_top, false)? {
                    return Err(Error::policy("dirty-parent", format!("{} has uncommitted changes; commit or stash them first", parent_top.display())));
                }
                row.insert("into".into(), json!(parent_top.display().to_string()));
                let commits = count(&parent_top, &format!("{onto}..{branch}"))?;
                if commits == 0 {
                    return Ok(vec![ok("up-to-date", row)]);
                }
                let ff = git_yes(&parent_top, &["merge-base", "--is-ancestor", &onto, &branch])?;
                let how = if r.p.has("--ff-only") {
                    if !ff {
                        return Err(Error::policy("not-ff", format!("{onto} has moved on since {branch} branched; checkpoint (rebase) it first")));
                    }
                    "ff"
                } else if r.p.has("--no-ff") || !ff {
                    "no-ff"
                } else {
                    "ff"
                };
                row.insert("how".into(), json!(how));
                row.insert("commits".into(), json!(commits));
                if how == "ff" {
                    git_ok(&parent_top, &["merge", "--ff-only", "-q", &branch])?;
                } else {
                    let msg = format!("Merge {branch} ({who})");
                    if !git(&parent_top, &["merge", "--no-ff", "-q", "-m", &msg, &branch])?.status.success() {
                        let files = conflicts(&parent_top);
                        git(&parent_top, &["merge", "--abort"])?;
                        return Err(Error::policy("conflict", format!("merging {branch} into {onto} conflicts in {files}; merge aborted, nothing changed")));
                    }
                }
                row.insert("commit".into(), json!(git_ok(&parent_top, &["rev-parse", "--short", "HEAD"])?));
                Ok(vec![ok("merged", row)])
            }
            Verb::Pause => {
                let i = live.ok_or_else(|| Error::invalid(format!("{who} is not running; wt-remove frees its worktree")))?;
                if !exists {
                    return Err(Error::invalid(format!("{who}'s worktree {} is gone", worktree.display())));
                }
                let point = self.checkpoint(&worktree, &onto, &who, r.p.one("--message"), row.clone(), false)?;
                let mut rec = rec.unwrap_or(Rec {
                    pane: v.id(i).0,
                    name: s.nodes[i].var(view::V_NAME).map(str::to_string),
                    kind: s.nodes[i].var(view::V_KIND).unwrap_or("cmd").into(),
                    cwd: worktree.display().to_string(),
                    cmd: s.nodes[i].var(view::V_CMD).map(str::to_string),
                    model: s.nodes[i].var(view::V_MODEL).map(str::to_string),
                    effort: s.nodes[i].var(view::V_EFFORT).map(str::to_string),
                    task: s.nodes[i].var(view::V_TASK).map(str::to_string),
                    worktree: Some(worktree.display().to_string()),
                    app: 0,
                    at: store::now(),
                });
                rec.worktree = Some(worktree.display().to_string());
                let name = rec.name.clone().unwrap_or(who.clone());
                let p = Paused {
                    name: name.clone(),
                    rec,
                    repo: parent_top.display().to_string(),
                    branch: branch.clone(),
                    onto,
                    commit: point.get("commit").and_then(Value::as_str).unwrap_or("").into(),
                    at: store::now(),
                };
                let path = self.paused_path(&name)?;
                store::atomic_write(&path, serde_json::to_string(&p).map_err(|e| Error::failed(e.to_string()))?.as_bytes())?;
                self.conn.call(Op::Close { ids: vec![v.id(i)] })?;
                let removed = git(&parent_top, &["worktree", "remove", &worktree.display().to_string()])?;
                row.insert("commit".into(), json!(p.commit));
                row.insert("removed".into(), json!(removed.status.success()));
                if !removed.status.success() {
                    row.insert("reason".into(), json!(String::from_utf8_lossy(&removed.stderr).trim()));
                }
                Ok(vec![ok("paused", row)])
            }
            Verb::WtRemove => {
                if live.is_some_and(|i| s.nodes[i].pane.as_ref().is_some_and(|p| p.exit.is_none())) {
                    return Err(Error::policy("live", format!("{who} still runs in {}; pause or close it first", worktree.display())));
                }
                if !exists {
                    return Err(Error::invalid(format!("{who}'s worktree {} is already gone", worktree.display())));
                }
                let changed = dirty(&worktree, true)?;
                let unmerged = count(&parent_top, &format!("{onto}..{branch}"))?;
                if (changed || unmerged > 0) && !r.force {
                    return Err(Error::policy("unmerged", format!("{who} has {unmerged} commit(s) not on {onto}{}; merge it, or pass --force", if changed { " and uncommitted changes" } else { "" })));
                }
                let path = worktree.display().to_string();
                let mut args = vec!["worktree", "remove"];
                if r.force {
                    args.push("--force");
                }
                args.push(&path);
                git_ok(&parent_top, &args)?;
                row.insert("unmerged".into(), json!(unmerged));
                Ok(vec![ok("removed", row)])
            }
            _ => unreachable!("worktree verbs only"),
        }
    }

    /// Commit everything, then rebase onto the parent's branch; a conflict aborts the rebase and
    /// is refused, leaving the worktree clean at its commit (`undo`: the leftovers uncommitted again, as they were).
    pub(crate) fn checkpoint(&self, dir: &Path, onto: &str, who: &str, message: Option<&str>, mut row: Obj, undo: bool) -> Result<Obj> {
        let changed = dirty(dir, true)?;
        row.insert("committed".into(), json!(changed));
        if changed {
            let msg = message.map(str::to_string).unwrap_or_else(|| format!("checkpoint {who}"));
            git_ok(dir, &["add", "-A"])?;
            git_ok(dir, &["commit", "-q", "-m", &msg])?;
        }
        let commit = git_ok(dir, &["rev-parse", "--short", "HEAD"])?;
        if !git_yes(dir, &["merge-base", "--is-ancestor", onto, "HEAD"])? && !git(dir, &["rebase", "-q", onto])?.status.success() {
            let files = conflicts(dir);
            git(dir, &["rebase", "--abort"])?;
            if undo {
                if changed {
                    git_ok(dir, &["reset", "-q", "HEAD~1"])?;
                }
                return Err(Error::policy("conflict", format!("rebasing onto {onto} conflicts in {files}; aborted, nothing changed: rebase {} by hand, then land again", dir.display())));
            }
            return Err(Error::policy("conflict", format!("rebasing onto {onto} conflicts in {files}; rebase aborted, the work is committed at {commit} and the worktree is clean")));
        }
        row.insert("commit".into(), json!(git_ok(dir, &["rev-parse", "--short", "HEAD"])?));
        row.insert("ahead".into(), json!(count(dir, &format!("{onto}..HEAD"))?));
        Ok(ok("checkpointed", row))
    }

    fn resume(&mut self, r: &Request, name: &str) -> Result<Obj> {
        let path = self.paused_path(name)?;
        // a Claude Code sub-agent (`agent-<id>`) when no paused agent of mine has that name
        if !path.is_file() {
            if let Some(sub) = crate::subagent::id_of(name).and_then(crate::subagent::find) {
                return self.resume_subagent(r, &sub);
            }
        }
        let p: Paused = store::read_json(&path).and_then(|v| serde_json::from_value(v).ok()).ok_or_else(|| Error::invalid(format!("no paused agent {name} under you")))?;
        let (s, v) = self.view()?;
        if (0..s.nodes.len()).any(|i| s.nodes[i].var(view::V_NAME) == Some(name) && v.mine(&s, i, true)) {
            return Err(Error::policy("name-taken", format!("a live agent is named {name}; close or rename it first")));
        }
        let wt = PathBuf::from(p.rec.worktree.clone().unwrap_or_default());
        let reuse = wt.is_dir();
        if reuse && branch_of(&wt).as_deref() != Some(p.branch.as_str()) {
            return Err(Error::policy("exists", format!("{} exists and is not {}'s worktree", wt.display(), p.branch)));
        }
        if !reuse {
            git_ok(Path::new(&p.repo), &["worktree", "add", "-q", &wt.display().to_string(), &p.branch])?;
        }
        let item = Item {
            name: Some(p.name.clone()),
            cwd: wt.display().to_string(),
            task: r.p.one("--task").map(str::to_string).or(p.rec.task.clone()),
            cmd: p.rec.cmd.clone(),
            model: r.p.one("--model").map(crate::agent::resolve_model).transpose()?.or(p.rec.model.clone()),
            effort: r.p.one("--effort").map(crate::agent::check_effort).transpose()?.or(p.rec.effort.clone()),
            worktree: Some(wt.display().to_string()),
            title: None,
        };
        let agent = matches!(p.rec.kind.as_str(), "claude" | "omp");
        let at = self.conn.env().me.ok_or_else(|| Error::invalid("agent resume needs your pane (run it inside iTerm2)"))?;
        let spawn = grammar::parse(grammar::Noun::Pane, &["new".to_string()]).map_err(|e| Error::invalid(e.to_string()))?;
        let mut rows = self.place_items(&spawn, &s, &v, std::slice::from_ref(&item), agent, Some(at))?;
        let mut row = rows.pop().unwrap_or_default();
        if row.get("status").and_then(Value::as_str) == Some("ok") {
            // the record is spent: into the Trash, never deleted outright
            let _ = Command::new("/usr/bin/trash").arg(&path).output();
            row.insert("code".into(), json!("resumed"));
            row.insert("branch".into(), json!(p.branch));
        }
        Ok(row)
    }

    /// `agent resume agent-<id>`: a Claude Code sub-agent as a new Claude agent beside the caller: its pane and
    /// session named as it was (its description), colored from that name, told its task and transcript.
    fn resume_subagent(&mut self, r: &Request, sub: &crate::subagent::Sub) -> Result<Obj> {
        let (s, v) = self.view()?;
        let at = self.conn.env().me.ok_or_else(|| Error::invalid("agent resume needs your pane (run it inside iTerm2)"))?;
        // its name as an agent name, unique among the live ones
        let base = crate::subagent::slug(&sub.name);
        let taken = |n: &str| (0..s.nodes.len()).any(|i| s.nodes[i].var(view::V_NAME) == Some(n));
        let name = std::iter::once(base.clone()).chain((2..100).map(|k| format!("{base}-{k}"))).find(|n| !taken(n)).unwrap_or(base);
        let cwd = sub.cwd.clone().filter(|c| Path::new(c).is_dir()).unwrap_or_else(|| self.conn.env().cwd.display().to_string());
        let worktree = match &r.multi.worktree {
            Some(b) => {
                let repo = crate::exec::git_top(Path::new(&cwd)).ok_or_else(|| Error::policy("not-git", format!("--worktree: {cwd} is not in a Git repository")))?;
                Some(crate::agents::worktree_create(&repo, &name, b.as_deref())?.display().to_string())
            }
            None => None,
        };
        let item = Item {
            name: Some(name),
            cwd: worktree.clone().unwrap_or(cwd),
            task: Some(r.p.one("--task").map(str::to_string).unwrap_or_else(|| crate::subagent::resume_task(sub))),
            cmd: None,
            model: r.p.one("--model").or(sub.model.as_deref()).map(crate::agent::resolve_model).transpose()?,
            effort: r.p.one("--effort").map(crate::agent::check_effort).transpose()?,
            worktree,
            title: Some(sub.name.clone()),
        };
        let mut args = vec!["new".to_string()];
        if r.p.has("--read-only") {
            args.push("--read-only".into());
        }
        let spawn = grammar::parse(grammar::Noun::Pane, &args).map_err(|e| Error::invalid(e.to_string()))?;
        let mut row = self.place_items(&spawn, &s, &v, std::slice::from_ref(&item), true, Some(at))?.pop().unwrap_or_default();
        if row.get("status").and_then(Value::as_str) == Some("ok") {
            // the pane and the session took its name at launch (`--name`), its color from that name
            row.insert("code".into(), json!("resumed"));
            row.insert("title".into(), json!(sub.name));
            row.insert("transcript".into(), json!(sub.transcript.display().to_string()));
        }
        Ok(row)
    }

    // ---- lends ----------------------------------------------------------------------------------

    /// `pane lend PANE [--to AGENT|--any] [--for 1h] | --revoke`: only the human lends (a caller
    /// with no agent environment whose pane runs no agent).
    pub fn lend(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let target = r.words.first().ok_or_else(|| Error::invalid("pane lend PANE [--to AGENT|--any] [--for 1h] | --revoke"))?;
        let (s, v) = self.view()?;
        let human = !self.conn.env().agent
            && v.m.me.is_none_or(|m| !view::is_agent_kind(s.nodes[m].var(view::V_KIND)) && !v.m.agent_parent.contains_key(&m));
        if !human {
            return Err(Error::policy("not-human", "only the human lends a pane (from a shell that runs no agent)"));
        }
        let e = grammar::selector(target).map_err(|e| Error::invalid(e.to_string()))?;
        let pane = self.one_node(&v, &e, Kind::Pane, "pane lend")?;
        if r.p.has("--revoke") {
            self.conn.call(Op::Set { id: v.id(pane), field: Field::Var(view::V_LEND.into(), None) })?;
            return Ok(vec![ok("revoke", crate::obj! {"pane" => v.handle(pane)})]);
        }
        let secs = parse_duration(r.p.one("--for").unwrap_or("1h"))?;
        let to = match (r.p.one("--to"), r.p.has("--any")) {
            (_, true) | (None, false) => None,
            (Some(a), false) => {
                let e = grammar::selector(a).map_err(|e| Error::invalid(e.to_string()))?;
                let found = crate::select::eval(&v.m, &e, Some(Kind::Pane), &Default::default()).ok().and_then(|f| f.first().copied());
                let named = (0..s.nodes.len()).find(|i| s.nodes[*i].var(view::V_NAME) == Some(a));
                Some(v.handle(found.or(named).ok_or_else(|| Error::invalid(format!("no agent {a}")))?).to_string())
            }
        };
        let until = store::now() + secs;
        let by = v.m.me.map(|m| v.handle(m).to_string());
        let val = json!({"to": to, "by": by, "at": store::now(), "until": until}).to_string();
        self.conn.call(Op::Set { id: v.id(pane), field: Field::Var(view::V_LEND.into(), Some(val)) })?;
        Ok(vec![ok("lend", crate::obj! {"pane" => v.handle(pane), "to" => to.unwrap_or_else(|| "any".into()), "until" => until})])
    }
}

/// `30m`, `1h`, `90s`, `1d`: at most a day.
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let bad = || Error::invalid(format!("--for {s}: N[s|m|h|d], at most 1d"));
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = n.parse().map_err(|_| bad())?;
    let unit = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(bad()),
    };
    let secs = n.checked_mul(unit).ok_or_else(bad)?;
    if secs <= 0 || secs > 86400 {
        return Err(bad());
    }
    Ok(secs)
}

/// The agent provider in a pane: the app's (it watches the foreground process), else what spawned it.
pub fn provider(n: &crate::app::Node) -> Option<&str> {
    n.var(view::V_PROVIDER).or_else(|| n.var(view::V_KIND).filter(|k| *k != "shell" && *k != "-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // regression: `--for 999999999999999d` overflowed (a panic in a debug build, a wrapped time in release)
    #[test]
    fn lend_durations_are_bounded_never_overflow() {
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("1d"), Ok(86400));
        for bad in ["999999999999999d", "9223372036854775807h", "2d", "0", "x", ""] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_worktree_base_is_not_an_option() {
        let e = worktree_create(Path::new("/nonexistent"), "w1", Some("--orphan")).unwrap_err();
        assert!(e.message().contains("--orphan"), "{e:?}");
    }
}
