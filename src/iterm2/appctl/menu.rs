//! iTerm2's main menu: every item iTerm2's `MenuItemRequest` can run (`menu_items.tsv`, generated from iTerm2's
//! own `MainMenu.xib` by `scripts/gen_menu_items.py`), and perf term's key-map actions mapped onto those items.

/// One runnable main-menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    /// "Edit > Copy".
    pub path: &'static str,
    /// What `MenuItemRequest` takes.
    pub id: &'static str,
    /// Its shortcut as iTerm2 ships it ("cmd+c"), empty when unbound.
    pub key: &'static str,
}

static TSV: &str = include_str!("menu_items.tsv");

/// Items iTerm2 binds a key to in code rather than in its menu file.
const CODE_KEYS: [(&str, &str); 1] = [("Maximize Active Pane", "shift+cmd+return")];

pub fn items() -> impl Iterator<Item = Item> {
    TSV.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).filter_map(|l| {
        let mut f = l.split('\t');
        let (path, id, key) = (f.next()?, f.next()?, f.next().unwrap_or(""));
        let key = if key.is_empty() { CODE_KEYS.iter().find(|(i, _)| *i == id).map_or("", |(_, k)| *k) } else { key };
        Some(Item { path, id, key })
    })
}

/// "Find…", "Find..." and "find" are one name.
fn norm(s: &str) -> String {
    s.trim().trim_end_matches('…').trim_end_matches("...").trim().to_lowercase()
}

fn last(path: &str) -> &str {
    path.rsplit(" > ").next().unwrap_or(path)
}

/// The item a name means: its path, its identifier, or its last part (case-insensitive, a trailing `…`
/// ignored). A last part shared by several items fails with them listed.
pub fn find(want: &str) -> Result<Item, String> {
    let w = norm(want);
    let all: Vec<Item> = items().collect();
    if let Some(i) = all.iter().find(|i| norm(i.path) == w).or_else(|| all.iter().find(|i| norm(i.id) == w)) {
        return Ok(*i);
    }
    let hits: Vec<&Item> = all.iter().filter(|i| norm(last(i.path)) == w).collect();
    match hits.as_slice() {
        [one] => Ok(**one),
        [] => {
            let near: Vec<&str> = all.iter().filter(|i| norm(i.path).contains(&w)).map(|i| i.path).take(5).collect();
            Err(if near.is_empty() {
                format!("no menu item {want:?} (iterm app menu lists them)")
            } else {
                format!("no menu item {want:?}; did you mean: {}", near.join(" | "))
            })
        }
        many => Err(format!("{want:?} names {} items; give the path: {}", many.len(), many.iter().map(|i| i.path).collect::<Vec<_>>().join(" | "))),
    }
}

/// A perf term key-map action on iTerm2: the menu item it runs, and whether it is itself a focus action (the
/// human's focus is then left where the action put it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub name: &'static str,
    pub item: &'static str,
    pub does: &'static str,
    pub focus: bool,
}

const fn a(name: &'static str, item: &'static str, does: &'static str, focus: bool) -> Action {
    Action { name, item, does, focus }
}

/// perf term's action names (`keymap::NAMES`) that iTerm2 has a menu item for. The rest (`vtab N`, `tab N`,
/// `swap_*`, `tab_to_vtab`, `move_vtab_*`, `sidebar`, `broadcast_vtab`, `broadcast_window`,
/// `broadcast_tab_agents`) have no menu item on iTerm2; their structural forms are `shape` commands.
pub const ACTIONS: &[Action] = &[
    a("new_window", "New Window", "Open a new window", true),
    a("new_tab", "New Tab", "Open a new tab", true),
    a("new_vtab", "Add Tab to New Group", "Put the tab in a new tab group (vertical tab)", false),
    a("close_pane", "Close", "Close the pane", false),
    a("split_right", "Split Vertically with Current Profile", "Split the pane, new pane on the right", true),
    a("split_down", "Split Horizontally with Current Profile", "Split the pane, new pane below", true),
    a("next_vtab", "Select Next Tab Group", "Select the next tab group", true),
    a("prev_vtab", "Select Previous Tab Group", "Select the previous tab group", true),
    a("next_tab", "Select Next Tab", "Select the next tab", true),
    a("prev_tab", "Select Previous Tab", "Select the previous tab", true),
    a("next_pane", "Select Split Pane.Next Pane", "Focus the next pane", true),
    a("prev_pane", "Select Split Pane.Previous Pane", "Focus the previous pane", true),
    a("focus_left", "Select Split Pane.Select Pane Left", "Focus the pane on the left", true),
    a("focus_right", "Select Split Pane.Select Pane Right", "Focus the pane on the right", true),
    a("focus_up", "Select Split Pane.Select Pane Above", "Focus the pane above", true),
    a("focus_down", "Select Split Pane.Select Pane Below", "Focus the pane below", true),
    a("move_tab_left", "Move Tab Left", "Move the tab left", false),
    a("move_tab_right", "Move Tab Right", "Move the tab right", false),
    a("rename", "Edit Tab Title", "Rename the tab (opens the title editor)", false),
    a("rename_pane", "Edit Session…", "Edit the pane's session (name, profile)", false),
    a("zoom", "Maximize Active Pane", "Maximize the pane over its tab, or restore", false),
    a("settings", "Preferences...", "Open Settings", false),
    a("help", "iTerm2 Help", "Open iTerm2's help", false),
    a("reopen", "Undo Close", "Reopen the last closed pane, tab or window", false),
    a("resize_left", "Resize Split Pane.Move Divider Left", "Move the pane's divider left", false),
    a("resize_right", "Resize Split Pane.Move Divider Right", "Move the pane's divider right", false),
    a("resize_up", "Resize Split Pane.Move Divider Up", "Move the pane's divider up", false),
    a("resize_down", "Resize Split Pane.Move Divider Down", "Move the pane's divider down", false),
    a("equalize", "Arrange Split Panes Evenly", "Give the tab's panes equal space", false),
    a("mark", "Marks and Annotations.Set Mark", "Mark the cursor's line", false),
    a("prev_mark", "Marks and Annotations.Previous Mark", "Scroll to the previous mark", false),
    a("next_mark", "Marks and Annotations.Next Mark", "Scroll to the next mark", false),
    a("find", "Find.Find...", "Open the find bar", false),
    a("find_next", "Find.Find Next", "Show the next match", false),
    a("find_prev", "Find.Find Previous", "Show the previous match", false),
    a("broadcast_tab", "Broadcast Input.Broadcast Input to All Panes in Current Tab", "Broadcast input to the tab's panes", false),
    a("broadcast_all", "Broadcast Input.Broadcast Input to All Panes in All Tabs", "Broadcast input to every pane", false),
    a("broadcast_pick", "Broadcast Input.Toggle Broadcast Input to Current Session", "Add or remove the pane from broadcast", false),
    a("clear_picks", "Broadcast Input.Send Input to Current Session Only", "Stop broadcasting", false),
    a("clear", "Clear Buffer", "Clear the pane's buffer", false),
    a("search", "Open Quickly", "Open Quickly (search everything by name)", false),
];

pub fn action(name: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|x| x.name == name)
}

/// The item an action runs (always in the table: a unit test checks every action).
pub fn action_item(x: &Action) -> Item {
    items().find(|i| i.id == x.item).unwrap_or(Item { path: x.item, id: x.item, key: "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_parses_and_names_resolve() {
        let all: Vec<Item> = items().collect();
        assert!(all.len() > 250, "{}", all.len());
        let c = find("copy").unwrap();
        assert_eq!((c.path, c.id, c.key), ("Edit > Copy", "Copy", "cmd+c"));
        assert_eq!(find("Edit > Find > Find…").unwrap().id, "Find.Find...");
        assert_eq!(find("find").unwrap().id, "Find.Find...");
        assert_eq!(find("Find.Find Next").unwrap().key, "cmd+g");
        assert_eq!(find("maximize active pane").unwrap().key, "shift+cmd+return");
        // "Reset" is Session > Reset (its identifier) and Session > Terminal State > Reset: the identifier wins
        assert_eq!(find("reset").unwrap().path, "Session > Reset");
        assert_eq!(find("Session > Terminal State > Reset").unwrap().id, "Reset Terminal State");
        assert!(find("nope nope").unwrap_err().starts_with("no menu item"));
        assert!(find("Paste Slow").unwrap_err().contains("Paste Slowly"));
    }

    #[test]
    fn every_action_runs_a_known_item() {
        for x in ACTIONS {
            assert!(items().any(|i| i.id == x.item), "{} -> {}", x.name, x.item);
        }
        assert_eq!(action_item(action("zoom").unwrap()).key, "shift+cmd+return");
        assert_eq!(action_item(action("split_right").unwrap()).key, "cmd+d");
    }
}
