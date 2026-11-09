//! Agent verbs the guide names that no other test runs: `agent wait`, `agent close`, `agent wt-remove`, and who
//! may run the worktree verbs. (`agent land|unland|landed` and close-refuses-unlanded are `tests/worktree.rs`'s.)

mod common;

use common::*;
use std::path::{Path, PathBuf};

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repository with one commit, the lead's pane in it, and a worktree agent `fix` with one commit of its own.
fn repo_with_agent(w: &World) -> (itermkit::app::Id, PathBuf, itermkit::app::Id) {
    let repo = w.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let lead = w.window();
    w.with(|m, _| m.nodes.get_mut(&lead).unwrap().cwd = repo.display().to_string());
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --name fix");
    assert_eq!(code, 0, "{rows:?}");
    (lead, repo, pid(&rows[0]))
}

#[test]
fn agent_wait_returns_when_the_agent_is_idle_or_gone() {
    let w = world("agent-wait");
    let lead = w.window();
    let a = pid(&run(w.env(Some(lead)), "agent new --name a1").1[0]);
    let (code, _) = run(w.env(Some(lead)), "agent wait a1 --until idle --timeout 0.2");
    assert_eq!(code, 3, "starting, not idle: a timeout");
    w.with(|m, _| m.nodes.get_mut(&a).unwrap().vars.insert("agent.state".into(), "idle".into()));
    let (code, rows) = run(w.env(Some(lead)), "agent wait a1 --until idle --timeout 2");
    assert_eq!((code, codes(&rows)), (0, vec!["ok until".to_string()]), "{rows:?}");
    let env = w.env(Some(lead));
    let t = std::thread::spawn(move || run(env, "agent wait a1 --until exit --timeout 10"));
    std::thread::sleep(std::time::Duration::from_millis(200));
    w.exit(a, 5);
    let (code, rows) = t.join().unwrap();
    assert_eq!((code, rows[0]["exit"].clone()), (0, serde_json::json!(5)), "{rows:?}");
}

#[test]
fn agent_close_needs_force_for_a_live_agent() {
    let w = world("agent-close");
    let lead = w.window();
    let a = pid(&run(w.env(Some(lead)), "agent new --name a1").1[0]);
    let (code, rows) = run(w.env(Some(lead)), "agent close a1");
    assert_eq!((code, s(&rows[0], "code")), (2, "live-agent"), "{rows:?}");
    assert!(w.node(a).is_some());
    let (code, rows) = run(w.env(Some(lead)), "agent close a1 --plan --force");
    assert_eq!((code, codes(&rows)), (0, vec!["planned close".to_string()]));
    assert!(w.node(a).is_some(), "--plan closes nothing");
    let (code, rows) = run(w.env(Some(lead)), "agent close a1 --force");
    assert_eq!((code, codes(&rows)), (0, vec!["ok closed".to_string()]), "{rows:?}");
    assert!(w.node(a).is_none());
    // a pane that runs no agent is not an agent
    let sh = pid(&run(w.env(Some(lead)), "pane new").1[0]);
    assert_eq!(run(w.env(Some(lead)), &format!("agent close p{}", sh.0)).0, 1);
}

#[test]
fn wt_remove_keeps_unmerged_work_unless_forced() {
    let w = world("agent-wtrm");
    let (lead, repo, a) = repo_with_agent(&w);
    let wt = repo.join(".worktrees/fix");
    // still running there: refused
    let (code, rows) = run(w.env(Some(lead)), "agent wt-remove fix");
    assert_eq!((code, s(&rows[0], "code")), (2, "live"), "{rows:?}");
    w.exit(a, 0);
    // a commit not on main, and an uncommitted file: refused, nothing lost
    std::fs::write(wt.join("a.txt"), "x").unwrap();
    git(&wt, &["add", "a.txt"]);
    git(&wt, &["commit", "-q", "-m", "a"]);
    std::fs::write(wt.join("b.txt"), "y").unwrap();
    let (code, rows) = run(w.env(Some(lead)), "agent wt-remove fix");
    assert_eq!((code, s(&rows[0], "code")), (2, "unmerged"), "{rows:?}");
    assert!(s(&rows[0], "reason").contains("1 commit(s)") && s(&rows[0], "reason").contains("uncommitted"), "{rows:?}");
    assert!(wt.join("b.txt").exists());
    // merged and clean: removed, the branch kept
    std::fs::remove_file(wt.join("b.txt")).unwrap();
    let (code, rows) = run(w.env(Some(lead)), "agent merge fix");
    assert_eq!(code, 0, "{rows:?}");
    let (code, rows) = run(w.env(Some(lead)), "agent wt-remove fix");
    assert_eq!((code, codes(&rows)), (0, vec!["ok removed".to_string()]), "{rows:?}");
    assert!(!wt.exists() && repo.join("a.txt").exists());
    let (code, _) = run(w.env(Some(lead)), "agent wt-remove fix");
    assert_eq!(code, 1, "already gone");
}

#[test]
fn the_human_may_merge_any_worktree_agent() {
    let w = world("agent-open");
    let (_, repo, _) = repo_with_agent(&w);
    let wt = repo.join(".worktrees/fix");
    std::fs::write(wt.join("a.txt"), "x").unwrap();
    git(&wt, &["add", "a.txt"]);
    git(&wt, &["commit", "-q", "-m", "a"]);
    let mut human = w.human();
    human.cwd = repo.clone();
    let (code, rows) = run(human, "agent merge fix");
    assert_eq!((code, codes(&rows)), (0, vec!["ok merged".to_string()]), "{rows:?}");
}
