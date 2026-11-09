//! `watch`: iTerm2's notifications as perf term's events. A second API connection (the command's own stays free
//! for reads) runs on a thread: it subscribes once to new/terminated sessions, layout, focus, broadcast, prompt
//! marks, custom escape sequences, screen updates and the variables a snapshot reads; each burst of notifications
//! (coalesced for [`COALESCE`]) rebuilds the snapshot and publishes the difference ([`diff`]) to a [`Hub`]. A pane
//! whose screen changed and then stood still for [`QUIET`] gets a `Quiet` event (and, if an agent runs there, a
//! new snapshot: its state may have changed). A bell is `bellCount` changing. User variables are watched by name
//! (`snapshot::USER_VARS` and the index): a key outside them shows on the next other change. No polling: the thread sleeps in the
//! socket until a notification or a quiet deadline.
//!
//! `--since`: the replay ring lives in this command's [`Hub`], so it covers this stream only (nothing runs between
//! commands to record events).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::{Event, Events, Fail, Hub, HubEvents, Id, Kind, Pushed, Snapshot};

use super::proto::{self as p, client_originated_message::Submessage as Req};
use super::snapshot::{self, A_PROVIDER};
use super::Live;

/// Notifications closer together than this are one change.
pub const COALESCE: Duration = Duration::from_millis(25);
/// A screen unchanged this long after printing is quiet.
pub const QUIET: Duration = Duration::from_millis(800);
/// How often the thread checks that its watcher is still there.
const TICK: Duration = Duration::from_millis(500);

/// Session variables whose change can change the snapshot (bellCount is a bell, not a change).
pub const WATCHED_VARS: [&str; 8] = ["path", "jobName", "jobPid", "autoNameFormat", "terminalWindowName", "terminalIconName", "profileName", "bellCount"];


fn sub(kind: p::NotificationType, session: Option<&str>, args: Option<p::notification_request::Arguments>) -> Req {
    Req::NotificationRequest(p::NotificationRequest { session: session.map(String::from), subscribe: Some(true), notification_type: Some(kind as i32), arguments: args })
}

fn var_monitor(scope: p::VariableScope, name: &str) -> Option<p::notification_request::Arguments> {
    Some(p::notification_request::Arguments::VariableMonitorRequest(p::VariableMonitorRequest { name: Some(name.into()), scope: Some(scope as i32), identifier: Some("all".into()) }))
}

/// Every subscription a watcher needs, in one batch.
pub fn subscriptions() -> Vec<Req> {
    use p::NotificationType as N;
    let mut v = vec![
        sub(N::NotifyOnNewSession, None, None),
        sub(N::NotifyOnTerminateSession, None, None),
        sub(N::NotifyOnLayoutChange, None, None),
        sub(N::NotifyOnFocusChange, None, None),
        sub(N::NotifyOnBroadcastChange, None, None),
        sub(N::NotifyOnPrompt, Some("all"), None),
        sub(N::NotifyOnCustomEscapeSequence, Some("all"), None),
        sub(N::NotifyOnScreenUpdate, Some("all"), None),
    ];
    v.extend(WATCHED_VARS.iter().map(|n| sub(N::NotifyOnVariableChange, None, var_monitor(p::VariableScope::Session, n))));
    v.extend(std::iter::once(&snapshot::INDEX).chain(snapshot::USER_VARS.iter()).map(|n| sub(N::NotifyOnVariableChange, None, var_monitor(p::VariableScope::Session, &format!("user.{}", snapshot::var_to_iterm(n))))));
    // windows accept "all"; tabs answer InvalidIdentifier (iTerm2 3.7.3), so a tab's title change waits for the
    // next other change
    v.push(sub(N::NotifyOnVariableChange, None, var_monitor(p::VariableScope::Window, "titleOverride")));
    v
}

/// What one notification means.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// The model may have changed: build the snapshot again.
    Changed,
    /// A pane's screen changed (quiet follows when it stops).
    Screen(String),
    Bell(String),
    /// OSC 133: `A` prompt, `C` command started, `D` command ended (with its status).
    Prompt(String, char, Option<i32>),
    /// A custom escape sequence (OSC 1337 Custom=id=…): its sender and payload.
    Custom(String, String, String),
    Ignore,
}

/// Maps one notification. Pure.
pub fn note(n: &p::Notification) -> Note {
    use p::prompt_notification::Event as P;
    if let Some(s) = &n.screen_update_notification {
        return Note::Screen(s.session.clone().unwrap_or_default());
    }
    if let Some(v) = &n.variable_changed_notification {
        if v.name.as_deref() == Some("bellCount") {
            return Note::Bell(v.identifier.clone().unwrap_or_default());
        }
        return Note::Changed;
    }
    if let Some(pr) = &n.prompt_notification {
        let s = pr.session.clone().unwrap_or_default();
        return match &pr.event {
            Some(P::Prompt(_)) => Note::Prompt(s, 'A', None),
            Some(P::CommandStart(_)) => Note::Prompt(s, 'C', None),
            Some(P::CommandEnd(e)) => Note::Prompt(s, 'D', e.status),
            None => Note::Ignore,
        };
    }
    if let Some(c) = &n.custom_escape_sequence_notification {
        return Note::Custom(c.session.clone().unwrap_or_default(), c.sender_identity.clone().unwrap_or_default(), c.payload.clone().unwrap_or_default());
    }
    if n.new_session_notification.is_some()
        || n.terminate_session_notification.is_some()
        || n.layout_changed_notification.is_some()
        || n.focus_changed_notification.is_some()
        || n.broadcast_domains_changed.is_some()
        || n.profile_changed_notification.is_some()
    {
        return Note::Changed;
    }
    Note::Ignore
}

/// The events that turn `a` into `b` (perf term's `api::snap::diff`). O(nodes). `exited` remembers panes whose
/// exit was already sent.
pub fn diff(a: &Snapshot, b: &Snapshot, exited: &mut HashSet<Id>) -> Vec<Event> {
    let app = b.nodes.first().map(|n| n.id).unwrap_or(Id(0));
    let mut out = Vec::new();
    let old: HashMap<Id, &crate::app::Node> = a.nodes.iter().map(|n| (n.id, n)).collect();
    let mut seen = HashSet::with_capacity(b.nodes.len());
    for n in &b.nodes {
        seen.insert(n.id);
        let parent = n.parent.map(|p| b.nodes[p as usize].id).unwrap_or(app);
        let Some(o) = old.get(&n.id) else {
            if n.kind != Kind::App {
                out.push(Event::Created { id: n.id, kind: n.kind, parent });
            }
            continue;
        };
        let oparent = o.parent.map(|p| a.nodes[p as usize].id).unwrap_or(app);
        if oparent != parent {
            out.push(Event::Moved { id: n.id, parent });
        }
        if o.name != n.name {
            out.push(Event::Renamed { id: n.id, name: n.name.clone() });
        }
        if o.layout != n.layout && n.kind == Kind::Tab {
            out.push(Event::Layout { tab: n.id });
        }
        if o.vars != n.vars {
            let ov: BTreeMap<&str, &str> = o.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let nv: BTreeMap<&str, &str> = n.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            for (k, v) in &nv {
                if ov.get(k) != Some(v) {
                    out.push(Event::Var { id: n.id, key: k.to_string(), value: Some(v.to_string()) });
                }
            }
            for k in ov.keys().filter(|k| !nv.contains_key(*k)) {
                out.push(Event::Var { id: n.id, key: k.to_string(), value: None });
            }
        }
        if let (Some(op), Some(np)) = (&o.pane, &n.pane) {
            if op.title != np.title {
                out.push(Event::Title { pane: n.id, title: np.title.clone() });
            }
            if op.cwd != np.cwd && !np.cwd.is_empty() {
                out.push(Event::Cwd { pane: n.id, cwd: np.cwd.clone() });
            }
        }
    }
    for n in &b.nodes {
        if let Some(x) = n.pane.as_ref().and_then(|p| p.exit) {
            if exited.insert(n.id) {
                out.push(Event::Exited { pane: n.id, status: x });
            }
        }
    }
    for o in &a.nodes {
        if !seen.contains(&o.id) {
            out.push(Event::Closed { id: o.id, kind: o.kind });
            exited.remove(&o.id);
        }
    }
    if a.focus.last() != b.focus.last() {
        if let Some(p) = b.focus.last() {
            out.push(Event::Focus { id: *p });
        }
    }
    if a.broadcast != b.broadcast {
        out.push(Event::Broadcast { mode: b.broadcast.mode, targets: b.broadcast.targets.clone() });
    }
    out
}

/// Panes whose screen changed and has not stood still for [`QUIET`] yet, in the order of their last change: times
/// only grow, so a change moves its pane to the back, the next deadline is the front and a quiet pane leaves from
/// there. Amortized O(1) per screen update (a scan of every printing pane on each wake-up was O(printing)); the
/// queue holds only the updates of the last QUIET window.
#[derive(Default)]
pub struct Printing {
    last: HashMap<String, Instant>,
    order: VecDeque<(Instant, String)>,
}

impl Printing {
    pub fn changed(&mut self, session: String, at: Instant) {
        self.last.insert(session.clone(), at);
        self.order.push_back((at, session));
    }

    /// Drops entries a later change of their pane made stale, from the front.
    fn clean(&mut self) {
        while let Some((t, s)) = self.order.front() {
            if self.last.get(s) == Some(t) {
                break;
            }
            self.order.pop_front();
        }
    }

    /// When the next pane goes quiet, if one prints.
    pub fn next_quiet(&mut self) -> Option<Instant> {
        self.clean();
        self.order.front().map(|(t, _)| *t + QUIET)
    }

    /// The panes quiet at `now`, oldest change first; they stop printing.
    pub fn quiet(&mut self, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            self.clean();
            match self.order.front() {
                Some((t, _)) if now.saturating_duration_since(*t) >= QUIET => {
                    let Some((_, s)) = self.order.pop_front() else { break };
                    self.last.remove(&s);
                    out.push(s);
                }
                _ => return out,
            }
        }
        out
    }

    /// Entries queued, stale ones included.
    pub fn queued(&self) -> usize {
        self.order.len()
    }
}

/// The panes an agent runs in: a quiet one may have finished its turn.
fn agent_panes(s: &Snapshot) -> HashSet<Id> {
    s.nodes.iter().filter(|n| n.var(A_PROVIDER).is_some()).map(|n| n.id).collect()
}

/// The watcher's end of the stream: dropping it stops the thread.
struct Stream {
    events: HubEvents,
    stop: Arc<AtomicBool>,
}

impl Events for Stream {
    fn next(&mut self, timeout: Option<Duration>) -> Result<Option<Pushed>, Fail> {
        self.events.next(timeout)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn fresh(w: &mut Live) -> Result<Snapshot, Fail> {
    w.cache = None;
    snapshot::snapshot(w)
}

pub fn watch(live: &mut Live, since: Option<u64>) -> Result<(Box<dyn Events>, Snapshot), Fail> {
    let mut w = Live::connect(&live.env)?;
    // subscribe before the first snapshot, so no change falls between them
    for r in w.api.call_many(subscriptions()).map_err(Fail::from)? {
        r.map_err(Fail::from)?;
    }
    let first = fresh(&mut w)?;
    let hub = Hub::new(first.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let (events, snap) = hub.watch(since);
    let flag = stop.clone();
    std::thread::Builder::new().name("iterm-watch".into()).spawn(move || run(w, hub, first, flag)).map_err(|e| Fail::new(crate::app::FailCode::Internal, format!("watch thread: {e}")))?;
    Ok((Box::new(Stream { events, stop }), snap))
}

/// The thread: notifications in, events out, until the watcher drops its stream or the connection ends.
fn run(mut w: Live, hub: Hub, mut last: Snapshot, stop: Arc<AtomicBool>) {
    let mut exited: HashSet<Id> = last.nodes.iter().filter(|n| n.pane.as_ref().is_some_and(|p| p.exit.is_some())).map(|n| n.id).collect();
    let mut agents = agent_panes(&last);
    let mut dirty: Option<Instant> = None;
    let mut printing = Printing::default();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let mut due = now + TICK;
        if let Some(d) = dirty {
            due = due.min(d + COALESCE);
        }
        if let Some(t) = printing.next_quiet() {
            due = due.min(t);
        }
        let n = match w.api.wait_notification(due.saturating_duration_since(now)) {
            Ok(n) => n,
            Err(_) => return,
        };
        if let Some(n) = n {
            let pane = |w: &mut Live, s: &str| w.ids.id(Kind::Pane, s).ok();
            match note(&n) {
                Note::Changed => {
                    dirty.get_or_insert_with(Instant::now);
                }
                Note::Screen(s) => {
                    printing.changed(s, Instant::now());
                }
                Note::Bell(s) => {
                    if let Some(id) = pane(&mut w, &s) {
                        hub.emit(Event::Bell { pane: id });
                    }
                }
                Note::Prompt(s, mark, status) => {
                    if let Some(id) = pane(&mut w, &s) {
                        hub.emit(Event::Prompt { pane: id, mark, status });
                    }
                    // a command's start or end changes the job (and a pane's agent)
                    dirty.get_or_insert_with(Instant::now);
                }
                Note::Custom(s, title, body) => {
                    if let Some(id) = pane(&mut w, &s) {
                        hub.emit(Event::Notify { pane: id, title, body });
                    }
                }
                Note::Ignore => {}
            }
        }
        let now = Instant::now();
        for s in printing.quiet(now) {
            let Some(id) = w.ids.get(Kind::Pane, &s) else { continue };
            hub.emit(Event::Quiet { pane: id });
            // an agent's screen decides prompt and a finished turn
            if agents.contains(&id) {
                dirty.get_or_insert(now);
            }
        }
        if dirty.is_some_and(|d| now.duration_since(d) >= COALESCE) {
            dirty = None;
            let Ok(next) = fresh(&mut w) else { return };
            let evs = diff(&last, &next, &mut exited);
            if !evs.is_empty() || next != last {
                hub.publish(next.clone(), evs);
                agents = agent_panes(&next);
            }
            last = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Node, PaneInfo};

    fn node(id: u64, kind: Kind, parent: Option<u32>) -> Node {
        Node { id: Id(id), kind, parent, name: String::new(), rect: None, created: id, vars: vec![], pane: (kind == Kind::Pane).then(PaneInfo::default), layout: None }
    }

    #[test]
    fn notifications_map_to_notes() {
        let mut n = p::Notification::default();
        n.screen_update_notification = Some(p::ScreenUpdateNotification { session: Some("S".into()) });
        assert_eq!(note(&n), Note::Screen("S".into()));
        let mut n = p::Notification::default();
        n.variable_changed_notification = Some(p::VariableChangedNotification { scope: None, identifier: Some("S".into()), name: Some("bellCount".into()), json_new_value: None });
        assert_eq!(note(&n), Note::Bell("S".into()));
        let mut n = p::Notification::default();
        n.prompt_notification = Some(p::PromptNotification { session: Some("S".into()), unique_prompt_id: None, event: Some(p::prompt_notification::Event::CommandEnd(p::PromptNotificationCommandEnd { status: Some(2) })) });
        assert_eq!(note(&n), Note::Prompt("S".into(), 'D', Some(2)));
        let mut n = p::Notification::default();
        n.terminate_session_notification = Some(p::TerminateSessionNotification { session_id: Some("S".into()) });
        assert_eq!(note(&n), Note::Changed);
        assert_eq!(note(&p::Notification::default()), Note::Ignore);
    }

    #[test]
    fn diff_reports_creation_close_vars_and_exit_once() {
        let a = Snapshot { nodes: vec![node(0, Kind::App, None), node(1, Kind::Window, Some(0)), node(2, Kind::VTab, Some(1)), node(3, Kind::Tab, Some(2)), node(4, Kind::Pane, Some(3))], ..Default::default() };
        let mut b = a.clone();
        b.nodes.push(node(5, Kind::Pane, Some(3)));
        b.nodes[4].vars = vec![("agent.state".into(), "busy".into())];
        b.nodes[4].pane.as_mut().unwrap().exit = Some(0);
        b.focus = vec![Id(5)];
        let mut sent = HashSet::new();
        let ev = diff(&a, &b, &mut sent);
        assert!(ev.contains(&Event::Created { id: Id(5), kind: Kind::Pane, parent: Id(3) }));
        assert!(ev.contains(&Event::Var { id: Id(4), key: "agent.state".into(), value: Some("busy".into()) }));
        assert!(ev.contains(&Event::Exited { pane: Id(4), status: 0 }));
        assert!(ev.contains(&Event::Focus { id: Id(5) }));
        assert!(!diff(&b, &b, &mut sent).iter().any(|e| matches!(e, Event::Exited { .. })), "an exit is sent once");
        let c = Snapshot { nodes: b.nodes[..4].to_vec(), ..b.clone() };
        let ev = diff(&b, &c, &mut sent);
        assert!(ev.contains(&Event::Closed { id: Id(4), kind: Kind::Pane }) && ev.contains(&Event::Closed { id: Id(5), kind: Kind::Pane }));
    }

    #[test]
    fn subscriptions_cover_every_source() {
        let s = subscriptions();
        assert_eq!(s.len(), 8 + WATCHED_VARS.len() + snapshot::USER_VARS.len() + 2);
    }

    // regression: every wake-up scanned every printing pane twice (the next deadline, then which are quiet), and each
    // quiet pane scanned the whole snapshot for its agent: O(printing + quiet·nodes) per notification
    #[test]
    fn quiet_panes_leave_in_order_with_bounded_work() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut p = Printing::default();
        assert_eq!(p.next_quiet(), None);
        // A prints once; B prints every 10 ms for 5 s
        p.changed("A".into(), ms(0));
        let mut most = 0;
        for i in 1..=500u64 {
            p.changed("B".into(), ms(i * 10));
            // A's deadline until A is quiet, then B's latest change's
            let next = if i * 10 <= QUIET.as_millis() as u64 { ms(0) } else { ms(i * 10) };
            assert_eq!(p.next_quiet(), Some(next + QUIET));
            let q = p.quiet(ms(i * 10));
            assert_eq!(q.is_empty(), i * 10 != QUIET.as_millis() as u64, "A goes quiet once, at QUIET: {q:?}");
            most = most.max(p.queued());
        }
        // only the last QUIET window's updates stay queued, never all 500
        assert!(most <= (QUIET.as_millis() / 10) as usize + 2, "{most} queued");
        assert!(p.quiet(ms(5000) + QUIET - Duration::from_millis(1)).is_empty());
        assert_eq!(p.quiet(ms(5000) + QUIET), ["B"]);
        assert_eq!((p.next_quiet(), p.queued()), (None, 0));
    }
}
