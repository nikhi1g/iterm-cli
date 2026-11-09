//! The model as the protocol sees it: app → window → vertical tab → tab → (split tree) → pane, as one
//! flat, ordered list of nodes. The app owns the live model; a [`Snapshot`] is the copy it publishes
//! after each change, and [`Index`] gives O(1) lookups over one.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A node's stable ID. The app assigns IDs from one counter, so an ID is unique across kinds and
/// never reused; it never changes when a node moves (a pane keeps its ID in another tab or window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Id(pub u64);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Kind {
    App,
    Window,
    VTab,
    Tab,
    Pane,
}

impl Kind {
    pub const fn letter(self) -> char {
        match self {
            Kind::App => 'a',
            Kind::Window => 'w',
            Kind::VTab => 'v',
            Kind::Tab => 't',
            Kind::Pane => 'p',
        }
    }
    pub fn from_letter(c: char) -> Option<Kind> {
        Some(match c {
            'a' => Kind::App,
            'w' => Kind::Window,
            'v' => Kind::VTab,
            't' => Kind::Tab,
            'p' => Kind::Pane,
            _ => return None,
        })
    }
    pub const fn name(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Window => "window",
            Kind::VTab => "vtab",
            Kind::Tab => "tab",
            Kind::Pane => "pane",
        }
    }
    /// The kind a node of this kind is a direct child of.
    pub const fn parent(self) -> Option<Kind> {
        match self {
            Kind::App => None,
            Kind::Window => Some(Kind::App),
            Kind::VTab => Some(Kind::Window),
            Kind::Tab => Some(Kind::VTab),
            Kind::Pane => Some(Kind::Tab),
        }
    }
}

/// `p12`: a node's display handle, the kind letter and the ID. Selectors accept it and the bare ID.
pub fn handle(kind: Kind, id: Id) -> String {
    format!("{}{}", kind.letter(), id.0)
}

/// Parses `p12` (or `12`) into an ID, checking the letter against `kind` when given.
pub fn parse_handle(s: &str) -> Option<(Option<Kind>, Id)> {
    let mut chars = s.chars();
    let first = chars.next()?;
    if first.is_ascii_digit() {
        return s.parse().ok().map(|n| (None, Id(n)));
    }
    let k = Kind::from_letter(first)?;
    let rest = chars.as_str();
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .then(|| rest.parse().ok().map(|n| (Some(k), Id(n))))
        .flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A tab's split tree. `vertical`: children side by side (vertical dividers). Fractions sum to 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Layout {
    Pane(Id),
    Split { vertical: bool, children: Vec<(f32, Layout)> },
}

impl Layout {
    pub fn panes(&self, out: &mut Vec<Id>) {
        match self {
            Layout::Pane(id) => out.push(*id),
            Layout::Split { children, .. } => children.iter().for_each(|(_, c)| c.panes(out)),
        }
    }
}

/// What a pane adds to a node.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PaneInfo {
    /// The shell's (or command's) process ID.
    pub pid: i32,
    /// From OSC 7 when the shell reports it, else the starting directory.
    pub cwd: String,
    /// The terminal title (OSC 0/2).
    pub title: String,
    /// The foreground job's name (the PTY's process group leader), empty when unknown.
    pub job: String,
    pub cols: u16,
    pub rows: u16,
    /// Exit status, once the process has ended and the pane is still open.
    pub exit: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: Id,
    pub kind: Kind,
    /// Index of the parent in [`Snapshot::nodes`]; None only for the app (index 0).
    pub parent: Option<u32>,
    /// Window title, vertical-tab name, tab title or pane name; empty when unset.
    pub name: String,
    /// Panes: cells within their tab (x, y, columns, rows); windows: points on screen.
    pub rect: Option<Rect>,
    /// Creation order (higher is newer), for `recent:N`.
    pub created: u64,
    /// User variables, sorted by key. The agent overlay lives here (`agent.parent`, `agent.name`, …).
    pub vars: Vec<(String, String)>,
    pub pane: Option<PaneInfo>,
    /// Tabs: the split tree.
    pub layout: Option<Layout>,
}

impl Node {
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .binary_search_by(|(k, _)| k.as_str().cmp(key))
            .ok()
            .map(|i| self.vars[i].1.as_str())
    }
    pub fn handle(&self) -> String {
        handle(self.kind, self.id)
    }
}

/// The whole model at one moment. `nodes` is in tree order (pre-order, children in display order),
/// the app at index 0.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The event sequence number this snapshot reflects (set by the server on publish).
    pub seq: u64,
    pub nodes: Vec<Node>,
    /// The focused window, vertical tab, tab and pane (the key window's), each when there is one.
    pub focus: Vec<Id>,
    pub app_version: String,
    pub app_pid: i32,
    /// Broadcast input: its mode and the panes receiving now.
    pub broadcast: Broadcast,
    /// Keep-awake: off (the setting), none, system, or system+display.
    pub awake: String,
}

/// What the focused pane's typing is copied to (`app/src/broadcast.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BroadcastMode {
    #[default]
    Off,
    /// The agent panes of a window's current vertical tab (⌘⇧I).
    VTab,
    /// The agent panes of a whole window (⌃⌘⇧A).
    Window,
    /// Every pane of a window's current tab (⌥⌘I).
    Tab,
    /// Every pane of a whole window (⌃⌘⇧I).
    All,
    /// A hand-picked set of panes (⌘⇧-click, ⌃⌥⌘I).
    Pick,
    /// The agent panes of a window's current tab (⌘⇧A).
    TabAgents,
}

impl BroadcastMode {
    pub const fn name(self) -> &'static str {
        match self {
            BroadcastMode::Off => "off",
            BroadcastMode::VTab => "vtab",
            BroadcastMode::Window => "window",
            BroadcastMode::Tab => "tab",
            BroadcastMode::All => "all",
            BroadcastMode::Pick => "pick",
            BroadcastMode::TabAgents => "tab-agents",
        }
    }
    pub fn parse(s: &str) -> Option<BroadcastMode> {
        [BroadcastMode::Off, BroadcastMode::VTab, BroadcastMode::Window, BroadcastMode::Tab, BroadcastMode::All, BroadcastMode::Pick, BroadcastMode::TabAgents]
            .into_iter().find(|m| m.name() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Broadcast {
    pub mode: BroadcastMode,
    /// The window a scoped mode belongs to.
    pub window: Option<Id>,
    /// The panes receiving broadcast input now (the agent-only scopes: their agent panes), ascending.
    pub targets: Vec<Id>,
}

/// O(1) lookups over one snapshot: ID → index, and each node's children.
pub struct Index<'a> {
    pub snap: &'a Snapshot,
    by_id: HashMap<Id, u32>,
    children: Vec<Vec<u32>>,
}

impl<'a> Index<'a> {
    pub fn new(snap: &'a Snapshot) -> Index<'a> {
        let n = snap.nodes.len();
        let mut by_id = HashMap::with_capacity(n);
        let mut children = vec![Vec::new(); n];
        for (i, node) in snap.nodes.iter().enumerate() {
            by_id.insert(node.id, i as u32);
            if let Some(p) = node.parent {
                if let Some(c) = children.get_mut(p as usize) {
                    c.push(i as u32);
                }
            }
        }
        Index { snap, by_id, children }
    }
    pub fn get(&self, id: Id) -> Option<usize> {
        self.by_id.get(&id).map(|i| *i as usize)
    }
    pub fn node(&self, i: usize) -> &'a Node {
        &self.snap.nodes[i]
    }
    pub fn children(&self, i: usize) -> &[u32] {
        &self.children[i]
    }
    pub fn len(&self) -> usize {
        self.snap.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.snap.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_round_trip() {
        assert_eq!(handle(Kind::Pane, Id(12)), "p12");
        assert_eq!(parse_handle("p12"), Some((Some(Kind::Pane), Id(12))));
        assert_eq!(parse_handle("12"), Some((None, Id(12))));
        assert_eq!(parse_handle("p"), None);
        assert_eq!(parse_handle("lead"), None);
        assert_eq!(parse_handle("x12"), None);
    }
}
