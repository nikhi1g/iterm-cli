//! The operations, replies and events between the CLI and the app (perf term's protocol types, kept as the
//! CLI's contract with a [`super::App`] backend: the iTerm2 adapter or the in-memory fake).

use serde::{Deserialize, Serialize};

use super::model::{BroadcastMode, Id, Kind, Layout, Rect, Snapshot};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Hello { proto: u32, token: String },
    /// The last published snapshot.
    Snapshot,
    /// One operation, run by the app.
    Call(Op),
    /// Stream events from `since` (exclusive; None: from now). The first reply is
    /// [`Reply::Watching`] with the snapshot the stream starts from.
    Watch { since: Option<u64> },
    /// Exit statuses the server recorded for these panes (they may be closed by now).
    Exits(Vec<Id>),
    /// Settings, profiles and key overrides (answered by `Backend::setting`).
    Setting(SettingOp),
}

/// The persisted settings (the app's settings file), applied live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SettingOp {
    /// The schema: `Done.lines`, one JSON object per key (`key`, `type`, `default`, `doc`).
    Keys,
    /// Current values: `Done.lines[0]` is one JSON object `{key: value}` (every key when empty).
    Get { keys: Vec<String> },
    /// Validated as a whole (an unknown key fails with the closest match), then applied live and
    /// persisted; `Done.lines[0]` is the JSON object of what was set.
    Set { pairs: Vec<(String, String)> },
    /// Back to the defaults (every key when empty).
    Reset { keys: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    /// Must stay variant 0: the server writes a cached snapshot body after a 0 tag (`server.rs`).
    Snapshot(Snapshot),
    Done(Done),
    Fail(Fail),
    Watching(Snapshot),
    Event(Pushed),
    Exits(Vec<(Id, Option<i32>)>),
}

/// A successful operation: the nodes it created (in order), and any text it read.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Done {
    pub ids: Vec<Id>,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailCode {
    /// Bad token or wire version.
    Auth,
    /// An ID that names no node (or a node of the wrong kind).
    NotFound,
    /// Arguments the app cannot apply (a move into itself, a layout with other panes).
    Invalid,
    /// An operation this build does not implement yet.
    Unsupported,
    Internal,
    /// It didn't happen in time (a delivery that never showed on screen).
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fail {
    pub code: FailCode,
    pub msg: String,
}

impl Fail {
    pub fn new(code: FailCode, msg: impl Into<String>) -> Fail {
        Fail { code, msg: msg.into() }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.msg)
    }
}

/// What to run in a new pane. Defaults: the user's login shell in the parent's directory.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Spawn {
    pub cwd: Option<String>,
    /// A command line, run as `$SHELL -lc CMD`; None runs the login shell.
    pub cmd: Option<String>,
    /// Added to the pane's environment (after PERFTERM_SOCKET, PERFTERM_TOKEN, PERFTERM_PANE).
    pub env: Vec<(String, String)>,
    /// Pane variables set before the process starts (sorted or not; the app sorts).
    pub vars: Vec<(String, String)>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Left,
    Right,
    Up,
    Down,
}

/// Where a moved node goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Dest {
    /// A pane split beside another pane.
    Beside { at: Id, side: Side },
    /// As the last child of a container: a tab into a vertical tab, a vertical tab into a window.
    Into(Id),
    /// A new tab in the node's vertical tab (panes).
    NewTab,
    /// A new vertical tab in the node's window (panes, tabs).
    NewVTab,
    NewWindow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Field {
    Name(String),
    /// None removes the variable.
    Var(String, Option<String>),
    /// A pane's or tab's color (`#rrggbb`); None clears it.
    Color(Option<String>),
    /// A window's frame, in points.
    Frame(Rect),
    /// A window's state, as its title bar buttons and a title-bar double click set it.
    WindowState(WinState),
    /// A pane buried (true: out of its tab, kept running, listed under Session > Buried Sessions) or brought back.
    Buried(bool),
}

/// [`Field::WindowState`]: normal (restored from any of the others), zoomed (the title-bar double click: the
/// screen's visible frame, and back), minimized to the Dock, or full screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WinState {
    Normal,
    Zoomed,
    Minimized,
    FullScreen,
}

/// Which lines a read returns. Rows are absolute: 0 is the oldest history line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lines {
    Screen,
    /// The last N lines (history and screen).
    Tail(u32),
    /// Rows from..to (end exclusive).
    Range(u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollTo {
    Lines(i32),
    Top,
    Bottom,
}

/// A key press. The app encodes it with the pane's current modes (application cursor keys, the kitty
/// keyboard protocol), as if typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Esc,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub cmd: bool,
}

impl Key {
    /// `enter`, `ctrl-c`, `C-c`, `shift-tab`, `alt-x`, `f5`, `up`, `a`.
    pub fn parse(s: &str) -> Option<Key> {
        let mut mods = Mods::default();
        let mut rest = s;
        loop {
            let Some((m, r)) = rest.split_once(['-', '+']) else { break };
            if r.is_empty() {
                break; // `ctrl--` is ctrl and '-'
            }
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => mods.ctrl = true,
                "alt" | "opt" | "option" | "meta" | "m" => mods.alt = true,
                "shift" | "s" => mods.shift = true,
                "cmd" | "super" => mods.cmd = true,
                _ => break,
            }
            rest = r;
        }
        let lower = rest.to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" | "return" | "cr" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "esc" | "escape" => KeyCode::Esc,
            "backspace" | "bs" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "space" => KeyCode::Char(' '),
            _ => {
                if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                    if (1..=20).contains(&n) {
                        KeyCode::F(n)
                    } else {
                        return None;
                    }
                } else {
                    let mut cs = rest.chars();
                    let c = cs.next()?;
                    if cs.next().is_some() {
                        return None;
                    }
                    KeyCode::Char(c)
                }
            }
        };
        Some(Key { code, mods })
    }
}

/// Everything that changes the app or reads a pane's text. Each runs on the app's main thread
/// against its model and answers with [`Done`] or [`Fail`]; none moves the human's focus unless
/// `focus` is set or it is [`Op::Select`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    NewWindow { spawn: Spawn, frame: Option<Rect>, focus: bool },
    /// In `window` (None: the focused window, else a new one).
    NewVTab { window: Option<Id>, name: Option<String>, spawn: Spawn, focus: bool },
    /// In `vtab` (None: the focused vertical tab).
    NewTab { vtab: Option<Id>, title: Option<String>, spawn: Spawn, focus: bool },
    /// A new pane beside `at`, taking `ratio` of its space (default half).
    Split { at: Id, side: Side, ratio: Option<f32>, spawn: Spawn, focus: bool },
    /// Close nodes and everything under them (their processes get SIGHUP).
    Close { ids: Vec<Id> },
    Set { id: Id, field: Field },
    /// Focus: bring the window forward and make the node current at every level.
    Select { id: Id },
    Move { id: Id, to: Dest },
    /// Siblings (same parent, same kind) in their new order; the parent's other children keep
    /// their relative order after them.
    Order { ids: Vec<Id> },
    /// Replace a tab's split tree; it must hold exactly the tab's panes.
    SetLayout { tab: Id, layout: Layout },
    /// Every tab of `from` (windows) into window `into`; the emptied windows close.
    Merge { into: Id, from: Vec<Id> },
    /// Write bytes to the pane's PTY; `paste` wraps them in bracketed paste when the app enabled it.
    Send { pane: Id, bytes: Vec<u8>, paste: bool },
    Key { pane: Id, key: Key },
    Read { pane: Id, lines: Lines },
    Scroll { pane: Id, to: ScrollTo },
    Clear { pane: Id, scrollback: bool },
    Quit,
    /// Broadcast input: `mode` switches it (a scoped mode in `window`, else the focused window); `add` and
    /// `remove` edit the picked set (adding switches to `Pick`).
    Broadcast { mode: Option<BroadcastMode>, window: Option<Id>, add: Vec<Id>, remove: Vec<Id> },
    /// Unified search (⌘/) over everything named, without the panes' text: up to `limit` rows, each
    /// "group\tname\tdetail\tid" (id: the vertical tab, tab, pane or window it shows, else empty).
    Search { query: String, limit: u32 },
    /// A message into a pane, through the app's delivery engine: one writer per pane (messages never interleave),
    /// streamed into the PTY with backpressure, as a bracketed paste when the program enabled it (canonical-mode
    /// readers get lines under the kernel's limit), then `enter` once the text shows on screen. Held while the
    /// pane shows a permission or trust prompt. The reply's line says `delivered`, `queued reason=prompt`, or
    /// fails with the reason.
    Deliver { pane: Id, text: String, enter: bool },
    /// The key map's actions, one line each: "name\tkey\tdescription" (key: the binding now, overrides
    /// included; empty when unbound).
    Actions,
    /// A key-map action (`keymap::by_name`: `zoom`, `reopen`, `vtab 3`, …) run as if its key were pressed with
    /// `pane` focused (None: the human's focused pane). The human's focus is put back afterwards unless the
    /// action is itself a focus action (`next_tab`, `focus_left`, `vtab 3`, …).
    Action { name: String, pane: Option<Id> },
    /// Resize `window` so `pane` (None: its focused pane) holds `cols` × `rows` cells; the top-left stays put.
    /// The line says the size it got ("COLSxROWS").
    WindowCells { window: Id, pane: Option<Id>, cols: Option<u16>, rows: Option<u16> },
    /// A pane's selection and marks (see [`ContentAct`]).
    Content { pane: Id, act: ContentAct },
    /// The main menu (`context` None) or a node's right-click menu (a pane, tab or vertical tab): `item` None
    /// lists it, one line each "path\tkey" (path: "Edit > Copy"); `item` Some runs the item whose path (or
    /// last part) matches it, case-insensitive, and the line names it.
    Menu { context: Option<Id>, item: Option<String> },
    /// The app's open dialogs and panels, one line each "id\tkind\ttitle\tbutton|button…" (kind: alert,
    /// panel, find; id: the window number, 0 for the find bar).
    UiList,
    /// Click button `button` (its title, case-insensitive, or its 1-based number) of dialog `id`; `close`
    /// closes a panel.
    UiAnswer { id: i64, button: String },
}

/// What [`Op::Content`] does. Rows are absolute, as for [`Lines::Range`] (0 is the oldest history line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentAct {
    /// lines[0] "sel\tR1:C1-R2:C2\tTEXT" (or "sel" alone: none), then "mark\tROW\tNAME" for each mark.
    Get,
    /// Select (the human sees it, scrolled into view); the line is the span and its text, as `Get`.
    Select(Grab),
    /// To the clipboard: the grab, else the selection; the line is the text copied.
    Copy(Option<Grab>),
    /// Mark a row (None: the cursor's), optionally by name; the line is "ROW".
    Mark { row: Option<u64>, name: Option<String> },
    /// Scroll to a named mark.
    Jump(String),
    /// Cmd-click the first match of the regex, newest row first: its URL opens; the line is the URL.
    Open(String),
    /// The find bar (⌘F) on this pane with this query, its newest match shown, as the human would type it.
    FindBar(String),
}

/// A span of a pane's text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Grab {
    /// (row, column) to (row, column), both inclusive.
    Range((u64, u32), (u64, u32)),
    Word(u64, u32),
    Line(u64),
    /// The newest match of the regex in the history and screen.
    Match(String),
}

/// What the app pushes. Events follow the snapshot they belong to (see `Server::publish`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Created { id: Id, kind: Kind, parent: Id },
    Closed { id: Id, kind: Kind },
    /// The pane's process ended (the pane may close right after).
    Exited { pane: Id, status: i32 },
    Focus { id: Id },
    Renamed { id: Id, name: String },
    Title { pane: Id, title: String },
    Cwd { pane: Id, cwd: String },
    Var { id: Id, key: String, value: Option<String> },
    /// Splits, moves and resizes inside a tab.
    Layout { tab: Id },
    /// A node moved to a new parent or position.
    Moved { id: Id, parent: Id },
    Bell { pane: Id },
    /// OSC 133 prompt marks: `A` prompt start, `D` command end with status.
    Prompt { pane: Id, mark: char, status: Option<i32> },
    /// The pane's output went quiet after printing (coalesced by the app: at most one per pane per
    /// burst). Agents use it as "idle".
    Quiet { pane: Id },
    Notify { pane: Id, title: String, body: String },
    /// The watcher fell behind and `missed` events were dropped (never blocks the app).
    Gap { missed: u64 },
    /// Broadcast input changed: its mode or the panes it reaches.
    Broadcast { mode: BroadcastMode, targets: Vec<Id> },
    /// One of the app's windows (a panel or an alert; a terminal window too) showed, hid, closed or took the
    /// keyboard, or the find bar (window 0) opened or closed: `ui watch` lists the dialogs again on it.
    Ui { window: i64 },
}

impl Event {
    /// The node an event is about (None for a gap).
    pub fn subject(&self) -> Option<Id> {
        Some(match self {
            Event::Created { id, .. }
            | Event::Closed { id, .. }
            | Event::Focus { id }
            | Event::Renamed { id, .. }
            | Event::Var { id, .. }
            | Event::Moved { id, .. } => *id,
            Event::Exited { pane, .. }
            | Event::Title { pane, .. }
            | Event::Cwd { pane, .. }
            | Event::Bell { pane }
            | Event::Prompt { pane, .. }
            | Event::Quiet { pane }
            | Event::Notify { pane, .. } => *pane,
            Event::Layout { tab } => *tab,
            Event::Gap { .. } | Event::Broadcast { .. } | Event::Ui { .. } => return None,
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            Event::Created { .. } => "created",
            Event::Closed { .. } => "closed",
            Event::Exited { .. } => "exited",
            Event::Focus { .. } => "focus",
            Event::Renamed { .. } => "renamed",
            Event::Title { .. } => "title",
            Event::Cwd { .. } => "cwd",
            Event::Var { .. } => "var",
            Event::Layout { .. } => "layout",
            Event::Moved { .. } => "moved",
            Event::Bell { .. } => "bell",
            Event::Prompt { .. } => "prompt",
            Event::Quiet { .. } => "quiet",
            Event::Notify { .. } => "notify",
            Event::Gap { .. } => "gap",
            Event::Broadcast { .. } => "broadcast",
            Event::Ui { .. } => "ui",
        }
    }
}

/// An event with its sequence number (1, 2, … per app run) and wall-clock time in ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pushed {
    pub seq: u64,
    pub ms: u64,
    pub ev: Event,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        let k = Key::parse("ctrl-c").unwrap();
        assert_eq!((k.code, k.mods.ctrl), (KeyCode::Char('c'), true));
        assert_eq!(Key::parse("C-c"), Key::parse("ctrl-c"));
        let k = Key::parse("shift-tab").unwrap();
        assert!(k.mods.shift && k.code == KeyCode::Tab);
        assert_eq!(Key::parse("enter").unwrap().code, KeyCode::Enter);
        assert_eq!(Key::parse("f5").unwrap().code, KeyCode::F(5));
        assert_eq!(Key::parse("ctrl--").unwrap().code, KeyCode::Char('-'));
        assert_eq!(Key::parse("A").unwrap().code, KeyCode::Char('A'));
        assert!(Key::parse("nope").is_none());
    }
}
