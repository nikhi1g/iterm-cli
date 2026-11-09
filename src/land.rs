//! The worktree protocol: an orchestrator splits work into features, one agent and one worktree each
//! (`.worktrees/<name>` on `wt/<name>`, named for the feature), and lands each back in one standard way.
//!
//! `agent land NAME…|--all [--check CMD] [--close]`: per agent, in spawn order, checkpoint (commit leftovers,
//! rebase onto the parent branch's tip), run the check, `git merge --no-ff` (always: one merge commit per
//! feature), then remove the worktree and the branch. The merge message is the record:
//!
//! ```text
//! land wt/<name>: <the task's first heading or line>
//!
//! - <sha> <subject>        (the feature's commits, oldest first)
//!
//! Landed-Branch: wt/<name>
//! Landed-Agent: <name>
//! Landed-Base: <the parent branch's tip it landed on>
//! ```
//!
//! so `git log --first-parent` reads one commit per feature, `agent landed` lists them from history alone (no
//! state file), and `agent unland NAME…` reverts any set of them (`git revert -m 1`, newest first), marked
//! `Unlanded-Branch:` / `Unlanded-Merge:`. Closing a worktree agent whose work is not landed is refused.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::agents::{branch_of, conflicts, count, dirty, git, git_ok, git_yes, made_in, WtTarget};
use crate::app::{Op, Snapshot};
use crate::exec::Exec;
use crate::grammar::{Kind, Request};
use crate::out::{from_error, ok, planned, Error, Obj, Result};
use crate::view::{self, View};

/// A feature name's length: short enough for a pane title, a folder and a branch.
pub const NAME_MAX: usize = 24;
/// A merge subject's length after `land wt/<name>: `.
const SUBJECT_MAX: usize = 72;

// ---- names ------------------------------------------------------------------------------------

/// The line a task names its feature by: its first Markdown heading, else its first non-empty line, without
/// list or heading markup.
pub fn feature_line(task: &str) -> String {
    let lines = || task.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines().find(|l| l.starts_with('#')).or_else(|| lines().next()).unwrap_or("");
    let line = line.trim_start_matches('#').trim_start();
    let line = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).unwrap_or(line);
    let line = match line.split_once(". ") {
        Some((n, rest)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => line,
    };
    line.replace(['*', '`'], "").trim().to_string()
}

/// `Fix the login bug in auth.rs` → `fix-the-login-bug-in`: lowercase letters and digits, `-` between words,
/// at most [`NAME_MAX`], cut at a word when one ends late enough; `task` when nothing is left.
pub fn slug(line: &str) -> String {
    let mut s = String::new();
    for c in line.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-');
    if s.len() <= NAME_MAX {
        return if s.is_empty() { "task".into() } else { s.into() };
    }
    let cut = &s[..NAME_MAX];
    let cut = match cut.rfind('-') {
        Some(k) if k >= NAME_MAX / 2 => &cut[..k],
        _ => cut,
    };
    cut.trim_end_matches('-').to_string()
}

/// `base`, else `base-2`, `base-3`, … (each within [`NAME_MAX`]): the first name not taken.
pub fn unique(base: &str, taken: &[String]) -> String {
    let free = |n: &str| !taken.iter().any(|t| t == n);
    if free(base) {
        return base.to_string();
    }
    (2..1000)
        .map(|k| {
            let suffix = format!("-{k}");
            let head = &base[..base.len().min(NAME_MAX - suffix.len())];
            format!("{}{suffix}", head.trim_end_matches('-'))
        })
        .find(|n| free(n))
        .unwrap_or_else(|| base.to_string())
}

/// A worktree agent's name when none is given: its feature (the task's first heading or line), unique among
/// `taken` (which it joins). Never `w<N>`: the name is the branch, and the branch is the history.
pub fn feature_name(task: Option<&str>, taken: &mut Vec<String>) -> Result<String> {
    let task = task.filter(|t| !t.trim().is_empty()).ok_or_else(|| Error::invalid("--worktree needs --name N or a task: the feature names its worktree and branch (wt/<name>)"))?;
    let name = unique(&slug(&feature_line(task)), taken);
    taken.push(name.clone());
    Ok(name)
}

// ---- messages and history ---------------------------------------------------------------------

/// The merge subject: `land wt/fix-login: Fix the login bug` (the feature line cut at a word), or `land wt/fix`.
pub fn land_subject(branch: &str, task: Option<&str>) -> String {
    let line = task.map(feature_line).unwrap_or_default();
    if line.is_empty() {
        return format!("land {branch}");
    }
    let mut cut: String = line.chars().take(SUBJECT_MAX).collect();
    if cut.len() < line.len() {
        if let Some(k) = cut.rfind(' ').filter(|k| *k > SUBJECT_MAX / 2) {
            cut.truncate(k);
        }
        cut.push('…');
    }
    format!("land {branch}: {cut}")
}

/// The whole merge message: subject, the commit list, the trailers.
pub fn land_message(subject: &str, commits: &str, branch: &str, agent: &str, base: &str) -> String {
    format!("{subject}\n\n{commits}\n\nLanded-Branch: {branch}\nLanded-Agent: {agent}\nLanded-Base: {base}\n")
}

/// The revert's message: it names the landing it undoes, so history alone says what is landed.
pub fn unland_message(landing: &Entry) -> String {
    let rest = landing.subject.strip_prefix("land ").unwrap_or(&landing.subject);
    format!(
        "unland {rest}\n\nThis reverts the landing {} (git revert -m 1).\n\nUnlanded-Branch: {}\nUnlanded-Merge: {}\n",
        landing.sha, landing.branch, landing.sha
    )
}

/// One landing or unlanding on a branch's first-parent history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub sha: String,
    pub date: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub branch: String,
    pub agent: String,
    /// An unlanding: the landing (merge) it reverts.
    pub undoes: Option<String>,
    /// Files the commit changed (a merge: against its first parent).
    pub files: usize,
}

const FIELDS: &str = "%x1e%H%x1f%cs%x1f%P%x1f%s%x1f%(trailers:key=Landed-Branch,valueonly)%x1f%(trailers:key=Landed-Agent,valueonly)%x1f%(trailers:key=Unlanded-Branch,valueonly)%x1f%(trailers:key=Unlanded-Merge,valueonly)%x1f";

/// `git log` records (see [`FIELDS`]) as entries, newest first; commits that only mention a trailer in passing are
/// dropped.
pub fn parse_log(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for rec in text.split('\x1e').filter(|r| !r.trim().is_empty()) {
        let f: Vec<&str> = rec.split('\x1f').collect();
        if f.len() < 9 {
            continue;
        }
        let first = |s: &str| s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
        let (landed, unlanded) = (first(f[4]), first(f[6]));
        // the shortstat line: ` 3 files changed, …`
        let files = f[8].split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let e = Entry {
            sha: f[0].trim().to_string(),
            date: f[1].trim().to_string(),
            parents: f[2].split_whitespace().map(str::to_string).collect(),
            subject: f[3].trim().to_string(),
            branch: if landed.is_empty() { unlanded.clone() } else { landed.clone() },
            agent: first(f[5]),
            undoes: if landed.is_empty() { Some(first(f[7])).filter(|m| !m.is_empty()) } else { None },
            files,
        };
        // a landing is a merge; an unlanding names its merge
        if (!landed.is_empty() && e.parents.len() == 2) || e.undoes.is_some() {
            out.push(e);
        }
    }
    out
}

/// Every landing and unlanding on `onto`'s first-parent history, newest first: one `git log`.
pub fn history(top: &Path, onto: &str) -> Result<Vec<Entry>> {
    let format = format!("--format={FIELDS}");
    let text = git_ok(top, &["log", "--first-parent", "--diff-merges=first-parent", "--shortstat", "-E", "--grep=^(Landed|Unlanded)-Branch: ", &format, onto, "--"])?;
    Ok(parse_log(&text))
}

/// The newest landing of `branch` that no later unlanding undid.
pub fn current_landing<'a>(entries: &'a [Entry], branch: &str) -> Option<&'a Entry> {
    let newest = entries.iter().find(|e| e.branch == branch)?;
    newest.undoes.is_none().then_some(newest)
}

/// Run `cmd` with `sh -c` in `dir`: its exit code and its last non-empty output line.
fn check(dir: &Path, cmd: &str) -> Result<(i32, String)> {
    let out = Command::new("/bin/sh").arg("-c").arg(cmd).current_dir(dir).stdin(Stdio::null()).output().map_err(|e| Error::failed(format!("--check: {e}")))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let last: String = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").chars().take(160).collect();
    Ok((out.status.code().unwrap_or(-1), last))
}

/// The agent's turn is still running (or it waits on the human): landing now would take half its work.
fn busy(s: &Snapshot, i: usize) -> Option<&str> {
    let n = &s.nodes[i];
    let agent = view::is_agent_kind(n.var(view::V_KIND)) || n.var(view::V_PROVIDER).is_some();
    let st = n.var(view::V_STATE)?;
    let live = n.pane.as_ref().is_some_and(|p| p.exit.is_none());
    (agent && live && matches!(st, "busy" | "starting" | "prompt")).then_some(st)
}

/// One worktree agent to land, checked before anything is written.
struct Landing {
    t: WtTarget,
    pane: Option<crate::app::Id>,
    task: Option<String>,
}

impl Exec<'_> {
    /// Names a new feature may not take: live agents', my children's, and every `wt/*` branch and `.worktrees/*`
    /// folder of `repo` (one `git for-each-ref`).
    pub(crate) fn names_taken(&self, s: &Snapshot, repo: &Path) -> Vec<String> {
        let mut taken: Vec<String> = s.nodes.iter().filter_map(|n| n.var(view::V_NAME).map(str::to_string)).collect();
        taken.extend(self.ledger(&self.parent_key()).into_iter().filter_map(|x| x.name));
        if let Ok(refs) = git_ok(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/wt/"]) {
            taken.extend(refs.lines().filter_map(|b| b.strip_prefix("wt/")).map(str::to_string));
        }
        if let Ok(d) = std::fs::read_dir(repo.join(".worktrees")) {
            taken.extend(d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()));
        }
        taken
    }

    /// `agent land NAME…|--all [--check CMD] [--close] [--plan]`. Every refusal is checked for every agent before
    /// the first is landed; then each lands in spawn order, stopping at the first failure (its row says why).
    pub(crate) fn land(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let usage = "agent land NAME…|--all [--check CMD] [--close] [--plan]";
        let close = r.p.has("--close");
        let check_cmd = r.p.one("--check").map(str::to_string);
        if check_cmd.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return Err(Error::invalid("--check CMD: a command to run in each worktree"));
        }
        let recs = self.ledger(&self.parent_key());
        // spawn order: first record of each name
        let mut order: Vec<String> = Vec::new();
        for rec in recs.iter().filter(|x| x.worktree.is_some()) {
            if let Some(n) = &rec.name {
                if !order.contains(n) {
                    order.push(n.clone());
                }
            }
        }
        let mut names: Vec<String> = match (r.all, r.words.is_empty()) {
            (true, true) => order.clone(),
            (false, false) => r.words.clone(),
            _ => return Err(Error::invalid(usage)),
        };
        names.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
        names.dedup();
        let mut todo = Vec::new();
        for name in &names {
            let (t, s, v) = self.wt_target(name)?;
            let branch_exists = git_yes(&t.parent_top, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", t.branch)])?;
            if !t.exists && !branch_exists {
                if r.all {
                    continue; // landed (or removed) already
                }
                return Err(Error::policy("gone", format!("{name}: neither its worktree nor {} exists; landed already? iterm agent landed", t.branch)));
            }
            if let Some(st) = t.live.and_then(|i| busy(&s, i)) {
                return Err(Error::policy("busy", format!("{name} is {st}: wait for its turn to end (iterm agent wait {name} --until idle), then land")));
            }
            let pane = t.live.filter(|i| s.nodes[*i].kind == crate::app::Kind::Pane).map(|i| v.id(i));
            if let (Some(p), false) = (pane, close) {
                return Err(Error::policy("open", format!("{name}'s pane {} is open: land it with --close (closes the pane once landed)", view::pane_handle(p))));
            }
            if check_cmd.is_some() && !t.exists {
                return Err(Error::policy("no-worktree", format!("{name} has no worktree to run --check in (paused?): agent resume {name}, or land without --check")));
            }
            if dirty(&t.parent_top, false)? {
                return Err(Error::policy("dirty-parent", format!("{} has uncommitted changes: commit or stash them, then land", t.parent_top.display())));
            }
            // the ledger's whole task first: the pane's agent.task holds only its first 200 characters, which may end
            // before the heading the feature is named by
            let task = t.rec.as_ref().and_then(|x| x.task.clone()).or_else(|| t.live.and_then(|i| s.nodes[i].var(view::V_TASK).map(str::to_string)));
            todo.push(Landing { t, pane, task });
        }
        if todo.is_empty() {
            return Ok(vec![ok("none", crate::obj! {"reason" => "no worktree agents of yours left to land"})]);
        }
        if !r.apply {
            let mut rows = Vec::new();
            for l in &todo {
                let t = &l.t;
                let ahead = count(&t.parent_top, &format!("{}..{}", t.onto, t.branch)).unwrap_or(0);
                let uncommitted = t.exists && dirty(&t.worktree, true)?;
                rows.push(planned("land", crate::obj! {"agent" => t.name, "branch" => t.branch, "onto" => t.onto, "ahead" => ahead, "uncommitted" => uncommitted, "close" => l.pane.map(view::pane_handle), "check" => check_cmd}));
            }
            return Ok(rows);
        }
        let mut rows = Vec::new();
        for l in &todo {
            match self.land_one(l, check_cmd.as_deref()) {
                Ok(row) => rows.push(row),
                Err(e) => {
                    rows.push(from_error(&e, crate::obj! {"agent" => l.t.name}));
                    break;
                }
            }
        }
        Ok(rows)
    }

    fn land_one(&mut self, l: &Landing, check_cmd: Option<&str>) -> Result<Obj> {
        let t = &l.t;
        let (top, onto, branch) = (&t.parent_top, t.onto.as_str(), t.branch.as_str());
        let mut row = crate::obj! {"agent" => t.name, "branch" => branch, "into" => onto};
        if t.exists {
            // leftovers committed, rebased onto the tip: the feature's commits sit on top of what it lands on
            self.checkpoint(&t.worktree, onto, &t.name, None, Obj::new(), true)?;
        }
        if let Some(cmd) = check_cmd {
            let (code, last) = check(&t.worktree, cmd)?;
            if code != 0 {
                return Err(Error::policy("check-failed", format!("--check {cmd:?} failed in {} (exit {code}): {last}; nothing landed", t.worktree.display())));
            }
            row.insert("checked".into(), json!(true));
        }
        let commits = git_ok(top, &["log", "--reverse", "--format=- %h %s", &format!("{onto}..{branch}")])?;
        let n = commits.lines().count();
        row.insert("commits".into(), json!(n));
        if n > 0 {
            let base = git_ok(top, &["rev-parse", onto])?;
            let msg = land_message(&land_subject(branch, l.task.as_deref()), &commits, branch, &t.name, &base);
            if !git(top, &["merge", "--no-ff", "-q", "-m", &msg, branch])?.status.success() {
                let files = conflicts(top);
                git(top, &["merge", "--abort"])?;
                return Err(Error::policy("conflict", format!("merging {branch} into {onto} conflicts in {files}; merge aborted, nothing changed: iterm agent checkpoint {}, then land again", t.name)));
            }
            row.insert("commit".into(), json!(git_ok(top, &["rev-parse", "--short", "HEAD"])?));
            row.insert("files".into(), json!(git_ok(top, &["diff", "--name-only", "HEAD^1", "HEAD"])?.lines().count()));
        }
        // landed (or nothing to land): the pane, the worktree and the branch go
        if let Some(p) = l.pane {
            self.conn.call(Op::Close { ids: vec![p] })?;
            row.insert("closed".into(), json!(view::pane_handle(p)));
        }
        if t.exists {
            let out = git(top, &["worktree", "remove", &t.worktree.display().to_string()])?;
            if !out.status.success() {
                let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
                return Err(Error::policy("cleanup", format!("{branch} landed, but its worktree stays: {why}; free it with iterm agent wt-remove {} --force", t.name)));
            }
        }
        git_ok(top, &["branch", "-q", "-d", branch])?;
        Ok(ok(if n > 0 { "landed" } else { "empty" }, row))
    }

    /// The checkout `agent unland|landed` work on when a name is not one of my worktree children: my pane's folder,
    /// else the CLI's.
    fn here_top(&mut self) -> Result<(PathBuf, String)> {
        let (s, v) = self.view()?;
        let cwd = v
            .m
            .me
            .and_then(|m| s.nodes[m].pane.as_ref())
            .map(|p| p.cwd.clone())
            .filter(|c| !c.is_empty() && Path::new(c).is_dir())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.conn.env().cwd.clone());
        let top = crate::exec::git_top(&cwd).ok_or_else(|| Error::policy("not-git", format!("{} is not in a Git repository", cwd.display())))?;
        let onto = branch_of(&top).ok_or_else(|| Error::policy("detached", format!("{} has no branch checked out", top.display())))?;
        Ok((top, onto))
    }

    /// Where NAME landed: the checkout its worktree was made in (from my ledger), else mine.
    fn land_repo(&mut self, name: &str) -> Result<(PathBuf, String)> {
        let rec = self.ledger(&self.parent_key()).into_iter().rev().find(|x| x.name.as_deref() == Some(name) && x.worktree.is_some());
        if let Some(top) = rec.and_then(|x| x.worktree).and_then(|w| made_in(Path::new(&w)).map(Path::to_path_buf)).filter(|t| t.is_dir()) {
            if let Some(onto) = branch_of(&top) {
                return Ok((top, onto));
            }
        }
        self.here_top()
    }

    /// `agent unland NAME…`: revert each one's landing (`git revert -m 1`), newest first; one commit each.
    pub(crate) fn unland(&mut self, r: &Request) -> Result<Vec<Obj>> {
        if r.words.is_empty() {
            return Err(Error::invalid("agent unland NAME… (the landings to revert)"));
        }
        let mut cache: HashMap<(PathBuf, String), Vec<Entry>> = HashMap::new();
        let mut todo: Vec<(usize, PathBuf, String, Entry)> = Vec::new();
        for name in &r.words {
            let (top, onto) = self.land_repo(name)?;
            let entries = match cache.get(&(top.clone(), onto.clone())) {
                Some(e) => e.clone(),
                None => {
                    let e = history(&top, &onto)?;
                    cache.insert((top.clone(), onto.clone()), e.clone());
                    e
                }
            };
            let branch = entries.iter().find(|e| e.agent == *name || e.branch == format!("wt/{name}")).map(|e| e.branch.clone());
            let landing = branch.as_deref().and_then(|b| current_landing(&entries, b).map(|l| (b, l)));
            let Some((_, landing)) = landing else {
                return Err(Error::policy("not-landed", format!("{name} is not landed on {onto} (see iterm agent landed)")));
            };
            let pos = entries.iter().position(|e| e.sha == landing.sha).unwrap_or(0);
            todo.push((pos, top, onto, landing.clone()));
        }
        // newest first: a later landing may build on an earlier one
        todo.sort_by_key(|x| x.0);
        todo.dedup_by(|a, b| a.3.sha == b.3.sha);
        for (_, top, _, _) in &todo {
            if dirty(top, false)? {
                return Err(Error::policy("dirty-parent", format!("{} has uncommitted changes: commit or stash them, then unland", top.display())));
            }
        }
        if !r.apply {
            return Ok(todo.iter().map(|(_, _, onto, e)| planned("unland", crate::obj! {"agent" => e.agent, "branch" => e.branch, "merge" => short(&e.sha), "on" => onto})).collect());
        }
        let mut rows = Vec::new();
        for (_, top, onto, e) in &todo {
            let res = (|| -> Result<Obj> {
                if !git(top, &["revert", "-m", "1", "--no-commit", &e.sha])?.status.success() {
                    let files = conflicts(top);
                    git(top, &["revert", "--abort"])?;
                    return Err(Error::policy("conflict", format!("reverting {}'s landing {} conflicts in {files}; aborted, nothing changed: unland the later landings that touch them first", e.agent, short(&e.sha))));
                }
                if git_yes(top, &["diff", "--cached", "--quiet"])? {
                    git(top, &["revert", "--abort"])?;
                    return Err(Error::policy("nothing", format!("{}'s landing {} is undone already on {onto}", e.agent, short(&e.sha))));
                }
                git_ok(top, &["commit", "-q", "-m", &unland_message(e)])?;
                let commit = git_ok(top, &["rev-parse", "--short", "HEAD"])?;
                Ok(ok("unlanded", crate::obj! {"agent" => e.agent, "branch" => e.branch, "merge" => short(&e.sha), "commit" => commit, "on" => onto}))
            })();
            match res {
                Ok(row) => rows.push(row),
                Err(err) => {
                    rows.push(from_error(&err, crate::obj! {"agent" => e.agent}));
                    break;
                }
            }
        }
        Ok(rows)
    }

    /// `agent landed`: one line per landing on my branch, newest first (from history alone).
    pub(crate) fn landed(&mut self) -> Result<Vec<Obj>> {
        let (top, onto) = self.here_top()?;
        let entries = history(&top, &onto)?;
        let undone: Vec<&str> = entries.iter().filter_map(|e| e.undoes.as_deref()).collect();
        let mut rows = Vec::new();
        for e in entries.iter().filter(|e| e.undoes.is_none()) {
            let commits = count(&top, &format!("{}..{}", e.parents[0], e.parents[1])).unwrap_or(0);
            rows.push(ok(
                "landed",
                crate::obj! {"agent" => e.agent, "branch" => e.branch, "commit" => short(&e.sha), "date" => e.date, "commits" => commits, "files" => e.files,
                "unlanded" => undone.contains(&e.sha.as_str()), "on" => onto},
            ));
        }
        Ok(rows)
    }

    /// `children`'s worktree fields: `branch=wt/x ahead=N landed=yes|no`. Per checkout one `git for-each-ref` (every
    /// `wt/*` branch's commits ahead) and, only when some branch has none ahead, one history read; HEADs are read
    /// from their files.
    pub(crate) fn branch_fields(&self, rows: &mut [Obj]) {
        struct Top {
            onto: String,
            ahead: HashMap<String, i64>,
            entries: Option<Vec<Entry>>,
        }
        let mut tops: HashMap<PathBuf, Option<Top>> = HashMap::new();
        for row in rows.iter_mut() {
            let Some(wt) = row.get("worktree").and_then(Value::as_str).map(PathBuf::from) else { continue };
            let Some(top) = made_in(&wt).map(Path::to_path_buf).filter(|t| t.is_dir()) else { continue };
            let known = tops.entry(top.clone()).or_insert_with(|| {
                let onto = head_branch(&top)?;
                let refs = git_ok(&top, &["for-each-ref", &format!("--format=%(refname:short) %(ahead-behind:{onto})"), "refs/heads/wt/"]).ok()?;
                let ahead = refs.lines().filter_map(|l| l.split_whitespace().collect::<Vec<_>>().get(..2).map(|x| (x[0].to_string(), x[1].parse().unwrap_or(0)))).collect();
                Some(Top { onto, ahead, entries: None })
            });
            let Some(known) = known.as_mut() else { continue };
            let name = row.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let branch = head_branch(&wt).unwrap_or_else(|| format!("wt/{name}"));
            // a branch gone (landed, or removed) has nothing ahead
            let ahead = known.ahead.get(&branch).copied().unwrap_or(0);
            let landed = ahead == 0 && {
                let entries = known.entries.get_or_insert_with(|| history(&top, &known.onto).unwrap_or_default());
                current_landing(entries, &branch).is_some()
            };
            row.insert("branch".into(), json!(branch));
            row.insert("ahead".into(), json!(ahead));
            row.insert("landed".into(), json!(landed));
        }
    }

    /// The close path's worktree rail: a worktree agent whose branch has commits (or changes) not on its parent's
    /// branch is not closed without `--force`; `agent land` is how its work goes home.
    pub(crate) fn refuse_unlanded(&self, s: &Snapshot, v: &View, nodes: &[usize], force: bool) -> Result<()> {
        if force {
            return Ok(());
        }
        for n in nodes {
            for p in v.m.project(*n, Some(Kind::Pane)) {
                let Some(wt) = s.nodes[p].var(view::V_WORKTREE).map(PathBuf::from).filter(|w| w.is_dir()) else { continue };
                let Some(top) = made_in(&wt).filter(|t| t.is_dir()) else { continue };
                let (Some(onto), Some(branch)) = (head_branch(top), head_branch(&wt)) else { continue };
                if onto == branch {
                    continue;
                }
                let ahead = count(top, &format!("{onto}..{branch}")).unwrap_or(0);
                let changed = dirty(&wt, true).unwrap_or(false);
                if ahead > 0 || changed {
                    let name = s.nodes[p].var(view::V_NAME).map(str::to_string).unwrap_or_else(|| v.handle(p).to_string());
                    let what = match (ahead, changed) {
                        (0, _) => "uncommitted changes".to_string(),
                        (k, false) => format!("{k} commit{}", if k == 1 { "" } else { "s" }),
                        (k, true) => format!("{k} commit{} and uncommitted changes", if k == 1 { "" } else { "s" }),
                    };
                    return Err(Error::policy("unlanded", format!("{name} has {what} not landed: iterm agent land {name} --close (or --force)")));
                }
            }
        }
        Ok(())
    }
}

/// The branch a checkout (or a worktree) has out, read from its HEAD file: no process. `None` when detached.
pub fn head_branch(dir: &Path) -> Option<String> {
    let dot = dir.join(".git");
    let git_dir = if dot.is_dir() {
        dot
    } else {
        let text = std::fs::read_to_string(&dot).ok()?;
        let d = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if d.is_absolute() { d } else { dir.join(d) }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_feature() {
        assert_eq!(feature_line("# Fix the login bug\nbody"), "Fix the login bug");
        assert_eq!(feature_line("intro line\n## Dock layout\nmore"), "Dock layout");
        assert_eq!(feature_line("\n\n- **Add** `land` verb\n"), "Add land verb");
        assert_eq!(feature_line("3. Third thing"), "Third thing");
        assert_eq!(slug("Fix the login bug in auth.rs"), "fix-the-login-bug-in");
        assert_eq!(slug("Dock layout"), "dock-layout");
        assert_eq!(slug("ÜÜÜ"), "task");
        assert_eq!(slug("abcdefghijklmnopqrstuvwxyz0123"), "abcdefghijklmnopqrstuvwx");
        assert!(slug("a very long feature name that goes on and on").len() <= NAME_MAX);
        let mut taken = vec!["dock-layout".to_string()];
        assert_eq!(feature_name(Some("Dock layout"), &mut taken).unwrap(), "dock-layout-2");
        assert_eq!(feature_name(Some("Dock layout"), &mut taken).unwrap(), "dock-layout-3");
        assert_eq!(unique("abcdefghijklmnopqrstuvwx", &["abcdefghijklmnopqrstuvwx".into()]), "abcdefghijklmnopqrstuv-2");
        assert!(feature_name(None, &mut taken).unwrap_err().message().contains("--name"));
        for t in ["Fix it", "a b c d e f g h i j k l m n o p q r s t u v w x y z", "--- !!", "## 12. Ünïcode"] {
            let n = slug(&feature_line(t));
            assert!(crate::grammar::is_slug(&n) && n.len() <= NAME_MAX, "{t:?} → {n}");
        }
    }

    #[test]
    fn the_message_carries_the_record() {
        assert_eq!(land_subject("wt/fix", None), "land wt/fix");
        assert_eq!(land_subject("wt/fix", Some("# Fix login\nmore")), "land wt/fix: Fix login");
        let long = "word ".repeat(30);
        let s = land_subject("wt/x", Some(&long));
        assert!(s.ends_with('…') && s.chars().count() <= "land wt/x: ".len() + SUBJECT_MAX + 1, "{s}");
        let m = land_message("land wt/fix: Fix", "- abc one\n- def two", "wt/fix", "fix", "0123");
        assert!(m.ends_with("Landed-Branch: wt/fix\nLanded-Agent: fix\nLanded-Base: 0123\n"), "{m}");
    }

    #[test]
    fn history_parses_landings_and_unlandings() {
        let rec = |sha: &str, parents: &str, subj: &str, lb: &str, la: &str, ub: &str, um: &str, stat: &str| format!("\x1e{sha}\x1f2026-11-07\x1f{parents}\x1f{subj}\x1f{lb}\x1f{la}\x1f{ub}\x1f{um}\x1f\n{stat}\n");
        let text = [
            rec("ccc", "bbb", "unland wt/a: A", "", "", "wt/a\n", "aaa\n", " 1 file changed, 1 deletion(-)"),
            rec("bbb", "aaa x2", "land wt/b: B", "wt/b\n", "b\n", "", "", " 2 files changed, 2 insertions(+)"),
            rec("aaa", "000 x1", "land wt/a: A", "wt/a\n", "a\n", "", "", " 1 file changed, 1 insertion(+)"),
            // a plain commit quoting a trailer is not a landing
            rec("ddd", "000", "docs", "wt/z\n", "", "", "", ""),
        ]
        .concat();
        let e = parse_log(&text);
        assert_eq!(e.len(), 3, "{e:?}");
        assert_eq!((e[1].branch.as_str(), e[1].agent.as_str(), e[1].files), ("wt/b", "b", 2));
        assert_eq!(e[0].undoes.as_deref(), Some("aaa"));
        assert!(current_landing(&e, "wt/a").is_none(), "unlanded");
        assert_eq!(current_landing(&e, "wt/b").map(|x| x.sha.as_str()), Some("bbb"));
        let u = unland_message(&e[2]);
        assert!(u.starts_with("unland wt/a: A\n") && u.contains("Unlanded-Merge: aaa\n"), "{u}");
    }
}
