//! The grammar as typed values: `iterm <noun> <verb> [SELECTOR] [PLACEMENT] [GEOMETRY] [MULTIPLICITY]
//! [PAYLOAD] [--plan]`. Pure: no app calls.
//!
//! It owns, top to bottom: the nouns and verbs and which verbs each noun has (`MATRIX`, with one line of help
//! each, and the CRUD [`Class`] of each cell); the key-map actions, old command names and mouse gestures each
//! mapped to a command (`KEY_VERBS`, `ALIASES`, `GESTURES`); the selector language, a small recursive-descent
//! parser into an [`Expr`]; the slots, typed structs built from the shared argument parser ([`parse`] into a
//! [`Request`]); and `help NOUN` and `coverage --json`, read from the same tables the dispatcher (`exec`) uses.

use std::fmt;

use crate::args::{self, extend, flag, one, Opt, Parsed};
use crate::geometry::{Frame, Tile};

pub use crate::app::SCHEMA_VERSION;

/// Most ports one agent may take (`--ports N`).
pub const MAX_PORTS: u32 = 1024;

/// `--port-pool LO-HI`, with 1024 <= LO <= HI <= 65535.
pub fn parse_pool(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("--port-pool {s}: LO-HI with 1024 <= LO <= HI <= 65535");
    let (a, b) = s.trim().split_once('-').ok_or_else(bad)?;
    let (lo, hi): (u32, u32) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
    if lo < 1024 || hi < lo || hi > 65535 {
        return Err(bad());
    }
    Ok((lo, hi))
}

/// 1–64 letters, digits, `_` and `-`, starting with a letter or digit.
pub fn is_slug(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    value.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

macro_rules! names {
    ($name:ident { $($v:ident => $s:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($v),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$v),+];
            pub fn name(self) -> &'static str {
                match self { $($name::$v => $s),+ }
            }
            pub fn parse(s: &str) -> Option<$name> {
                match s { $($s => Some($name::$v),)+ _ => None }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.name()) }
        }
    };
}

names!(Noun {
    App => "app", Window => "window", VTab => "vtab", Tab => "tab", Pane => "pane", Content => "content", Ui => "ui",
    Group => "group", Agent => "agent", Task => "task", Profile => "profile", Setting => "setting", Key => "key",
    Layout => "layout", Script => "script", Preset => "preset", Snippet => "snippet", Action => "action",
    Broadcast => "broadcast", Tree => "tree",
});

names!(Verb {
    List => "list", Get => "get", Read => "read", Find => "find", Watch => "watch", Wait => "wait", Keys => "keys",
    New => "new", Copy => "copy", Launch => "launch", Mark => "mark",
    Set => "set", Move => "move", Order => "order", Resize => "resize", Merge => "merge", Select => "select", Send => "send",
    Key => "key", Run => "run", Answer => "answer", Open => "open", Reopen => "reopen", Menu => "menu", Adopt => "adopt", Lend => "lend",
    Msg => "msg", Inbox => "inbox", Checkpoint => "checkpoint", Pause => "pause", Resume => "resume", Claim => "claim", Done => "done",
    Scroll => "scroll", Save => "save", Import => "import", Reset => "reset",
    Close => "close", Delete => "delete", Quit => "quit", WtRemove => "wt-remove", Clear => "clear",
    Land => "land", Unland => "unland", Landed => "landed",
});

/// The CRUD safety class. Nothing is gated by it (writes run at once; `--plan` is the dry run): it says which verbs
/// `--plan` applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Read,
    Create,
    Update,
    Delete,
}

impl Class {
    pub fn letter(self) -> &'static str {
        match self {
            Class::Read => "R",
            Class::Create => "C",
            Class::Update => "U",
            Class::Delete => "D",
        }
    }
}

impl Verb {
    pub fn class(self) -> Class {
        use Verb::*;
        match self {
            List | Get | Read | Find | Watch | Wait | Keys | Inbox | Landed => Class::Read,
            New | Copy | Launch | Mark | Save | Import | Reopen => Class::Create,
            Close | Delete | Quit | WtRemove | Clear => Class::Delete,
            _ => Class::Update,
        }
    }
}

/// A verb's class on a noun: the verb's own, except where the noun gives it another meaning
/// (`content copy` writes the clipboard, an update; `agent run` creates agents).
pub fn class_of(noun: Noun, verb: Verb) -> Class {
    match (noun, verb) {
        (Noun::Content, Verb::Copy) => Class::Update,
        (Noun::Agent, Verb::Run) => Class::Create,
        _ => verb.class(),
    }
}

/// Nodes a selector picks; the containment kinds plus the agent overlay (agents are panes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    App,
    Window,
    VTab,
    Tab,
    Pane,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "app" => Kind::App,
            "window" | "w" => Kind::Window,
            "vtab" | "v" => Kind::VTab,
            "tab" | "t" => Kind::Tab,
            "pane" | "p" | "agent" | "content" => Kind::Pane,
            _ => return None,
        })
    }
    pub fn letter(self) -> &'static str {
        match self {
            Kind::App => "a",
            Kind::Window => "w",
            Kind::VTab => "v",
            Kind::Tab => "t",
            Kind::Pane => "p",
        }
    }
    /// The container indices count within.
    pub fn parent(self) -> Option<Kind> {
        match self {
            Kind::Pane => Some(Kind::Tab),
            // iTerm2 nests top tabs inside a vertical tab
            Kind::Tab => Some(Kind::VTab),
            Kind::VTab => Some(Kind::Window),
            Kind::Window => Some(Kind::App),
            Kind::App => None,
        }
    }
}

impl Noun {
    /// The tree kind a noun's selector picks (None: a resource or set, selected by name).
    pub fn kind(self) -> Option<Kind> {
        Some(match self {
            Noun::App => Kind::App,
            Noun::Window => Kind::Window,
            Noun::VTab => Kind::VTab,
            Noun::Tab => Kind::Tab,
            Noun::Pane | Noun::Content | Noun::Agent => Kind::Pane,
            _ => return None,
        })
    }
}

/// Which verbs each noun has, with one line of help each. A cell not listed is n/a: it has no
/// meaning (only windows merge; content does not move).
pub const MATRIX: &[(Noun, &[(Verb, &str)])] = &[
    (Noun::App, &[(Verb::Get, "status, version, frontmost"), (Verb::Watch, "app events"), (Verb::Launch, "start the app (clean env)"),
                  (Verb::Quit, "stop the app"), (Verb::Reopen, "the last closed pane, tab or vertical tab, in its place (⇧⌘T)"), (Verb::Menu, "[ITEM] the main menu, classified C/R/U/D; --context pane|tab|vtab SEL"),
                  (Verb::Find, "QUERY [--kind tabs|actions|settings|themes|cli] [--limit N] (unified search, as ⌘/)")]),
    (Noun::Window, &[(Verb::List, ""), (Verb::Get, ""), (Verb::Find, "--where k=v"), (Verb::Watch, ""), (Verb::Wait, "--until COND"),
                     (Verb::New, "[--cwd D] [--cmd C|--agent]"), (Verb::Set, "--frame x,y,w,h | --tile columns|rows|grid [--screen S] | --cols N --rows N | --title T | --state normal|zoomed|minimized|fullscreen | --sidebar on|off|toggle (⌥S)"),
                     (Verb::Merge, "SEL|--all [--into W]: every tab into one window"), (Verb::Select, "bring forward (takes focus)"), (Verb::Close, "")]),
    (Noun::VTab, &[(Verb::List, ""), (Verb::Get, ""), (Verb::Find, ""), (Verb::Watch, ""), (Verb::Wait, ""), (Verb::New, "[NAME] [--in WINDOW] [--cwd D] [--agent]"),
                   (Verb::Set, "--name N"), (Verb::Move, "--to new-window|W [--groups N|--by K]"), (Verb::Order, "SEL… (new order)"),
                   (Verb::Select, ""), (Verb::Close, "")]),
    (Noun::Tab, &[(Verb::List, ""), (Verb::Get, ""), (Verb::Find, ""), (Verb::Watch, ""), (Verb::Wait, ""), (Verb::New, "[--in VTAB|WINDOW] [--cwd D] [--agent]"),
                  (Verb::Set, "--title T | --layout GRID|NAME | --equalize | --tile grid|columns|rows (its panes, in order)"), (Verb::Move, "--to new-window|new-vtab|W|V [--groups N|--by K] (parts go to their own new windows) | --at PANE --side S (its panes split in there)"), (Verb::Order, "SEL…"),
                  (Verb::Select, ""), (Verb::Close, "")]),
    (Noun::Pane, &[(Verb::List, ""), (Verb::Get, ""), (Verb::Read, "[--tail N|--range A:B]"), (Verb::Find, "--match RE"), (Verb::Watch, ""), (Verb::Wait, "--until COND"),
                   (Verb::New, "[--agent|--cmd C] [--cwd D (default: your pane's folder)] [--at SEL --side S (an agent: your dock by default)] [--stack D] [--count N] [--tile grid|columns|rows] [--weights …|--ratio R|--no-equalize] [--worktree] [--ports N]"),
                   (Verb::Set, "--name N (a live agent's session too, unless --no-session) | --color red|…|#rrggbb|none | --bury | --group G | --zoom on|off|toggle (maximized over its tab, ⇧⌘↩; focuses it)"), (Verb::Move, "--at SEL --side S | --to new-tab|new-vtab|new-window"),
                   (Verb::Order, "SEL…"), (Verb::Resize, "--ratio R | --weights … | --cols N --rows N | --equalize"), (Verb::Select, ""),
                   (Verb::Send, "--text T [--enter] | --path F"), (Verb::Key, "--key K"), (Verb::Run, "--cmd C --until RE"),
                   (Verb::Lend, "PANE [--to AGENT|--any] [--for 1h] | --revoke (the human's: agents may split beside it)"), (Verb::Close, "[--force] (a live agent)")]),
    (Noun::Content, &[(Verb::Get, "selection, marks"), (Verb::Read, "[--tail N]"), (Verb::Find, "--match RE [--show: the find bar (⌘F) with it]"), (Verb::Watch, "--screen"), (Verb::Wait, ""),
                      (Verb::Select, "--range r1:c1-r2:c2 | --word R:C | --line R | --match RE (rows absolute)"), (Verb::Open, "--match RE (cmd-click)"), (Verb::Mark, "--name N"),
                      (Verb::Copy, "[--match RE | --range A:B] (selection by default) to the clipboard"), (Verb::Scroll, "--lines N | --to top|bottom|mark:NAME"),
                      (Verb::Save, "--to FILE [--range A:B | --tail N] (scrollback to a file)"), (Verb::Clear, "[--scrollback] (own panes)")]),
    (Noun::Ui, &[(Verb::List, "open dialogs and panels"), (Verb::Get, ""), (Verb::Watch, ""), (Verb::Wait, ""), (Verb::Answer, "ID --button B"), (Verb::Open, "settings|help (the Settings window ⌘,, the guide ⌘?)")]),
    (Noun::Group, &[(Verb::List, ""), (Verb::Get, "NAME"), (Verb::New, "NAME [--kind label|broadcast] [--add SEL…] (select it: --group NAME, group=NAME)"), (Verb::Set, "NAME --add SEL | --remove SEL | --kind K"),
                    (Verb::Delete, "NAME")]),
    (Noun::Agent, &[(Verb::List, "[--paused | --history [N] [--all | --cwd D]: Claude Code sub-agents of your project, newest first]"), (Verb::Get, ""), (Verb::Watch, ""), (Verb::Wait, "--until exit|idle"), (Verb::New, "= pane new --agent [--detached]"),
                    (Verb::Run, "TASK… [--worktree] [--model M --effort E] [--max N]: spawn one each, wait, one line per child"),
                    (Verb::Adopt, "X [--to me|human]"), (Verb::Msg, "TO…|@parent --text T | --file F (durable mailbox)"),
                    (Verb::Inbox, "[--since ID|--all] [--mark-read] [--wait [S]] | --sent (read receipts: read once the receiver --mark-read) | --all-panes (every message between any panes)"), (Verb::Checkpoint, "NAME [--message M] (commit, rebase on parent)"),
                    (Verb::Merge, "NAME [--ff-only|--no-ff]"), (Verb::Pause, "NAME (commit, free the worktree)"),
                    (Verb::Resume, "NAME [--model M --effort E --task T] | agent-ID (a Claude Code sub-agent: its name, color, task and transcript) [--worktree] [--read-only]"), (Verb::WtRemove, "NAME [--force]"),
                    (Verb::Land, "NAME…|--all [--check CMD] [--close]: per agent, checkpoint, check, merge --no-ff (one commit per feature), remove its worktree and branch"),
                    (Verb::Unland, "NAME… (revert each landing, newest first)"), (Verb::Landed, "(one line per landing on your branch)"),
                    (Verb::Close, "[--force] (a live agent; a worktree agent with work not landed)")]),
    (Noun::Task, &[(Verb::List, "[--status S] [--mine] [--ready]"), (Verb::Wait, "--until COND"), (Verb::New, "TITLE [--body T|--body-file F] [--after ID…] [--assign NAME]"),
                   (Verb::Claim, "ID|next"), (Verb::Done, "ID --status done|failed|blocked \"summary\""),
                   (Verb::Watch, "[--until change|done|all-done|ready|claimed|count>=N] (wakes on board writes)")]),
    (Noun::Profile, &[(Verb::List, ""), (Verb::Get, "NAME [--grep K]"), (Verb::Keys, "[--grep K]"), (Verb::New, "NAME [--from P] [--set k=v…]"),
                      (Verb::Copy, "NAME --from P"), (Verb::Set, "NAME --set k=v… | --preset P"), (Verb::Delete, "NAME (not Default; the one in use falls back to another)"),
                      (Verb::Import, "FILE (.itermcolors, Ghostty, kitty theme)")]),
    (Noun::Setting, &[(Verb::List, ""), (Verb::Get, "KEY…"), (Verb::Keys, "[--grep K]"), (Verb::Set, "--set k=v… (applied live, persisted)"), (Verb::Reset, "KEY…|--all")]),
    (Noun::Key, &[(Verb::List, "action and binding"), (Verb::Get, "ACTION"), (Verb::Keys, "action names"), (Verb::Set, "ACTION --key COMBO"), (Verb::Reset, "ACTION…|--all"), (Verb::Delete, "ACTION (= reset)")]),
    (Noun::Layout, &[(Verb::List, ""), (Verb::Get, "NAME (its tree and panes)"), (Verb::New, "NAME [--in TAB] [--force]: save a tab (split tree, fractions, each pane's cwd, command or agent, name)"),
                     (Verb::Run, "NAME [--in VTAB]: recreate it in a new tab"), (Verb::Delete, "NAME")]),
    (Noun::Script, &[(Verb::List, "(batch files in STATE/scripts)"), (Verb::Run, "NAME [--keep-going] (= batch --file STATE/scripts/NAME[.batch])")]),
    (Noun::Preset, &[(Verb::List, "[--grep Q] (bundled presets, then the profiles)"), (Verb::Get, "NAME (its colors)"),
                     (Verb::Import, "FILE (.itermcolors, Ghostty, kitty): a new profile, switched to")]),
    (Noun::Snippet, &[(Verb::List, ""), (Verb::Get, "NAME"), (Verb::New, "NAME --text T | --file F [--force]"), (Verb::Run, "NAME [--in PANE] [--enter] (delivered as pane send)"), (Verb::Delete, "NAME")]),
    (Noun::Action, &[(Verb::List, "every key-map action: name, key, what it does"), (Verb::Get, "NAME"),
                     (Verb::Run, "NAME [--in PANE] (as its key, with PANE focused; the human's focus is kept unless it is a focus action)")]),
    (Noun::Tree, &[(Verb::List, "[SEL]: windows > vertical tabs > tabs > panes, one line each (* focused)")]),
    (Noun::Broadcast, &[(Verb::Get, "mode, window, receiving panes (events: app watch --kind broadcast)"),
                        (Verb::Set, "--mode vtab|tab-agents|window|tab|all|pick|off [--in WINDOW] | --add SEL | --remove SEL")]),
];

/// Cells the grammar parses that iTerm2 does not implement yet: they answer `unsupported`, and
/// `help` and `coverage --json` mark them (`C*`, `R*`, …). The rest wait for the
/// app's UI: the transient UI and menus.
pub fn slot_only(noun: Noun, verb: Verb) -> bool {
    // every slot is wired; a verb added to the matrix before its handler goes here
    let _ = (noun, verb);
    false
}

pub fn verbs(noun: Noun) -> &'static [(Verb, &'static str)] {
    MATRIX
        .iter()
        .find(|(n, _)| *n == noun)
        .map(|(_, v)| *v)
        .unwrap_or(&[])
}

pub fn has(noun: Noun, verb: Verb) -> bool {
    verbs(noun).iter().any(|(v, _)| *v == verb)
}

/// Every key-map action (the app's `keymap::NAMES`, and `vtab N`, `tab N`), the CLI command for it, and whether that
/// command does exactly what the key does (true) or the nearest thing by name (false: relative moves such as
/// next/prev, swap and resize by a step, toggles such as broadcast modes, the inline editors). `action run NAME`
/// presses any key exactly. A test in the app (every_key_map_action_has_a_cli_verb) keeps this whole, and each
/// command parses; `coverage --json` "keys" marks the approximate ones.
pub const KEY_VERBS: &[(&str, &str, bool)] = &[
    ("vtab N", "vtab select 2", true),
    ("tab N", "tab select 2", true),
    ("new_vtab", "vtab new", true),
    ("new_tab", "tab new", true),
    ("new_window", "window new", true),
    ("close_vtab", "vtab close @focused", true),
    ("close_pane", "pane close @focused", true),
    ("split_right", "pane new --at @focused --side right", true),
    ("split_down", "pane new --at @focused --side down", true),
    ("next_vtab", "vtab select 2", false),
    ("prev_vtab", "vtab select 1", false),
    ("next_tab", "tab select 2", false),
    ("prev_tab", "tab select 1", false),
    ("next_pane", "pane select 2", false),
    ("prev_pane", "pane select 1", false),
    ("focus_left", "pane select neighbor:left", true),
    ("focus_right", "pane select neighbor:right", true),
    ("focus_up", "pane select neighbor:up", true),
    ("focus_down", "pane select neighbor:down", true),
    ("move_vtab_down", "vtab order 2 1", false),
    ("move_vtab_up", "vtab order 2 1", false),
    ("tab_to_vtab", "tab move @focused --to new-vtab", true),
    ("move_tab_left", "tab order 2 1", false),
    ("move_tab_right", "tab order 2 1", false),
    ("rename", "vtab set @focused --name N", false),
    ("sidebar", "window set @focused --sidebar toggle", true),
    ("zoom", "pane set @focused --zoom toggle", true),
    ("settings", "ui open settings", true),
    ("help", "ui open help", true),
    ("reopen", "app reopen", true),
    ("resize_left", "pane resize @focused --ratio 0.4", false),
    ("resize_right", "pane resize @focused --ratio 0.6", false),
    ("resize_up", "pane resize @focused --ratio 0.4", false),
    ("resize_down", "pane resize @focused --ratio 0.6", false),
    ("swap_left", "pane order 2 1", false),
    ("swap_right", "pane order 2 1", false),
    ("swap_up", "pane order 2 1", false),
    ("swap_down", "pane order 2 1", false),
    ("equalize", "tab set @focused --equalize", true),
    ("rename_pane", "pane set @focused --name N", false),
    ("mark", "content mark @focused --name N", false),
    ("prev_mark", "content scroll @focused --to mark:N", false),
    ("next_mark", "content scroll @focused --to mark:N", false),
    ("find", "content find @focused --match RE --show", true),
    ("find_next", "content find @focused --match RE --show", false),
    ("find_prev", "content find @focused --match RE --show", false),
    ("broadcast_vtab", "broadcast set --mode vtab", false),
    ("broadcast_tab_agents", "broadcast set --mode tab-agents", false),
    ("broadcast_window", "broadcast set --mode window", false),
    ("broadcast_tab", "broadcast set --mode tab", false),
    ("broadcast_all", "broadcast set --mode all", false),
    ("broadcast_pick", "broadcast set --add @focused", false),
    ("clear_picks", "broadcast set --mode off", false),
    ("clear", "content clear @focused", false),
    ("search", "app find QUERY", false),
];

/// Old commands and what they are now.
pub const ALIASES: &[(&str, &str)] = &[
    ("spawn", "pane new --agent"),
    ("split", "pane new"),
    ("layout", "tab set --layout"),
    ("close", "pane close"),
    ("tab-close", "tab close"),
    ("tab-create", "tab new"),
    ("tab-name", "tab set --title"),
    ("tab-move", "tab move"),
    ("pane-move", "pane move"),
    ("window-merge", "window merge"),
    ("vtab-new", "vtab new"),
    ("vtab-select", "vtab select"),
    ("vtab-rename", "vtab set --name"),
    ("vtab-close", "vtab close"),
    ("vtab-move", "vtab order"),
    ("tab-move-vtab", "tab move --to VTAB"),
    ("menu", "app menu"),
    ("panes", "pane list"),
    ("tabs", "tab list"),
    ("windows", "window list"),
    ("vtabs", "vtab list"),
    ("agents", "agent list"),
    ("read", "pane read"),
    ("send", "pane send"),
    ("msg", "agent msg"),
    ("inbox", "agent inbox"),
    ("key", "pane key"),
    ("wait", "pane wait"),
    ("resize", "pane resize"),
    ("select", "pane select"),
    ("name", "pane set --name"),
    ("color", "pane set --color"),
    ("run", "pane run"),
    ("layout-save", "layout new"),
    ("layout-restore", "layout run"),
    ("layouts", "layout list"),
    ("state", "pane get"),
    ("search", "app find"),
];

/// Mouse gestures and the command each maps to (the coverage scan reads this).
pub const GESTURES: &[(&str, &str)] = &[
    ("drag a split divider", "pane resize --ratio|--weights"),
    ("drag a pane onto another", "pane move --at X --side S"),
    ("drag a tab", "tab order | tab move --to | tab move --at PANE --side S"),
    ("double-click empty tab-bar space, or the title bar buttons", "window set --state zoomed|normal|minimized|fullscreen"),
    ("find in a pane (⌘F) and type", "content find --match TEXT --show"),
    ("drag a sidebar row", "vtab order | vtab move --to"),
    ("select text", "content select --range r1:c1-r2:c2"),
    ("double-click a word", "content select --word R:C"),
    ("triple-click a line", "content select --line R"),
    ("cmd-click a URL or path", "content open --match RE"),
    ("drop a file", "pane send --path F"),
    (
        "right-click a pane, tab or sidebar row",
        "app menu --context pane|tab|vtab SEL",
    ),
    (
        "resize or move a window",
        "window set --frame | --tile | --cols N --rows N",
    ),
    (
        "click a pane, tab or sidebar row to focus it",
        "pane select | tab select | vtab select",
    ),
    ("scroll", "content scroll --lines N | --to top|bottom|mark"),
];

/// Usage errors, typed; each names what to do instead.
#[derive(Debug, Clone, PartialEq)]
pub enum GrammarError {
    UnknownVerb { noun: Noun, verb: String },
    NotApplicable { noun: Noun, verb: Verb },
    Selector { input: String, msg: String },
    Flag { flag: String, msg: String },
    Slot { slot: &'static str, verb: Verb },
}

impl fmt::Display for GrammarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrammarError::UnknownVerb { noun, verb } => write!(
                f,
                "{noun} has no verb '{verb}'; {noun} verbs: {}",
                verb_list(*noun)
            ),
            GrammarError::NotApplicable { noun, verb } => {
                let near = nearest_verbs(*noun, *verb);
                write!(
                    f,
                    "{noun} {verb} is n/a (no meaning for a {noun}); nearest: {}",
                    near.join(", ")
                )
            }
            GrammarError::Selector { input, msg } => write!(f, "selector '{input}': {msg}"),
            GrammarError::Flag { flag, msg } => write!(f, "{flag}: {msg}"),
            GrammarError::Slot { slot, verb } => write!(
                f,
                "{slot} flags do not apply to {verb} (a {} verb)",
                match verb.class() {
                    Class::Read => "read",
                    Class::Create => "create",
                    Class::Update => "update",
                    Class::Delete => "delete",
                }
            ),
        }
    }
}

fn verb_list(noun: Noun) -> String {
    verbs(noun)
        .iter()
        .map(|(v, _)| v.name())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The noun's verbs of the same safety class first, then the rest (at most 4).
pub fn nearest_verbs(noun: Noun, verb: Verb) -> Vec<String> {
    let all = verbs(noun);
    let mut out: Vec<String> = all
        .iter()
        .filter(|(v, _)| v.class() == verb.class())
        .map(|(v, _)| format!("{noun} {v}"))
        .collect();
    out.extend(
        all.iter()
            .filter(|(v, _)| v.class() != verb.class())
            .map(|(v, _)| format!("{noun} {v}")),
    );
    // other nouns that have this verb
    for (n, vs) in MATRIX {
        if *n != noun && vs.iter().any(|(v, _)| *v == verb) && out.len() < 6 {
            out.insert(out.len().min(2), format!("{n} {verb}"));
        }
    }
    out.truncate(4);
    out
}

// ---- selectors -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    Atom(Atom),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rel {
    Me,
    Focused,
    Parent,
    Right,
    Left,
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn parse(s: &str) -> Option<Dir> {
        Some(match s {
            "left" => Dir::Left,
            "right" => Dir::Right,
            "up" | "above" => Dir::Up,
            "down" | "below" => Dir::Down,
            _ => return None,
        })
    }
    pub fn vertical_divider(self) -> bool {
        matches!(self, Dir::Left | Dir::Right)
    }
    pub fn before(self) -> bool {
        matches!(self, Dir::Left | Dir::Up)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// 1-based; negative counts from the end.
    One(i64),
    Span(i64, i64),
    Odd,
    Even,
    /// Part k of n equal parts (`half:1` is Part(1, 2)).
    Part(usize, usize),
    Last,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Match {
    Eq(String),
    Ne(String),
    Glob(String),
    Re(String),
}

impl Match {
    pub fn test(&self, value: &str) -> bool {
        match self {
            Match::Eq(v) => value.eq_ignore_ascii_case(v),
            Match::Ne(v) => !value.eq_ignore_ascii_case(v),
            Match::Glob(g) => glob(g, value),
            Match::Re(r) => compiled(r, |re| re.is_some_and(|re| re.is_match(value))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Atom {
    All,
    /// An ID, a unique ID prefix or a name.
    Word(String),
    Glob(String),
    /// `/w/2/v/3/t/1/p/2`: (kind, index or word) steps from the app down.
    Path(Vec<(Kind, String)>),
    Index(Pick),
    Rel(Rel, Option<Box<Atom>>),
    State(String),
    Neighbor(Dir),
    Recent(usize),
    Var(String, Match),
    Env(String, Match),
    /// `name=x`, `kind=claude`, `cwd~*/api`, `group=frontend`, `state!=BUSY`.
    Field(String, Match),
    /// `$NAME`: what an earlier batch line created (`--as NAME`).
    Bind(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Open,
    Close,
    Comma,
    And,
    Or,
    Not,
    Word(String),
}

fn tokenize(input: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' => i += 1,
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '!' if chars.get(i + 1).map(|n| *n != '=').unwrap_or(false)
                && out
                    .last()
                    .map(|t| !matches!(t, Tok::Word(_)))
                    .unwrap_or(true) =>
            {
                out.push(Tok::Not);
                i += 1;
            }
            _ => {
                let mut w = String::new();
                while i < chars.len() {
                    let c = chars[i];
                    // a regex `~/…/` runs to its closing slash, whatever it holds
                    if c == '/' && w.ends_with('~') {
                        w.push(c);
                        i += 1;
                        let mut closed = false;
                        while i < chars.len() {
                            let r = chars[i];
                            w.push(r);
                            i += 1;
                            if r == '\\' && i < chars.len() {
                                w.push(chars[i]);
                                i += 1;
                                continue;
                            }
                            if r == '/' {
                                closed = true;
                                break;
                            }
                        }
                        if !closed {
                            return Err("unterminated ~/regex/".into());
                        }
                        continue;
                    }
                    if c.is_whitespace() || c == '(' || c == ')' || c == ',' {
                        break;
                    }
                    w.push(c);
                    i += 1;
                }
                out.push(match w.as_str() {
                    "and" | "&&" => Tok::And,
                    "or" | "||" => Tok::Or,
                    "not" => Tok::Not,
                    _ => Tok::Word(w),
                });
            }
        }
    }
    Ok(out)
}

struct P {
    toks: Vec<Tok>,
    at: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }
    // or := and ((',' | 'or' | juxtaposition) and)*
    fn or(&mut self) -> Result<Expr, String> {
        let mut terms = vec![self.and()?];
        loop {
            match self.peek() {
                Some(Tok::Comma) | Some(Tok::Or) => {
                    self.at += 1;
                    terms.push(self.and()?);
                }
                Some(Tok::Word(_)) | Some(Tok::Not) | Some(Tok::Open) => terms.push(self.and()?),
                _ => break,
            }
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap_or(Expr::Atom(Atom::All))
        } else {
            Expr::Or(terms)
        })
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut terms = vec![self.unary()?];
        while self.peek() == Some(&Tok::And) {
            self.at += 1;
            terms.push(self.unary()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap_or(Expr::Atom(Atom::All))
        } else {
            Expr::And(terms)
        })
    }
    fn unary(&mut self) -> Result<Expr, String> {
        match self.toks.get(self.at).cloned() {
            Some(Tok::Not) => {
                self.at += 1;
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            Some(Tok::Open) => {
                self.at += 1;
                let e = self.or()?;
                if self.peek() != Some(&Tok::Close) {
                    return Err("missing ')'".into());
                }
                self.at += 1;
                Ok(e)
            }
            Some(Tok::Word(w)) => {
                self.at += 1;
                Ok(Expr::Atom(atom(&w)?))
            }
            Some(t) => Err(format!("unexpected {t:?}")),
            None => Err("expected a selector".into()),
        }
    }
}

/// `f` with the regex `r`, compiled once for a run of tests against it (a selector tests one regex per node).
fn compiled<T>(r: &str, f: impl FnOnce(Option<&regex::Regex>) -> T) -> T {
    thread_local! {
        static LAST: std::cell::RefCell<Option<(String, Option<regex::Regex>)>> = const { std::cell::RefCell::new(None) };
    }
    LAST.with(|l| {
        let mut l = l.borrow_mut();
        if l.as_ref().is_none_or(|(k, _)| k != r) {
            *l = Some((r.to_string(), regex::Regex::new(r).ok()));
        }
        f(l.as_ref().and_then(|(_, re)| re.as_ref()))
    })
}

fn matcher(op: &str, v: &str) -> Match {
    match op {
        "!=" => Match::Ne(v.to_string()),
        "~" => match v.strip_prefix('/').and_then(|r| r.strip_suffix('/')) {
            Some(re) => Match::Re(re.replace("\\/", "/")),
            None => Match::Glob(v.to_string()),
        },
        _ => Match::Eq(v.to_string()),
    }
}

/// `k=v`, `k!=v`, `k~glob`, `k~/re/`; a regex that doesn't compile is no field (it matched nothing, silently, before).
fn field(s: &str) -> Option<(String, Match)> {
    field_of(s).filter(|(_, m)| !matches!(m, Match::Re(r) if regex::Regex::new(r).is_err()))
}

fn field_of(s: &str) -> Option<(String, Match)> {
    let ops = ["!=", "~", "="];
    let (i, op) = ops
        .iter()
        .filter_map(|op| s.find(op).map(|i| (i, *op)))
        .min_by_key(|(i, _)| *i)?;
    let (k, v) = (&s[..i], &s[i + op.len()..]);
    (!k.is_empty()).then(|| (k.to_string(), matcher(op, v)))
}

fn pick(s: &str) -> Option<Pick> {
    if s == "odd" {
        return Some(Pick::Odd);
    }
    if s == "even" {
        return Some(Pick::Even);
    }
    if s == "last" {
        return Some(Pick::Last);
    }
    for (word, n) in [("half", 2usize), ("third", 3), ("quarter", 4)] {
        if let Some(k) = s.strip_prefix(word).and_then(|r| r.strip_prefix(':')) {
            let k: usize = k.parse().ok()?;
            return (1..=n).contains(&k).then_some(Pick::Part(k, n));
        }
    }
    if let Some(r) = s.strip_prefix("part:") {
        let (k, n) = r.split_once('/')?;
        let (k, n): (usize, usize) = (k.parse().ok()?, n.parse().ok()?);
        return (n > 0 && (1..=n).contains(&k)).then_some(Pick::Part(k, n));
    }
    if let Ok(n) = s.parse::<i64>() {
        return (n != 0).then_some(Pick::One(n));
    }
    let (a, b) = s.split_once('-')?;
    let (a, b): (i64, i64) = (a.parse().ok()?, b.parse().ok()?);
    (a >= 1 && b >= a).then_some(Pick::Span(a, b))
}

fn atom(w: &str) -> Result<Atom, String> {
    if w == "all" || w == "*" {
        return Ok(Atom::All);
    }
    if let Some(name) = w.strip_prefix('$') {
        return if name.is_empty() {
            Err("$ needs a name".into())
        } else {
            Ok(Atom::Bind(name.to_string()))
        };
    }
    if let Some(r) = w.strip_prefix('@') {
        let (rel, of) = match r.split_once(':') {
            Some((a, b)) => (a, Some(Box::new(atom(b)?))),
            None => (r, None),
        };
        let rel = match rel {
            "me" => Rel::Me,
            "focused" => Rel::Focused,
            "parent" => Rel::Parent,
            "right" => Rel::Right,
            "left" => Rel::Left,
            "above" | "up" => Rel::Above,
            "below" | "down" => Rel::Below,
            other => {
                return Err(format!(
                    "unknown @{other} (@me @focused @parent @right @left @above @below)"
                ))
            }
        };
        return Ok(Atom::Rel(rel, of));
    }
    if w.starts_with('/') && !w.contains('~') {
        let parts: Vec<&str> = w
            .trim_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() || !parts.len().is_multiple_of(2) {
            return Err("a path is /w/N/v/N/t/N/p/N (kind letter, then index or ID)".into());
        }
        let mut steps = Vec::new();
        for pair in parts.chunks(2) {
            let k = Kind::parse(pair[0])
                .filter(|k| *k != Kind::App)
                .ok_or_else(|| format!("path step '{}' is not w, v, t or p", pair[0]))?;
            steps.push((k, pair[1].to_string()));
        }
        return Ok(Atom::Path(steps));
    }
    if let Some(rest) = w.strip_prefix("state:") {
        return Ok(Atom::State(rest.to_string()));
    }
    if let Some(rest) = w.strip_prefix("neighbor:") {
        return Dir::parse(rest).map(Atom::Neighbor).ok_or_else(|| "neighbor:left|right|up|down".into());
    }
    if let Some(rest) = w.strip_prefix("recent:") {
        return rest.parse().map(Atom::Recent).map_err(|_| "recent:N".into());
    }
    if let Some(rest) = w.strip_prefix("var:") {
        return field(rest).map(|(k, m)| Atom::Var(k, m)).ok_or_else(|| "var:k=v".into());
    }
    if let Some(rest) = w.strip_prefix("env:") {
        return field(rest).map(|(k, m)| Atom::Env(k, m)).ok_or_else(|| "env:k=v".into());
    }
    if let Some(p) = pick(w) {
        return Ok(Atom::Index(p));
    }
    if let Some((k, m)) = field(w) {
        return Ok(Atom::Field(k, m));
    }
    if w.contains('*') || w.contains('?') {
        return Ok(Atom::Glob(w.to_string()));
    }
    Ok(Atom::Word(w.to_string()))
}

/// Parse a selector (positional words are joined: `pane close w1 w2` is `w1 or w2`).
pub fn selector(input: &str) -> Result<Expr, GrammarError> {
    let err = |msg: String| GrammarError::Selector {
        input: input.to_string(),
        msg,
    };
    let toks = tokenize(input).map_err(err)?;
    let mut p = P { toks, at: 0 };
    let e = p.or().map_err(err)?;
    if p.at < p.toks.len() {
        return Err(GrammarError::Selector {
            input: input.to_string(),
            msg: "unexpected ')'".into(),
        });
    }
    Ok(e)
}

impl Expr {
    /// Every `$NAME` it uses.
    pub fn binds(&self, out: &mut Vec<String>) {
        match self {
            Expr::Or(v) | Expr::And(v) => v.iter().for_each(|e| e.binds(out)),
            Expr::Not(e) => e.binds(out),
            Expr::Atom(Atom::Bind(n)) => out.push(n.clone()),
            Expr::Atom(Atom::Rel(_, Some(a))) => Expr::Atom((**a).clone()).binds(out),
            Expr::Atom(_) => {}
        }
    }
    /// Whether it depends on positions or relations that an earlier batch line may change.
    pub fn is_static(&self) -> bool {
        match self {
            Expr::Or(v) | Expr::And(v) => v.iter().all(Expr::is_static),
            Expr::Not(e) => e.is_static(),
            Expr::Atom(a) => matches!(
                a,
                Atom::Word(_)
                    | Atom::Glob(_)
                    | Atom::Field(..)
                    | Atom::Rel(Rel::Me, None)
                    | Atom::Bind(_)
            ),
        }
    }
}

/// `*` and `?` glob, case-insensitive.
pub fn glob(pattern: &str, text: &str) -> bool {
    // greedy with one backtrack point, the last `*`: an earlier star can never need more, so O(|p|·|t|) at worst
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        match p.get(i) {
            Some('*') => {
                star = Some((i + 1, j));
                i += 1;
            }
            Some(c) if *c == '?' || c.eq_ignore_ascii_case(&t[j]) => {
                i += 1;
                j += 1;
            }
            _ => match star {
                Some((after, from)) => {
                    star = Some((after, from + 1));
                    i = after;
                    j = from + 1;
                }
                None => return false,
            },
        }
    }
    p[i.min(p.len())..].iter().all(|c| *c == '*')
}

// ---- the slots -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum To {
    NewWindow,
    NewVTab,
    NewTab,
    Sel(Expr),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Placement {
    pub at: Option<Expr>,
    pub side: Option<Dir>,
    /// As a child of (create verbs).
    pub into: Option<Expr>,
    pub to: Option<To>,
    pub stack: Option<Dir>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geometry {
    pub weights: Option<Vec<f64>>,
    pub ratio: Option<f64>,
    pub cols: Option<i64>,
    pub rows: Option<i64>,
    pub equalize: bool,
    pub fit: bool,
    pub tile: Option<Tile>,
    pub screen: Option<String>,
    pub frame: Option<Frame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Cwd,
    Project,
    Kind,
    Group,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Multiplicity {
    pub count: Option<usize>,
    pub tasks: Vec<String>,
    pub task_files: Vec<String>,
    pub each: Option<String>,
    /// `--worktree` (Some(None)) or `--worktree=BASE`.
    pub worktree: Option<Option<String>>,
    pub max: Option<usize>,
    pub groups: Option<usize>,
    pub by: Option<By>,
    /// `--ports N`: a disjoint block of N ports per new agent.
    pub ports: Option<u32>,
}

/// One parsed request: every slot typed, plus the noun-specific options (`p`).
#[derive(Debug, Clone)]
pub struct Request {
    pub noun: Noun,
    pub verb: Verb,
    pub sel: Option<Expr>,
    /// The selector's words as given, and the other positionals (names, values).
    pub words: Vec<String>,
    pub within: Option<Expr>,
    pub of: Option<Kind>,
    pub wheres: Vec<(String, Match)>,
    pub group: Option<String>,
    pub all: bool,
    pub place: Placement,
    pub geom: Geometry,
    pub multi: Multiplicity,
    pub payload: Vec<(String, String)>,
    pub apply: bool,
    pub force: bool,
    pub focus: bool,
    /// batch: bind what this line creates to $NAME.
    pub bind: Option<String>,
    pub p: Parsed,
    /// The words after the verb, as given (a queued launch re-parses them when it starts).
    pub rest: Vec<String>,
}

const SLOT_PLACE: &[&str] = &["--at", "--side", "--to", "--stack"];
const SLOT_GEOM: &[&str] = &[
    "--weights",
    "--ratio",
    "--cols",
    "--rows",
    "--equalize",
    "--no-equalize",
    "--fit",
    "--tile",
    "--frame",
];
const SLOT_MULTI: &[&str] = &[
    "--count",
    "--task",
    "--task-file",
    "--split",
    "--each",
    "--worktree",
    "--max",
    "--groups",
    "--by",
    "--ports",
    "--port-pool",
];

/// Every v2 option (one table; flags that a verb gives no meaning are refused by slot).
pub fn options() -> Vec<Opt> {
    vec![
        // selector
        one("--in"),
        one("--of"),
        extend("--where"),
        one("--group"),
        flag("--all"),
        // placement
        one("--at"),
        one("--side"),
        one("--to"),
        one("--stack"),
        one("--into"),
        // geometry
        one("--weights"),
        one("--ratio"),
        one("--cols"),
        one("--rows"),
        flag("--equalize"),
        flag("--no-equalize"),
        flag("--fit"),
        one("--tile"),
        crate::args::flag_or_value("--screen"),
        one("--frame"),
        // multiplicity
        one("--count"),
        extend("--task"),
        extend("--task-file"),
        flag("--split"),
        one("--each"),
        crate::args::flag_or_value("--worktree"),
        one("--max"),
        one("--groups"),
        one("--by"),
        // payload and noun options
        extend("--set"),
        one("--from"),
        one("--name"),
        one("--title"),
        one("--cmd"),
        extend("--cwd"),
        flag("--agent"),
        one("--provider"),
        one("--text"),
        flag("--enter"),
        one("--path"),
        one("--key"),
        one("--button"),
        one("--until"),
        one("--timeout"),
        extend("--kind"),
        one("--since"),
        flag("--any"),
        one("--model"),
        one("--effort"),
        one("--budget"),
        flag("--read-only"),
        flag("--detached"),
        flag("--force"),
        flag("--focus"),
        extend("--add"),
        extend("--remove"),
        one("--layout"),
        one("--grep"),
        one("--match"),
        one("--range"),
        one("--word"),
        one("--line"),
        one("--color"),
        flag("--bury"),
        flag("--no-session"),
        flag("--unbury"),
        one("--context"),
        one("--zoom"),
        one("--sidebar"),
        one("--state"),
        flag("--show"),
        crate::args::flag_or_value("--history"),
        one("--item"),
        one("--as"),
        one("--tail"),
        one("--preset"),
        one("--events"),
        one("--suite"),
        one("--app-path"),
        one("--command"),
        crate::args::flag_or_value("--wait"),
        flag("--end-sessions"),
        one("--ready-timeout"),
        one("--value"),
        one("--mode"),
        one("--file"),
        flag("--mark-read"),
        flag("--sent"),
        flag("--all-panes"),
        one("--body"),
        one("--body-file"),
        extend("--after"),
        one("--assign"),
        one("--status"),
        flag("--mine"),
        flag("--ready"),
        one("--message"),
        flag("--ff-only"),
        flag("--no-ff"),
        one("--check"),
        flag("--close"),
        flag("--paused"),
        flag("--heads"),
        one("--lines"),
        one("--limit"),
        flag("--scrollback"),
        one("--stall"),
        one("--ports"),
        one("--port-pool"),
        one("--for"),
        flag("--revoke"),
        flag("--keep-going"),
    ]
}

fn bad(flag: &str, msg: impl Into<String>) -> GrammarError {
    GrammarError::Flag {
        flag: flag.to_string(),
        msg: msg.into(),
    }
}

fn sel_opt(p: &Parsed, name: &str) -> Result<Option<Expr>, GrammarError> {
    p.one(name).map(selector).transpose()
}

fn num<T: std::str::FromStr>(p: &Parsed, name: &str) -> Result<Option<T>, GrammarError> {
    p.one(name)
        .map(|v| {
            v.trim()
                .parse::<T>()
                .map_err(|_| bad(name, format!("'{v}' is not a number")))
        })
        .transpose()
}

/// `<noun> <verb> …` as a typed Request. `words` are the arguments after the noun.
pub fn parse(noun: Noun, words: &[String]) -> Result<Request, GrammarError> {
    // `tree [SEL]`: one verb, so its words are the selector
    let listed: Vec<String>;
    let words = if noun == Noun::Tree && words.first().is_none_or(|w| w != "list") {
        listed = std::iter::once("list".to_string()).chain(words.iter().cloned()).collect();
        &listed[..]
    } else {
        words
    };
    let verb_word = words.first().map(String::as_str).unwrap_or("list");
    let verb = Verb::parse(verb_word).ok_or_else(|| GrammarError::UnknownVerb {
        noun,
        verb: verb_word.to_string(),
    })?;
    if !has(noun, verb) {
        return Err(GrammarError::NotApplicable { noun, verb });
    }
    let mut rest: Vec<String> = words.iter().skip(1).cloned().collect();
    join_mark_name(&mut rest);
    let p = args::parse(&rest, &options(), 64).map_err(|m| bad("arguments", m))?;
    refuse_read_slots(class_of(noun, verb), verb, &p)?;
    // each slot checked in turn: the first bad flag is the error
    let (sel, place, within) = selection_and_placement(noun, verb, &p)?;
    let geom = geometry(&p)?;
    let multi = multiplicity(&p)?;
    let payload = payload(&p)?;
    let wheres = wheres(&p)?;
    let of = p
        .one("--of")
        .map(|k| Kind::parse(k).ok_or_else(|| bad("--of", "window|vtab|tab|pane")))
        .transpose()?;
    if let Some(b) = p.one("--as") {
        if !is_slug(b) {
            return Err(bad("--as", "a plain name (letters, digits, _ and -)"));
        }
    }
    Ok(Request {
        noun,
        verb,
        sel,
        words: p.positional.clone(),
        within,
        of,
        wheres,
        group: p.one("--group").map(str::to_string),
        all: p.has("--all"),
        place,
        geom,
        multi,
        payload,
        // open by default: every write runs unless --plan asks for a dry run (--apply is accepted, a no-op)
        apply: !p.has("--plan"),
        force: p.has("--force"),
        focus: p.has("--focus") || verb == Verb::Select,
        bind: p.one("--as").map(str::to_string),
        p,
        rest,
    })
}

/// `content scroll --to mark NAME`: the mark's name belongs to `--to` (`mark:NAME`), not to the selector.
fn join_mark_name(rest: &mut Vec<String>) {
    if let Some(i) = rest.windows(2).position(|w| w[0] == "--to" && w[1] == "mark") {
        if let Some(name) = rest.get(i + 2).filter(|n| !n.starts_with('-')).cloned() {
            rest.splice(i + 1..i + 3, [format!("mark:{name}")]);
        }
    }
}

/// A read verb (but `watch` and `wait`) takes no placement, geometry, multiplicity or payload.
fn refuse_read_slots(class: Class, verb: Verb, p: &Parsed) -> Result<(), GrammarError> {
    if class != Class::Read || verb == Verb::Watch || verb == Verb::Wait {
        return Ok(());
    }
    for (slot, names) in [("placement", SLOT_PLACE), ("geometry", SLOT_GEOM), ("multiplicity", SLOT_MULTI)] {
        if names.iter().any(|n| p.given(n)) {
            return Err(GrammarError::Slot { slot, verb });
        }
    }
    if p.given("--set") {
        return Err(GrammarError::Slot { slot: "payload", verb });
    }
    Ok(())
}

/// What a request acts on and where what it makes goes: the selector (from the positional words), the
/// placement, and `--in` as "under" (`within`). `--in` is a placement for a create, "under" for the rest, and
/// for `content` the one pane it acts on.
fn selection_and_placement(noun: Noun, verb: Verb, p: &Parsed) -> Result<(Option<Expr>, Placement, Option<Expr>), GrammarError> {
    let create = class_of(noun, verb) == Class::Create;
    let words = &p.positional;
    // names after `new` are its payload, not a selector; content's creates (mark, save) act on panes: their
    // words select, except `content mark NAME`
    let names_payload = create && noun.kind().is_some() && (noun != Noun::Content || (verb == Verb::Mark && !p.given("--name")));
    // `app menu ITEM`: the item's title, not a selector
    let menu_item = noun == Noun::App && verb == Verb::Menu;
    let sel = if words.is_empty() || names_payload || menu_item || noun.kind().is_none() {
        None
    } else {
        Some(selector(&words.join(" "))?)
    };
    let side = p
        .one("--side")
        .map(|s| Dir::parse(s).ok_or_else(|| bad("--side", "left|right|up|down")))
        .transpose()?;
    let stack = p
        .one("--stack")
        .map(|s| {
            Dir::parse(s)
                .filter(|d| matches!(d, Dir::Right | Dir::Down))
                .ok_or_else(|| bad("--stack", "right|down"))
        })
        .transpose()?;
    // content scroll|save: --to is a position or a file, not a placement
    let raw_to = noun == Noun::Content && matches!(verb, Verb::Scroll | Verb::Save);
    let to = match p.one("--to") {
        None => None,
        Some(_) if raw_to => None,
        Some("new-window") => Some(To::NewWindow),
        Some("new-vtab") => Some(To::NewVTab),
        Some("new-tab") => Some(To::NewTab),
        Some(s) => Some(To::Sel(selector(s)?)),
    };
    let (into, within) = if create {
        (sel_opt(p, "--in")?.or(sel_opt(p, "--into")?), None)
    } else {
        (sel_opt(p, "--into")?, sel_opt(p, "--in")?)
    };
    // content acts on one pane's grid: `content VERB --in PANE` names that pane (never a placement, never "under")
    let (sel, into, within) = match (noun == Noun::Content, sel) {
        (true, None) if p.given("--in") => (sel_opt(p, "--in")?, None, None),
        (_, sel) => (sel, into, within),
    };
    let place = Placement { at: sel_opt(p, "--at")?, side, into, to, stack };
    Ok((sel, place, within))
}

/// The geometry slot: `--weights`, `--ratio`, `--cols/--rows`, `--equalize`, `--fit`, `--tile`, `--screen`,
/// `--frame`, each checked.
fn geometry(p: &Parsed) -> Result<Geometry, GrammarError> {
    let weights = p
        .one("--weights")
        .map(|w| crate::geometry::numbers(w).map_err(|e| bad("--weights", e)))
        .transpose()?;
    if let Some(w) = &weights {
        if w.is_empty() || w.iter().any(|x| *x <= 0.0) {
            return Err(bad("--weights", "positive numbers, one per node"));
        }
    }
    let ratio: Option<f64> = num(p, "--ratio")?;
    if ratio.is_some_and(|r| !(0.05..=0.95).contains(&r)) {
        return Err(bad("--ratio", "between 0.05 and 0.95"));
    }
    let frame = match p.one("--frame") {
        Some(f) => {
            let n = crate::geometry::numbers(f).map_err(|e| bad("--frame", e))?;
            if n.len() != 4 || n[2] <= 0.0 || n[3] <= 0.0 {
                return Err(bad("--frame", "x,y,w,h with w and h positive"));
            }
            Some(Frame { x: n[0], y: n[1], width: n[2], height: n[3] })
        }
        None => None,
    };
    let tile = p
        .one("--tile")
        .map(|t| Tile::parse(t).ok_or_else(|| bad("--tile", "columns|rows|grid")))
        .transpose()?;
    Ok(Geometry {
        weights,
        ratio,
        cols: num(p, "--cols")?,
        rows: num(p, "--rows")?,
        equalize: p.has("--equalize"),
        fit: p.has("--fit"),
        tile,
        screen: p.one("--screen").map(str::to_string),
        frame,
    })
}

/// The multiplicity slot: how many of a create (`--count`, `--task…`, `--each`), `--worktree`, `--max`, the
/// partitions of a move (`--groups`, `--by`) and `--ports`, each checked.
fn multiplicity(p: &Parsed) -> Result<Multiplicity, GrammarError> {
    let count: Option<usize> = num(p, "--count")?;
    if count.is_some_and(|c| !(1..=32).contains(&c)) {
        return Err(bad("--count", "1–32"));
    }
    let max: Option<usize> = num(p, "--max")?;
    let groups: Option<usize> = num(p, "--groups")?;
    if groups.is_some_and(|g| g == 0 || g > 32) {
        return Err(bad("--groups", "1–32 parts"));
    }
    let by = match p.one("--by") {
        None => None,
        Some("cwd") => Some(By::Cwd),
        Some("project") => Some(By::Project),
        Some("kind") => Some(By::Kind),
        Some("group") => Some(By::Group),
        Some(_) => return Err(bad("--by", "cwd|project|kind|group")),
    };
    if groups.is_some() && by.is_some() {
        return Err(bad("--groups", "pass --groups N or --by K, not both"));
    }
    let ports: Option<u32> = num(p, "--ports")?;
    if ports.is_some_and(|n| n == 0 || n > MAX_PORTS) {
        return Err(bad("--ports", format!("1–{MAX_PORTS}")));
    }
    if let Some(pool) = p.one("--port-pool") {
        parse_pool(pool).map_err(|e| bad("--port-pool", e))?;
        if ports.is_none() {
            return Err(bad("--port-pool", "needs --ports N"));
        }
    }
    Ok(Multiplicity {
        count,
        tasks: p.many("--task"),
        task_files: p.many("--task-file"),
        each: p.one("--each").map(str::to_string),
        worktree: p.given("--worktree").then(|| p.one("--worktree").map(str::to_string)),
        max,
        groups,
        by,
        ports,
    })
}

/// `--set key=value…`.
fn payload(p: &Parsed) -> Result<Vec<(String, String)>, GrammarError> {
    p.many("--set")
        .into_iter()
        .map(|kv| {
            let (k, v) = kv.split_once('=').ok_or_else(|| bad("--set", format!("'{kv}' is not key=value")))?;
            Ok((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// `--where k=v…` (also `k!=v`, `k~glob`, `k~/re/`).
fn wheres(p: &Parsed) -> Result<Vec<(String, Match)>, GrammarError> {
    p.many("--where")
        .into_iter()
        .map(|w| field(&w).ok_or_else(|| bad("--where", format!("'{w}' is not k=v, k!=v, k~glob or k~/re/"))))
        .collect()
}

impl Request {
    /// The CRUD class of this request's verb on its noun.
    pub fn class(&self) -> Class {
        class_of(self.noun, self.verb)
    }
}

/// Whether `argv` (after global flags) is a v2 command: a noun, then a verb or nothing that
/// an old command of the same name takes (`layout --grid`, `key --pane`, `app launch`).
pub fn is_v2(words: &[String]) -> Option<Noun> {
    let noun = Noun::parse(words.first()?)?;
    let second = words.get(1).map(String::as_str);
    match noun {
        // old commands with these names: `layout --grid …`, `key --pane …`, `app launch|quit|status`
        Noun::Layout | Noun::Key => second.and_then(Verb::parse).map(|_| noun),
        Noun::App => match second {
            Some("launch") | Some("quit") | Some("status") | None => None,
            Some(s) if s.starts_with('-') => None,
            _ => Some(noun),
        },
        _ => Some(noun),
    }
}

// ---- help and coverage ---------------------------------------------------------------------

pub const HELP: &str = "\
iterm <noun> <verb> [SELECTOR] [PLACEMENT] [GEOMETRY] [MULTIPLICITY] [PAYLOAD] [--plan]   schema v2 (vterm's grammar; old commands still work)
nouns    tree app window vtab tab pane content ui group agent task profile setting key layout script preset snippet action broadcast   (help NOUN)
select   ID|prefix|name|glob|/w/1/t/2|1-4|odd|half:1|last|@me|@focused|@parent|@right[:X]|state:busy|neighbor:left|recent:3|var:k=v
         k=v k~glob k~/re/ · and or not ( ) , · --in SEL --of KIND --where k=v --group G --all · $NAME (batch --as NAME)
place    --at SEL --side left|right|up|down · --in SEL · --to new-window|new-vtab|new-tab|SEL · --stack right|down
geometry --weights 1,2,1 · --ratio 0.6 · --cols N --rows N · --equalize · --fit · --tile columns|rows|grid [--screen S] · --frame x,y,w,h
many     --count N · {i} {n} · --task T… · --task-file F (one task; --split: one per ## section or list item) · --each FILE · --worktree[=BASE] · --max N · --groups N | --by cwd|project|kind · --ports N
payload  --set key=value… (keys: profile keys | setting keys)
safety   writes run at once; --plan shows what one would do (--apply is accepted) · --force: close a live agent or your own pane, quit with live agents
         no write moves the human's focus unless select or --focus; --json replies carry schema_version";

pub fn noun_help(noun: Noun) -> String {
    let mut out = vec![format!(
        "iterm {noun} <verb>   ({})",
        match noun.kind() {
            Some(k) => format!(
                "selects {} nodes",
                match k {
                    Kind::App => "the app",
                    Kind::Window => "window",
                    Kind::VTab => "vertical-tab",
                    Kind::Tab => "tab",
                    Kind::Pane => "pane",
                }
            ),
            None => "by name".to_string(),
        }
    )];
    for (v, h) in verbs(noun) {
        let slot = if slot_only(noun, *v) {
            " (not wired yet)"
        } else {
            ""
        };
        out.push(
            format!(
                "  {:<10} {} {h}{slot}",
                v.name(),
                class_of(noun, *v).letter()
            )
            .trim_end()
            .to_string(),
        );
    }
    out.join("\n")
}

/// `iterm coverage --json`: what the CLI covers, from the dispatcher's own tables.
pub fn coverage() -> serde_json::Value {
    use serde_json::{json, Map, Value};
    let mut grammar = Map::new();
    for noun in Noun::ALL {
        let mut row = Map::new();
        for verb in Verb::ALL {
            let cell = if has(*noun, *verb) {
                let c = class_of(*noun, *verb).letter();
                if slot_only(*noun, *verb) {
                    format!("{c}*")
                } else {
                    c.to_string()
                }
            } else {
                "n/a".to_string()
            };
            row.insert(verb.name().into(), Value::String(cell));
        }
        grammar.insert(noun.name().into(), Value::Object(row));
    }
    json!({
        "schema_version": SCHEMA_VERSION,
        "grammar": grammar,
        "menu": {"via": "app menu", "wired": true},
        "keys": KEY_VERBS
            .iter()
            .map(|(a, c, exact)| {
                let v = if *exact { format!("iterm {c}") } else { format!("~ iterm {c} (approximate; exact: iterm action run {a})") };
                (a.to_string(), Value::String(v))
            })
            .collect::<Map<String, Value>>(),
        "gestures": GESTURES.iter().map(|(g, c)| (g.to_string(), Value::String(c.to_string()))).collect::<Map<String, Value>>(),
        "settings": "schema (iterm setting keys)",
        "context_menus": true,
        "backend": "iterm",
        "aliases": ALIASES.iter().map(|(a, b)| (a.to_string(), Value::String(b.to_string()))).collect::<Map<String, Value>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn selectors_parse_into_a_typed_ast() {
        assert_eq!(selector("w1").unwrap(), Expr::Atom(Atom::Word("w1".into())));
        assert_eq!(
            selector("w1 w2").unwrap(),
            Expr::Or(vec![
                Expr::Atom(Atom::Word("w1".into())),
                Expr::Atom(Atom::Word("w2".into()))
            ])
        );
        assert_eq!(
            selector("1-4").unwrap(),
            Expr::Atom(Atom::Index(Pick::Span(1, 4)))
        );
        assert_eq!(
            selector("half:2").unwrap(),
            Expr::Atom(Atom::Index(Pick::Part(2, 2)))
        );
        assert_eq!(
            selector("@right:w1").unwrap(),
            Expr::Atom(Atom::Rel(
                Rel::Right,
                Some(Box::new(Atom::Word("w1".into())))
            ))
        );
        let e = selector("state:busy and not (name~w* or kind=omp), $team").unwrap();
        let Expr::Or(parts) = &e else { panic!("{e:?}") };
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1], Expr::Atom(Atom::Bind("team".into())));
        let Expr::And(a) = &parts[0] else {
            panic!("{e:?}")
        };
        assert_eq!(a[0], Expr::Atom(Atom::State("busy".into())));
        assert!(matches!(&a[1], Expr::Not(_)));
        assert_eq!(
            selector(r"cwd~/api(\/v2)?$/").unwrap(),
            Expr::Atom(Atom::Field("cwd".into(), Match::Re("api(/v2)?$".into())))
        );
        assert_eq!(
            selector("!w1").unwrap(),
            Expr::Not(Box::new(Expr::Atom(Atom::Word("w1".into()))))
        );
        assert_eq!(
            selector("state!=BUSY").unwrap(),
            Expr::Atom(Atom::Field("state".into(), Match::Ne("BUSY".into())))
        );
        assert_eq!(
            selector("/w/1/t/2").unwrap(),
            Expr::Atom(Atom::Path(vec![
                (Kind::Window, "1".into()),
                (Kind::Tab, "2".into())
            ]))
        );
        assert!(selector("(w1").is_err());
        assert!(selector("@nowhere").is_err());
        assert!(glob("w*", "W12") && !glob("w?", "w12"));
    }

    #[test]
    fn requests_fill_typed_slots_and_refuse_na_cells() {
        let r = parse(
            Noun::Pane,
            &s(&[
                "new",
                "--agent",
                "--count",
                "7",
                "--at",
                "@right",
                "--stack",
                "down",
                "--worktree",
                "--weights",
                "1,1,2,2,2,1,1",
            ]),
        )
        .unwrap();
        assert_eq!(r.verb, Verb::New);
        assert_eq!(r.multi.count, Some(7));
        assert_eq!(r.multi.worktree, Some(None));
        assert_eq!(r.place.stack, Some(Dir::Down));
        assert_eq!(r.geom.weights.as_ref().map(Vec::len), Some(7));
        let e = parse(Noun::Content, &s(&["move"])).unwrap_err();
        assert!(
            e.to_string().contains("n/a") && e.to_string().contains("content"),
            "{e}"
        );
        let e = parse(Noun::Window, &s(&["list", "--weights", "1,2"])).unwrap_err();
        assert!(
            matches!(
                e,
                GrammarError::Slot {
                    slot: "geometry",
                    ..
                }
            ),
            "{e}"
        );
        assert!(
            parse(Noun::Pane, &s(&["new", "--worktree=main"]))
                .unwrap()
                .multi
                .worktree
                == Some(Some("main".into()))
        );
        assert_eq!(is_v2(&s(&["layout", "--grid", "2x2"])), None);
        assert_eq!(is_v2(&s(&["layout", "list"])), Some(Noun::Layout));
        assert_eq!(is_v2(&s(&["app", "launch"])), None);
        assert_eq!(is_v2(&s(&["app", "menu"])), Some(Noun::App));
        assert_eq!(is_v2(&s(&["key", "--pane", "x"])), None);
        let cov = coverage();
        assert_eq!(cov["grammar"]["window"]["merge"], "U");
        assert_eq!(cov["grammar"]["content"]["move"], "n/a");
        assert_eq!(cov["grammar"]["task"]["claim"], "U");
    }

    // regression: the recursive matcher backtracked exponentially (14 stars on 40 chars: no answer in 45 s)
    #[test]
    fn globs_match_as_before_without_backtracking() {
        for (p, t, want) in [
            ("w*", "w12", true), ("*", "", true), ("", "", true), ("", "x", false), ("?", "", false), ("?", "é", true),
            ("a*b", "acb", true), ("a*b", "acbc", false), ("A*B", "axb", true), ("*.rs", "a.b.rs", true), ("a**b", "ab", true),
            ("a*?", "a", false), ("a*?", "ab", true), ("*a*b*", "xaxbx", true), ("*a*b", "xbxa", false),
        ] {
            assert_eq!(glob(p, t), want, "{p} {t}");
        }
        assert!(!glob(&("a*".repeat(30) + "b"), &"a".repeat(200)));
    }
}
