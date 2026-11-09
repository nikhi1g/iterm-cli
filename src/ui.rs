//! The app's own UI from the CLI: `ui list|get|watch|wait|answer` (open dialogs and panels; answer clicks a
//! button), `app menu [ITEM] [--context SEL]` (the main menu or a node's right-click menu, listed or run), and
//! `app launch` (start iTerm2 when it isn't running: `open -g -a`, a clean environment, then wait for its API).
//!
//! Rail: a permission or folder-trust prompt inside an agent's pane is pane content, not app UI: nothing here
//! reaches it (`ui` sees only the app's own windows).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::app::{Event, Op};

use crate::conn::Conn;
use crate::exec::Exec;
use crate::grammar::{self, Kind, Noun, Request, Verb};
use crate::out::{ok, planned, Error, Obj, Result};

/// One "id\tkind\ttitle\tbutton|button…" line as a row.
pub fn row(line: &str) -> Obj {
    let mut f = line.splitn(4, '\t');
    let (id, kind, title, buttons) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
    let buttons: Vec<&str> = buttons.split('|').filter(|b| !b.is_empty()).collect();
    ok("ui", crate::obj! {"id" => id, "kind" => kind, "title" => title, "buttons" => buttons})
}

fn list(x: &mut Exec<'_>) -> Result<Vec<String>> {
    Ok(x.conn.call(Op::UiList)?.lines)
}

fn ids(lines: &[String]) -> Vec<&str> {
    lines.iter().map(|l| l.split('\t').next().unwrap_or("")).collect()
}

pub fn run(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    match (r.noun, r.verb) {
        (Noun::App, Verb::Launch) => launch(x, r),
        (Noun::App, Verb::Menu) => menu(x, r),
        (_, Verb::List) => Ok(list(x)?.iter().map(|l| row(l)).collect()),
        (_, Verb::Get) => {
            let want = r.words.first().ok_or_else(|| Error::invalid("ui get ID (ui list)"))?;
            let rows: Vec<Obj> = list(x)?.iter().filter(|l| l.split('\t').next() == Some(want.as_str())).map(|l| row(l)).collect();
            if rows.is_empty() {
                return Err(Error::invalid(format!("no dialog {want} (ui list)")));
            }
            Ok(rows)
        }
        (_, Verb::Answer) => {
            let id: i64 = r.words.first().and_then(|w| w.parse().ok()).ok_or_else(|| Error::invalid("ui answer ID --button B (ui list)"))?;
            let button = r.p.one("--button").or(r.words.get(1).map(String::as_str)).ok_or_else(|| Error::invalid("ui answer ID --button B"))?;
            if !r.apply {
                return Ok(vec![planned("answer", crate::obj! {"id" => id, "button" => button})]);
            }
            let d = x.conn.call(Op::UiAnswer { id, button: button.to_string() })?;
            Ok(vec![ok("answered", crate::obj! {"id" => id, "button" => d.lines.first()})])
        }
        (_, Verb::Watch | Verb::Wait) => watch(x, r),
        // the app's own panels, as their keys open them (the key map's `settings` and `help` actions)
        (_, Verb::Open) => {
            let what = r.words.first().map(String::as_str).unwrap_or("");
            if !matches!(what, "settings" | "help") {
                return Err(Error::invalid("ui open settings|help"));
            }
            if !r.apply {
                return Ok(vec![planned("open", crate::obj! {"ui" => what})]);
            }
            x.conn.call(Op::Action { name: what.into(), pane: None })?;
            Ok(vec![ok("opened", crate::obj! {"ui" => what})])
        }
        (n, vb) => Err(Error::invalid(format!("{n} {vb} is not a ui operation"))),
    }
}

/// `ui watch` streams `opened`/`closed` rows as dialogs come and go; `ui wait [--until open|closed|change]`
/// returns at the first that holds (default: open, a dialog is showing). Event-driven: the app pushes a `ui` event
/// as its windows show, hide, close or take the keyboard, and as the find bar opens or closes; the list
/// is read again only then. No CPU while nothing changes.
fn watch(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    let until = r.p.one("--until").unwrap_or(if r.verb == Verb::Wait { "open" } else { "" });
    if !["", "open", "closed", "change"].contains(&until) {
        return Err(Error::invalid("ui wait --until open|closed|change"));
    }
    let timeout = crate::watch::timeout_of(r)?;
    let end = timeout.map(|t| Instant::now() + t);
    // subscribe first, then list: a dialog that opens in between is an event, never missed
    let (mut w, _) = x.conn.watch(None, timeout)?;
    let mut last = list(x)?;
    let holds = |now: &[String], changed: bool| match until {
        "open" => !now.is_empty(),
        "closed" => now.is_empty(),
        "change" => changed,
        _ => false,
    };
    if until != "change" && holds(&last, false) {
        return Ok(last.iter().map(|l| row(l)).chain(last.is_empty().then(|| ok("closed", crate::obj! {}))).collect());
    }
    loop {
        let left = end.map(|e| e.saturating_duration_since(Instant::now()));
        if left.is_some_and(|l| l.is_zero()) {
            return Ok(vec![crate::out::from_error(&Error::Timeout(format!("ui: --until {until} did not hold in time")), crate::obj! {})]);
        }
        w.set_deadline(left)?;
        match w.next()? {
            Some(p) if matches!(p.ev, Event::Ui { .. } | Event::Gap { .. }) => {}
            _ => continue,
        }
        let now = list(x)?;
        let (was, is) = (ids(&last), ids(&now));
        let changed = was != is;
        let mut rows = Vec::new();
        for l in &now {
            if !was.contains(&l.split('\t').next().unwrap_or("")) {
                let mut o = row(l);
                o.insert("code".into(), "opened".into());
                rows.push(o);
            }
        }
        for id in was.iter().filter(|w| !is.contains(w)) {
            rows.push(ok("closed", crate::obj! {"id" => *id}));
        }
        if until.is_empty() {
            for o in &rows {
                (x.sink)(o);
            }
        } else if holds(&now, changed) {
            return Ok(if rows.is_empty() { now.iter().map(|l| row(l)).collect() } else { rows });
        }
        last = now;
    }
}

/// `app menu [ITEM] [--context SEL | --context pane|tab|vtab SEL]`.
fn menu(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    let mut words = r.words.clone();
    let context = match r.p.one("--context") {
        None => None,
        Some(k @ ("pane" | "tab" | "vtab")) => {
            let sel = words.pop().ok_or_else(|| Error::invalid(format!("--context {k} SEL")))?;
            let kind = Kind::parse(k).unwrap_or(Kind::Pane);
            Some((sel, kind))
        }
        Some(sel) => Some((sel.to_string(), Kind::Pane)),
    };
    let context = match context {
        None => None,
        Some((sel, kind)) => {
            let (_, v) = x.view()?;
            let e = grammar::selector(&sel).map_err(|e| Error::invalid(e.to_string()))?;
            // a bare `--context t3` names its own kind; `--context SEL` alone defaults to a pane
            let kind = crate::app::model::parse_handle(&sel).and_then(|(k, _)| k).map(crate::view::kind_of).unwrap_or(kind);
            Some(v.id(x.one_node(&v, &e, kind, "--context")?))
        }
    };
    let item = (!words.is_empty()).then(|| words.join(" "));
    if let Some(i) = &item {
        // the quit rail: `app quit` checks for live agents
        if i.to_lowercase().contains("quit") && !r.force {
            return Err(Error::policy("quit", "quit with `iterm app quit` (it checks for live agents), or pass --force"));
        }
        if !r.apply {
            return Ok(vec![planned("menu", crate::obj! {"item" => i})]);
        }
    }
    let d = x.conn.call(Op::Menu { context, item: item.clone() })?;
    if item.is_some() {
        return Ok(vec![ok("chosen", crate::obj! {"item" => d.lines.first()})]);
    }
    Ok(d.lines
        .iter()
        .map(|l| menu_row(l))
        .collect())
}

/// One "path\tkey[\tstate]" menu line as a row (state: checked, disabled, missing in this iTerm2, or empty).
pub fn menu_row(l: &str) -> Obj {
    let mut f = l.splitn(3, '\t');
    let (path, key, state) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
    let mut o = crate::obj! {"item" => path, "key" => key};
    if !state.is_empty() {
        o.insert("state".into(), state.into());
    }
    ok("item", o)
}

/// What `app launch` starts: `--app-path`, else `$ITERM_APP_PATH`, else stock iTerm2 (`/Applications/iTerm.app`).
pub fn app_path(given: Option<&str>) -> PathBuf {
    given
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("ITERM_APP_PATH").filter(|p| !p.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/Applications/iTerm.app"))
}

/// The environment a launched app gets: the login basics and iTerm2's own settings, never the caller's
/// pane identity or an agent's markers.
pub fn clean_env(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const KEEP: [&str; 8] = ["HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"];
    let mut out: Vec<(String, String)> = vars
        .filter(|(k, _)| KEEP.contains(&k.as_str()) || (k == "ITERM_STATE"))
        .collect();
    out.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
    out.sort();
    out
}

fn launch(x: &mut Exec<'_>, r: &Request) -> Result<Vec<Obj>> {
    let socket = x.conn.env().socket.clone();
    if let Ok(s) = x.conn.snapshot() {
        return Ok(vec![ok("running", crate::obj! {"pid" => s.app_pid, "socket" => socket.display().to_string()})]);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let app = app_path(r.p.one("--app-path"));
    if !app.exists() {
        return Err(Error::invalid(format!("no app at {} (--app-path P)", app.display())));
    }
    if !r.apply {
        return Ok(vec![planned("launch", crate::obj! {"app" => app.display().to_string()})]);
    }
    let env = clean_env(std::env::vars());
    let mut cmd = if app.extension().is_some_and(|e| e == "app") {
        // LaunchServices starts the bundle with launchd's environment (never the caller's: no agent session
        // variables leak into its panes), in the background, and never a second instance
        let mut c = std::process::Command::new("/usr/bin/open");
        c.arg("-g").arg("-a").arg(&app);
        c.env_clear().envs(env.iter().cloned());
        c
    } else {
        let mut c = std::process::Command::new(&app);
        c.env_clear().envs(env.iter().cloned());
        // its own session: the app outlives this CLI and the caller's terminal
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut c, || {
                libc::setsid();
                Ok(())
            });
        }
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    cmd.current_dir(&home);
    let child = cmd.spawn().map_err(|e| Error::failed(format!("{}: {e}", app.display())))?;
    drop(child);
    let wait = r.p.duration("--ready-timeout").map_err(Error::invalid)?.unwrap_or(Duration::from_secs(10));
    let end = Instant::now() + wait;
    let base = x.conn.env().clone();
    loop {
        // the token appears with the socket: read it again each try
        let env = base.clone();
        let mut c = Conn::new(env);
        if let Ok(s) = c.snapshot() {
            let pid = s.app_pid;
            x.conn = c;
            return Ok(vec![ok("launched", crate::obj! {"pid" => pid, "app" => app.display().to_string(), "socket" => socket.display().to_string()})]);
        }
        if Instant::now() >= end {
            return Err(Error::Timeout(format!("the app did not answer on {} in {} s", socket.display(), wait.as_secs_f32())));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_paths_and_env() {
        let r = row("812\talert\tClose and end the running job?\tClose|Cancel");
        assert_eq!(crate::out::line(&r), "ok ui id=812 kind=alert title=\"Close and end the running job?\" buttons=Close,Cancel");
        let e = clean_env(
            [("HOME", "/h"), ("ITERM_SESSION_ID", "w0t0p0:X"), ("ITERM_STATE", "/s"), ("CLAUDECODE", "1"), ("PATH", "/opt/x")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let keys: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["HOME", "ITERM_STATE", "PATH"]);
        assert_eq!(e[2].1, "/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(app_path(Some("/x/iTerm.app")), PathBuf::from("/x/iTerm.app"));
        assert_eq!(crate::out::line(&menu_row("Edit > Copy\tcmd+c\tdisabled")), "ok item item=\"Edit > Copy\" key=cmd+c state=disabled");
        assert_eq!(crate::out::line(&menu_row("Edit > Copy\tcmd+c")), "ok item item=\"Edit > Copy\" key=cmd+c");
        if std::env::var_os("ITERM_APP_PATH").is_none() {
            assert_eq!(app_path(None), PathBuf::from("/Applications/iTerm.app"));
        }
    }
}
