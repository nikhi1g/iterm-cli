//! iTerm2: content, menus, actions, ui dialogs, search, settings, quit. Owner: the `appctl` agent.
//!
//! - [`menu`]: the main menu (iTerm2's `MenuItemRequest`, items from its own menu file) and perf term's
//!   key-map actions mapped onto it. An item or action aimed at a node runs with that node focused; the human's
//!   focus is put back afterwards unless the action is itself a focus action.
//! - [`prefs`]: settings, profiles, color presets and key bindings (`ListProfiles`, `SetProfileProperty`,
//!   `PreferencesRequest`, `ColorPresetRequest`; new profiles are iTerm2 dynamic profiles).
//! - [`content`]: selection, copy, marks, open, find (`SelectionRequest`, `GetBufferRequest`).
//!
//! iTerm2's API exposes no dialogs and no right-click menus: `UiList` answers an empty list and `UiAnswer`
//! refuses; a node's context menu is the main menu run with that node focused.

pub mod content;
pub mod menu;
pub mod prefs;

use std::time::Duration;

use crate::app::{Done, Fail, FailCode, Id, Kind, Op};

use super::proto::{self as p, client_originated_message::Submessage as Req, server_originated_message::Submessage as Resp};
use super::{unsupported, Live};

pub(crate) fn invalid(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::Invalid, m)
}

pub(crate) fn internal(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::Internal, m)
}

fn lines(lines: Vec<String>) -> Result<Done, Fail> {
    Ok(Done { ids: vec![], lines })
}

pub fn call(live: &mut Live, op: Op, wait: Option<Duration>) -> Result<Done, Fail> {
    let _ = wait;
    match op {
        Op::Menu { context, item: None } => {
            let _ = context; // the same main menu, run with the node focused
            let all: Vec<menu::Item> = menu::items().collect();
            let states = menu_states(live, &all)?;
            lines(all.iter().zip(states).map(|(i, st)| format!("{}\t{}\t{}", i.path, i.key, st)).collect())
        }
        Op::Menu { context, item: Some(name) } => {
            let item = menu::find(&name).map_err(invalid)?;
            run_item(live, item, context, false)?;
            lines(vec![item.path.to_string()])
        }
        Op::Actions => {
            // the user's own shortcuts (by menu title) over iTerm2's
            let over = prefs::key_overrides(live);
            lines(
                menu::ACTIONS
                    .iter()
                    .map(|x| {
                        let item = menu::action_item(x);
                        let title = item.path.rsplit(" > ").next().unwrap_or(item.path);
                        format!("{}\t{}\t{}", x.name, over.get(title).map_or(item.key, String::as_str), x.does)
                    })
                    .collect(),
            )
        }
        Op::Action { name, pane } => {
            let x = menu::action(&name).ok_or_else(|| {
                invalid(format!("no action {name:?} on iTerm2 (iterm action list); any menu item runs with: iterm app menu ITEM"))
            })?;
            run_item(live, menu::action_item(x), pane, x.focus)?;
            Ok(Done { ids: pane.into_iter().collect(), lines: vec![name] })
        }
        Op::Content { pane, act } => content::call(live, pane, act),
        Op::Search { query, limit } => search(live, &query, limit),
        Op::UiList => lines(vec![]),
        Op::UiAnswer { .. } => Err(unsupported("answering a dialog (iTerm2's API exposes no dialogs; never a permission prompt)")),
        Op::Quit => Err(invalid("quitting iTerm2 from the CLI is off: quit it by hand (⌘Q), so no one's sessions end by accident")),
        other => Err(unsupported(&format!("{other:?}").split([' ', '{', '(']).next().unwrap_or("op").to_string())),
    }
}

pub fn setting(live: &mut Live, op: crate::app::SettingOp) -> Result<Done, Fail> {
    prefs::setting(live, op)
}

/// One request, its reply or the transport's failure.
pub(crate) fn rpc(live: &mut Live, req: Req) -> Result<Resp, Fail> {
    live.api.call(req).map_err(Fail::from)
}

/// Runs a main-menu item; with `at`, as if that node were focused (the human's focus is restored after unless
/// `keep_focus`). The reply's (checked, enabled) before the run.
pub(crate) fn run_item(live: &mut Live, item: menu::Item, at: Option<Id>, keep_focus: bool) -> Result<(), Fail> {
    let before = match at {
        Some(id) => {
            let (uuid, _) = pane_for(live, id)?;
            let before = focus(live)?;
            activate(live, p::activate_request::Identifier::SessionId(uuid), true)?;
            Some(before)
        }
        None => None,
    };
    let r = menu_item(live, item.id, false);
    if let (Some(b), false) = (&before, keep_focus) {
        restore(live, b)?;
    }
    match r {
        // most items act on the key window, and iTerm2 has none while another app is frontmost
        Err(e) if e.msg.contains("is disabled") && !focus(live).map(|f| f.active).unwrap_or(true) => Err(invalid(format!(
            "{}: iTerm2 is in the background, and the item acts on its key window (bring it forward first: iterm pane select SEL)",
            item.path
        ))),
        r => r.map(|_| ()),
    }
}

/// `MenuItemRequest`: (checked, enabled).
pub(crate) fn menu_item(live: &mut Live, id: &str, query_only: bool) -> Result<(bool, bool), Fail> {
    match rpc(live, Req::MenuItemRequest(p::MenuItemRequest { identifier: Some(id.into()), query_only: Some(query_only) }))? {
        Resp::MenuItemResponse(r) => match r.status.unwrap_or(0) {
            0 => Ok((r.checked.unwrap_or(false), r.enabled.unwrap_or(false))),
            1 => Err(invalid(format!("iTerm2 has no menu item {id:?} (an older iTerm2?)"))),
            2 => Err(invalid(format!("menu item {id:?} is disabled now (nothing for it to act on)"))),
            s => Err(internal(format!("menu item {id:?}: status {s}"))),
        },
        _ => Err(internal("unexpected reply to a menu item request")),
    }
}

/// Each item's state now, pipelined in one batch: `checked`, `disabled`, `missing` (not in this iTerm2), or empty.
fn menu_states(live: &mut Live, items: &[menu::Item]) -> Result<Vec<&'static str>, Fail> {
    let reqs = items.iter().map(|i| Req::MenuItemRequest(p::MenuItemRequest { identifier: Some(i.id.into()), query_only: Some(true) })).collect();
    Ok(live
        .api
        .call_many(reqs)
        .map_err(Fail::from)?
        .into_iter()
        .map(|r| match r {
            Ok(Resp::MenuItemResponse(m)) if m.status.unwrap_or(0) == 1 => "missing",
            Ok(Resp::MenuItemResponse(m)) if m.checked.unwrap_or(false) => "checked",
            Ok(Resp::MenuItemResponse(m)) if !m.enabled.unwrap_or(true) => "disabled",
            _ => "",
        })
        .collect())
}

/// The pane a node means for focusing: itself, or the active pane of the tab, vertical tab or window. Its
/// session UUID and iTerm2 tab ID.
fn pane_for(live: &mut Live, id: Id) -> Result<(String, String), Fail> {
    let s = super::snapshot::snapshot(live)?;
    let idx = s.nodes.iter().position(|n| n.id == id).ok_or_else(|| Fail::new(FailCode::NotFound, format!("no node {id}")))?;
    let under = |p: usize, anc: usize| {
        let mut c = Some(p as u32);
        while let Some(i) = c {
            if i as usize == anc {
                return true;
            }
            c = s.nodes[i as usize].parent;
        }
        false
    };
    let focused: Vec<Id> = s.focus.clone();
    let pane = if s.nodes[idx].kind == Kind::Pane {
        idx
    } else {
        let panes: Vec<usize> = (0..s.nodes.len()).filter(|&i| s.nodes[i].kind == Kind::Pane && under(i, idx)).collect();
        *panes.iter().find(|&&i| focused.contains(&s.nodes[i].id)).or(panes.first()).ok_or_else(|| invalid(format!("{id} has no pane")))?
    };
    let mut tab = s.nodes[pane].parent;
    while let Some(t) = tab {
        if s.nodes[t as usize].kind == Kind::Tab {
            break;
        }
        tab = s.nodes[t as usize].parent;
    }
    let uuid = match live.ids.foreign(s.nodes[pane].id) {
        Some((Kind::Pane, u)) => u.to_string(),
        _ => return Err(Fail::new(FailCode::NotFound, format!("no pane {}", s.nodes[pane].id))),
    };
    let tab = tab.and_then(|t| live.ids.foreign(s.nodes[t as usize].id)).map(|(_, f)| f.to_string()).unwrap_or_default();
    Ok((uuid, tab))
}

/// Who has focus: the key window, each window's selected tab and each tab's active session.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Focus {
    pub key_window: Option<String>,
    pub tabs: Vec<String>,
    pub sessions: Vec<String>,
    /// iTerm2 is the frontmost app.
    pub active: bool,
}

pub(crate) fn focus(live: &mut Live) -> Result<Focus, Fail> {
    use p::focus_changed_notification::Event as E;
    let Resp::FocusResponse(r) = rpc(live, Req::FocusRequest(p::FocusRequest {}))? else { return Err(internal("unexpected reply to focus")) };
    let mut f = Focus::default();
    for n in r.notifications {
        match n.event {
            Some(E::Window(w)) if w.window_status == Some(p::focus_changed_notification::window::WindowStatus::TerminalWindowBecameKey as i32) => f.key_window = w.window_id,
            Some(E::ApplicationActive(a)) => f.active = a,
            Some(E::SelectedTab(t)) => f.tabs.push(t),
            Some(E::Session(s)) => f.sessions.push(s),
            _ => {}
        }
    }
    Ok(f)
}

fn activate(live: &mut Live, what: p::activate_request::Identifier, front: bool) -> Result<(), Fail> {
    let req = p::ActivateRequest { order_window_front: Some(front), select_tab: Some(true), select_session: Some(true), activate_app: None, identifier: Some(what) };
    match rpc(live, Req::ActivateRequest(req))? {
        Resp::ActivateResponse(r) if r.status.unwrap_or(0) == 0 => Ok(()),
        Resp::ActivateResponse(r) => Err(invalid(format!("could not focus it (status {})", r.status.unwrap_or(0)))),
        _ => Err(internal("unexpected reply to activate")),
    }
}

/// Puts the focus back as `b` recorded it: each tab's active session, each window's tab, then the key window.
fn restore(live: &mut Live, b: &Focus) -> Result<(), Fail> {
    let now = focus(live)?;
    use p::activate_request::Identifier as I;
    let mut reqs = Vec::new();
    let one = |what: I, tab: bool, session: bool, front: bool| {
        Req::ActivateRequest(p::ActivateRequest { order_window_front: Some(front), select_tab: Some(tab), select_session: Some(session), activate_app: None, identifier: Some(what) })
    };
    for s in b.sessions.iter().filter(|s| !now.sessions.contains(s)) {
        reqs.push(one(I::SessionId(s.clone()), false, true, false));
    }
    for t in b.tabs.iter().filter(|t| !now.tabs.contains(t)) {
        reqs.push(one(I::TabId(t.clone()), true, false, false));
    }
    if let Some(w) = b.key_window.as_ref().filter(|w| now.key_window.as_ref() != Some(*w)) {
        reqs.push(one(I::WindowId(w.clone()), false, false, true));
    }
    if !reqs.is_empty() {
        live.api.call_many(reqs).map_err(Fail::from)?;
    }
    Ok(())
}

/// Unified search over everything named: actions, menu items, profiles, and the windows, tabs and panes by name.
fn search(live: &mut Live, query: &str, limit: u32) -> Result<Done, Fail> {
    let q = query.to_lowercase();
    let hit = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
    let mut out: Vec<String> = Vec::new();
    if let Ok(s) = super::snapshot::snapshot(live) {
        for n in s.nodes.iter().filter(|n| n.kind != Kind::App && !n.name.is_empty() && hit(&n.name)) {
            out.push(format!("{}\t{}\t{}\t{}", n.kind.name(), n.name, n.pane.as_ref().map(|p| p.cwd.clone()).unwrap_or_default(), crate::app::model::handle(n.kind, n.id)));
        }
    }
    for x in menu::ACTIONS.iter().filter(|x| hit(x.name) || hit(x.does)) {
        out.push(format!("Actions\t{}\t{}\t", x.name, menu::action_item(x).key));
    }
    for i in menu::items().filter(|i| hit(i.path)) {
        out.push(format!("Menu\t{}\t{}\t", i.path, i.key));
    }
    if let Ok(names) = prefs::profile_names(live) {
        for n in names.into_iter().filter(|n| hit(n)) {
            out.push(format!("Profiles\t{n}\t\t"));
        }
    }
    out.truncate(limit.max(1) as usize);
    lines(out)
}
