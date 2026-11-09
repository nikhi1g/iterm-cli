//! A team's task board (ported from iterm_api's `tasks.rs`): `<state>/tasks/<head>.json` (0600),
//! one per team, where the team is the agent subtree of the caller's head (its topmost live
//! ancestor). Every change reads, edits and atomically rewrites the board under its lock, so a
//! claim has exactly one winner. Task IDs are 1, 2, … and never reused, so a lookup is O(1).
//!
//! A task is pending until claimed; it can be claimed only when every dependency is done, and only
//! by its assignee when it has one. done and failed are final; blocked keeps its owner. A claim
//! lasts while its owner's pane does: every operation first returns gone owners' claims to pending.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::out::{ok, Error, Obj, Result};
use crate::store::{self, Lock};

pub const TITLE_LIMIT: usize = 200;
pub const BODY_LIMIT: usize = 16 * 1024;
pub const SUMMARY_LIMIT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Claimed,
    Done,
    Failed,
    Blocked,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Claimed => "claimed",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Blocked => "blocked",
        }
    }
    pub fn parse(s: &str) -> Result<Status> {
        Ok(match s {
            "pending" => Status::Pending,
            "claimed" => Status::Claimed,
            "done" => Status::Done,
            "failed" => Status::Failed,
            "blocked" => Status::Blocked,
            _ => return Err(Error::invalid("status: pending|claimed|done|failed|blocked")),
        })
    }
    pub fn is_final(self) -> bool {
        matches!(self, Status::Done | Status::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Id(u64),
    Next,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub status: Option<Status>,
    pub mine: bool,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub deps: Vec<u64>,
    #[serde(default)]
    pub assignee: Option<String>,
    pub status: Status,
    #[serde(default)]
    pub owner: Option<String>,
    pub by: String,
    pub at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub released: u32,
}

impl Task {
    pub fn row(&self, ready: Option<bool>) -> Obj {
        let mut r = crate::obj! {"id" => self.id, "title" => self.title, "deps" => self.deps, "assignee" => self.assignee,
            "owner" => self.owner, "summary" => self.summary, "text" => self.body};
        if let Some(ready) = ready {
            r.insert("ready".into(), json!(ready));
        }
        if self.released > 0 {
            r.insert("released".into(), json!(self.released));
        }
        r.retain(|_, v| !v.is_null() && v.as_str() != Some("") && v.as_array().is_none_or(|a| !a.is_empty()));
        ok(self.status.as_str(), r)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Board {
    pub tasks: Vec<Task>,
}

impl Board {
    pub fn get(&self, id: u64) -> Option<&Task> {
        id.checked_sub(1).and_then(|i| self.tasks.get(i as usize)).filter(|t| t.id == id)
    }
    fn get_mut(&mut self, id: u64) -> Option<&mut Task> {
        id.checked_sub(1).and_then(|i| self.tasks.get_mut(i as usize)).filter(|t| t.id == id)
    }
    fn waiting_on(&self, t: &Task) -> (Vec<u64>, bool) {
        let (mut wait, mut failed) = (Vec::new(), false);
        for d in &t.deps {
            match self.get(*d).map(|x| x.status) {
                Some(Status::Done) => {}
                Some(Status::Failed) => {
                    failed = true;
                    wait.push(*d);
                }
                _ => wait.push(*d),
            }
        }
        (wait, failed)
    }
    pub fn claimable_by(&self, t: &Task, me: &str) -> bool {
        t.status == Status::Pending && t.assignee.as_deref().is_none_or(|a| a == me) && self.waiting_on(t).0.is_empty()
    }
    /// Return the claims of gone owners to pending; whether anything changed.
    pub fn reap(&mut self, alive: &dyn Fn(&str) -> bool) -> bool {
        let mut changed = false;
        for t in self.tasks.iter_mut() {
            if matches!(t.status, Status::Claimed | Status::Blocked) && !t.owner.as_deref().is_some_and(alive) {
                t.status = Status::Pending;
                t.owner = None;
                t.released += 1;
                changed = true;
            }
        }
        changed
    }
    pub fn all_final(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.iter().all(|t| t.status.is_final())
    }
}

/// The caller's team: who it is and the board's head.
pub struct Team<'a> {
    pub me: String,
    pub head: String,
    pub alive: &'a dyn Fn(&str) -> bool,
    pub root: &'a Path,
}

pub fn board_path(root: &Path, head: &str) -> Result<PathBuf> {
    Ok(store::dir(root, &["tasks"])?.join(store::sanitize(head) + ".json"))
}

pub fn read_board(root: &Path, head: &str) -> Result<Board> {
    match std::fs::read_to_string(board_path(root, head)?) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| Error::failed(format!("task board: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Board::default()),
        Err(e) => Err(Error::failed(format!("task board: {e}"))),
    }
}

fn one_line(text: &str, what: &str, limit: usize) -> Result<String> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() || t.chars().count() > limit {
        return Err(Error::invalid(format!("give a one-line {what} (1–{limit} characters)")));
    }
    Ok(t)
}

impl Team<'_> {
    fn with_board<T>(&self, f: impl FnOnce(&mut Board) -> Result<(T, bool)>) -> Result<T> {
        let lock = store::dir(self.root, &["tasks"])?.join(store::sanitize(&self.head) + ".lock");
        let _held = Lock::acquire(&lock)?;
        let mut b = read_board(self.root, &self.head)?;
        let reaped = b.reap(self.alive);
        let (out, changed) = f(&mut b)?;
        if changed || reaped {
            let text = serde_json::to_string(&b).map_err(|e| Error::failed(e.to_string()))? + "\n";
            store::atomic_write(&board_path(self.root, &self.head)?, text.as_bytes())?;
        }
        Ok(out)
    }

    pub fn new_task(&self, title: &str, body: &str, deps: &[u64], assignee: Option<String>) -> Result<Obj> {
        let title = one_line(title, "title", TITLE_LIMIT)?;
        if body.len() > BODY_LIMIT {
            return Err(Error::invalid(format!("a task body is limited to {} KiB", BODY_LIMIT / 1024)));
        }
        let mut deps = deps.to_vec();
        deps.sort_unstable();
        deps.dedup();
        self.with_board(|b| {
            for d in &deps {
                match b.get(*d).map(|t| t.status) {
                    None => return Err(Error::invalid(format!("no task {d} on your team's board"))),
                    Some(Status::Failed) => return Err(Error::policy("dep-failed", format!("task {d} failed; it can never unblock this one"))),
                    _ => {}
                }
            }
            let t = Task {
                id: b.tasks.len() as u64 + 1,
                title,
                body: body.to_string(),
                deps,
                assignee,
                status: Status::Pending,
                owner: None,
                by: self.me.clone(),
                at: store::now(),
                ended_at: None,
                summary: None,
                released: 0,
            };
            let row = t.row(Some(b.waiting_on(&t).0.is_empty()));
            b.tasks.push(t);
            Ok((row, true))
        })
    }

    /// Exactly one caller wins a race; the rest are refused (`taken`).
    pub fn claim(&self, which: Claim) -> Result<Obj> {
        let me = self.me.clone();
        self.with_board(|b| {
            let id = match which {
                Claim::Id(id) => {
                    let t = b.get(id).ok_or_else(|| Error::invalid(format!("no task {id} on your team's board")))?;
                    match t.status {
                        Status::Pending => {}
                        Status::Claimed | Status::Blocked => {
                            return Err(Error::policy("taken", format!("task {id} is claimed by {}", t.owner.clone().unwrap_or_default())));
                        }
                        s => return Err(Error::policy("finished", format!("task {id} is {}", s.as_str()))),
                    }
                    if let Some(a) = t.assignee.as_deref().filter(|a| *a != me) {
                        return Err(Error::policy("assigned", format!("task {id} is assigned to {a}")));
                    }
                    let (wait, failed) = b.waiting_on(t);
                    if failed {
                        return Err(Error::policy("dep-failed", format!("task {id} depends on a failed task ({wait:?})")));
                    }
                    if !wait.is_empty() {
                        return Err(Error::policy("deps", format!("task {id} waits on tasks {wait:?}")));
                    }
                    id
                }
                Claim::Next => {
                    let mine = b.tasks.iter().find(|t| t.assignee.as_deref() == Some(me.as_str()) && b.claimable_by(t, &me));
                    match mine.or_else(|| b.tasks.iter().find(|t| b.claimable_by(t, &me))) {
                        Some(t) => t.id,
                        None => {
                            let mut by: HashMap<Status, usize> = HashMap::new();
                            for t in &b.tasks {
                                *by.entry(t.status).or_default() += 1;
                            }
                            return Err(Error::policy(
                                "none-ready",
                                format!(
                                    "no task is ready for you ({} pending wait on dependencies or another assignee, {} claimed)",
                                    by.get(&Status::Pending).unwrap_or(&0),
                                    by.get(&Status::Claimed).unwrap_or(&0)
                                ),
                            ));
                        }
                    }
                }
            };
            let t = b.get_mut(id).expect("checked above");
            t.status = Status::Claimed;
            t.owner = Some(me.clone());
            Ok((t.row(None), true))
        })
    }

    pub fn done(&self, id: u64, summary: &str, status: Status) -> Result<Obj> {
        if !matches!(status, Status::Done | Status::Failed | Status::Blocked) {
            return Err(Error::invalid("a task ends done, failed or blocked"));
        }
        let summary = one_line(summary, "summary", SUMMARY_LIMIT)?;
        self.with_board(|b| {
            let t = b.get_mut(id).ok_or_else(|| Error::invalid(format!("no task {id} on your team's board")))?;
            if !matches!(t.status, Status::Claimed | Status::Blocked) {
                return Err(Error::policy("not-claimed", format!("task {id} is {}; claim it first", t.status.as_str())));
            }
            if t.owner.as_deref() != Some(self.me.as_str()) {
                return Err(Error::policy("not-yours", format!("task {id} is claimed by {}", t.owner.clone().unwrap_or_default())));
            }
            t.status = status;
            t.summary = Some(summary);
            t.ended_at = Some(store::now());
            Ok((t.row(None), true))
        })
    }

    pub fn list(&self, f: &Filter) -> Result<Vec<Obj>> {
        let me = self.me.clone();
        self.with_board(|b| {
            let rows = b
                .tasks
                .iter()
                .filter(|t| f.status.is_none_or(|s| s == t.status))
                .filter(|t| !f.mine || t.owner.as_deref() == Some(me.as_str()) || t.assignee.as_deref() == Some(me.as_str()))
                .filter(|t| !f.ready || b.claimable_by(t, &me))
                .map(|t| t.row((t.status == Status::Pending).then(|| b.claimable_by(t, &me))))
                .collect();
            Ok((rows, false))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_have_one_winner_and_deps_gate() {
        let root = std::env::temp_dir().join(format!("iterm-board-{}-{:?}", std::process::id(), std::time::Instant::now()));
        let alive = |p: &str| p != "gone";
        let team = |me: &str| Team { me: me.into(), head: "p1".into(), alive: &alive, root: &root };
        let a = team("p2");
        a.new_task("build", "", &[], None).unwrap();
        let r = a.new_task("test", "", &[1], None).unwrap();
        assert_eq!(r["ready"], json!(false));
        assert!(a.new_task("x", "", &[9], None).is_err());
        assert_eq!(a.claim(Claim::Next).unwrap()["id"], json!(1));
        let e = team("p3").claim(Claim::Id(1)).unwrap_err();
        assert!(matches!(e, Error::Policy { ref code, .. } if code == "taken"), "{e:?}");
        assert!(matches!(team("p3").claim(Claim::Id(2)).unwrap_err(), Error::Policy { ref code, .. } if code == "deps"));
        assert!(team("p3").done(1, "x", Status::Done).is_err());
        a.done(1, "built", Status::Done).unwrap();
        assert_eq!(team("p3").claim(Claim::Next).unwrap()["id"], json!(2));
        // a gone owner's claim returns to pending
        let g = team("gone");
        g.new_task("third", "", &[], None).unwrap();
        g.claim(Claim::Id(3)).unwrap();
        let rows = a.list(&Filter::default()).unwrap();
        assert_eq!(rows[2]["code"], json!("pending"));
        assert_eq!(rows[2]["released"], json!(1));
    }
}
