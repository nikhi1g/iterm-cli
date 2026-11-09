//! `iterm plan run FILE.json [--detach] | status [NAME] | cancel NAME`: a task DAG run by a scheduler process,
//! not by a lead's tokens. Each task is an agent (`agent new --task`); a task is finished when its agent goes idle
//! after working (or ends); its gate (a shell command in its folder) decides done or a retry (the failure is sent
//! to the agent, which keeps its context). Dependents of a failed task are skipped. Waits ride the app's pushed
//! events (agent.state changes), never polling. Status is a JSON file per plan, rewritten on every change.
//!
//! ```json
//! { "name": "release", "max": 3,
//!   "tasks": [ { "id": "parse", "task": "…", "provider": "claude", "model": "opus", "effort": "high",
//!                "worktree": true, "deps": [], "gate": "cargo test -q", "retries": 1 } ] }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::{Event, Op};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::exec::Exec;
use crate::out::{Error, Obj, Result, from_error, ok};
use crate::view;

#[derive(Debug, Clone, Deserialize)]
pub struct PlanFile {
    pub name: Option<String>,
    pub max: Option<usize>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: String,
    pub task: String,
    #[serde(default)]
    pub deps: Vec<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub worktree: bool,
    pub gate: Option<String>,
    #[serde(default)]
    pub retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum St {
    Waiting,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rec {
    pub id: String,
    pub status: St,
    pub pane: Option<String>,
    pub tries: u32,
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub pid: u32,
    pub state: String,
    pub tasks: Vec<Rec>,
}

/// Unique ids, known deps, no cycles.
pub fn check(p: &PlanFile) -> Result<()> {
    let ids: HashMap<&str, usize> = p.tasks.iter().enumerate().rev().map(|(k, t)| (t.id.as_str(), k)).collect();
    for (k, t) in p.tasks.iter().enumerate() {
        if !crate::grammar::is_slug(&t.id) {
            return Err(Error::invalid(format!("task id '{}': letters, digits, _ and -", t.id)));
        }
        if ids[t.id.as_str()] != k {
            return Err(Error::invalid(format!("task '{}' twice", t.id)));
        }
        if let Some(d) = t.deps.iter().find(|d| !ids.contains_key(d.as_str())) {
            return Err(Error::invalid(format!("task '{}' depends on unknown '{d}'", t.id)));
        }
    }
    // Kahn: every task reachable in dependency order, O(tasks + edges)
    let mut blockers: Vec<usize> = p.tasks.iter().map(|t| t.deps.len()).collect();
    let mut after: Vec<Vec<usize>> = vec![Vec::new(); p.tasks.len()];
    for (k, t) in p.tasks.iter().enumerate() {
        for d in &t.deps {
            after[ids[d.as_str()]].push(k);
        }
    }
    let mut ready: Vec<usize> = (0..p.tasks.len()).filter(|k| blockers[*k] == 0).collect();
    while let Some(k) = ready.pop() {
        for &n in &after[k] {
            blockers[n] -= 1;
            if blockers[n] == 0 {
                ready.push(n);
            }
        }
    }
    let left: Vec<&str> = p.tasks.iter().enumerate().filter(|(k, _)| blockers[*k] > 0).map(|(_, t)| t.id.as_str()).collect();
    if !left.is_empty() {
        return Err(Error::invalid(format!("a dependency cycle among: {}", left.join(", "))));
    }
    Ok(())
}

/// What to do next: tasks to start (deps done, under `max` running) and tasks to skip (a dep failed or skipped).
pub fn step(p: &PlanFile, recs: &[Rec], max: usize) -> (Vec<usize>, Vec<usize>) {
    let by_id: HashMap<&str, St> = recs.iter().map(|r| (r.id.as_str(), r.status)).collect();
    let st = |id: &str| by_id.get(id).copied();
    let running = recs.iter().filter(|r| r.status == St::Running).count();
    let mut start = Vec::new();
    let mut skip = Vec::new();
    for (i, t) in p.tasks.iter().enumerate() {
        if recs[i].status != St::Waiting {
            continue;
        }
        if t.deps.iter().any(|d| matches!(st(d), Some(St::Failed | St::Skipped))) {
            skip.push(i);
        } else if t.deps.iter().all(|d| st(d) == Some(St::Done)) && running + start.len() < max {
            start.push(i);
        }
    }
    (start, skip)
}

fn status_dir(x: &Exec) -> Result<PathBuf> {
    crate::store::dir(&x.conn.env().state, &["plans"])
}

fn save(path: &Path, s: &Status) {
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let _ = crate::store::atomic_write(path, &b);
    }
}

fn rec_row(r: &Rec) -> Obj {
    let mut o = crate::obj! {"task" => r.id.clone(), "status" => json!(r.status), "tries" => r.tries};
    if let Some(p) = &r.pane {
        o.insert("pane".into(), json!(p));
    }
    if let (Some(a), Some(b)) = (r.started, r.ended) {
        o.insert("secs".into(), json!(b - a));
    }
    if let Some(s) = &r.summary {
        o.insert("summary".into(), json!(s));
    }
    o
}

/// The CLI entry: `plan run|status|cancel`.
pub fn run(x: &mut Exec, words: &[String]) -> Vec<Obj> {
    let r = (|| -> Result<Vec<Obj>> {
        match words.first().map(String::as_str) {
            Some("run") => {
                let file = words.get(1).ok_or_else(|| Error::invalid("plan run FILE.json [--detach]"))?;
                let path = x.conn.env().cwd.join(file);
                let text = std::fs::read_to_string(&path).map_err(|e| Error::invalid(format!("{file}: {e}")))?;
                let plan: PlanFile = serde_json::from_str(&text).map_err(|e| Error::invalid(format!("{file}: {e}")))?;
                check(&plan)?;
                let name = plan.name.clone().unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("plan".into()));
                let name = crate::store::sanitize(&name);
                if words.iter().any(|w| w == "--plan") {
                    return Ok(plan.tasks.iter().map(|t| crate::out::planned("task", crate::obj! {"task" => t.id.clone(), "deps" => t.deps.clone()})).collect());
                }
                if words.iter().any(|w| w == "--detach") {
                    return detach(x, file, &name);
                }
                execute(x, &plan, &name)
            },
            Some("status") => {
                let dir = status_dir(x)?;
                let mut rows = Vec::new();
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let Ok(s) = serde_json::from_slice::<Status>(&std::fs::read(e.path()).unwrap_or_default()) else { continue };
                    if words.get(1).is_some_and(|n| *n != s.name) {
                        continue;
                    }
                    rows.push(ok("plan", crate::obj! {"name" => s.name.clone(), "state" => s.state.clone(), "pid" => s.pid, "tasks" => s.tasks.len()}));
                    rows.extend(s.tasks.iter().map(|t| ok("task", rec_row(t))));
                }
                Ok(rows)
            },
            Some("cancel") => {
                let name = words.get(1).ok_or_else(|| Error::invalid("plan cancel NAME"))?;
                let path = status_dir(x)?.join(format!("{}.json", crate::store::sanitize(name)));
                let mut s: Status = serde_json::from_slice(&std::fs::read(&path).map_err(|_| Error::invalid(format!("no plan {name}")))?)
                    .map_err(|e| Error::failed(e.to_string()))?;
                if s.state == "running" {
                    unsafe { libc::kill(s.pid as i32, libc::SIGTERM) };
                }
                s.state = "cancelled".into();
                save(&path, &s);
                Ok(vec![ok("cancelled", crate::obj! {"plan" => s.name, "note" => "its agents keep running; close them with iterm agent close"})])
            },
            _ => Err(Error::invalid("plan run FILE.json [--detach] | status [NAME] | cancel NAME")),
        }
    })();
    r.unwrap_or_else(|e| vec![from_error(&e, crate::obj! {})])
}

/// The scheduler, in the background: this same command without --detach, its output in plans/NAME.log.
fn detach(x: &Exec, file: &str, name: &str) -> Result<Vec<Obj>> {
    let dir = status_dir(x)?;
    let log = std::fs::File::create(dir.join(format!("{name}.log"))).map_err(|e| Error::failed(e.to_string()))?;
    let exe = std::env::current_exe().map_err(|e| Error::failed(e.to_string()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["plan", "run", file]).current_dir(&x.conn.env().cwd).stdin(std::process::Stdio::null());
    cmd.stdout(log.try_clone().map_err(|e| Error::failed(e.to_string()))?).stderr(log);
    // its own session: closing the caller's terminal doesn't stop it
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| Error::failed(e.to_string()))?;
    Ok(vec![ok("started", crate::obj! {"plan" => name, "pid" => child.id(), "status" => format!("iterm plan status {name}")})])
}

/// The last lines of an agent's answer (above its input box), for the result record.
fn summary(x: &mut Exec, pane: crate::app::Id) -> Option<String> {
    let d = x.conn.call(Op::Read { pane, lines: crate::app::wire::Lines::Screen }).ok()?;
    let lines: Vec<&str> = d.lines.iter().map(|l| l.trim_end()).collect();
    // above the input box's top border, if there is one
    let end = lines.iter().rposition(|l| l.starts_with("──") || l.starts_with("╭")).map_or(lines.len(), |b| {
        lines[..b].iter().rposition(|l| l.starts_with("──") || l.starts_with("╭")).unwrap_or(b)
    });
    // the answer's lines, not the agent's status lines (Claude's "✻ … for Ns", effort and mode footers)
    let status = |l: &str| {
        let t = l.trim_start();
        t.starts_with('✻') || t.starts_with('◐') || t.starts_with('⏵') || t.contains("/effort")
    };
    let text: Vec<&str> = lines[..end].iter().rev().filter(|l| !l.trim().is_empty() && !status(l)).take(3).copied().collect();
    (!text.is_empty()).then(|| text.into_iter().rev().map(str::trim).collect::<Vec<_>>().join(" / ").chars().take(300).collect())
}

/// A pane's folder: its agent's worktree, else its working directory.
fn folder_of(x: &mut Exec, pane: &str) -> Option<PathBuf> {
    x.conn.invalidate();
    let s = x.conn.snapshot().ok()?;
    let n = s.nodes.iter().find(|n| view::pane_handle(n.id) == pane)?;
    n.var(view::V_WORKTREE).map(PathBuf::from).or_else(|| n.pane.as_ref().map(|p| PathBuf::from(&p.cwd)).filter(|p| !p.as_os_str().is_empty()))
}

/// The state the app reports for a pane (agent.state), from the latest snapshot.
fn state_of(x: &mut Exec, pane: &str) -> Option<String> {
    let s = x.conn.snapshot().ok()?;
    let n = s.nodes.iter().find(|n| view::pane_handle(n.id) == pane)?;
    if n.pane.as_ref().is_some_and(|p| p.exit.is_some()) {
        return Some("exited".into());
    }
    n.var(view::V_STATE).map(str::to_string)
}

fn execute(x: &mut Exec, plan: &PlanFile, name: &str) -> Result<Vec<Obj>> {
    let path = status_dir(x)?.join(format!("{name}.json"));
    let max = plan.max.unwrap_or(4).max(1);
    let mut st = Status {
        name: name.into(),
        pid: std::process::id(),
        state: "running".into(),
        tasks: plan.tasks.iter().map(|t| Rec { id: t.id.clone(), status: St::Waiting, pane: None, tries: 0, started: None, ended: None, summary: None }).collect(),
    };
    let mut cwd_of: HashMap<usize, PathBuf> = HashMap::new();
    let (mut watch, _) = x.conn.watch(None, None)?;
    loop {
        let (start, skip) = step(plan, &st.tasks, max);
        // a step that started or skipped something looks again before blocking
        let mut progressed = !start.is_empty() || !skip.is_empty();
        for i in skip {
            st.tasks[i].status = St::Skipped;
        }
        for i in start {
            let t = &plan.tasks[i];
            let mut words = vec!["agent".to_string(), "new".into(), "--name".into(), t.id.clone(), "--task".into(), t.task.clone()];
            for (flag, v) in [("--provider", &t.provider), ("--model", &t.model), ("--effort", &t.effort)] {
                if let Some(v) = v {
                    words.extend([flag.to_string(), v.clone()]);
                }
            }
            if t.worktree {
                words.push("--worktree".into());
            }
            // run from outside iTerm2: each agent in a tab of its own
            if x.conn.env().me.is_none() {
                words.extend(["--to".to_string(), "new-tab".to_string()]);
            }
            let rows = crate::cli::run_words(x, &words);
            let pane = rows.first().and_then(|r| r.get("id")).and_then(|v| v.as_str()).map(str::to_string);
            let rec = &mut st.tasks[i];
            rec.tries = 1;
            rec.started = Some(crate::store::now());
            match pane {
                Some(p) => {
                    rec.status = St::Running;
                    // the gate runs in the agent's folder: its worktree, else where the pane is
                    let cwd = folder_of(x, &p);
                    cwd_of.insert(i, cwd.unwrap_or_else(|| x.conn.env().cwd.clone()));
                    rec.pane = Some(p);
                },
                None => {
                    rec.status = St::Failed;
                    rec.ended = rec.started;
                    rec.summary = rows.first().and_then(|r| r.get("reason")).and_then(|v| v.as_str()).map(str::to_string);
                },
            }
        }
        save(&path, &st);
        if st.tasks.iter().all(|r| !matches!(r.status, St::Waiting | St::Running)) {
            break;
        }
        // a running task is finished when its agent went idle after working, or ended: one fresh snapshot a sweep
        x.conn.invalidate();
        for i in 0..st.tasks.len() {
            if st.tasks[i].status != St::Running {
                continue;
            }
            let Some(pane) = st.tasks[i].pane.clone() else { continue };
            let state = state_of(x, &pane).unwrap_or_default();
            // its turn finished (idle, also when it finished inside its startup output) or it ended
            if !matches!(state.as_str(), "idle" | "done" | "failed" | "exited") {
                continue;
            }
            progressed = true;
            let id = crate::app::model::parse_handle(&pane).map(|(_, id)| id);
            st.tasks[i].summary = id.and_then(|id| summary(x, id));
            let t = &plan.tasks[i];
            let passed = match (&t.gate, state.as_str()) {
                (_, "failed") => Some(false),
                (None, _) => Some(true),
                (Some(g), _) => {
                    let out = std::process::Command::new("/bin/sh").arg("-c").arg(g).current_dir(&cwd_of[&i]).output();
                    match out {
                        Ok(o) if o.status.success() => Some(true),
                        Ok(o) if st.tasks[i].tries <= t.retries && state == "idle" => {
                            // a retry: the failure goes back to the agent, which keeps its context
                            let tail: String = String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                            let msg = format!("The gate `{g}` failed. Fix it, then stop.\n{tail}");
                            if let Some(id) = id {
                                let _ = x.conn.call(Op::Deliver { pane: id, text: msg, enter: true });
                            }
                            st.tasks[i].tries += 1;
                            None
                        },
                        _ => Some(false),
                    }
                },
            };
            if let Some(p) = passed {
                st.tasks[i].status = if p { St::Done } else { St::Failed };
                st.tasks[i].ended = Some(crate::store::now());
            }
        }
        save(&path, &st);
        if !progressed {
            // the next agent state change (or any event) brings the next look
            loop {
                match watch.next()? {
                    Some(p) if matches!(p.ev, Event::Var { .. } | Event::Exited { .. } | Event::Closed { .. } | Event::Gap { .. }) => break,
                    Some(_) => continue,
                    None => break,
                }
            }
        }
        let _ = watch.set_deadline(Some(Duration::from_secs(600)));
    }
    st.state = if st.tasks.iter().all(|r| r.status == St::Done) { "done".into() } else { "failed".into() };
    save(&path, &st);
    Ok(st
        .tasks
        .iter()
        .map(|r| match r.status {
            St::Done => ok("done", rec_row(r)),
            St::Skipped => ok("skipped", rec_row(r)),
            _ => from_error(&Error::failed(r.summary.clone().unwrap_or_else(|| "failed".into())), rec_row(r)),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(json: &str) -> PlanFile {
        serde_json::from_str(json).unwrap()
    }

    fn recs(p: &PlanFile) -> Vec<Rec> {
        p.tasks.iter().map(|t| Rec { id: t.id.clone(), status: St::Waiting, pane: None, tries: 0, started: None, ended: None, summary: None }).collect()
    }

    #[test]
    fn dag_checks_and_scheduling() {
        let p = plan(r#"{"max": 2, "tasks": [
            {"id": "a", "task": "x"}, {"id": "b", "task": "x"}, {"id": "c", "task": "x"},
            {"id": "d", "task": "x", "deps": ["a", "b"]}, {"id": "e", "task": "x", "deps": ["d"]}]}"#);
        check(&p).unwrap();
        let mut r = recs(&p);
        // fan-out under max
        let (start, skip) = step(&p, &r, 2);
        assert_eq!((start, skip), (vec![0, 1], vec![]));
        r[0].status = St::Running;
        r[1].status = St::Running;
        assert_eq!(step(&p, &r, 2).0, Vec::<usize>::new(), "max reached");
        // fan-in waits for both deps
        r[0].status = St::Done;
        assert_eq!(step(&p, &r, 2).0, vec![2]);
        r[1].status = St::Done;
        r[2].status = St::Running;
        assert_eq!(step(&p, &r, 2).0, vec![3]);
        // a failure skips everything after it
        r[3].status = St::Failed;
        assert_eq!(step(&p, &r, 2).1, vec![4]);
        assert!(check(&plan(r#"{"tasks": [{"id": "a", "task": "x", "deps": ["b"]}, {"id": "b", "task": "x", "deps": ["a"]}]}"#)).is_err());
        assert!(check(&plan(r#"{"tasks": [{"id": "a", "task": "x", "deps": ["z"]}]}"#)).is_err());
        assert!(check(&plan(r#"{"tasks": [{"id": "a", "task": "x"}, {"id": "a", "task": "y"}]}"#)).is_err());
    }
}
