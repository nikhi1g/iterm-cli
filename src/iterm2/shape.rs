//! iTerm2: create, split, close, move, order, merge, layout, set, select, window cells, broadcast. Owner: the
//! `shape` agent.
//!
//! Each `Op` becomes official API requests (pipelined where they don't depend on each other) and the stock
//! `iterm2.*` functions: `move_session`, `move_session_to_new_tab`, `move_session_to_new_window`,
//! `move_tab_to_window`, `apply_layout` (any split tree, sessions across tabs and windows), `set_name`,
//! `set_title`; tab groups (`create_tab_group`, …) when the app speaks API 1.19. The geometry is pure and tested
//! in [`layout`]. Writes never move the human's focus unless the op asks for it (`focus`, `Select`): a split puts
//! the tab's active pane back, a new window gives the key window back.

mod layout;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::app::wire::{Dest, Field, Side, Spawn, WinState};
use crate::app::{BroadcastMode, Done, Fail, FailCode, Id, Kind, Layout, Op, Rect};

use super::proto::{self as p, client_originated_message::Submessage as Req, server_originated_message::Submessage as Resp};
use super::snapshot::{var_to_iterm, vtab_foreign};
use super::{unsupported, Live};
use layout::{lit, Plan, Tree};

type List = p::ListSessionsResponse;
type Win = p::list_sessions_response::Window;
type Tab = p::list_sessions_response::Tab;

fn invalid(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::Invalid, m)
}

fn internal(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::Internal, m)
}

fn not_found(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::NotFound, m)
}

/// Tab groups (vertical tabs) need API 1.19; iTerm2 3.7.3 speaks 1.18.
fn groups_supported(live: &Live) -> bool {
    live.api.protocol >= (1, 19)
}

fn need_groups(live: &Live, what: &str) -> Result<(), Fail> {
    if groups_supported(live) {
        return Ok(());
    }
    let (a, b) = live.api.protocol;
    Err(Fail::new(
        FailCode::Unsupported,
        format!("{what}: vertical tabs are iTerm2 tab groups, which need API 1.19 (this iTerm2 speaks {a}.{b}); use tab new or window new"),
    ))
}

// ---------------------------------------------------------------- IDs

fn foreign(live: &Live, id: Id) -> Result<(Kind, String), Fail> {
    live.ids.foreign(id).map(|(k, f)| (k, f.to_string())).ok_or_else(|| not_found(format!("no node {id}")))
}

fn foreign_of(live: &Live, id: Id, kind: Kind) -> Result<String, Fail> {
    match foreign(live, id)? {
        (k, f) if k == kind => Ok(f),
        (k, _) => Err(invalid(format!("{} is a {}, not a {}", crate::app::model::handle(k, id), k.name(), kind.name()))),
    }
}

/// iTerm2's session UUID for a pane ID.
pub fn session_of(live: &Live, id: Id) -> Result<String, Fail> {
    foreign_of(live, id, Kind::Pane)
}

fn id_of(live: &mut Live, kind: Kind, foreign: &str) -> Result<Id, Fail> {
    live.ids.id(kind, foreign).map_err(|e| internal(format!("id map: {e}")))
}

fn ids_of(live: &mut Live, items: &[(Kind, &str)]) -> Result<Vec<Id>, Fail> {
    live.ids.assign(items).map_err(|e| internal(format!("id map: {e}")))
}

/// A vertical tab's iTerm2 parts: its window, and its tab group (None: the window's implicit one).
fn vtab_parts(list: &List, foreign: &str) -> Result<(String, Option<String>), Fail> {
    if let Some(w) = foreign.strip_prefix("win:") {
        return Ok((w.to_string(), None));
    }
    let g = foreign.strip_prefix("grp:").unwrap_or(foreign);
    for w in &list.windows {
        if w.tabs.iter().any(|t| t.tab_group_id.as_deref() == Some(g)) {
            return Ok((w.window_id.clone().unwrap_or_default(), Some(g.to_string())));
        }
    }
    Err(not_found(format!("no vertical tab {foreign}")))
}

// ---------------------------------------------------------------- reading iTerm2

fn rpc(live: &mut Live, req: Req) -> Result<Resp, Fail> {
    live.api.call(req).map_err(Fail::from)
}

fn list(live: &mut Live) -> Result<List, Fail> {
    match rpc(live, Req::ListSessionsRequest(p::ListSessionsRequest {}))? {
        Resp::ListSessionsResponse(l) => Ok(l),
        _ => Err(internal("unexpected reply to list sessions")),
    }
}

fn tab_by_id<'a>(l: &'a List, tab: &str) -> Option<(&'a Win, &'a Tab)> {
    l.windows.iter().find_map(|w| w.tabs.iter().find(|t| t.tab_id.as_deref() == Some(tab)).map(|t| (w, t)))
}

fn window_by_id<'a>(l: &'a List, w: &str) -> Option<&'a Win> {
    l.windows.iter().find(|x| x.window_id.as_deref() == Some(w))
}

fn tree_of(t: &Tab) -> Option<Tree> {
    t.root.as_ref().and_then(Tree::from_proto)
}

/// Every session of a tab: its split tree's, then any hidden by a maximized pane.
fn sessions_in(t: &Tab) -> Vec<String> {
    let mut s = tree_of(t).map(|x| x.leaves()).unwrap_or_default();
    s.extend(t.minimized_sessions.iter().filter_map(|x| x.unique_identifier.clone()));
    s
}

fn tab_of_session<'a>(l: &'a List, uuid: &str) -> Option<(&'a Win, &'a Tab)> {
    l.windows.iter().find_map(|w| w.tabs.iter().find(|t| sessions_in(t).iter().any(|s| s == uuid)).map(|t| (w, t)))
}

fn summary<'a>(n: &'a p::SplitTreeNode, uuid: &str) -> Option<&'a p::SessionSummary> {
    use p::split_tree_node::split_tree_link::Child;
    n.links.iter().find_map(|l| match &l.child {
        Some(Child::Session(s)) if s.unique_identifier.as_deref() == Some(uuid) => Some(s),
        Some(Child::Node(c)) => summary(c, uuid),
        _ => None,
    })
}

/// The key window when iTerm2 is the active app (what a new window would take from the human).
fn key_window(live: &mut Live) -> Result<Option<String>, Fail> {
    use p::focus_changed_notification::{window::WindowStatus, Event as F};
    let Resp::FocusResponse(f) = rpc(live, Req::FocusRequest(p::FocusRequest {}))? else { return Err(internal("unexpected reply to focus")) };
    let mut active = false;
    let mut window = None;
    for n in f.notifications {
        match n.event {
            Some(F::ApplicationActive(a)) => active = a,
            Some(F::Window(w)) if w.window_status == Some(WindowStatus::TerminalWindowBecameKey as i32) || w.window_status == Some(WindowStatus::TerminalWindowIsCurrent as i32) => {
                window = w.window_id
            }
            _ => {}
        }
    }
    Ok(window.filter(|_| active))
}

// ---------------------------------------------------------------- requests

/// Runs requests in one pipelined batch; the first failure (transport or a refusing status) is the answer.
fn run_all(live: &mut Live, reqs: Vec<Req>) -> Result<Vec<Resp>, Fail> {
    if reqs.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for r in live.api.call_many(reqs).map_err(Fail::from)? {
        let r = r.map_err(Fail::from)?;
        refused(&r)?;
        out.push(r);
    }
    Ok(out)
}

/// A reply whose status says no, as one line.
fn refused(r: &Resp) -> Result<(), Fail> {
    let bad = |what: &str, s: Option<i32>| match s.unwrap_or(0) {
        0 => Ok(()),
        s => Err(invalid(format!("{what} refused (status {s})"))),
    };
    match r {
        Resp::VariableResponse(v) => bad("setting variables", v.status),
        Resp::SetPropertyResponse(v) => bad("setting a property", v.status),
        Resp::ActivateResponse(v) => bad("activate", v.status),
        Resp::SetTabLayoutResponse(v) => bad("the layout", v.status),
        Resp::ReorderTabsResponse(v) => bad("reordering tabs", v.status),
        Resp::SetBroadcastDomainsResponse(v) => bad("broadcast", v.status),
        Resp::SetProfilePropertyResponse(v) => bad("setting a profile property", v.status),
        Resp::InvokeFunctionResponse(v) => invoke_value(v).map(|_| ()),
        _ => Ok(()),
    }
}

enum Ctx<'a> {
    App,
    /// A method on a session, tab or window (`iterm2.set_name`, `iterm2.set_title`, `iterm2.create_tab_group`).
    On(&'a str),
    Tab(&'a str),
}

fn invoke_req(ctx: Ctx, invocation: String) -> Req {
    use p::invoke_function_request::{App, Context, Method, Tab};
    let context = match ctx {
        Ctx::App => Context::App(App {}),
        Ctx::On(r) => Context::Method(Method { receiver: Some(r.to_string()) }),
        Ctx::Tab(t) => Context::Tab(Tab { tab_id: Some(t.to_string()) }),
    };
    Req::InvokeFunctionRequest(p::InvokeFunctionRequest { context: Some(context), invocation: Some(invocation), timeout: Some(-1.0) })
}

fn invoke_value(r: &p::InvokeFunctionResponse) -> Result<Value, Fail> {
    use p::invoke_function_response::Disposition;
    match &r.disposition {
        Some(Disposition::Success(s)) => Ok(serde_json::from_str(s.json_result.as_deref().unwrap_or("null")).unwrap_or(Value::Null)),
        Some(Disposition::Error(e)) => {
            let why = e.error_reason.clone().unwrap_or_else(|| format!("status {}", e.status.unwrap_or(0)));
            // iTerm2's reasons quote the receiver's whole description: keep the part that says what failed
            let why = why.split(" on <").next().unwrap_or(&why).trim_start_matches("The operation couldn’t be completed. ").to_string();
            Err(invalid(format!("iTerm2 refused: {why}")))
        }
        None => Ok(Value::Null),
    }
}

fn invoke(live: &mut Live, ctx: Ctx, invocation: String) -> Result<Value, Fail> {
    match rpc(live, invoke_req(ctx, invocation))? {
        Resp::InvokeFunctionResponse(r) => invoke_value(&r),
        _ => Err(internal("unexpected reply to invoke")),
    }
}

/// `iterm2.apply_layout`: reshape tabs (any split tree), moving sessions across tabs and windows.
fn apply_layout(live: &mut Live, spec: Value) -> Result<(), Fail> {
    invoke(live, Ctx::App, format!("iterm2.apply_layout(spec_json_b64: {})", lit(&layout::base64(spec.to_string().as_bytes())))).map(|_| ())
}

/// The cells a tab of window `w` covers. iTerm2 lays out only the selected tab (a tab never shown reports a few
/// points), and every tab of a window shares its content area, so the selected tab's extent is the measure.
/// Sizes that sum to anything else make `SetTabLayout` resize the window.
fn tab_extent(w: &Win, t: &Tab) -> (i32, i32) {
    let sel = w.tabs.iter().find(|x| x.tab_id.is_some() && x.tab_id == w.selected_tab_id).unwrap_or(t);
    tree_of(sel).or_else(|| tree_of(t)).map(|x| x.extent()).unwrap_or((80, 24))
}

/// A tree's proportions as cell sizes over its window's content area.
fn fit(w: &Win, t: &Tab, tree: &Tree) -> HashMap<String, (i32, i32)> {
    let (c, r) = tab_extent(w, t);
    let mut sizes = HashMap::new();
    tree.plan().sizes(c, r, &mut sizes);
    sizes
}

/// `SetTabLayout` giving a tab's sessions these cell sizes, over iTerm2's own tree (None: one pane, nothing to size).
fn size_req(t: &Tab, sizes: &HashMap<String, (i32, i32)>) -> Option<Req> {
    let root = t.root.as_ref()?;
    if tree_of(t).map(|x| x.leaves().len()).unwrap_or(0) < 2 {
        return None;
    }
    Some(Req::SetTabLayoutRequest(p::SetTabLayoutRequest { root: Some(layout::resized(root, sizes)), tab_id: t.tab_id.clone() }))
}

fn activate_req(target: p::activate_request::Identifier, select_tab: bool, front: bool, app: bool) -> Req {
    // iTerm2 refuses select_session (and select_tab) for a target that is not a session (or a tab)
    use p::activate_request::Identifier as I;
    let (session, tab) = (matches!(target, I::SessionId(_)), !matches!(target, I::WindowId(_)));
    Req::ActivateRequest(p::ActivateRequest {
        identifier: Some(target),
        order_window_front: Some(front),
        select_tab: Some(select_tab && tab),
        select_session: Some(session),
        activate_app: app.then_some(p::activate_request::App { raise_all_windows: Some(false), ignoring_other_apps: Some(true) }),
    })
}

fn session_target(uuid: &str) -> p::activate_request::Identifier {
    p::activate_request::Identifier::SessionId(uuid.to_string())
}

fn frame_req(window: &str, r: Rect) -> Req {
    let v = json!({ "origin": { "x": r.x, "y": r.y }, "size": { "width": r.w, "height": r.h } });
    Req::SetPropertyRequest(p::SetPropertyRequest { name: Some("frame".into()), json_value: Some(v.to_string()), identifier: Some(p::set_property_request::Identifier::WindowId(window.to_string())) })
}

fn var_req(scope: p::variable_request::Scope, vars: &[(String, Option<String>)]) -> Req {
    let set = vars
        .iter()
        .map(|(k, v)| p::variable_request::Set {
            name: Some(format!("user.{}", var_to_iterm(k))),
            value: Some(v.as_ref().map(|v| Value::from(v.as_str())).unwrap_or(Value::Null).to_string()),
        })
        .collect();
    Req::VariableRequest(p::VariableRequest { scope: Some(scope), get: vec![], set })
}

/// `vars` plus the index update (`user.vars`) the snapshot needs to see keys outside `snapshot::USER_VARS`: one
/// read of the node's index, only when such a key is set or unset (`now`: the index is known, e.g. a new session's
/// is empty).
fn indexed_vars(live: &mut Live, scope: &p::variable_request::Scope, vars: &[(String, Option<String>)], now: Option<String>) -> Result<Vec<(String, Option<String>)>, Fail> {
    let mut out = vars.to_vec();
    if !vars.iter().any(|(k, _)| crate::iterm2::snapshot::indexed(k)) {
        return Ok(out);
    }
    let now = match now {
        Some(n) => n,
        None => match live.api.call(Req::VariableRequest(p::VariableRequest { scope: Some(scope.clone()), get: vec![format!("user.{}", crate::iterm2::snapshot::INDEX)], set: vec![] })).map_err(Fail::from)? {
            Resp::VariableResponse(v) => v.values.first().and_then(|r| serde_json::from_str::<Value>(r).ok()).and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            _ => String::new(),
        },
    };
    if let Some(ix) = crate::iterm2::snapshot::index_after(Some(&now), vars) {
        out.push((crate::iterm2::snapshot::INDEX.to_string(), Some(ix)));
    }
    Ok(out)
}

/// Sets `user.*` variables on a session (the CLI's `agent.parent` is stored as `agent__parent`).
pub fn set_vars(live: &mut Live, uuid: &str, vars: &[(String, Option<String>)]) -> Result<(), Fail> {
    if vars.is_empty() {
        return Ok(());
    }
    let scope = p::variable_request::Scope::SessionId(uuid.into());
    let vars = indexed_vars(live, &scope, vars, None)?;
    run_all(live, vec![var_req(scope, &vars)]).map(|_| ())
}

// ---------------------------------------------------------------- spawning

/// POSIX single quotes.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The profile properties that start `spawn` in the new session: its folder, and its command run by the login
/// shell with the spawn's environment exported (`$SHELL -lc 'export K=V; exec CMD'`).
///
/// These properties make the session's profile, which iTerm2's Cmd-D / Shift-Cmd-D ("Split with Current Profile")
/// copy. So a command is guarded by `once`, a file made for this launch: the first run deletes it and runs the
/// command; a copy finds it gone and starts a plain login shell instead (`once` None: no guard, for tests).
pub fn spawn_props(spawn: &Spawn, once: Option<&Path>) -> Vec<p::ProfileProperty> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    props_with(spawn, &shell, once)
}

fn props_with(spawn: &Spawn, shell: &str, once: Option<&Path>) -> Vec<p::ProfileProperty> {
    let prop = |k: &str, v: Value| p::ProfileProperty { key: Some(k.into()), json_value: Some(v.to_string()) };
    let mut out = Vec::new();
    let cwd = spawn.cwd.as_deref().filter(|d| !d.is_empty());
    if cwd.is_some() {
        // "Recycle": a Cmd-D copy opens in the folder its pane is in now, not the folder this launch began in; the
        // first launch gets its folder from the `cd` below
        out.push(prop("Custom Directory", Value::from("Recycle")));
    }
    {
        // iTerm2 may itself have been started from inside an agent (Claude Code's session variables in its own
        // environment, so in every pane): a Claude launched with them saves no transcript. Drop them first.
        let mut script = format!("unset {}; ", CALLER_SESSION_VARS.join(" "));
        let login = format!("exec {} -l", sh_quote(shell));
        if let (true, Some(f)) = (spawn.cmd.is_some() || cwd.is_some(), once) {
            // `rm` fails once the file is gone, and only one of two racing launches can remove it
            script.push_str(&format!("/bin/rm {} 2>/dev/null || {login}; ", sh_quote(&f.to_string_lossy())));
        }
        if let Some(d) = cwd {
            // a folder that is gone by now: say so and leave a shell, never run the command somewhere else
            let q = sh_quote(d);
            script.push_str(&format!("cd {q} 2>/dev/null || {{ echo \"iterm: no folder \"{q}\", the command did not run\" >&2; {login}; }}; "));
        }
        for (k, v) in &spawn.env {
            script.push_str(&format!("export {}={}; ", k, sh_quote(v)));
        }
        match &spawn.cmd {
            Some(c) => script.push_str(c),
            None => script.push_str(&login),
        }
        out.push(prop("Custom Command", Value::from("Yes")));
        out.push(prop("Command", Value::from(format!("{} -lc {}", sh_quote(shell), sh_quote(&script)))));
    }
    out
}

/// The one-shot file guarding a launch's command (see `spawn_props`): empty, 0600, under `STATE/launch/`. Files a
/// pane never consumed (it failed to open) are pruned after a day.
fn once_file(live: &Live, spawn: &Spawn) -> Result<Option<PathBuf>, Fail> {
    if spawn.cmd.is_none() && spawn.cwd.as_deref().is_none_or(str::is_empty) {
        return Ok(None);
    }
    let io = |e: std::io::Error| internal(format!("launch file: {e}"));
    let dir = crate::store::dir(&live.env.state, &["launch"]).map_err(|e| internal(e.to_string()))?;
    let day = std::time::Duration::from_secs(86400);
    for e in std::fs::read_dir(&dir).map_err(io)?.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > day);
        if old && e.file_name().to_string_lossy().ends_with(".once") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let f = dir.join(format!("{}-{nanos}.once", std::process::id()));
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&f).map_err(io)?;
    }
    Ok(Some(f))
}

fn launch_props(live: &Live, spawn: &Spawn) -> Result<Vec<p::ProfileProperty>, Fail> {
    let once = once_file(live, spawn)?;
    Ok(spawn_props(spawn, once.as_deref()))
}

/// A caller's agent-session variables, which a spawned pane must not inherit (Claude Code saves no transcript for a
/// session that carries `CLAUDE_CODE_CHILD_SESSION`, and the rest name the caller's session, not the child's).
pub const CALLER_SESSION_VARS: [&str; 10] = [
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDECODE",
];

/// What a new session gets right after it starts: the spawn's `user.*` variables and its name.
fn after_spawn(uuid: &str, spawn: &Spawn) -> Vec<Req> {
    let mut reqs = Vec::new();
    let mut vars: Vec<(String, Option<String>)> = spawn.vars.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    // a new session's index is empty
    if let Some(ix) = crate::iterm2::snapshot::index_after(None, &vars) {
        vars.push((crate::iterm2::snapshot::INDEX.to_string(), Some(ix)));
    }
    if !vars.is_empty() {
        reqs.push(var_req(p::variable_request::Scope::SessionId(uuid.into()), &vars));
    }
    if let Some(n) = spawn.name.as_deref().filter(|n| !n.is_empty()) {
        reqs.push(invoke_req(Ctx::On(uuid), format!("iterm2.set_name(name: {})", lit(n))));
    }
    reqs
}

/// A new tab (in `window`, else a new window) running `spawn`: (window, tab, session).
fn create_tab(live: &mut Live, window: Option<&str>, spawn: Option<&Spawn>, select: bool) -> Result<(String, String, String), Fail> {
    let req = p::CreateTabRequest {
        window_id: window.map(str::to_string),
        custom_profile_properties: match spawn {
            Some(sp) => launch_props(live, sp)?,
            None => vec![],
        },
        select_tab: Some(select),
        ..Default::default()
    };
    match rpc(live, Req::CreateTabRequest(req))? {
        // 3: created, just not at the wanted index
        Resp::CreateTabResponse(r) if matches!(r.status.unwrap_or(0), 0 | 3) => match (r.window_id, r.tab_id, r.session_id) {
            (Some(w), Some(t), Some(s)) => Ok((w, t.to_string(), s)),
            _ => Err(internal("iTerm2 did not report the new tab")),
        },
        Resp::CreateTabResponse(r) => Err(invalid(match r.status.unwrap_or(0) {
            2 => "no such window".to_string(),
            s => format!("creating a tab refused (status {s})"),
        })),
        _ => Err(internal("unexpected reply to create tab")),
    }
}

/// After a create: the human's focus moves to the new session (`focus`), or stays where it was.
fn focus_after(live: &mut Live, mut reqs: Vec<Req>, new: &str, focus: bool, give_back: Option<String>) -> Result<(), Fail> {
    if focus {
        reqs.push(activate_req(session_target(new), true, true, true));
    } else if let Some(w) = give_back {
        reqs.push(activate_req(p::activate_request::Identifier::WindowId(w), false, true, false));
    }
    run_all(live, reqs).map(|_| ())
}

fn new_window(live: &mut Live, spawn: Spawn, frame: Option<Rect>, focus: bool) -> Result<Done, Fail> {
    let give_back = if focus { None } else { key_window(live)? };
    let (w, t, s) = create_tab(live, None, Some(&spawn), true)?;
    let vf = vtab_foreign(&w, None);
    let ids = ids_of(live, &[(Kind::Window, &w), (Kind::VTab, &vf), (Kind::Tab, &t), (Kind::Pane, &s)])?;
    let mut reqs = after_spawn(&s, &spawn);
    if let Some(f) = frame {
        reqs.push(frame_req(&w, f));
    }
    focus_after(live, reqs, &s, focus, give_back.filter(|g| *g != w))?;
    Ok(Done { ids, lines: vec![] })
}

/// The focused node of a kind (the key window's), from the snapshot.
fn focused(live: &mut Live, kind: Kind) -> Result<Option<Id>, Fail> {
    let snap = super::snapshot::snapshot(live)?;
    Ok(snap.focus.iter().copied().find(|id| live.ids.foreign(*id).map(|(k, _)| k) == Some(kind)))
}

fn new_tab(live: &mut Live, vtab: Option<Id>, title: Option<String>, spawn: Spawn, focus: bool) -> Result<Done, Fail> {
    let v = match vtab {
        Some(v) => v,
        None => focused(live, Kind::VTab)?.ok_or_else(|| invalid("no focused vertical tab: name one (tab new --in VTAB) or open a window"))?,
    };
    let vf = foreign_of(live, v, Kind::VTab)?;
    let l = if vf.starts_with("win:") { List::default() } else { list(live)? };
    let (window, group) = vtab_parts(&l, &vf)?;
    if group.is_some() {
        need_groups(live, "a tab in a vertical tab")?;
    }
    let (_, t, s) = create_tab(live, Some(&window), Some(&spawn), focus)?;
    let ids = ids_of(live, &[(Kind::Tab, &t), (Kind::Pane, &s)])?;
    let mut reqs = after_spawn(&s, &spawn);
    if let Some(g) = &group {
        reqs.push(invoke_req(Ctx::On(&window), format!("iterm2.add_tab_to_group(tab_id: {}, group_id: {})", lit(&t), lit(g))));
    }
    if let Some(tt) = title.as_deref().filter(|x| !x.is_empty()) {
        reqs.push(invoke_req(Ctx::On(&t), format!("iterm2.set_title(title: {})", lit(tt))));
    }
    focus_after(live, reqs, &s, focus, None)?;
    Ok(Done { ids, lines: vec![] })
}

fn new_vtab(live: &mut Live, window: Option<Id>, name: Option<String>, spawn: Spawn, focus: bool) -> Result<Done, Fail> {
    need_groups(live, "vtab new")?;
    let window = match window {
        Some(w) => Some(foreign_of(live, w, Kind::Window)?),
        None => match focused(live, Kind::Window)? {
            Some(w) => Some(foreign_of(live, w, Kind::Window)?),
            None => None,
        },
    };
    let give_back = if focus || window.is_some() { None } else { key_window(live)? };
    let (w, t, s) = create_tab(live, window.as_deref(), Some(&spawn), focus)?;
    let g = invoke(live, Ctx::On(&w), format!("iterm2.create_tab_group(tab_ids: [{}], name: {}, color: \"\")", lit(&t), lit(name.as_deref().unwrap_or(""))))?;
    let g = g.as_str().filter(|g| !g.is_empty()).ok_or_else(|| internal("iTerm2 did not report the new tab group"))?.to_string();
    let vf = format!("grp:{g}");
    let ids = ids_of(live, &[(Kind::VTab, &vf), (Kind::Tab, &t), (Kind::Pane, &s)])?;
    focus_after(live, after_spawn(&s, &spawn), &s, focus, give_back)?;
    Ok(Done { ids, lines: vec![] })
}

fn split(live: &mut Live, at: Id, side: Side, ratio: Option<f32>, spawn: Spawn, focus: bool) -> Result<Done, Fail> {
    use p::split_pane_request::SplitDirection;
    let uuid = session_of(live, at)?;
    let (vertical, before) = side_axis(side);
    // the tab's active pane, to give back (iTerm2 makes the new pane active)
    let l0 = list(live)?;
    let was_active = tab_of_session(&l0, &uuid).and_then(|(_, t)| t.active_session_id.clone());
    let resp = rpc(
        live,
        Req::SplitPaneRequest(p::SplitPaneRequest {
            session: Some(uuid.clone()),
            split_direction: Some(if vertical { SplitDirection::Vertical } else { SplitDirection::Horizontal } as i32),
            before: Some(before),
            profile_name: None,
            custom_profile_properties: launch_props(live, &spawn)?,
        }),
    )?;
    let new = match resp {
        Resp::SplitPaneResponse(r) if r.status.unwrap_or(0) == 0 && !r.session_id.is_empty() => r.session_id[0].clone(),
        Resp::SplitPaneResponse(r) => {
            return Err(invalid(match r.status.unwrap_or(0) {
                1 => "split failed: pane not found".to_string(),
                3 => "split failed: the pane is too small to split".to_string(),
                s => format!("split failed (status {s})"),
            }))
        }
        _ => return Err(internal("unexpected reply to split")),
    };
    let id = id_of(live, Kind::Pane, &new)?;
    let mut reqs = after_spawn(&new, &spawn);
    if !focus {
        if let Some(a) = was_active {
            reqs.push(activate_req(session_target(&a), false, false, false));
        }
    }
    focus_after(live, reqs, &new, focus, None)?;
    if let Some(r) = ratio {
        let l = list(live)?;
        if let Some((w, t)) = tab_of_session(&l, &new) {
            if let Some(mut tree) = tree_of(t) {
                if tree.set_ratio(&uuid, &new, vertical, r) {
                    run_all(live, size_req(t, &fit(w, t, &tree)).into_iter().collect())?;
                }
            }
        }
    }
    Ok(Done { ids: vec![id], lines: vec![] })
}

/// (vertical dividers, before): left and right split side by side.
fn side_axis(side: Side) -> (bool, bool) {
    match side {
        Side::Left => (true, true),
        Side::Right => (true, false),
        Side::Up => (false, true),
        Side::Down => (false, false),
    }
}

// ---------------------------------------------------------------- close

/// Closes nodes and everything under them (their processes get SIGHUP), forced: the CLI's `--force` rail is
/// checked before this.
fn close(live: &mut Live, ids: Vec<Id>) -> Result<Done, Fail> {
    use p::close_request::{CloseSessions, CloseTabs, CloseWindows, Target};
    let (mut sessions, mut tabs, mut windows) = (Vec::new(), Vec::new(), Vec::new());
    let mut l: Option<List> = None;
    for id in &ids {
        match foreign(live, *id)? {
            (Kind::Pane, f) => sessions.push(f),
            (Kind::Tab, f) => tabs.push(f),
            (Kind::Window, f) => windows.push(f),
            (Kind::VTab, f) => match f.strip_prefix("win:") {
                // the implicit vertical tab holds every tab of its window
                Some(w) => windows.push(w.to_string()),
                None => {
                    if l.is_none() {
                        l = Some(list(live)?);
                    }
                    let g = f.trim_start_matches("grp:");
                    for w in &l.as_ref().expect("listed").windows {
                        tabs.extend(w.tabs.iter().filter(|t| t.tab_group_id.as_deref() == Some(g)).filter_map(|t| t.tab_id.clone()));
                    }
                }
            },
            (Kind::App, _) => return Err(invalid("closing the app is app quit")),
        }
    }
    // biggest first; a pane or tab already gone with its window counts as closed
    let mut reqs = Vec::new();
    for target in [
        (!windows.is_empty()).then(|| Target::Windows(CloseWindows { window_ids: windows })),
        (!tabs.is_empty()).then(|| Target::Tabs(CloseTabs { tab_ids: tabs })),
        (!sessions.is_empty()).then(|| Target::Sessions(CloseSessions { session_ids: sessions })),
    ]
    .into_iter()
    .flatten()
    {
        reqs.push(Req::CloseRequest(p::CloseRequest { force: Some(true), target: Some(target) }));
    }
    let n = reqs.len();
    for (k, r) in live.api.call_many(reqs).map_err(Fail::from)?.into_iter().enumerate() {
        match r.map_err(Fail::from)? {
            Resp::CloseResponse(r) if r.statuses.iter().all(|s| *s == 0 || (k > 0 && *s == 1)) => {}
            Resp::CloseResponse(r) => return Err(invalid(format!("close refused (statuses {:?})", r.statuses))),
            _ => return Err(internal(format!("unexpected reply to close {}/{n}", k + 1))),
        }
    }
    Ok(Done { ids: vec![], lines: vec![] })
}

// ---------------------------------------------------------------- set, select

fn set(live: &mut Live, id: Id, field: Field) -> Result<Done, Fail> {
    use p::variable_request::Scope;
    let (kind, f) = foreign(live, id)?;
    let reqs = match (kind, field) {
        (Kind::Pane, Field::Name(n)) => vec![invoke_req(Ctx::On(&f), format!("iterm2.set_name(name: {})", lit(&n)))],
        (Kind::Tab | Kind::Window, Field::Name(n)) => vec![invoke_req(Ctx::On(&f), format!("iterm2.set_title(title: {})", lit(&n)))],
        (Kind::VTab, Field::Name(n)) => match f.strip_prefix("grp:") {
            Some(g) => {
                need_groups(live, "naming a vertical tab")?;
                vec![invoke_req(Ctx::App, format!("iterm2.set_tab_group_name(group_id: {}, name: {})", lit(g), lit(&n)))]
            }
            None => return Err(Fail::new(FailCode::Unsupported, "a window's implicit vertical tab has no name on iTerm2: name the window or a tab")),
        },
        (Kind::Pane | Kind::Tab | Kind::Window, Field::Var(k, v)) => {
            let scope = match kind {
                Kind::Pane => Scope::SessionId(f),
                Kind::Tab => Scope::TabId(f),
                _ => Scope::WindowId(f),
            };
            let vars = indexed_vars(live, &scope, &[(k, v)], None)?;
            vec![var_req(scope, &vars)]
        }
        (Kind::App, Field::Var(k, v)) => vec![var_req(Scope::App(true), &[(k, v)])],
        // the implicit vertical tab is its window's one: its variables are the window's
        (Kind::VTab, Field::Var(k, v)) => match f.strip_prefix("win:") {
            Some(w) => {
                let scope = Scope::WindowId(w.to_string());
                let vars = indexed_vars(live, &scope, &[(k, v)], None)?;
                vec![var_req(scope, &vars)]
            }
            None => return Err(Fail::new(FailCode::Unsupported, "variables on a tab group: iTerm2 has no variable scope for groups")),
        },
        (Kind::Pane | Kind::Tab | Kind::VTab, Field::Color(c)) => return set_color(live, kind, &f, c),
        (Kind::Window, Field::Frame(r)) => vec![frame_req(&f, r)],
        (Kind::Window, Field::WindowState(st)) => return window_state(live, &f, st),
        (Kind::Pane, Field::Buried(b)) => vec![Req::SetPropertyRequest(p::SetPropertyRequest {
            name: Some("buried".into()),
            json_value: Some(b.to_string()),
            identifier: Some(p::set_property_request::Identifier::SessionId(f)),
        })],
        (k, field) => return Err(invalid(format!("a {} has no {}", k.name(), field_name(&field)))),
    };
    run_all(live, reqs)?;
    Ok(Done::default())
}

fn field_name(f: &Field) -> &'static str {
    match f {
        Field::Name(_) => "name",
        Field::Var(..) => "variables here",
        Field::Color(_) => "color",
        Field::Frame(_) => "frame",
        Field::WindowState(_) => "window state",
        Field::Buried(_) => "burial (only a pane is buried)",
    }
}

/// A tab's color is its sessions' profile `Tab Color` (iTerm2 shows the active session's); a group has its own.
fn set_color(live: &mut Live, kind: Kind, f: &str, c: Option<String>) -> Result<Done, Fail> {
    let dict = match &c {
        Some(h) => Some(layout::color_dict(h).ok_or_else(|| invalid(format!("color {h}: want #rrggbb")))?),
        None => None,
    };
    if kind == Kind::VTab {
        let Some(g) = f.strip_prefix("grp:") else {
            return Err(Fail::new(FailCode::Unsupported, "a window's implicit vertical tab has no color on iTerm2: color its tabs"));
        };
        need_groups(live, "coloring a vertical tab")?;
        run_all(live, vec![invoke_req(Ctx::App, format!("iterm2.set_tab_group_color(group_id: {}, color: {})", lit(g), lit(c.as_deref().unwrap_or(""))))])?;
        return Ok(Done::default());
    }
    let sessions = if kind == Kind::Pane {
        vec![f.to_string()]
    } else {
        let l = list(live)?;
        tab_by_id(&l, f).map(|(_, t)| sessions_in(t)).ok_or_else(|| not_found(format!("no tab {f}")))?
    };
    let mut assignments = vec![p::set_profile_property_request::Assignment { key: Some("Use Tab Color".into()), json_value: Some(dict.is_some().to_string()) }];
    if let Some(d) = dict {
        assignments.push(p::set_profile_property_request::Assignment { key: Some("Tab Color".into()), json_value: Some(d.to_string()) });
    }
    // the tab color is what the human sees; `user.color` is what the snapshot reads back: write both, one batch
    let var = [("color".to_string(), c.clone())];
    let reqs = sessions
        .into_iter()
        .flat_map(|s| {
            [
                Req::SetProfilePropertyRequest(p::SetProfilePropertyRequest {
                    key: None,
                    json_value: None,
                    assignments: assignments.clone(),
                    target: Some(p::set_profile_property_request::Target::Session(s.clone())),
                }),
                var_req(p::variable_request::Scope::SessionId(s), &var),
            ]
        })
        .collect();
    run_all(live, reqs)?;
    Ok(Done::default())
}

fn window_state(live: &mut Live, w: &str, st: WinState) -> Result<Done, Fail> {
    let fs = |on: bool| Req::SetPropertyRequest(p::SetPropertyRequest { name: Some("fullscreen".into()), json_value: Some(on.to_string()), identifier: Some(p::set_property_request::Identifier::WindowId(w.to_string())) });
    let get = Req::GetPropertyRequest(p::GetPropertyRequest { name: Some("fullscreen".into()), identifier: Some(p::get_property_request::Identifier::WindowId(w.to_string())) });
    let now = match rpc(live, get)? {
        Resp::GetPropertyResponse(r) if r.status.unwrap_or(0) == 0 => r.json_value.as_deref() == Some("true"),
        Resp::GetPropertyResponse(_) => return Err(not_found(format!("no window {w}"))),
        _ => return Err(internal("unexpected reply to get property")),
    };
    match st {
        WinState::FullScreen if !now => run_all(live, vec![fs(true)]).map(|_| ())?,
        WinState::Normal if now => run_all(live, vec![fs(false)]).map(|_| ())?,
        WinState::FullScreen | WinState::Normal => {}
        WinState::Zoomed | WinState::Minimized => {
            return Err(Fail::new(FailCode::Unsupported, "window zoom and minimize: iTerm2's API has no such property (only frame and fullscreen); set --frame instead"))
        }
    }
    Ok(Done::default())
}

/// Focus: the window forward, the node current at every level, iTerm2 the active app.
fn select(live: &mut Live, id: Id) -> Result<Done, Fail> {
    use p::activate_request::Identifier as I;
    let (kind, f) = foreign(live, id)?;
    let target = match kind {
        Kind::Pane => I::SessionId(f),
        Kind::Tab => I::TabId(f),
        Kind::Window => I::WindowId(f),
        Kind::VTab => match f.strip_prefix("win:") {
            Some(w) => I::WindowId(w.to_string()),
            None => {
                let l = list(live)?;
                let g = f.trim_start_matches("grp:");
                let t = l.windows.iter().flat_map(|w| &w.tabs).find(|t| t.tab_group_id.as_deref() == Some(g)).and_then(|t| t.tab_id.clone());
                I::TabId(t.ok_or_else(|| not_found(format!("no vertical tab {f}")))?)
            }
        },
        Kind::App => return Err(invalid("select a window, tab or pane")),
    };
    run_all(live, vec![activate_req(target, true, true, true)])?;
    Ok(Done::default())
}

// ---------------------------------------------------------------- layout, order, move, merge

/// The plan for a CLI layout, over session UUIDs.
fn plan_of(live: &Live, l: &Layout) -> Result<Plan, Fail> {
    Ok(match l {
        Layout::Pane(id) => Plan::Leaf(session_of(live, *id)?),
        Layout::Split { vertical, children } => {
            Plan::Split { vertical: *vertical, kids: children.iter().map(|(f, c)| Ok((*f, plan_of(live, c)?))).collect::<Result<_, Fail>>()? }
        }
    })
}

/// Gives tab `tab` the plan's shape (through `apply_layout` when the shape differs) and its fractions (cell sizes
/// over the tab's current extent, one `SetTabLayout`).
fn shape_tab(live: &mut Live, tab: &str, plan: Plan) -> Result<(), Fail> {
    let plan = plan.normalize();
    let l = list(live)?;
    let (w, t) = tab_by_id(&l, tab).ok_or_else(|| not_found(format!("no tab {tab}")))?;
    let cur = tree_of(t).ok_or_else(|| internal("the tab has no split tree"))?;
    let extent = tab_extent(w, t);
    let mut sizes = HashMap::new();
    plan.sizes(extent.0, extent.1, &mut sizes);
    if plan.same_shape(&cur) {
        run_all(live, size_req(t, &sizes).into_iter().collect())?;
        return Ok(());
    }
    apply_layout(live, json!({ "tabs": [{ "tab_id": tab, "root": plan.spec() }] }))?;
    let l = list(live)?;
    let (_, t) = tab_by_id(&l, tab).ok_or_else(|| internal("the tab is gone after its layout"))?;
    run_all(live, size_req(t, &sizes).into_iter().collect())?;
    Ok(())
}

fn set_layout(live: &mut Live, tab: Id, layout: Layout) -> Result<Done, Fail> {
    let t = foreign_of(live, tab, Kind::Tab)?;
    let plan = plan_of(live, &layout)?;
    let l = list(live)?;
    let (_, cur) = tab_by_id(&l, &t).ok_or_else(|| not_found(format!("no tab {tab}")))?;
    let (mut a, mut b) = (plan.leaves(), tree_of(cur).map(|x| x.leaves()).unwrap_or_default());
    a.sort();
    b.sort();
    if a != b {
        return Err(invalid("a layout must hold exactly the tab's panes"));
    }
    shape_tab(live, &t, plan)?;
    Ok(Done::default())
}

fn order(live: &mut Live, ids: Vec<Id>) -> Result<Done, Fail> {
    let Some(first) = ids.first() else { return Ok(Done::default()) };
    let kind = foreign(live, *first)?.0;
    let fs: Vec<String> = ids.iter().map(|i| foreign_of(live, *i, kind)).collect::<Result<_, _>>()?;
    let l = list(live)?;
    match kind {
        Kind::Tab => {
            let (w, _) = tab_by_id(&l, &fs[0]).ok_or_else(|| not_found(format!("no tab {first}")))?;
            let all: Vec<String> = w.tabs.iter().filter_map(|t| t.tab_id.clone()).collect();
            if fs.iter().any(|f| !all.contains(f)) {
                return Err(invalid("siblings only: tabs of one window"));
            }
            let assignments = vec![p::reorder_tabs_request::Assignment { window_id: w.window_id.clone(), tab_ids: layout::reorder(&all, &fs) }];
            run_all(live, vec![Req::ReorderTabsRequest(p::ReorderTabsRequest { assignments })])?;
        }
        Kind::Pane => {
            let (_, t) = tab_of_session(&l, &fs[0]).ok_or_else(|| not_found(format!("no pane {first}")))?;
            let tree = tree_of(t).ok_or_else(|| internal("the tab has no split tree"))?;
            let all = tree.leaves();
            if fs.iter().any(|f| !all.contains(f)) {
                return Err(invalid("siblings only: panes of one tab"));
            }
            let order = layout::reorder(&all, &fs);
            if order != all {
                let tab = t.tab_id.clone().unwrap_or_default();
                let moved = tree.permuted(&order);
                apply_layout(live, json!({ "tabs": [{ "tab_id": tab, "root": moved.plan().spec() }] }))?;
                let l = list(live)?;
                if let Some((w, t)) = tab_by_id(&l, &tab) {
                    run_all(live, size_req(t, &fit(w, t, &moved)).into_iter().collect())?;
                }
            }
        }
        // one implicit vertical tab per window: already in order
        Kind::VTab if fs.iter().all(|f| f.starts_with("win:")) && fs.len() == 1 => {}
        Kind::VTab => return Err(Fail::new(FailCode::Unsupported, "ordering vertical tabs: needs iTerm2 tab groups")),
        Kind::Window => return Err(Fail::new(FailCode::Unsupported, "ordering windows: iTerm2's API has no window order")),
        Kind::App => return Err(invalid("the app has no siblings")),
    }
    Ok(Done::default())
}

/// A tab into another window: a placeholder tab there takes the tab's split tree in one `apply_layout` (the
/// placeholder's shell closes, the emptied tab and window close by themselves), then its proportions. The tab
/// keeps its short ID. Returns the tab's new iTerm2 ID.
fn tab_into_window(live: &mut Live, tab: &str, dest: &str) -> Result<String, Fail> {
    let l = list(live)?;
    let (w, t) = tab_by_id(&l, tab).ok_or_else(|| not_found(format!("no tab {tab}")))?;
    if w.window_id.as_deref() == Some(dest) {
        return Ok(tab.to_string());
    }
    let old = tree_of(t).ok_or_else(|| internal("the tab has no split tree"))?;
    let plan = old.plan();
    let (_, t2, placeholder) = create_tab(live, Some(dest), None, false)?;
    let spec = json!({ "tabs": [{ "tab_id": t2, "root": plan.spec() }], "close_sessions": [placeholder] });
    if let Err(e) = apply_layout(live, spec) {
        let _ = close_sessions(live, vec![placeholder]);
        return Err(e);
    }
    if let Some(old) = live.ids.get(Kind::Tab, tab) {
        live.ids.alias(Kind::Tab, &t2, old).map_err(|e| internal(format!("id map: {e}")))?;
    }
    // the move is done; its proportions, over the new window's content area, are best effort
    let l = list(live)?;
    if let Some((w, t)) = tab_by_id(&l, &t2) {
        let _ = run_all(live, size_req(t, &fit(w, t, &old)).into_iter().collect());
    }
    Ok(t2)
}

fn close_sessions(live: &mut Live, s: Vec<String>) -> Result<(), Fail> {
    let target = p::close_request::Target::Sessions(p::close_request::CloseSessions { session_ids: s });
    rpc(live, Req::CloseRequest(p::CloseRequest { force: Some(true), target: Some(target) })).map(|_| ())
}

/// `iterm2.move_session`: `session` split in beside `dest`. iTerm2 3.7.3 places the session after the destination
/// when told `before: true` and before it when told `false` (seen live), so the flag goes over inverted.
fn move_session(session: &str, dest: &str, vertical: bool, before: bool) -> String {
    format!("iterm2.move_session(session: {}, destination: {}, vertical: {vertical}, before: {})", lit(session), lit(dest), !before)
}

fn move_node(live: &mut Live, id: Id, to: Dest) -> Result<Done, Fail> {
    let (kind, f) = foreign(live, id)?;
    match (kind, to) {
        (Kind::Pane, Dest::Beside { at, side }) => {
            let dest = session_of(live, at)?;
            if dest == f {
                return Err(invalid("a pane does not move beside itself"));
            }
            let (vertical, before) = side_axis(side);
            invoke(live, Ctx::App, move_session(&f, &dest, vertical, before))?;
        }
        (Kind::Pane, Dest::Into(t)) => {
            let tab = foreign_of(live, t, Kind::Tab)?;
            let l = list(live)?;
            let last = tab_by_id(&l, &tab).and_then(|(_, t)| tree_of(t)).and_then(|x| x.leaves().pop()).ok_or_else(|| not_found(format!("no tab {t}")))?;
            if last != f {
                invoke(live, Ctx::App, move_session(&f, &last, true, false))?;
            }
        }
        (Kind::Pane, Dest::NewTab) => {
            let v = invoke(live, Ctx::App, format!("iterm2.move_session_to_new_tab(session: {})", lit(&f)))?;
            let t = v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())).ok_or_else(|| internal("iTerm2 did not report the new tab"))?;
            return Ok(Done { ids: vec![id_of(live, Kind::Tab, &t)?], lines: vec![] });
        }
        (Kind::Pane, Dest::NewWindow) => {
            let l = list(live)?;
            let alone = tab_of_session(&l, &f).map(|(w, t)| w.tabs.len() == 1 && sessions_in(t).len() == 1).unwrap_or(false);
            if alone {
                return Ok(Done::default());
            }
            let v = invoke(live, Ctx::App, format!("iterm2.move_session_to_new_window(session: {})", lit(&f)))?;
            let w = v.as_str().ok_or_else(|| internal("iTerm2 did not report the new window"))?.to_string();
            return Ok(Done { ids: vec![id_of(live, Kind::Window, &w)?], lines: vec![] });
        }
        (Kind::Tab, Dest::NewWindow) => {
            let l = list(live)?;
            let alone = tab_by_id(&l, &f).map(|(w, _)| w.tabs.len() == 1).ok_or_else(|| not_found(format!("no tab {id}")))?;
            if alone {
                return Ok(Done::default());
            }
            let v = invoke(live, Ctx::Tab(&f), "iterm2.move_tab_to_window()".into())?;
            let w = v.as_str().ok_or_else(|| internal("iTerm2 did not report the new window"))?.to_string();
            return Ok(Done { ids: vec![id_of(live, Kind::Window, &w)?], lines: vec![] });
        }
        (Kind::Tab, Dest::Into(v)) => {
            let vf = foreign_of(live, v, Kind::VTab)?;
            let l = list(live)?;
            let (window, group) = vtab_parts(&l, &vf)?;
            let t = tab_into_window(live, &f, &window)?;
            match group {
                Some(g) => {
                    need_groups(live, "a tab into a vertical tab")?;
                    invoke(live, Ctx::On(&window), format!("iterm2.add_tab_to_group(tab_id: {}, group_id: {})", lit(&t), lit(&g)))?;
                }
                None if groups_supported(live) && tab_by_id(&l, &f).and_then(|(_, x)| x.tab_group_id.clone()).is_some() => {
                    invoke(live, Ctx::On(&window), format!("iterm2.remove_tab_from_group(tab_id: {})", lit(&t)))?;
                }
                None => {}
            }
        }
        (Kind::Tab | Kind::Pane, Dest::NewVTab) => {
            need_groups(live, "a new vertical tab")?;
            let t = if kind == Kind::Pane {
                let v = invoke(live, Ctx::App, format!("iterm2.move_session_to_new_tab(session: {})", lit(&f)))?;
                v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())).ok_or_else(|| internal("iTerm2 did not report the new tab"))?
            } else {
                f.clone()
            };
            let l = list(live)?;
            let w = tab_by_id(&l, &t).and_then(|(w, _)| w.window_id.clone()).ok_or_else(|| not_found("the tab is gone"))?;
            let g = invoke(live, Ctx::On(&w), format!("iterm2.create_tab_group(tab_ids: [{}], name: \"\", color: \"\")", lit(&t)))?;
            let g = g.as_str().unwrap_or_default().to_string();
            return Ok(Done { ids: vec![id_of(live, Kind::VTab, &format!("grp:{g}"))?], lines: vec![] });
        }
        (Kind::VTab, Dest::Into(w)) => {
            let dest = foreign_of(live, w, Kind::Window)?;
            let l = list(live)?;
            let (src, group) = vtab_parts(&l, &f)?;
            let tabs: Vec<String> =
                window_by_id(&l, &src).map(|x| x.tabs.iter().filter(|t| t.tab_group_id.as_deref().filter(|g| !g.is_empty()) == group.as_deref()).filter_map(|t| t.tab_id.clone()).collect()).unwrap_or_default();
            if group.is_some() {
                need_groups(live, "moving a vertical tab")?;
            }
            for t in tabs {
                tab_into_window(live, &t, &dest)?;
            }
        }
        // a window holds one implicit vertical tab on iTerm2 without groups: it is already alone
        (Kind::VTab, Dest::NewWindow) if f.starts_with("win:") => {}
        (k, d) => return Err(Fail::new(FailCode::Unsupported, format!("moving a {} to {d:?}", k.name()))),
    }
    Ok(Done::default())
}

/// Every tab of `from` into window `into`, in order; the emptied windows close by themselves.
fn merge(live: &mut Live, into: Id, from: Vec<Id>) -> Result<Done, Fail> {
    let dest = foreign_of(live, into, Kind::Window)?;
    let l = list(live)?;
    for w in from {
        let wf = foreign_of(live, w, Kind::Window)?;
        if wf == dest {
            continue;
        }
        let tabs: Vec<String> = window_by_id(&l, &wf).map(|x| x.tabs.iter().filter_map(|t| t.tab_id.clone()).collect()).ok_or_else(|| not_found(format!("no window {w}")))?;
        for t in tabs {
            tab_into_window(live, &t, &dest)?;
        }
    }
    Ok(Done::default())
}

// ---------------------------------------------------------------- window cells, broadcast

/// Resize `window` so `pane` (else the selected tab's active pane) holds `cols` × `rows`; the top-left stays.
/// iTerm2 snaps a window to whole cells and shares growth among panes, so this converges over a few passes.
fn window_cells(live: &mut Live, window: Id, pane: Option<Id>, cols: Option<u16>, rows: Option<u16>) -> Result<Done, Fail> {
    let w = foreign_of(live, window, Kind::Window)?;
    let pane = pane.map(|p| session_of(live, p)).transpose()?;
    let mut got = (0, 0);
    for pass in 0..5 {
        let l = list(live)?;
        let win = window_by_id(&l, &w).ok_or_else(|| not_found(format!("no window {window}")))?;
        let uuid = match &pane {
            Some(u) => u.clone(),
            None => win
                .tabs
                .iter()
                .find(|t| t.tab_id == win.selected_tab_id)
                .or(win.tabs.first())
                .and_then(|t| t.active_session_id.clone().or_else(|| sessions_in(t).into_iter().next()))
                .ok_or_else(|| internal("the window has no pane"))?,
        };
        let s = win.tabs.iter().find_map(|t| t.root.as_ref().and_then(|r| summary(r, &uuid))).ok_or_else(|| invalid("the pane is not in that window (or is hidden by a maximized pane)"))?;
        let g = s.grid_size.unwrap_or_default();
        got = (g.width.unwrap_or(0), g.height.unwrap_or(0));
        // measured after each resize; the last pass only measures
        if pass == 4 || (cols.is_none_or(|c| c as i32 == got.0) && rows.is_none_or(|r| r as i32 == got.1)) {
            break;
        }
        let pf = s.frame.unwrap_or_default().size.unwrap_or_default();
        let wf = win.frame.unwrap_or_default();
        let (o, z) = (wf.origin.unwrap_or_default(), wf.size.unwrap_or_default());
        let nf = layout::frame_for_cells(
            (o.x.unwrap_or(0) as f64, o.y.unwrap_or(0) as f64, z.width.unwrap_or(0) as f64, z.height.unwrap_or(0) as f64),
            (pf.width.unwrap_or(0) as f64, pf.height.unwrap_or(0) as f64),
            got,
            (cols, rows),
        );
        run_all(live, vec![frame_req(&w, Rect { x: nf.0 as f32, y: nf.1 as f32, w: nf.2 as f32, h: nf.3 as f32 })])?;
    }
    Ok(Done { ids: vec![], lines: vec![format!("{}x{}", got.0, got.1)] })
}

/// The app variable that remembers the broadcast mode and its window (iTerm2 keeps only the domains).
const BROADCAST_VAR: &str = "user.iterm_broadcast";

fn broadcast(live: &mut Live, mode: Option<BroadcastMode>, window: Option<Id>, add: Vec<Id>, remove: Vec<Id>) -> Result<Done, Fail> {
    use p::variable_request::Scope;
    let reqs = vec![
        Req::GetBroadcastDomainsRequest(p::GetBroadcastDomainsRequest {}),
        Req::VariableRequest(p::VariableRequest { scope: Some(Scope::App(true)), get: vec![BROADCAST_VAR.into()], set: vec![] }),
    ];
    let mut it = live.api.call_many(reqs).map_err(Fail::from)?.into_iter();
    let current: Vec<String> = match it.next() {
        Some(Ok(Resp::GetBroadcastDomainsResponse(r))) => r.broadcast_domains.into_iter().flat_map(|d| d.session_ids).collect(),
        _ => return Err(internal("unexpected reply to broadcast domains")),
    };
    let stored: String = match it.next() {
        Some(Ok(Resp::VariableResponse(v))) => v.values.first().and_then(|x| serde_json::from_str::<Value>(x).ok()).and_then(|x| x.as_str().map(str::to_string)).unwrap_or_default(),
        _ => String::new(),
    };
    let report = |live: &mut Live, mode: BroadcastMode, window: Option<Id>, targets: &[String]| -> Result<Done, Fail> {
        let ids = targets.iter().filter_map(|s| live.ids.get(Kind::Pane, s)).collect();
        Ok(Done { ids, lines: vec![mode.name().into(), window.map(|w| w.0.to_string()).unwrap_or_default()] })
    };
    let Some(mode) = mode.or((!add.is_empty() || !remove.is_empty()).then_some(BroadcastMode::Pick)) else {
        // get: what iTerm2 broadcasts to now, under the mode the CLI last set
        if current.is_empty() {
            return report(live, BroadcastMode::Off, None, &[]);
        }
        let mut parts = stored.split_whitespace();
        let m = parts.next().and_then(BroadcastMode::parse).filter(|m| *m != BroadcastMode::Off).unwrap_or(BroadcastMode::Pick);
        let w = parts.next().and_then(|x| x.parse().ok()).map(Id);
        return report(live, m, w, &current);
    };
    let to_uuids = |live: &Live, v: &[Id]| v.iter().map(|i| session_of(live, *i)).collect::<Result<Vec<_>, _>>();
    let (targets, scope) = match mode {
        BroadcastMode::Off => (vec![], None),
        BroadcastMode::Pick => (layout::pick(&current, &to_uuids(live, &add)?, &to_uuids(live, &remove)?), None),
        m => {
            let w = match window {
                Some(w) => w,
                None => focused(live, Kind::Window)?.ok_or_else(|| invalid("no focused window: broadcast set --in WINDOW"))?,
            };
            (scoped_targets(live, w, m)?, Some(w))
        }
    };
    // iTerm2 drops a domain of one session (typing there reaches nobody else): that is off
    let targets = if targets.len() < 2 { vec![] } else { targets };
    let domains = if targets.is_empty() { vec![] } else { vec![p::BroadcastDomain { session_ids: targets.clone() }] };
    let remember = format!("{} {}", mode.name(), scope.map(|w| w.0.to_string()).unwrap_or_default());
    run_all(
        live,
        vec![
            Req::SetBroadcastDomainsRequest(p::SetBroadcastDomainsRequest { broadcast_domains: domains }),
            var_req(Scope::App(true), &[("iterm_broadcast".into(), Some(remember.trim().to_string()))]),
        ],
    )?;
    let mode = if targets.is_empty() { BroadcastMode::Off } else { mode };
    report(live, mode, scope, &targets)
}

/// The panes a scoped broadcast mode reaches in window `w`: all of them, its selected tab's, or the agent panes of
/// either (or of the selected tab's vertical tab).
fn scoped_targets(live: &mut Live, w: Id, mode: BroadcastMode) -> Result<Vec<String>, Fail> {
    let snap = super::snapshot::snapshot(live)?;
    let ix = crate::app::model::Index::new(&snap);
    let wi = ix.get(w).ok_or_else(|| not_found(format!("no window {w}")))?;
    let wf = foreign_of(live, w, Kind::Window)?;
    let l = list(live)?;
    let sel = window_by_id(&l, &wf).and_then(|x| x.selected_tab_id.clone()).and_then(|t| live.ids.get(Kind::Tab, &t));
    let sel_i = sel.and_then(|t| ix.get(t));
    let scope = match mode {
        BroadcastMode::Tab | BroadcastMode::TabAgents => sel_i.ok_or_else(|| invalid("the window has no selected tab"))?,
        BroadcastMode::VTab => sel_i.and_then(|t| snap.nodes[t].parent.map(|p| p as usize)).ok_or_else(|| invalid("the window has no selected tab"))?,
        _ => wi,
    };
    let agents_only = matches!(mode, BroadcastMode::VTab | BroadcastMode::Window | BroadcastMode::TabAgents);
    let mut out = Vec::new();
    let mut stack = vec![scope];
    while let Some(i) = stack.pop() {
        let n = ix.node(i);
        if n.kind == Kind::Pane && (!agents_only || crate::view::is_agent_kind(n.var(crate::view::V_KIND))) {
            if let Some((_, u)) = live.ids.foreign(n.id) {
                out.push(u.to_string());
            }
        }
        stack.extend(ix.children(i).iter().rev().map(|c| *c as usize));
    }
    Ok(out)
}

pub fn call(live: &mut Live, op: Op, wait: Option<Duration>) -> Result<Done, Fail> {
    let _ = wait;
    match op {
        Op::NewWindow { spawn, frame, focus } => new_window(live, spawn, frame, focus),
        Op::NewVTab { window, name, spawn, focus } => new_vtab(live, window, name, spawn, focus),
        Op::NewTab { vtab, title, spawn, focus } => new_tab(live, vtab, title, spawn, focus),
        Op::Split { at, side, ratio, spawn, focus } => split(live, at, side, ratio, spawn, focus),
        Op::Close { ids } => close(live, ids),
        Op::Set { id, field } => set(live, id, field),
        Op::Select { id } => select(live, id),
        Op::Move { id, to } => move_node(live, id, to),
        Op::Order { ids } => order(live, ids),
        Op::SetLayout { tab, layout } => set_layout(live, tab, layout),
        Op::Merge { into, from } => merge(live, into, from),
        Op::WindowCells { window, pane, cols, rows } => window_cells(live, window, pane, cols, rows),
        Op::Broadcast { mode, window, add, remove } => broadcast(live, mode, window, add, remove),
        other => Err(unsupported(format!("{other:?}").split([' ', '{', '(']).next().unwrap_or("op"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_runs_clean_with_env_and_cwd() {
        let s = Spawn { cwd: Some("/tmp/x y".into()), cmd: Some("claude --model opus".into()), env: vec![("ITERM_PARENT".into(), "p1".into())], vars: vec![], name: None };
        let props = props_with(&s, "/bin/zsh", Some(Path::new("/st/launch/1.once")));
        let get = |k: &str| props.iter().find(|p| p.key.as_deref() == Some(k)).and_then(|p| p.json_value.clone()).unwrap();
        assert_eq!(get("Custom Directory"), "\"Recycle\"");
        let cmd: String = serde_json::from_str(&get("Command")).unwrap();
        assert!(cmd.contains("unset CLAUDE_CODE_CHILD_SESSION"), "{cmd}");
        assert!(cmd.contains("cd '\\''/tmp/x y'\\'' 2>/dev/null || { echo "), "{cmd}");
        assert!(cmd.contains("the command did not run\" >&2; exec /bin/zsh -l; }; export ITERM_PARENT=p1; claude --model opus"), "{cmd}");
        assert!(cmd.contains("/bin/rm /st/launch/1.once 2>/dev/null || exec /bin/zsh -l; cd "), "{cmd}");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    /// Runs a launch's `Command` the way iTerm2 does (through `sh -c`), with a stand-in login shell that records how
    /// it was started, `n` times (the first launch, then Cmd-D copies of its profile).
    fn launch_times(s: &Spawn, with_once: bool, n: usize) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("iterm-once-{}-{}", std::process::id(), s.cmd.as_deref().map(str::len).unwrap_or(0)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let shell = dir.join("fakesh");
        std::fs::write(&shell, format!("#!/bin/sh\n[ \"$1\" = -lc ] && exec /bin/sh -c \"$2\"\necho \"login $* in $PWD\" >> {}\n", out.display())).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let once = dir.join("m.once");
        std::fs::write(&once, "").unwrap();
        let mut s = s.clone();
        s.cmd = s.cmd.map(|c| c.replace("OUT", &out.display().to_string()));
        let props = props_with(&s, &shell.to_string_lossy(), with_once.then_some(once.as_path()));
        let cmd: String = serde_json::from_str(props.iter().find(|p| p.key.as_deref() == Some("Command")).unwrap().json_value.as_ref().unwrap()).unwrap();
        for _ in 0..n {
            assert!(std::process::Command::new("/bin/sh").arg("-c").arg(&cmd).status().unwrap().success());
        }
        let got = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        got
    }

    #[test]
    fn a_copy_of_the_profile_gets_a_login_shell_not_the_command() {
        let s = Spawn { cmd: Some("echo \"agent $A\" >> OUT".into()), env: vec![("A".into(), "x y".into())], ..Default::default() };
        // first launch: the command, with its environment; the copy (Cmd-D): a plain login shell, twice over
        assert_eq!(launch_times(&s, true, 1), "agent x y\n");
        assert_eq!(launch_times(&s, true, 3), format!("agent x y\nlogin -l in {0}\nlogin -l in {0}\n", std::env::current_dir().unwrap().display()));
        // without the guard the copy would rerun the command
        assert_eq!(launch_times(&s, false, 2), "agent x y\nagent x y\n");
    }

    #[test]
    fn only_the_first_launch_changes_folder() {
        // a copy stays where iTerm2 puts it (the pane's current folder: "Recycle"), not in the launch folder
        let to = std::env::temp_dir().canonicalize().unwrap();
        let s = Spawn { cwd: Some(to.display().to_string()), cmd: Some("pwd >> OUT".into()), ..Default::default() };
        let start = std::env::current_dir().unwrap();
        assert_eq!(launch_times(&s, true, 2), format!("{}\nlogin -l in {}\n", to.display(), start.display()));
        // a folder gone by launch time: a login shell where iTerm2 started it, and the command never runs
        let s = Spawn { cwd: Some("/nonexistent/it's gone".into()), cmd: Some("pwd >> OUT".into()), ..Default::default() };
        assert_eq!(launch_times(&s, true, 1), format!("login -l in {}\n", start.display()));
    }

    #[test]
    fn after_spawn_sets_vars_and_name_in_one_batch() {
        let s = Spawn { vars: vec![("agent.parent".into(), "p1".into())], name: Some("w\"1".into()), ..Default::default() };
        let reqs = after_spawn("U", &s);
        assert_eq!(reqs.len(), 2);
        let Req::VariableRequest(v) = &reqs[0] else { panic!() };
        assert_eq!(v.set[0].name.as_deref(), Some("user.agent__parent"));
        assert_eq!(v.set[0].value.as_deref(), Some("\"p1\""));
        let Req::InvokeFunctionRequest(i) = &reqs[1] else { panic!() };
        assert_eq!(i.invocation.as_deref(), Some("iterm2.set_name(name: \"w\\\"1\")"));
        assert!(after_spawn("U", &Spawn::default()).is_empty());
    }

    #[test]
    fn move_session_sends_before_inverted() {
        assert_eq!(move_session("A", "B", false, false), "iterm2.move_session(session: \"A\", destination: \"B\", vertical: false, before: true)");
    }

    #[test]
    fn sides_map_to_iterm2_splits() {
        assert_eq!(side_axis(Side::Left), (true, true));
        assert_eq!(side_axis(Side::Down), (false, false));
    }

    #[test]
    fn vtab_parts_find_groups() {
        let mut l = List::default();
        l.windows.push(Win { window_id: Some("W".into()), tabs: vec![Tab { tab_id: Some("3".into()), tab_group_id: Some("G".into()), ..Default::default() }], ..Default::default() });
        assert_eq!(vtab_parts(&l, "win:W").unwrap(), ("W".into(), None));
        assert_eq!(vtab_parts(&l, "grp:G").unwrap(), ("W".into(), Some("G".into())));
        assert_eq!(vtab_parts(&l, "grp:X").unwrap_err().code, FailCode::NotFound);
    }

    #[test]
    fn refusals_are_one_line() {
        use p::invoke_function_response::{Disposition, Error};
        let r = p::InvokeFunctionResponse {
            disposition: Some(Disposition::Error(Error { status: Some(1), error_reason: Some("The operation couldn’t be completed. No method found on <PseudoTerminal: 0x1 …> with signature x".into()) })),
        };
        assert_eq!(invoke_value(&r).unwrap_err().msg, "iTerm2 refused: No method found");
        assert!(refused(&Resp::SetTabLayoutResponse(p::SetTabLayoutResponse { status: Some(2) })).is_err());
        assert!(refused(&Resp::SetTabLayoutResponse(p::SetTabLayoutResponse { status: Some(0) })).is_ok());
    }
}
