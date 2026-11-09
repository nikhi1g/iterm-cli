//! The worktree protocol end to end on the fake app and a real temporary Git repository: feature names, `agent land`
//! (happy, conflict, busy, open pane, dirty parent, failing --check, --plan), `agent unland`, `agent landed`,
//! `children`'s branch fields and the close rail for work not landed.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use itermkit::app::Id;
use serde_json::json;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository on `main` with one commit (README, a.txt) and the lead's pane in it.
fn setup(tag: &str) -> (World, PathBuf, Id) {
    let w = world(tag);
    let repo = w.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    std::fs::write(repo.join("README"), "r\n").unwrap();
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let lead = w.window();
    w.with(|m, _| m.nodes.get_mut(&lead).unwrap().cwd = repo.display().to_string());
    (w, repo, lead)
}

fn commit(dir: &Path, file: &str, text: &str, msg: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

fn first_parent(repo: &Path) -> Vec<String> {
    git(repo, &["log", "--first-parent", "--format=%s", "main"]).lines().map(str::to_string).collect()
}

fn state(w: &World, pane: Id, st: &str) {
    w.with(|m, _| {
        m.nodes.get_mut(&pane).unwrap().vars.insert("agent.state".into(), st.into());
    });
}

#[test]
fn worktree_agents_are_named_for_their_feature() {
    let (w, repo, lead) = setup("wtnames");
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task 'Fix the login bug in auth.rs' --task '## Add docs\nbody'");
    assert_eq!(code, 0, "{rows:?}");
    let names: Vec<&str> = rows.iter().map(|r| s(r, "name")).collect();
    assert_eq!(names, ["fix-the-login-bug-in", "add-docs"]);
    assert!(repo.join(".worktrees/add-docs").is_dir());
    assert_eq!(git(&repo.join(".worktrees/add-docs"), &["symbolic-ref", "--short", "HEAD"]), "wt/add-docs");
    // the same feature again: unique, never w<N>
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task 'Add docs'");
    assert_eq!(s(&rows[0], "name"), "add-docs-2", "{rows:?}");
    // --name still wins; a worktree with neither a name nor a task is refused with the fix
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --name mine");
    assert_eq!(s(&rows[0], "name"), "mine");
    let (code, rows) = run(w.env(Some(lead)), "pane new --cmd worker --worktree");
    assert_eq!(code, 1, "{rows:?}");
    assert!(s(&rows[0], "reason").contains("--name"), "{rows:?}");
    // agents without a worktree keep w<N>
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --read-only --task 'look around'");
    assert!(s(&rows[0], "name").starts_with('w'), "{rows:?}");
}

#[test]
fn land_merges_each_feature_no_ff_cleans_up_and_unland_reverts_newest_first() {
    let (w, repo, lead) = setup("wtland");
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task 'Fix login' --task 'Add docs'");
    assert_eq!(code, 0, "{rows:?}");
    let (pa, pb) = (pid(&rows[0]), pid(&rows[1]));
    let (wa, wb) = (repo.join(".worktrees/fix-login"), repo.join(".worktrees/add-docs"));
    commit(&wa, "login.rs", "fn login() {}\n", "login: fix");
    commit(&wb, "docs.md", "# docs\n", "docs: add");
    // leftovers are committed by land's checkpoint; ignored files stay behind without blocking the removal
    std::fs::write(wb.join("more.md"), "more\n").unwrap();
    std::fs::create_dir_all(wb.join("target")).unwrap();
    std::fs::write(wb.join("target/x"), "build\n").unwrap();
    // main moved on meanwhile: each feature is rebased onto the tip before it lands
    commit(&repo, "main.txt", "m\n", "main: moved");

    // children: the branch and what is not landed yet
    let (_, rows) = run(w.env(Some(lead)), "children");
    let a = rows.iter().find(|r| s(r, "name") == "fix-login").unwrap();
    assert_eq!((s(a, "branch"), a["ahead"].clone(), a["landed"].clone()), ("wt/fix-login", json!(1), json!(false)), "{rows:?}");

    // the panes are open: --close is the land's --force; nothing changed
    let (code, rows) = run(w.env(Some(lead)), "agent land --all");
    assert_eq!((code, s(&rows[0], "code")), (2, "open"), "{rows:?}");
    assert!(s(&rows[0], "reason").contains("--close"), "{rows:?}");
    // --plan: every agent, in spawn order, nothing written
    let (code, rows) = run(w.env(Some(lead)), "agent land --all --close --plan");
    assert_eq!((code, codes(&rows)), (0, vec!["planned land".to_string(), "planned land".to_string()]), "{rows:?}");
    assert_eq!((s(&rows[0], "agent"), rows[1]["uncommitted"].clone()), ("fix-login", json!(true)));
    assert_eq!(first_parent(&repo).len(), 2);

    let (code, rows) = run(w.env(Some(lead)), "agent land --all --close --check 'test -f README'");
    assert_eq!((code, codes(&rows)), (0, vec!["ok landed".to_string(), "ok landed".to_string()]), "{rows:?}");
    assert_eq!((s(&rows[0], "agent"), rows[0]["commits"].clone(), rows[1]["commits"].clone()), ("fix-login", json!(1), json!(2)));
    assert_eq!(rows[0]["checked"], json!(true));
    // main --first-parent: one merge commit per feature, even for one commit
    assert_eq!(first_parent(&repo), ["land wt/add-docs: Add docs", "land wt/fix-login: Fix login", "main: moved", "init"]);
    let parents = git(&repo, &["log", "--first-parent", "-2", "--format=%P", "main"]);
    assert!(parents.lines().all(|l| l.split(' ').count() == 2), "merges: {parents}");
    let body = git(&repo, &["log", "-1", "--format=%B", "main~1"]);
    let base = git(&repo, &["rev-parse", "main~2"]);
    assert!(body.contains("- ") && body.contains("login: fix"), "{body}");
    assert!(body.contains("Landed-Branch: wt/fix-login\nLanded-Agent: fix-login\nLanded-Base: ") && body.contains(&base), "{body}");
    // cleaned up: panes closed, worktrees and branches gone
    assert!(w.node(pa).is_none() && w.node(pb).is_none());
    assert!(!wa.exists() && !wb.exists());
    assert_eq!(git(&repo, &["branch", "--list", "wt/*"]), "");
    assert!(repo.join("login.rs").exists() && repo.join("more.md").exists());

    // landed: from history alone, newest first
    let (code, rows) = run(w.env(Some(lead)), "agent landed");
    assert_eq!((code, codes(&rows)), (0, vec!["ok landed".to_string(), "ok landed".to_string()]), "{rows:?}");
    assert_eq!((s(&rows[0], "agent"), rows[0]["commits"].clone(), rows[0]["files"].clone(), rows[0]["unlanded"].clone()), ("add-docs", json!(2), json!(2), json!(false)));
    let (_, rows) = run(w.env(Some(lead)), "children");
    let a = rows.iter().find(|r| s(r, "name") == "fix-login").unwrap();
    assert_eq!((a["ahead"].clone(), a["landed"].clone()), (json!(0), json!(true)), "{rows:?}");
    // nothing left to land
    let (code, rows) = run(w.env(Some(lead)), "agent land --all --close");
    assert_eq!((code, codes(&rows)), (0, vec!["ok none".to_string()]), "{rows:?}");

    // unland any set: newest first, one revert each
    let (code, rows) = run(w.env(Some(lead)), "agent unland fix-login add-docs");
    assert_eq!((code, codes(&rows)), (0, vec!["ok unlanded".to_string(), "ok unlanded".to_string()]), "{rows:?}");
    assert_eq!((s(&rows[0], "agent"), s(&rows[1], "agent")), ("add-docs", "fix-login"));
    assert!(!repo.join("login.rs").exists() && !repo.join("docs.md").exists() && repo.join("main.txt").exists());
    let log = first_parent(&repo);
    assert_eq!(&log[..2], ["unland wt/fix-login: Fix login", "unland wt/add-docs: Add docs"]);
    let (_, rows) = run(w.env(Some(lead)), "agent landed");
    assert!(rows.iter().all(|r| r["unlanded"] == json!(true)), "{rows:?}");
    let (_, rows) = run(w.env(Some(lead)), "children");
    assert!(rows.iter().filter(|r| !s(r, "branch").is_empty()).all(|r| r["landed"] == json!(false)), "{rows:?}");
    // twice is refused
    let (code, rows) = run(w.env(Some(lead)), "agent unland fix-login");
    assert_eq!((code, s(&rows[0], "code")), (2, "not-landed"), "{rows:?}");
}

#[test]
fn a_conflict_aborts_and_leaves_everything_as_it_was() {
    let (w, repo, lead) = setup("wtconflict");
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task 'Edit a'");
    let pane = pid(&rows[0]);
    let wt = repo.join(".worktrees/edit-a");
    commit(&wt, "a.txt", "theirs\n", "a: theirs");
    std::fs::write(wt.join("a.txt"), "theirs, more\n").unwrap();
    commit(&repo, "a.txt", "ours\n", "a: ours");
    let (main_before, wt_before) = (git(&repo, &["rev-parse", "main"]), git(&wt, &["rev-parse", "HEAD"]));
    let (code, rows) = run(w.env(Some(lead)), "agent land edit-a --close");
    assert_eq!((code, s(&rows[0], "code")), (2, "conflict"), "{rows:?}");
    assert!(s(&rows[0], "reason").contains("a.txt"), "{rows:?}");
    assert_eq!(git(&repo, &["rev-parse", "main"]), main_before);
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), wt_before, "the leftover commit is undone");
    assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "theirs, more\n", "the leftovers are back, uncommitted");
    assert!(!git(&wt, &["status", "--porcelain"]).is_empty());
    assert!(w.node(pane).is_some(), "the pane stays open");
    assert!(!repo.join(".git/worktrees/edit-a/rebase-merge").exists());
}

#[test]
fn land_refuses_busy_agents_dirty_parents_and_failing_checks_before_writing() {
    let (w, repo, lead) = setup("wtrefuse");
    let (_, rows) = run(w.env(Some(lead)), "agent new --worktree --task 'One' --task 'Two'");
    let (p1, p2) = (pid(&rows[0]), pid(&rows[1]));
    commit(&repo.join(".worktrees/one"), "one.txt", "1\n", "one");
    commit(&repo.join(".worktrees/two"), "two.txt", "2\n", "two");
    let before = git(&repo, &["rev-parse", "main"]);
    // the second agent's turn is still running: neither lands (every refusal is checked first)
    state(&w, p1, "idle");
    state(&w, p2, "busy");
    let (code, rows) = run(w.env(Some(lead)), "agent land --all --close");
    assert_eq!((code, s(&rows[0], "code")), (2, "busy"), "{rows:?}");
    assert_eq!(git(&repo, &["rev-parse", "main"]), before);
    state(&w, p2, "idle");
    // the parent has uncommitted changes to tracked files
    std::fs::write(repo.join("a.txt"), "dirty\n").unwrap();
    let (code, rows) = run(w.env(Some(lead)), "agent land one --close");
    assert_eq!((code, s(&rows[0], "code")), (2, "dirty-parent"), "{rows:?}");
    git(&repo, &["checkout", "--", "a.txt"]);
    // a failing check stops at the first agent: nothing merged, its worktree and pane stay
    let (code, rows) = run(w.env(Some(lead)), "agent land one two --close --check 'echo broke; exit 3'");
    assert_eq!((code, codes(&rows)), (2, vec!["refused check-failed".to_string()]), "{rows:?}");
    assert!(s(&rows[0], "reason").contains("exit 3") && s(&rows[0], "reason").contains("broke"), "{rows:?}");
    assert_eq!(git(&repo, &["rev-parse", "main"]), before);
    assert!(repo.join(".worktrees/one").is_dir() && w.node(p1).is_some());
    // open by default: another pane's caller may land them too (who spawned whom is never a permission)
    let other = w.window();
    let (code, rows) = run(w.env(Some(other)), "agent land one --close --plan");
    assert_eq!((code, codes(&rows)), (0, vec!["planned land".to_string()]), "{rows:?}");
    // explicit names land in spawn order, whatever order they're given in
    let (code, rows) = run(w.env(Some(lead)), "agent land two one --close");
    assert_eq!((code, s(&rows[0], "agent"), s(&rows[1], "agent")), (0, "one", "two"), "{rows:?}");
}

#[test]
fn closing_a_worktree_agent_with_work_not_landed_is_refused() {
    let (w, repo, lead) = setup("wtclose");
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task 'Some feature'");
    let pane = pid(&rows[0]);
    let wt = repo.join(".worktrees/some-feature");
    // nothing done yet: it closes
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task 'Idle feature'");
    let (code, rows) = run(w.env(Some(lead)), &format!("pane close p{}", pid(&rows[0]).0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok closed".to_string()]), "{rows:?}");
    // commits not landed: refused with the command that lands it
    commit(&wt, "f.txt", "f\n", "f1");
    commit(&wt, "g.txt", "g\n", "f2");
    let (code, rows) = run(w.env(Some(lead)), "pane close some-feature");
    assert_eq!((code, s(&rows[0], "code")), (2, "unlanded"), "{rows:?}");
    assert!(s(&rows[0], "reason").starts_with("some-feature has 2 commits not landed: iterm agent land some-feature"), "{rows:?}");
    // uncommitted changes count too
    std::fs::write(wt.join("h.txt"), "h\n").unwrap();
    let (_, rows) = run(w.env(Some(lead)), &format!("pane close p{}", pane.0));
    assert!(s(&rows[0], "reason").contains("2 commits and uncommitted changes"), "{rows:?}");
    // --force closes anyway; the worktree and branch stay for land or wt-remove
    let (code, rows) = run(w.env(Some(lead)), "pane close some-feature --force");
    assert_eq!((code, codes(&rows)), (0, vec!["ok closed".to_string()]), "{rows:?}");
    assert!(w.node(pane).is_none() && wt.is_dir());
    // a closed agent lands from the ledger: no pane, no --close needed
    let (code, rows) = run(w.env(Some(lead)), "agent land some-feature");
    assert_eq!((code, codes(&rows)), (0, vec!["ok landed".to_string()]), "{rows:?}");
    assert_eq!(rows[0]["commits"], json!(3));
    assert_eq!(first_parent(&repo)[0], "land wt/some-feature: Some feature");
    // a Claude agent idle after its turn: `agent close` names the land before the live-agent rail
    let (_, rows) = run(w.env(Some(lead)), "agent new --worktree --task 'Claude work'");
    let claude = pid(&rows[0]);
    state(&w, claude, "idle");
    commit(&repo.join(".worktrees/claude-work"), "c.txt", "c\n", "c");
    let (code, rows) = run(w.env(Some(lead)), "agent close claude-work");
    assert_eq!((code, s(&rows[0], "code")), (2, "unlanded"), "{rows:?}");
    assert!(s(&rows[0], "reason").starts_with("claude-work has 1 commit not landed"), "{rows:?}");
    let (code, rows) = run(w.env(Some(lead)), "agent close claude-work --force");
    assert_eq!((code, codes(&rows)), (0, vec!["ok closed".to_string()]), "{rows:?}");
}

#[test]
fn a_long_brief_is_named_and_landed_by_its_heading() {
    // live run 2026-11-07: a brief opening with a long preamble landed as "land wt/land: You are land, spawned by …",
    // since the pane keeps only the task's first 200 characters
    let (w, repo, lead) = setup("wtbrief");
    let brief = format!("You are the agent. {}\n\n# Make the parser fast\n\nDetails.", "Context. ".repeat(30));
    std::fs::write(w.dir.join("brief.md"), &brief).unwrap();
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --task-file brief.md");
    assert_eq!((code, s(&rows[0], "name")), (0, "make-the-parser-fast"), "{rows:?}");
    commit(&repo.join(".worktrees/make-the-parser-fast"), "p.rs", "fn p() {}\n", "parser: fast");
    let (code, rows) = run(w.env(Some(lead)), "agent land make-the-parser-fast --close");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(first_parent(&repo)[0], "land wt/make-the-parser-fast: Make the parser fast");
}
