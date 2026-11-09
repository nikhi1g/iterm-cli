//! The model from iTerm2: windows → vertical tabs (tab groups; one implicit vertical tab per window when the app
//! reports none, var `implicit=1`) → tabs → split tree → panes, with variables, frames, focus, broadcast and agent
//! info. Owner: the `see` agent.
//!
//! One snapshot costs two pipelined round trips, three when agents run: (1) ListSessions, Focus, broadcast domains
//! and the app's variables; (2) every pane's, tab's and window's variables; (3) the screens of agent panes only (a
//! permission prompt is seen only there). Between (2) and (3) the CLI reads each pane's foreground process
//! (`sense::proc_of`, one sysctl) and a Claude pane's session record (one small file). Events: [`super::events`].
//!
//! Agent states persist between commands in `STATE/agents.tsv` (`uuid  provider  state`), so a turn seen running in
//! one command reads `idle` in the next; exit statuses are files `STATE/exits/<uuid>` (see [`exit_wrapper`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::app::agent::{self, Provider, Screen, Signal, State};
use crate::app::{Broadcast, BroadcastMode, Fail, FailCode, Id, Kind, Layout, Node, PaneInfo, Rect, Snapshot};

use super::proto::{self as p, client_originated_message::Submessage as Req, server_originated_message::Submessage as Resp};
use super::sense::{self, ClaudeRecord};
use super::Live;

/// A snapshot with the raw iTerm2 answer it came from (kept for the command's later operations).
pub struct Built {
    pub snap: Snapshot,
    pub list: p::ListSessionsResponse,
}

/// The session variables a snapshot reads for every pane (pipelined, one batch), then [`user_names`]. Indices are
/// `V_*`.
pub const PANE_VARS: [&str; 8] = ["path", "jobName", "jobPid", "pid", "autoNameFormat", "terminalWindowName", "terminalIconName", "profileName"];
const V_PATH: usize = 0;
const V_JOB: usize = 1;
const V_JOBPID: usize = 2;
const V_PID: usize = 3;
const V_AUTONAME: usize = 4;
const V_WINTITLE: usize = 5;
const V_ICONTITLE: usize = 6;
const V_PROFILE: usize = 7;

/// A tab's and a window's variables: the name the human or the CLI gave it, then [`user_names`].
pub const SCOPE_VARS: [&str; 1] = ["titleOverride"];

/// The CLI's user variables, read by name: iTerm2 answers `user` with null and `*` costs 3.5× as much (every
/// variable of the session and its parents). Any other key a write sets is listed in [`INDEX`] and read in one
/// more small batch, only for the nodes that have one.
pub const USER_VARS: [&str; 17] = [
    "name", "color", "zoom", "sidebar", "lend", "groups", "agent.parent", "agent.kind", "agent.name", "agent.task", "agent.cmd",
    "agent.model", "agent.effort", "agent.worktree", "agent.ports", "agent.provider", "agent.state",
];

/// The user variable listing a node's other user variables (CLI names, space-separated): `user.vars`.
pub const INDEX: &str = "vars";

/// The iTerm2 names of the index and [`USER_VARS`], in the order a reply's values follow.
pub fn user_names() -> Vec<String> {
    std::iter::once(INDEX).chain(USER_VARS).map(|k| format!("user.{}", var_to_iterm(k))).collect()
}

/// Whether `key` must be listed in the index to be seen.
pub fn indexed(key: &str) -> bool {
    !USER_VARS.contains(&key) && key != INDEX
}

/// The index after setting (Some) or unsetting (None) `sets` on a node whose index is `now`: None when unchanged.
/// A write of user variables adds this to its sets (`user.vars`), so the snapshot finds keys outside USER_VARS.
pub fn index_after(now: Option<&str>, sets: &[(String, Option<String>)]) -> Option<String> {
    let mut keys: Vec<String> = now.unwrap_or_default().split_whitespace().map(String::from).collect();
    let before = keys.clone();
    for (k, v) in sets.iter().filter(|(k, _)| indexed(k)) {
        match v {
            Some(_) if !keys.contains(k) => keys.push(k.clone()),
            None => keys.retain(|x| x != k),
            _ => {}
        }
    }
    (keys != before).then(|| keys.join(" "))
}

/// The vars the app's agent table sets on a pane (perf term's `set_agent`, plus Claude's own record).
pub const A_PROVIDER: &str = "agent.provider";
pub const A_STATE: &str = "agent.state";
pub const A_MODEL: &str = "agent.model";
pub const A_EFFORT: &str = "agent.effort";
/// Claude Code's session ID and name (`claude --resume ID`).
pub const A_SESSION: &str = "agent.session";
pub const A_SESSION_NAME: &str = "agent.session_name";

/// iTerm2 user variables cannot hold dots in a name segment: the CLI's `agent.parent` is stored as `agent__parent`.
pub fn var_to_iterm(key: &str) -> String {
    key.replace('.', "__")
}

pub fn var_from_iterm(key: &str) -> String {
    key.replace("__", ".")
}

/// The vertical tab a tab belongs to: its tab group, else its window's implicit one.
pub fn vtab_foreign(window: &str, group: Option<&str>) -> String {
    match group.filter(|g| !g.is_empty()) {
        Some(g) => format!("grp:{g}"),
        None => format!("win:{window}"),
    }
}

fn rect_of(f: Option<&p::Frame>) -> Option<Rect> {
    let f = f?;
    let o = f.origin.unwrap_or_default();
    let z = f.size.unwrap_or_default();
    Some(Rect { x: o.x.unwrap_or(0) as f32, y: o.y.unwrap_or(0) as f32, w: z.width.unwrap_or(0) as f32, h: z.height.unwrap_or(0) as f32 })
}

fn fail(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::Internal, m)
}

/// A pane as ListSessions reports it.
struct RawPane {
    uuid: String,
    title: String,
    frame: Option<Rect>,
    cols: u16,
    rows: u16,
}

fn raw_pane(s: &p::SessionSummary) -> RawPane {
    let g = s.grid_size.unwrap_or_default();
    RawPane {
        uuid: s.unique_identifier.clone().unwrap_or_default(),
        title: s.title.clone().unwrap_or_default(),
        frame: rect_of(s.frame.as_ref()),
        cols: g.width.unwrap_or(0).max(0) as u16,
        rows: g.height.unwrap_or(0).max(0) as u16,
    }
}

/// A subtree of the split tree: its layout, its frame in points when iTerm2 gave a consistent one, and its size in
/// cells.
struct Sub {
    layout: Layout,
    pts: Option<Rect>,
    cells: (f32, f32),
}

/// A frame worth using: a positive size.
fn real(f: Option<Rect>) -> Option<Rect> {
    f.filter(|f| f.w > 0.0 && f.h > 0.0)
}

/// Fractions along a split's axis, from (in order of trust) the children's frames when they lie side by side
/// without overlap, their sizes in cells, or equal shares. A background tab iTerm2 never laid out may report no
/// usable frames; it never yields a fraction that is negative, zero or NaN.
fn fractions(vertical: bool, kids: &[Sub]) -> (Vec<f32>, Option<Rect>) {
    let along = |r: &Rect| if vertical { (r.x, r.w) } else { (r.y, r.h) };
    let pts: Option<Vec<Rect>> = kids.iter().map(|k| k.pts).collect();
    let pts = pts.filter(|v| v.windows(2).all(|w| {
        let ((a, aw), (b, _)) = (along(&w[0]), along(&w[1]));
        b > a && b + 1.0 >= a + aw
    }));
    let norm = |sizes: Vec<f32>| -> Option<Vec<f32>> {
        let total: f32 = sizes.iter().sum();
        (sizes.iter().all(|x| *x > 0.0 && x.is_finite()) && total > 0.0).then(|| sizes.iter().map(|x| x / total).collect())
    };
    if let Some(v) = &pts {
        if let Some(f) = norm(v.iter().map(|r| along(r).1).collect()) {
            let (x0, y0) = (v.iter().map(|r| r.x).fold(f32::MAX, f32::min), v.iter().map(|r| r.y).fold(f32::MAX, f32::min));
            let (x1, y1) = (v.iter().map(|r| r.x + r.w).fold(0.0, f32::max), v.iter().map(|r| r.y + r.h).fold(0.0, f32::max));
            return (f, Some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }));
        }
    }
    let cells = kids.iter().map(|k| if vertical { k.cells.0 } else { k.cells.1 }).collect();
    (norm(cells).unwrap_or_else(|| vec![1.0 / kids.len() as f32; kids.len()]), None)
}

/// The split tree with fractions (see [`fractions`]), and its panes in order.
fn walk(node: &p::SplitTreeNode, panes: &mut Vec<RawPane>, ids: &HashMap<String, Id>) -> Option<Sub> {
    use p::split_tree_node::split_tree_link::Child;
    let vertical = node.vertical.unwrap_or(false);
    let mut kids: Vec<Sub> = Vec::new();
    for link in &node.links {
        match &link.child {
            Some(Child::Session(s)) => {
                let r = raw_pane(s);
                let id = *ids.get(&r.uuid)?;
                kids.push(Sub { layout: Layout::Pane(id), pts: real(r.frame), cells: (r.cols as f32, r.rows as f32) });
                panes.push(r);
            }
            Some(Child::Node(n)) => kids.extend(walk(n, panes, ids)),
            None => {}
        }
    }
    if kids.len() <= 1 {
        return kids.pop();
    }
    // vertical: side by side, so widths add and heights are shared
    let cells = if vertical {
        (kids.iter().map(|k| k.cells.0).sum(), kids.iter().map(|k| k.cells.1).fold(0.0, f32::max))
    } else {
        (kids.iter().map(|k| k.cells.0).fold(0.0, f32::max), kids.iter().map(|k| k.cells.1).sum())
    };
    let (f, pts) = fractions(vertical, &kids);
    let children = f.into_iter().zip(kids).map(|(f, k)| (f, k.layout)).collect();
    Some(Sub { layout: Layout::Split { vertical, children }, pts, cells })
}

fn sessions_of(node: &p::SplitTreeNode, out: &mut Vec<String>) {
    use p::split_tree_node::split_tree_link::Child;
    for link in &node.links {
        match &link.child {
            Some(Child::Session(s)) => out.push(s.unique_identifier.clone().unwrap_or_default()),
            Some(Child::Node(n)) => sessions_of(n, out),
            None => {}
        }
    }
}

/// A tab's panes outside its split tree: floating ones (var `floating=1`) and minimized ones (`minimized=1`).
fn loose(t: &p::list_sessions_response::Tab) -> impl Iterator<Item = (&p::SessionSummary, &'static str)> {
    let floating = t.floating_panes.iter().filter_map(|f| f.session.as_ref()).map(|s| (s, "floating"));
    floating.chain(t.minimized_sessions.iter().map(|s| (s, "minimized")))
}

/// A tab's session UUIDs: the split tree's in order, then floating and minimized ones.
fn tab_sessions(t: &p::list_sessions_response::Tab, out: &mut Vec<String>) {
    if let Some(r) = &t.root {
        sessions_of(r, out);
    }
    out.extend(loose(t).filter_map(|(x, _)| x.unique_identifier.clone()));
}

/// Each pane's screen height in rows (ListSessions' grid size), by session UUID.
fn screen_rows(list: &p::ListSessionsResponse) -> HashMap<String, u16> {
    use p::split_tree_node::split_tree_link::Child;
    fn walk(node: &p::SplitTreeNode, out: &mut HashMap<String, u16>) {
        for link in &node.links {
            match &link.child {
                Some(Child::Session(s)) => {
                    let r = raw_pane(s);
                    out.insert(r.uuid, r.rows);
                }
                Some(Child::Node(n)) => walk(n, out),
                None => {}
            }
        }
    }
    let mut out = HashMap::new();
    for t in list.windows.iter().flat_map(|w| &w.tabs) {
        if let Some(r) = &t.root {
            walk(r, &mut out);
        }
        for (s, _) in loose(t) {
            let r = raw_pane(s);
            out.insert(r.uuid, r.rows);
        }
    }
    out
}

/// How many bottom rows of an agent's screen a snapshot reads first. iTerm2's main thread pays ~0.22 ms per line
/// returned, after it answers (bench/latency-2026-11-07.md): a 49-row screen for each of 30 agents stalled it for
/// 70-95 ms on every command. `classify` looks at the bottom `agent::BOTTOM` non-blank rows, so these suffice
/// whenever they hold that many; else the whole screen is read, as before.
const SCREEN_TAIL: u16 = 24;

/// The read that gives `classify` what it needs: the bottom [`SCREEN_TAIL`] rows of a taller screen, else the
/// screen.
fn screen_req(uuid: &str, rows: u16, whole: bool) -> Req {
    let line_range = if whole || rows == 0 || rows <= SCREEN_TAIL {
        p::LineRange { screen_contents_only: Some(true), ..Default::default() }
    } else {
        // the buffer ends with the screen's last row, so its last lines are the screen's bottom rows
        p::LineRange { trailing_lines: Some(SCREEN_TAIL as i32), ..Default::default() }
    };
    Req::GetBufferRequest(p::GetBufferRequest { session: Some(uuid.to_string()), line_range: Some(line_range), include_styles: Some(false) })
}

/// Whether a read's lines settle `classify` as the whole screen would: they hold `agent::BOTTOM` non-blank rows.
fn enough(contents: &[p::LineContents]) -> bool {
    contents.iter().filter(|l| !l.text.as_deref().unwrap_or("").replace('\0', "").trim().is_empty()).count() >= agent::BOTTOM
}

/// Every pane's session UUID, in tree order.
pub fn all_sessions(list: &p::ListSessionsResponse) -> Vec<String> {
    let mut out = Vec::new();
    for w in &list.windows {
        for t in &w.tabs {
            tab_sessions(t, &mut out);
        }
    }
    out
}

fn decode(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::Null)
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The user variables in a reply's values from [`user_names`] on (unset ones skipped), sorted, and the index's
/// other keys.
fn user_vars(values: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let get = |i: usize| values.get(i).map(|r| decode(r)).unwrap_or(Value::Null);
    let index: Vec<String> = text(&get(0)).split_whitespace().filter(|k| indexed(k)).map(String::from).collect();
    let mut user: Vec<(String, String)> = USER_VARS.iter().enumerate().filter_map(|(i, k)| match get(i + 1) {
        Value::Null => None,
        v => Some((k.to_string(), text(&v))),
    }).collect();
    user.sort();
    (user, index)
}

/// Sets `key` in sorted vars, replacing any value it had.
pub fn put_var(vars: &mut Vec<(String, String)>, key: &str, value: String) {
    match vars.binary_search_by(|(k, _)| k.as_str().cmp(key)) {
        Ok(i) => vars[i].1 = value,
        Err(i) => vars.insert(i, (key.to_string(), value)),
    }
}

/// What the variable batch says about one pane.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct PaneVars {
    pub cwd: String,
    pub job: String,
    pub job_pid: i32,
    pub pid: i32,
    /// The name the CLI (`user.name`) or the human (Edit Session) gave the pane; empty when it only shows its title
    /// or profile.
    pub name: String,
    /// The terminal title (OSC 0/2), empty when the program set none.
    pub title: String,
    pub user: Vec<(String, String)>,
    /// User variables outside USER_VARS the index names (read in another batch).
    pub more: Vec<String>,
}

pub fn pane_vars(values: &[String]) -> PaneVars {
    let get = |i: usize| values.get(i).map(|r| decode(r)).unwrap_or(Value::Null);
    let (user, more) = user_vars(values.get(PANE_VARS.len()..).unwrap_or_default());
    let title = [V_WINTITLE, V_ICONTITLE].into_iter().map(|i| text(&get(i))).find(|t| !t.is_empty()).unwrap_or_default();
    let auto = text(&get(V_AUTONAME));
    let profile = text(&get(V_PROFILE));
    let given = user.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone());
    // iTerm2's own name follows the title and the profile; a different one was typed by the human
    let typed = (!auto.is_empty() && auto != profile && auto != title && !auto.contains("\\(") && text(&get(V_ICONTITLE)) != auto).then_some(auto);
    let num = |i: usize| match get(i) {
        Value::String(s) => s.parse().unwrap_or(0),
        v => v.as_i64().unwrap_or(0) as i32,
    };
    PaneVars { cwd: text(&get(V_PATH)), job: text(&get(V_JOB)), job_pid: num(V_JOBPID), pid: num(V_PID), name: given.or(typed).unwrap_or_default(), title, user, more }
}

/// A tab's or a window's variables: (name, user vars, the index's other keys).
pub type ScopeVars = (String, Vec<(String, String)>, Vec<String>);

fn scope_vars(values: &[String]) -> ScopeVars {
    let (user, more) = user_vars(values.get(SCOPE_VARS.len()..).unwrap_or_default());
    (values.first().map(|r| text(&decode(r))).unwrap_or_default(), user, more)
}

/// Everything iTerm2 answered for one snapshot.
#[derive(Default)]
pub struct Raw {
    pub list: p::ListSessionsResponse,
    pub focus: Vec<p::FocusChangedNotification>,
    pub domains: Vec<Vec<String>>,
    pub app_pid: i32,
    pub app_version: String,
    /// Session UUID → its `PANE_VARS` answers, parsed once.
    pub panes: HashMap<String, PaneVars>,
    /// Tab ID → its `SCOPE_VARS` answers.
    pub tabs: HashMap<String, ScopeVars>,
    /// Window ID → its `SCOPE_VARS` answers.
    pub windows: HashMap<String, ScopeVars>,
    /// Session UUID, tab ID or window ID → vars added to the node: user variables the index named, the agent
    /// table.
    pub extra: HashMap<String, Vec<(String, String)>>,
    /// Session UUID → recorded exit status.
    pub exits: HashMap<String, i32>,
}

/// Builds the snapshot from iTerm2's answers (pure: the tests feed it canned responses).
pub fn build(raw: &Raw, ids: &mut super::ids::IdMap) -> Result<Snapshot, Fail> {
    let list = &raw.list;
    // every object's ID first, in one lock
    let mut want: Vec<(Kind, String)> = vec![(Kind::App, "app".into())];
    for w in &list.windows {
        let wid = w.window_id.clone().unwrap_or_default();
        want.push((Kind::Window, wid.clone()));
        for t in &w.tabs {
            want.push((Kind::VTab, vtab_foreign(&wid, t.tab_group_id.as_deref())));
            want.push((Kind::Tab, t.tab_id.clone().unwrap_or_default()));
            let mut s = Vec::new();
            tab_sessions(t, &mut s);
            want.extend(s.into_iter().map(|u| (Kind::Pane, u)));
        }
    }
    let refs: Vec<(Kind, &str)> = want.iter().map(|(k, f)| (*k, f.as_str())).collect();
    let got = ids.assign(&refs).map_err(|e| fail(format!("id map: {e}")))?;
    // the file keeps what is live (buried panes too: they come back) and the newest dead; a failed compaction
    // leaves the file as it was, and the snapshot stands
    let buried: Vec<&str> = list.buried_sessions.iter().filter_map(|s| s.unique_identifier.as_deref()).collect();
    let live: Vec<(Kind, &str)> = refs.iter().copied().chain(buried.into_iter().map(|u| (Kind::Pane, u))).collect();
    let _ = ids.compact_if_due(&live);
    let id_of: HashMap<(Kind, String), Id> = want.into_iter().zip(got).collect();
    let pane_ids: HashMap<String, Id> = id_of.iter().filter(|((k, _), _)| *k == Kind::Pane).map(|((_, f), id)| (f.clone(), *id)).collect();

    let mut nodes = vec![Node { id: id_of[&(Kind::App, "app".to_string())], kind: Kind::App, parent: None, name: "iTerm2".into(), rect: None, created: 0, vars: vec![], pane: None, layout: None }];
    for w in &list.windows {
        let wid = w.window_id.clone().unwrap_or_default();
        let wi = nodes.len() as u32;
        let wnode = id_of[&(Kind::Window, wid.clone())];
        let (wname, mut wvars, _) = raw.windows.get(&wid).cloned().unwrap_or_default();
        for (k, x) in raw.extra.get(&wid).into_iter().flatten() {
            put_var(&mut wvars, k, x.clone());
        }
        nodes.push(Node { id: wnode, kind: Kind::Window, parent: Some(0), name: wname, rect: rect_of(w.frame.as_ref()), created: wnode.0, vars: wvars, pane: None, layout: None });
        // vertical tabs in the order their first tab appears
        let mut vtab_index: HashMap<String, u32> = HashMap::new();
        for t in &w.tabs {
            let vf = vtab_foreign(&wid, t.tab_group_id.as_deref());
            let vi = match vtab_index.get(&vf) {
                Some(i) => *i,
                None => {
                    let i = nodes.len() as u32;
                    let vid = id_of[&(Kind::VTab, vf.clone())];
                    let mut vars = vec![];
                    if t.tab_group_collapsed == Some(true) {
                        vars.push(("collapsed".to_string(), "1".to_string()));
                    }
                    if let Some(c) = t.tab_group_color.as_deref().filter(|c| !c.is_empty()) {
                        vars.push(("color".to_string(), c.to_string()));
                    }
                    if vf.starts_with("win:") {
                        vars.push(("implicit".to_string(), "1".to_string()));
                    }
                    nodes.push(Node { id: vid, kind: Kind::VTab, parent: Some(wi), name: t.tab_group_name.clone().unwrap_or_default(), rect: None, created: vid.0, vars, pane: None, layout: None });
                    vtab_index.insert(vf, i);
                    i
                }
            };
            let tfor = t.tab_id.clone().unwrap_or_default();
            let tid = id_of[&(Kind::Tab, tfor.clone())];
            let ti = nodes.len() as u32;
            let mut raws: Vec<(RawPane, Option<&str>)> = Vec::new();
            let mut tree = Vec::new();
            let layout = t.root.as_ref().and_then(|r| walk(r, &mut tree, &pane_ids)).map(|x| x.layout);
            let (tname, mut tvars, _) = raw.tabs.get(&tfor).cloned().unwrap_or_default();
            for (k, x) in raw.extra.get(&tfor).into_iter().flatten() {
                put_var(&mut tvars, k, x.clone());
            }
            nodes.push(Node { id: tid, kind: Kind::Tab, parent: Some(vi), name: tname, rect: None, created: tid.0, vars: tvars, pane: None, layout });
            raws.extend(tree.into_iter().map(|r| (r, None)));
            raws.extend(loose(t).map(|(s, how)| (raw_pane(s), Some(how))));
            // pane rects in cells within the tab: from point frames scaled by the pane's cell size
            for (r, how) in raws {
                // a loose pane reported without its identifier has no ID: never a panic over one odd answer
                let Some(&pid) = pane_ids.get(&r.uuid) else { continue };
                let v = raw.panes.get(&r.uuid).cloned().unwrap_or_default();
                // a pane iTerm2 gave no usable frame (a background tab never laid out) has no rect
                let rect = real(r.frame).map(|f| {
                    let (cw, ch) = if r.cols > 0 && r.rows > 0 && f.w > 0.0 && f.h > 0.0 { (f.w / r.cols as f32, f.h / r.rows as f32) } else { (1.0, 1.0) };
                    Rect { x: (f.x / cw).round(), y: (f.y / ch).round(), w: r.cols as f32, h: r.rows as f32 }
                });
                let mut vars = v.user;
                for (k, x) in raw.extra.get(&r.uuid).into_iter().flatten() {
                    put_var(&mut vars, k, x.clone());
                }
                if let Some(how) = how {
                    put_var(&mut vars, how, "1".into());
                }
                let title = if v.title.is_empty() && !raw.panes.contains_key(&r.uuid) { r.title } else { v.title };
                nodes.push(Node {
                    id: pid,
                    kind: Kind::Pane,
                    parent: Some(ti),
                    name: v.name,
                    rect,
                    created: pid.0,
                    vars,
                    pane: Some(PaneInfo { pid: v.pid, cwd: v.cwd, title, job: v.job, cols: r.cols, rows: r.rows, exit: raw.exits.get(&r.uuid).copied() }),
                    layout: None,
                });
            }
        }
    }
    let focus = focus_of(list, &raw.focus, &id_of, &pane_ids);
    let broadcast = broadcast_of(&nodes, &raw.domains, &pane_ids);
    Ok(Snapshot { seq: 0, nodes, focus, app_version: raw.app_version.clone(), app_pid: raw.app_pid, broadcast, awake: String::new() })
}

/// The key window (else the current one, else the first), its selected tab and that tab's active pane.
fn focus_of(list: &p::ListSessionsResponse, focus: &[p::FocusChangedNotification], id_of: &HashMap<(Kind, String), Id>, pane_ids: &HashMap<String, Id>) -> Vec<Id> {
    use p::focus_changed_notification::{window::WindowStatus, Event as F};
    let (mut key, mut current, mut tab, mut session) = (None, None, None, None);
    for n in focus {
        match &n.event {
            Some(F::Window(w)) if w.window_status == Some(WindowStatus::TerminalWindowBecameKey as i32) => key = w.window_id.clone(),
            Some(F::Window(w)) if w.window_status == Some(WindowStatus::TerminalWindowIsCurrent as i32) => current = w.window_id.clone(),
            Some(F::SelectedTab(t)) if tab.is_none() => tab = Some(t.clone()),
            Some(F::Session(s)) if session.is_none() => session = Some(s.clone()),
            _ => {}
        }
    }
    let win = key.or(current).and_then(|k| list.windows.iter().find(|w| w.window_id.as_ref() == Some(&k))).or(list.windows.first());
    let Some(win) = win else { return vec![] };
    let wid = win.window_id.clone().unwrap_or_default();
    let mut out = vec![id_of[&(Kind::Window, wid.clone())]];
    let sel = win.selected_tab_id.clone().or(tab);
    let t = sel.as_ref().and_then(|s| win.tabs.iter().find(|x| x.tab_id.as_ref() == Some(s))).or(win.tabs.first());
    if let Some(t) = t {
        out.push(id_of[&(Kind::VTab, vtab_foreign(&wid, t.tab_group_id.as_deref()))]);
        out.push(id_of[&(Kind::Tab, t.tab_id.clone().unwrap_or_default())]);
        let active = t.active_session_id.clone().or(session);
        if let Some(p) = active.and_then(|a| pane_ids.get(&a)) {
            out.push(*p);
        }
    }
    out
}

/// iTerm2's broadcast domains as perf term's broadcast: one domain covering a tab is `tab`, a window `all`,
/// anything else `pick`.
fn broadcast_of(nodes: &[Node], domains: &[Vec<String>], pane_ids: &HashMap<String, Id>) -> Broadcast {
    let mut targets: Vec<Id> = domains.iter().flatten().filter_map(|u| pane_ids.get(u).copied()).collect();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return Broadcast::default();
    }
    let at = |id: Id| nodes.iter().position(|n| n.id == id);
    let ancestor = |mut i: usize, kind: Kind| loop {
        if nodes[i].kind == kind {
            return Some(i);
        }
        i = nodes[i].parent? as usize;
    };
    let under = |root: usize| -> Vec<Id> {
        let mut out: Vec<Id> = nodes.iter().enumerate().filter(|(i, n)| n.kind == Kind::Pane && ancestor(*i, nodes[root].kind) == Some(root)).map(|(_, n)| n.id).collect();
        out.sort();
        out
    };
    let first = at(targets[0]).unwrap_or(0);
    let window = ancestor(first, Kind::Window);
    let tab = ancestor(first, Kind::Tab);
    let mode = if domains.len() == 1 && tab.is_some_and(|t| under(t) == targets) {
        BroadcastMode::Tab
    } else if domains.len() == 1 && window.is_some_and(|w| under(w) == targets) {
        BroadcastMode::All
    } else {
        BroadcastMode::Pick
    };
    Broadcast { mode, window: window.map(|w| nodes[w].id), targets }
}

// ---- agents -------------------------------------------------------------------------------------------------

/// What one snapshot saw of an agent pane.
pub struct Seen<'a> {
    pub provider: Option<Provider>,
    pub title: &'a str,
    pub record: Option<&'a ClaudeRecord>,
    /// The screen's meaning, when it was read.
    pub screen: Option<Screen>,
    /// The CLI started it with a task (`agent.task`): its first input box is not "ready".
    pub has_task: bool,
}

/// The agent table's next entry for a pane from what this snapshot saw and what the last one recorded. Pure.
/// Claude's own record (busy/idle) and its title glyph decide busy; the screen finds prompts and finished turns;
/// a pane whose agent left keeps its provider with `exited`.
pub fn agent_next(prev: Option<(Provider, State)>, seen: &Seen) -> Option<(Provider, State)> {
    let Some(p) = seen.provider else {
        return prev.map(|(p, s)| (p, if s.finished() { s } else { State::Exited }));
    };
    let base = match prev {
        Some((q, s)) if q == p && !s.finished() => s,
        _ => State::Starting,
    };
    let screen = seen.screen.unwrap_or(Screen::Unknown);
    let said = match seen.record.map(|r| r.status.as_str()) {
        Some("busy") => Some(true),
        Some("idle") => Some(false),
        _ => agent::title_working(p, seen.title),
    };
    let mut s = match said {
        Some(w) => agent::next_titled(base, Signal::Title(w, screen)),
        None => agent::next(base, Signal::Quiet(screen)),
    };
    if seen.has_task && s == State::Ready {
        s = State::Starting;
    }
    Some((p, s))
}

/// `STATE/agents.tsv`: the agent table between commands.
pub fn agents_path(state: &Path) -> PathBuf {
    state.join("agents.tsv")
}

pub fn read_agents(state: &Path) -> HashMap<String, (Provider, State)> {
    let text = std::fs::read_to_string(agents_path(state)).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (u, p, s) = (f.next()?, f.next()?, f.next()?);
            Some((u.to_string(), (Provider::parse(p)?, State::parse(s)?)))
        })
        .collect()
}

/// Rewrites the table atomically (only the panes that still exist).
pub fn write_agents(state: &Path, table: &HashMap<String, (Provider, State)>) {
    let mut rows: Vec<_> = table.iter().map(|(u, (p, s))| format!("{u}\t{}\t{}\n", p.as_str(), s.as_str())).collect();
    rows.sort();
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = state.join(format!("agents.tsv.{}", std::process::id()));
    let wrote = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(rows.concat().as_bytes()));
    if wrote.is_ok() {
        let _ = std::fs::rename(&tmp, agents_path(state));
    }
}

/// The meaning of a screen read with `screen_contents_only`.
pub fn screen_of(contents: &[p::LineContents]) -> Screen {
    let lines: Vec<String> = contents.iter().map(|l| l.text.as_deref().unwrap_or("").replace('\0', "")).collect();
    agent::classify(lines.iter().map(String::as_str))
}

/// The CLI's vars for one agent pane.
fn agent_vars(entry: (Provider, State), proc: Option<&sense::Proc>, record: Option<&ClaudeRecord>) -> Vec<(String, String)> {
    let (p, s) = entry;
    let mut out = vec![(A_PROVIDER.to_string(), p.as_str().to_string()), (A_STATE.to_string(), s.as_str().to_string())];
    if let Some(proc) = proc {
        let model = proc.flag("--model").or_else(|| proc.flag("-m")).or_else(|| proc.env("ANTHROPIC_MODEL").filter(|_| p == Provider::Claude));
        let effort = proc.flag("--effort").or_else(|| proc.env("CLAUDE_EFFORT").filter(|_| p == Provider::Claude));
        out.extend(model.map(|m| (A_MODEL.to_string(), m)));
        out.extend(effort.map(|e| (A_EFFORT.to_string(), e)));
    }
    if let Some(r) = record.filter(|r| !r.session.is_empty()) {
        out.push((A_SESSION.to_string(), r.session.clone()));
        if !r.name.is_empty() {
            out.push((A_SESSION_NAME.to_string(), r.name.clone()));
        }
    }
    out
}

// ---- exits --------------------------------------------------------------------------------------------------

/// Where a pane's exit status is recorded: `STATE/exits/<session uuid>`.
pub fn exits_dir(state: &Path) -> PathBuf {
    state.join("exits")
}

/// A command line that runs `cmd` and records its exit status for `exits` and the snapshot's `PaneInfo::exit`
/// (`$ITERM_SESSION_ID` names the pane, so the wrapper needs nothing from the caller), then exits with it. `cmd`
/// runs in a subshell, so its own `exit` still records.
pub fn exit_wrapper(state: &Path, cmd: &str) -> String {
    let dir = exits_dir(state);
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    format!("({cmd}); __s=$?; mkdir -p {d} && printf %s \"$__s\" > {d}/\"${{ITERM_SESSION_ID##*:}}\"; exit $__s", d = q(&dir.to_string_lossy()))
}

/// The recorded statuses of these panes: one open each, however many panes have ever exited (the folder only grows).
pub fn read_exits(state: &Path, panes: &[String]) -> HashMap<String, i32> {
    let dir = exits_dir(state);
    if !dir.is_dir() {
        return HashMap::new();
    }
    panes.iter().filter_map(|u| Some((u.clone(), std::fs::read_to_string(dir.join(u)).ok()?.trim().parse().ok()?))).collect()
}

// ---- reading iTerm2 -----------------------------------------------------------------------------------------

fn var_req(scope: p::variable_request::Scope, names: &[&str]) -> Req {
    Req::VariableRequest(p::VariableRequest { scope: Some(scope), get: names.iter().map(|s| s.to_string()).collect(), set: vec![] })
}

fn var_req_owned(scope: p::variable_request::Scope, names: &[String]) -> Req {
    Req::VariableRequest(p::VariableRequest { scope: Some(scope), get: names.to_vec(), set: vec![] })
}

/// `CFBundleShortVersionString` from the app bundle's Info.plist.
fn app_version(bundle: &str) -> String {
    // one read per process: a watcher rebuilds its snapshot on every change, and the bundle doesn't change under it
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| read_app_version(bundle)).clone()
}

fn read_app_version(bundle: &str) -> String {
    let path = if bundle.is_empty() { "/Applications/iTerm.app".to_string() } else { bundle.to_string() };
    let plist = std::fs::read_to_string(format!("{path}/Contents/Info.plist")).unwrap_or_default();
    let Some(at) = plist.find("<key>CFBundleShortVersionString</key>") else { return String::new() };
    let rest = &plist[at..];
    let (Some(a), Some(b)) = (rest.find("<string>"), rest.find("</string>")) else { return String::new() };
    rest.get(a + 8..b).unwrap_or_default().to_string()
}

/// Everything iTerm2 knows, in two pipelined round trips (three when agents run).
pub fn read(api: &mut super::conn::Conn, state: &Path) -> Result<Raw, Fail> {
    use p::variable_request::Scope;
    let replies = api
        .call_many(vec![
            Req::ListSessionsRequest(p::ListSessionsRequest {}),
            Req::FocusRequest(p::FocusRequest {}),
            Req::GetBroadcastDomainsRequest(p::GetBroadcastDomainsRequest {}),
            var_req(Scope::App(true), &["pid", "appBundlePath"]),
        ])
        .map_err(Fail::from)?;
    let mut it = replies.into_iter();
    let Some(Ok(Resp::ListSessionsResponse(list))) = it.next() else { return Err(fail("unexpected reply to list sessions")) };
    let mut raw = Raw { list, ..Raw::default() };
    if let Some(Ok(Resp::FocusResponse(f))) = it.next() {
        raw.focus = f.notifications;
    }
    if let Some(Ok(Resp::GetBroadcastDomainsResponse(b))) = it.next() {
        raw.domains = b.broadcast_domains.into_iter().map(|d| d.session_ids).collect();
    }
    if let Some(Ok(Resp::VariableResponse(v))) = it.next() {
        let get = |i: usize| v.values.get(i).map(|r| text(&decode(r))).unwrap_or_default();
        raw.app_pid = get(0).parse().unwrap_or(0);
        raw.app_version = app_version(&get(1));
    }

    // (2) every pane's, tab's and window's variables, one batch
    let uuids = all_sessions(&raw.list);
    let tabs: Vec<String> = raw.list.windows.iter().flat_map(|w| w.tabs.iter().filter_map(|t| t.tab_id.clone())).collect();
    let wins: Vec<String> = raw.list.windows.iter().filter_map(|w| w.window_id.clone()).collect();
    let user = user_names();
    let names = |fixed: &[&str]| -> Vec<String> { fixed.iter().map(|s| s.to_string()).chain(user.iter().cloned()).collect() };
    let (pane_names, scope_names) = (names(&PANE_VARS), names(&SCOPE_VARS));
    let mut reqs: Vec<Req> = uuids.iter().map(|u| var_req_owned(Scope::SessionId(u.clone()), &pane_names)).collect();
    reqs.extend(tabs.iter().map(|t| var_req_owned(Scope::TabId(t.clone()), &scope_names)));
    reqs.extend(wins.iter().map(|w| var_req_owned(Scope::WindowId(w.clone()), &scope_names)));
    if !reqs.is_empty() {
        let mut answers = api.call_many(reqs).map_err(Fail::from)?.into_iter().map(|r| match r {
            Ok(Resp::VariableResponse(v)) => v.values,
            _ => Vec::new(),
        });
        for u in &uuids {
            raw.panes.insert(u.clone(), pane_vars(&answers.next().unwrap_or_default()));
        }
        for t in &tabs {
            raw.tabs.insert(t.clone(), scope_vars(&answers.next().unwrap_or_default()));
        }
        for w in &wins {
            raw.windows.insert(w.clone(), scope_vars(&answers.next().unwrap_or_default()));
        }
    }
    // (2b) user variables outside USER_VARS, for the nodes whose index names some
    let mut more: Vec<(String, Scope, Vec<String>)> = Vec::new();
    for (u, v) in &raw.panes {
        if !v.more.is_empty() {
            more.push((u.clone(), Scope::SessionId(u.clone()), v.more.clone()));
        }
    }
    for (map, tab) in [(&raw.tabs, true), (&raw.windows, false)] {
        for (id, v) in map {
            if !v.2.is_empty() {
                more.push((id.clone(), if tab { Scope::TabId(id.clone()) } else { Scope::WindowId(id.clone()) }, v.2.clone()));
            }
        }
    }
    if !more.is_empty() {
        let reqs = more.iter().map(|(_, sc, keys)| var_req_owned(sc.clone(), &keys.iter().map(|k| format!("user.{}", var_to_iterm(k))).collect::<Vec<_>>())).collect();
        for ((id, _, keys), r) in more.iter().zip(api.call_many(reqs).map_err(Fail::from)?) {
            if let Ok(Resp::VariableResponse(v)) = r {
                let got = keys.iter().zip(v.values.iter()).filter_map(|(k, r)| match decode(r) {
                    Value::Null => None,
                    x => Some((k.clone(), text(&x))),
                });
                raw.extra.entry(id.clone()).or_default().extend(got);
            }
        }
    }

    // (3) agents: the foreground process of each pane; screens of agent panes only
    let none = PaneVars::default();
    let mut found: Vec<(String, Provider, Option<sense::Proc>, Option<ClaudeRecord>, &PaneVars)> = Vec::new();
    let mut gone: Vec<&String> = Vec::new();
    for u in &uuids {
        let v = raw.panes.get(u).unwrap_or(&none);
        let job = if v.job_pid > 0 { v.job_pid } else { v.pid };
        let proc = sense::proc_of(job);
        match proc.as_ref().and_then(sense::Proc::provider) {
            Some(p) => {
                let record = if p == Provider::Claude { sense::claude_record(job) } else { None };
                found.push((u.clone(), p, proc, record, v));
            }
            None => gone.push(u),
        }
    }
    let mut screens: HashMap<String, Screen> = HashMap::new();
    if !found.is_empty() {
        // the bottom rows first; the whole screen only where they hold too few non-blank rows to settle it
        let rows = screen_rows(&raw.list);
        let rows_of = |u: &str| rows.get(u).copied().unwrap_or(0);
        let reqs = found.iter().map(|(u, ..)| screen_req(u, rows_of(u), false)).collect();
        let mut again: Vec<&String> = Vec::new();
        for ((u, ..), r) in found.iter().zip(api.call_many(reqs).map_err(Fail::from)?) {
            if let Ok(Resp::GetBufferResponse(b)) = r {
                if b.status.unwrap_or(0) == 0 {
                    if rows_of(u) > SCREEN_TAIL && !enough(&b.contents) {
                        again.push(u);
                    } else {
                        screens.insert(u.clone(), screen_of(&b.contents));
                    }
                }
            }
        }
        if !again.is_empty() {
            let reqs = again.iter().map(|u| screen_req(u, rows_of(u), true)).collect();
            for (u, r) in again.iter().zip(api.call_many(reqs).map_err(Fail::from)?) {
                if let Ok(Resp::GetBufferResponse(b)) = r {
                    if b.status.unwrap_or(0) == 0 {
                        screens.insert((*u).clone(), screen_of(&b.contents));
                    }
                }
            }
        }
    }
    let prev = read_agents(state);
    let mut table = HashMap::new();
    for (u, provider, proc, record, v) in &found {
        let has_task = v.user.iter().any(|(k, _)| k == "agent.task");
        let seen = Seen { provider: Some(*provider), title: &v.title, record: record.as_ref(), screen: screens.get(u).copied(), has_task };
        if let Some(e) = agent_next(prev.get(u).copied(), &seen) {
            table.insert(u.clone(), e);
            raw.extra.entry(u.clone()).or_default().extend(agent_vars(e, proc.as_ref(), record.as_ref()));
        }
    }
    for u in gone {
        let seen = Seen { provider: None, title: "", record: None, screen: None, has_task: false };
        if let Some(e) = agent_next(prev.get(u).copied(), &seen) {
            table.insert(u.clone(), e);
            raw.extra.entry(u.clone()).or_default().extend(agent_vars(e, None, None));
        }
    }
    // rewritten only on a change (a closed pane's row drops out then too)
    if table != prev {
        write_agents(state, &table);
    }
    raw.exits = read_exits(state, &uuids);
    Ok(raw)
}

pub fn snapshot(live: &mut Live) -> Result<Snapshot, Fail> {
    if let Some(b) = &live.cache {
        return Ok(b.snap.clone());
    }
    let raw = read(&mut live.api, &live.env.state)?;
    let snap = build(&raw, &mut live.ids)?;
    live.cache = Some(Built { snap: snap.clone(), list: raw.list });
    Ok(snap)
}

/// Exit statuses recorded for panes (open or closed): `STATE/exits/<uuid>`.
pub fn exits(live: &mut Live, ids: Vec<Id>) -> Result<Vec<(Id, Option<i32>)>, Fail> {
    let dir = exits_dir(&live.env.state);
    Ok(ids
        .into_iter()
        .map(|id| {
            let code = match live.ids.foreign(id) {
                Some((Kind::Pane, u)) => std::fs::read_to_string(dir.join(u)).ok().and_then(|t| t.trim().parse().ok()),
                _ => None,
            };
            (id, code)
        })
        .collect())
}

pub fn watch(live: &mut Live, since: Option<u64>) -> Result<(Box<dyn crate::app::Events>, Snapshot), Fail> {
    super::events::watch(live, since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p::split_tree_node::{split_tree_link::Child, SplitTreeLink};

    fn sess(u: &str, x: i32, y: i32, w: i32, h: i32) -> Child {
        Child::Session(p::SessionSummary {
            unique_identifier: Some(u.into()),
            frame: Some(p::Frame { origin: Some(p::Point { x: Some(x), y: Some(y) }), size: Some(p::Size { width: Some(w), height: Some(h) }) }),
            grid_size: Some(p::Size { width: Some(w / 10), height: Some(h / 20) }),
            title: Some(format!("{u} title")),
        })
    }

    fn split(vertical: bool, kids: Vec<Child>) -> p::SplitTreeNode {
        p::SplitTreeNode { vertical: Some(vertical), links: kids.into_iter().map(|c| SplitTreeLink { child: Some(c) }).collect() }
    }

    fn tab(id: &str, root: p::SplitTreeNode, group: Option<&str>) -> p::list_sessions_response::Tab {
        p::list_sessions_response::Tab { tab_id: Some(id.into()), root: Some(root), tab_group_id: group.map(String::from), tab_group_name: group.map(|g| format!("{g} name")), ..Default::default() }
    }

    /// Two windows: w-a with a tab split left | (top / bottom) at 1:1 and 2:1, and a second tab; w-b with two tabs in
    /// one tab group.
    fn canned() -> Raw {
        let t1 = tab("1", split(true, vec![sess("A", 0, 0, 400, 600), Child::Node(split(false, vec![sess("B", 400, 0, 400, 400), sess("C", 400, 400, 400, 200)]))]), None);
        let t2 = tab("2", split(false, vec![sess("D", 0, 0, 800, 600)]), None);
        let t3 = tab("3", split(false, vec![sess("E", 0, 0, 800, 600)]), Some("G"));
        let mut t4 = tab("4", split(false, vec![sess("F", 0, 0, 800, 600)]), Some("G"));
        t4.floating_panes.push(p::FloatingPane { session: Some(p::SessionSummary { unique_identifier: Some("H".into()), ..Default::default() }) });
        let list = p::ListSessionsResponse {
            windows: vec![
                p::list_sessions_response::Window { window_id: Some("pty-a".into()), tabs: vec![t1, t2], selected_tab_id: Some("1".into()), frame: Some(p::Frame { origin: Some(p::Point { x: Some(10), y: Some(20) }), size: Some(p::Size { width: Some(800), height: Some(600) }) }), number: None },
                p::list_sessions_response::Window { window_id: Some("pty-b".into()), tabs: vec![t3, t4], selected_tab_id: Some("4".into()), frame: None, number: None },
            ],
            buried_sessions: vec![],
        };
        use p::focus_changed_notification::{window::WindowStatus, Event as F, Window};
        let focus = vec![p::FocusChangedNotification { event: Some(F::Window(Window { window_status: Some(WindowStatus::TerminalWindowBecameKey as i32), window_id: Some("pty-b".into()) })) }];
        let mut raw = Raw { list, focus, app_pid: 42, ..Raw::default() };
        let pv = |path: &str, user: &[(&str, &str)], auto: &str, title: &str| -> PaneVars {
            let mut v = vec![format!("{path:?}"), "\"zsh\"".into(), "\"7\"".into(), "\"6\"".into(), format!("{auto:?}"), format!("{title:?}"), "null".into(), "\"Default\"".into()];
            v.extend(uvals(user));
            pane_vars(&v)
        };
        raw.panes.insert("A".into(), pv("/a", &[("agent.parent", "p1"), ("name", "lead"), (INDEX, "mine")], "Default", ""));
        raw.panes.insert("B".into(), pv("/b", &[], "✳ Task", "✳ Task"));
        raw.panes.insert("C".into(), pv("/c", &[], "typed by hand", ""));
        raw.tabs.insert("2".into(), scope_vars(&std::iter::once("\"docs\"".to_string()).chain(uvals(&[("color", "#ff0000")])).collect::<Vec<_>>()));
        raw.windows.insert("pty-b".into(), scope_vars(&std::iter::once("\"second\"".to_string()).chain(uvals(&[])).collect::<Vec<_>>()));
        raw.extra.insert("B".into(), vec![(A_PROVIDER.into(), "claude".into()), (A_STATE.into(), "busy".into())]);
        raw.exits.insert("D".into(), 3);
        raw.domains = vec![vec!["E".into()]];
        raw
    }

    /// A reply's values for `user_names()`: the index, then USER_VARS (null when unset).
    fn uvals(set: &[(&str, &str)]) -> Vec<String> {
        std::iter::once(INDEX).chain(USER_VARS).map(|k| set.iter().find(|(x, _)| *x == k).map(|(_, v)| format!("{v:?}")).unwrap_or("null".into())).collect()
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iterm-snap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_snapshot_from_canned_answers() {
        let dir = tmp("canned");
        let mut ids = super::super::ids::IdMap::open(&dir).unwrap();
        let s = build(&canned(), &mut ids).unwrap();
        let kinds: String = s.nodes.iter().map(|n| n.kind.letter()).collect();
        // app; w-a: implicit vtab, t1 (A B C), t2 (D); w-b: group vtab, t3 (E), t4 (F, floating H)
        assert_eq!(kinds, "awvtppptpwvtptpp");
        let by = |u: &str| s.nodes.iter().find(|n| ids.get(Kind::Pane, u) == Some(n.id)).unwrap();
        // structure and names
        assert_eq!(s.nodes[2].var("implicit"), Some("1"));
        assert_eq!(s.nodes[10].var("implicit"), None);
        assert_eq!(s.nodes[10].name, "G name");
        assert_eq!(s.nodes[9].name, "second");
        assert_eq!(s.nodes[7].name, "docs");
        assert_eq!(s.nodes[7].var("color"), Some("#ff0000"));
        assert_eq!(by("A").name, "lead");
        assert_eq!(by("A").var("agent.parent"), Some("p1"));
        assert_eq!(by("A").var(INDEX), None, "the index is not a var");
        assert_eq!(by("B").name, "", "a title is not a name");
        assert_eq!(by("B").pane.as_ref().unwrap().title, "✳ Task");
        assert_eq!(by("C").name, "typed by hand");
        assert_eq!(by("B").var(A_STATE), Some("busy"));
        assert_eq!(by("D").pane.as_ref().unwrap().exit, Some(3));
        assert_eq!(by("H").var("floating"), Some("1"));
        let a = by("A").pane.as_ref().unwrap();
        assert_eq!((a.cwd.as_str(), a.pid, a.cols, a.rows), ("/a", 6, 40, 30));
        assert_eq!(by("C").rect, Some(Rect { x: 40.0, y: 20.0, w: 40.0, h: 10.0 }));
        assert_eq!(s.nodes[1].rect, Some(Rect { x: 10.0, y: 20.0, w: 800.0, h: 600.0 }));
        // the split tree, fractions from the frames
        let Some(Layout::Split { vertical: true, children }) = &s.nodes[3].layout else { panic!("{:?}", s.nodes[3].layout) };
        assert_eq!(children[0], (0.5, Layout::Pane(by("A").id)));
        let Layout::Split { vertical: false, children: inner } = &children[1].1 else { panic!() };
        assert!((inner[0].0 - 2.0 / 3.0).abs() < 1e-6);
        // focus: the key window, its selected tab, that tab's first pane when iTerm2 names none
        assert_eq!(s.focus, vec![s.nodes[9].id, s.nodes[10].id, s.nodes[13].id]);
        assert_eq!((s.app_pid, s.broadcast.mode, s.broadcast.targets.clone()), (42, BroadcastMode::Tab, vec![by("E").id]));
        // the same answers give the same IDs; a new pane gets a new one
        let again = build(&canned(), &mut super::super::ids::IdMap::open(&dir).unwrap()).unwrap();
        assert_eq!(again.nodes.iter().map(|n| n.id).collect::<Vec<_>>(), s.nodes.iter().map(|n| n.id).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unusable_frames_fall_back_to_cells_then_equal_shares() {
        let pane = |x: f32, w: f32, cols: f32| Sub { layout: Layout::Pane(Id(1)), pts: real(Some(Rect { x, y: 0.0, w, h: 10.0 })), cells: (cols, 5.0) };
        // side by side: the frames
        assert_eq!(fractions(true, &[pane(0.0, 100.0, 1.0), pane(101.0, 300.0, 1.0)]).0, [0.25, 0.75]);
        // overlapping frames (a background tab): the cells
        assert_eq!(fractions(true, &[pane(0.0, 400.0, 30.0), pane(0.0, 400.0, 10.0)]).0, [0.75, 0.25]);
        // zero frames and zero cells: equal shares
        assert_eq!(fractions(true, &[pane(0.0, 0.0, 0.0), pane(0.0, 0.0, 0.0)]).0, [0.5, 0.5]);
    }

    // regression: a floating pane reported without an identifier panicked the whole snapshot
    #[test]
    fn a_loose_pane_without_an_identifier_is_skipped() {
        let dir = tmp("noid");
        let mut raw = canned();
        raw.list.windows[0].tabs[0].floating_panes.push(p::FloatingPane { session: Some(p::SessionSummary::default()) });
        let s = build(&raw, &mut super::super::ids::IdMap::open(&dir).unwrap()).unwrap();
        assert_eq!(s.nodes.iter().filter(|n| n.kind == Kind::Pane).count(), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_key_window_falls_back_to_the_first() {
        let dir = tmp("nokey");
        let mut raw = canned();
        raw.focus.clear();
        let s = build(&raw, &mut super::super::ids::IdMap::open(&dir).unwrap()).unwrap();
        assert_eq!(s.focus[..3], [s.nodes[1].id, s.nodes[2].id, s.nodes[3].id]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seen<'a>(p: Option<Provider>, title: &'a str, record: Option<&'a ClaudeRecord>, screen: Option<Screen>, task: bool) -> Seen<'a> {
        Seen { provider: p, title, record, screen, has_task: task }
    }

    #[test]
    fn the_agent_table_moves_through_a_turn() {
        let c = Some(Provider::Claude);
        // a fresh Claude at its input box is ready; its spinning title is busy; the turn ends idle
        let s = agent_next(None, &seen(c, "✳ Claude Code", None, Some(Screen::Ready), false)).unwrap();
        assert_eq!(s, (Provider::Claude, State::Ready));
        let s = agent_next(Some(s), &seen(c, "◐ Working", None, Some(Screen::Busy), false)).unwrap();
        assert_eq!(s.1, State::Busy);
        let s = agent_next(Some(s), &seen(c, "✳ Done", None, Some(Screen::Ready), false)).unwrap();
        assert_eq!(s.1, State::Idle);
        // a permission prompt holds whatever the title says
        let s = agent_next(Some(s), &seen(c, "◐ Working", None, Some(Screen::Prompt), false)).unwrap();
        assert_eq!(s.1, State::Prompt);
        // Claude's own record beats its title
        let busy = ClaudeRecord { status: "busy".into(), ..Default::default() };
        let s = agent_next(Some((Provider::Claude, State::Idle)), &seen(c, "✳ x", Some(&busy), Some(Screen::Ready), false)).unwrap();
        assert_eq!(s.1, State::Busy);
        // the agent left: the pane keeps its provider, exited
        assert_eq!(agent_next(Some(s), &seen(None, "", None, None, false)), Some((Provider::Claude, State::Exited)));
        assert_eq!(agent_next(None, &seen(None, "", None, None, false)), None);
        // a new agent in that pane starts over
        assert_eq!(agent_next(Some((Provider::Claude, State::Exited)), &seen(c, "✳ x", None, Some(Screen::Ready), false)).unwrap().1, State::Ready);
        // started with a task: not ready until its turn ran (or the screen shows it finished)
        assert_eq!(agent_next(None, &seen(c, "✳ x", None, Some(Screen::Ready), true)).unwrap().1, State::Starting);
        assert_eq!(agent_next(None, &seen(c, "✳ x", None, Some(Screen::Answered), true)).unwrap().1, State::Idle);
        // an untitled agent (Codex) reads its screen
        let x = Some(Provider::Codex);
        let s = agent_next(None, &seen(x, "", None, Some(Screen::Busy), false)).unwrap();
        assert_eq!(s.1, State::Busy);
        assert_eq!(agent_next(Some(s), &seen(x, "", None, Some(Screen::Ready), false)).unwrap().1, State::Idle);
    }

    #[test]
    fn the_agent_table_persists() {
        let dir = tmp("agents");
        std::fs::create_dir_all(&dir).unwrap();
        let t: HashMap<String, (Provider, State)> = [("U1".to_string(), (Provider::Codex, State::Busy))].into_iter().collect();
        write_agents(&dir, &t);
        assert_eq!(read_agents(&dir), t);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_files_round_trip_through_the_wrapper() {
        let dir = tmp("exits");
        std::fs::create_dir_all(&dir).unwrap();
        let line = exit_wrapper(&dir, "exit 7");
        let st = std::process::Command::new("/bin/sh").arg("-c").arg(format!("({line})")).env("ITERM_SESSION_ID", "w0t0p0:UU-1").status().unwrap();
        assert_eq!(st.code(), Some(7));
        assert_eq!(read_exits(&dir, &["UU-1".to_string(), "UU-2".to_string()]), HashMap::from([("UU-1".to_string(), 7)]));
        assert_eq!(read_exits(&dir, &["UU-2".to_string()]), HashMap::new(), "only the panes asked about");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pane_names_are_given_not_titles() {
        let v = |auto: &str, title: &str, user: &[(&str, &str)]| {
            let mut x: Vec<String> = vec!["\"/\"".into(), "\"zsh\"".into(), "1".into(), "\"2\"".into(), format!("{auto:?}"), format!("{title:?}"), "null".into(), "\"Default\"".into()];
            x.extend(uvals(user));
            pane_vars(&x)
        };
        assert_eq!(v("Default", "", &[]).name, "");
        assert_eq!(v("◐ busy", "◐ busy", &[]).name, "");
        assert_eq!(v("mine", "", &[]).name, "mine");
        assert_eq!(v("mine", "", &[("name", "cli")]).name, "cli");
        assert_eq!((v("x", "", &[]).job_pid, v("x", "", &[]).pid), (1, 2));
        let x = v("x", "", &[("agent.kind", "claude"), (INDEX, "foo agent.kind bar.baz")]);
        assert_eq!(x.user, vec![("agent.kind".to_string(), "claude".to_string())]);
        assert_eq!(x.more, ["foo", "bar.baz"], "known keys are read anyway");
    }

    #[test]
    fn the_index_lists_other_keys() {
        let set = |k: &str, v: Option<&str>| vec![(k.to_string(), v.map(String::from))];
        assert_eq!(index_after(None, &set("agent.parent", Some("p1"))), None, "a known key needs no index");
        assert_eq!(index_after(None, &set("foo", Some("1"))).as_deref(), Some("foo"));
        assert_eq!(index_after(Some("foo"), &set("foo", Some("2"))), None);
        assert_eq!(index_after(Some("foo bar"), &set("foo", None)).as_deref(), Some("bar"));
    }

    // perf finding 2026-11-07: whole agent screens stalled iTerm2's main thread ~0.22 ms per line after every
    // snapshot; the bottom rows settle the state the same way whenever they hold BOTTOM non-blank rows
    #[test]
    fn an_agent_screen_is_read_from_its_bottom_rows_with_the_same_verdict() {
        let range = |r: &Req| match r {
            Req::GetBufferRequest(g) => g.line_range.clone().unwrap(),
            _ => panic!(),
        };
        assert_eq!(range(&screen_req("U", 49, false)).trailing_lines, Some(SCREEN_TAIL as i32));
        assert_eq!(range(&screen_req("U", 22, false)).screen_contents_only, Some(true), "a short screen is read whole");
        assert_eq!(range(&screen_req("U", 49, true)).screen_contents_only, Some(true));
        assert_eq!(range(&screen_req("U", 0, false)).screen_contents_only, Some(true), "unknown height: whole");
        let lc = |s: &str| p::LineContents { text: Some(s.into()), ..Default::default() };
        // a prompt far up, then a busy agent's output filling the bottom: the tail says busy, as the screen does
        let mut screen: Vec<p::LineContents> = vec![lc("Do you want to proceed?"), lc("❯ 1. Yes")];
        screen.extend((0..40).map(|i| lc(&format!("output line {i}"))));
        screen.extend([lc("✽ Mulling… (3m 9s · esc to interrupt)"), lc(""), lc("❯ "), lc("")]);
        let tail = &screen[screen.len() - SCREEN_TAIL as usize..];
        assert!(enough(tail));
        assert_eq!(screen_of(tail), screen_of(&screen));
        assert_eq!(screen_of(tail), Screen::Busy);
        // a fresh session: a few rows at the top, blanks below: too few in the tail, so the whole screen is read
        let mut fresh = vec![lc("Claude Code"), lc("─────"), lc("❯ "), lc("─────")];
        fresh.extend((0..45).map(|_| lc("")));
        assert!(!enough(&fresh[fresh.len() - SCREEN_TAIL as usize..]));
    }
}
