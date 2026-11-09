//! The dock (`geometry::dock`, `exec.rs` `place_items`) driven through every way an agent is spawned, with the
//! shapes that break an implementation tuned to the even 50/50 tab of `cli.rs`'s dock tests: uneven shares, a
//! companion that closes, children moved away, queued and `agent run` spawns, and `--plan` against the run.

mod common;

use common::*;
use itermkit::app::wire::Side;
use itermkit::app::{Id, Op};

/// The lead and `st -l` to its right (the screenshot's tab): (lead, companion).
fn lead_and_companion(w: &World, ratio: Option<&str>) -> (Id, Id) {
    let lead = w.window();
    let r = ratio.map_or(String::new(), |r| format!(" --ratio {r}"));
    let (code, rows) = run(w.env(Some(lead)), &format!("pane new --at @me --side right --cmd 'st -l'{r}"));
    assert_eq!(code, 0, "{rows:?}");
    (lead, pid(&rows[0]))
}

fn new_agent(w: &World, me: Id, args: &str) -> Id {
    let (code, rows) = run(w.env(Some(me)), &format!("agent new {args}"));
    assert_eq!(code, 0, "{args}: {rows:?}");
    pid(&rows[0])
}

/// The last split the CLI asked the app for: (pane split, side).
fn last_split(w: &World) -> (Id, Side) {
    w.with(|m, _| {
        m.ops.iter().rev().find_map(|o| match o {
            Op::Split { at, side, .. } => Some((*at, *side)),
            _ => None,
        })
    })
    .expect("a split")
}

#[test]
fn pane_new_agent_docks_as_agent_new_does() {
    // the spec names `pane new --agent` beside `agent new`; only the latter was driven
    let w = world("dock-pane-agent");
    let (lead, c) = lead_and_companion(&w, None);
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent");
    assert_eq!(code, 0, "{rows:?}");
    let a1 = pid(&rows[0]);
    let a2 = pid(&run(w.env(Some(lead)), "pane new --agent").1[0]);
    let (l, c, a1, a2) = (lead.0, c.0, a1.0, a2.0);
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{c}(0.50) / [{a1}(0.50) | {a2}(0.50)](0.50)](0.50)]"));
}

#[test]
fn agent_run_with_three_tasks_fills_one_row() {
    // the spec's "`agent run` with 3 tasks": one row of three equal columns under the companion
    let w = world("dock-run");
    let (lead, c) = lead_and_companion(&w, None);
    let (code, rows) = run(w.env(Some(lead)), "agent run one two three --read-only --timeout 0.1");
    assert_eq!(code, 3, "the three are still busy when the wait ends: {rows:?}");
    let kids: Vec<u64> = rows.iter().filter(|r| s(r, "code") == "running").map(|r| itermkit::app::model::parse_handle(s(r, "pane")).unwrap().1 .0).collect();
    assert_eq!(kids.len(), 3, "{rows:?}");
    let (l, c) = (lead.0, c.0);
    let row: Vec<String> = kids.iter().map(|k| format!("{k}(0.33)")).collect();
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{c}(0.50) / [{}](0.50)](0.50)]", row.join(" | ")));
}

#[test]
fn the_spawner_and_the_companion_keep_uneven_sizes() {
    // with every share at 0.50 an equalization one level too high or too low passes unseen
    let w = world("dock-uneven");
    let (lead, c) = lead_and_companion(&w, Some("0.3"));
    let (l, cc) = (lead.0, c.0);
    assert_eq!(w.shape_of(lead), format!("[{l}(0.70) | {cc}(0.30)]"));
    let a1 = new_agent(&w, lead, "").0;
    assert_eq!(w.shape_of(lead), format!("[{l}(0.70) | [{cc}(0.50) / {a1}(0.50)](0.30)]"));
    // the human gives the companion more of its column; the next agents leave that alone
    let (code, rows) = run(w.human(), &format!("pane resize p{cc} --ratio 0.7"));
    assert_eq!(code, 0, "{rows:?}");
    let a2 = new_agent(&w, lead, "").0;
    let a3 = new_agent(&w, lead, "").0;
    assert_eq!(w.shape_of(lead), format!("[{l}(0.70) | [{cc}(0.70) / [{a1}(0.33) | {a2}(0.33) | {a3}(0.33)](0.30)](0.30)]"));
}

#[test]
fn plan_names_the_pane_and_side_the_run_splits() {
    // `--plan` and the run each compute the dock; they must agree at every step
    let w = world("dock-plan");
    let lead = w.window();
    let check = |me: Id, what: &str| {
        let (_, rows) = run(w.env(Some(me)), "agent new --plan");
        let (beside, side) = (s(&rows[0], "beside").to_string(), s(&rows[0], "side").to_string());
        let made = new_agent(&w, me, "");
        let (at, d) = last_split(&w);
        assert_eq!((beside, side), (format!("p{}", at.0), if d == Side::Right { "right" } else { "down" }.to_string()), "{what}");
        made
    };
    let a1 = check(lead, "alone and wide: beside itself, right");
    check(lead, "a child already: right of it");
    w.with(|m, ev| m.apply(Op::Split { at: lead, side: Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap());
    check(lead, "a shell appeared to my right, but I have children: still right of the last");
    check(a1, "a child in its parent's row: under itself");
    check(a1, "and then right of its own first");
}

#[test]
fn the_row_stays_even_when_the_companion_or_a_column_closes() {
    let w = world("dock-close");
    let (lead, c) = lead_and_companion(&w, None);
    let a: Vec<Id> = (0..3).map(|_| new_agent(&w, lead, "")).collect();
    // a column closes: the next agent rejoins an even row
    run(w.env(Some(lead)), &format!("pane close p{} --force", a[1].0)).1;
    let a4 = new_agent(&w, lead, "");
    let (l, cc) = (lead.0, c.0);
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{cc}(0.50) / [{}(0.33) | {}(0.33) | {}(0.33)](0.50)](0.50)]", a[0].0, a[2].0, a4.0));
    // the companion closes: iTerm2 flattens the row into the lead's split; the lead keeps its half
    let (code, rows) = run(w.env(Some(lead)), &format!("pane close p{cc}"));
    assert_eq!(code, 0, "{rows:?}");
    let a5 = new_agent(&w, lead, "");
    let row: Vec<String> = [a[0], a[2], a4, a5].iter().map(|x| format!("{}(0.12)", x.0)).collect();
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | {}]", row.join(" | ")));
}

#[test]
fn children_moved_to_another_tab_no_longer_pull_the_dock() {
    let w = world("dock-moved");
    let (lead, c) = lead_and_companion(&w, None);
    let a1 = new_agent(&w, lead, "");
    let (code, rows) = run(w.env(Some(lead)), &format!("pane move p{} --to new-tab", a1.0));
    assert_eq!(code, 0, "{rows:?}");
    // its old tab: the lead and the companion again; the next one starts the row under the companion
    let a2 = new_agent(&w, lead, "");
    assert_eq!(w.shape_of(lead), format!("[{}(0.50) | [{}(0.50) / {}(0.50)](0.50)]", lead.0, c.0, a2.0));
}

#[test]
fn a_child_alone_in_its_own_tab_docks_beside_itself() {
    // `in_a_row` asks whether the parent shares the child's tab: a child in a tab of its own is a lone spawner
    let w = world("dock-own-tab");
    let lead = w.window();
    let (code, rows) = run(w.env(Some(lead)), "tab new --agent");
    assert_eq!(code, 0, "{rows:?}");
    let kid = itermkit::app::model::parse_handle(s(&rows[0], "pane")).unwrap().1;
    let g = new_agent(&w, kid, "");
    assert_eq!(w.shape_of(kid), format!("[{}(0.50) | {}(0.50)]", kid.0, g.0));
}

#[test]
fn any_placement_flag_bypasses_the_dock() {
    let w = world("dock-explicit");
    let (lead, c) = lead_and_companion(&w, None);
    // --side alone: beside me on that side (not under the companion)
    new_agent(&w, lead, "--side down");
    assert_eq!(last_split(&w), (lead, Side::Down));
    // --in / --to TAB: beside that tab's last pane
    let tab = w.node(lead).unwrap().parent.unwrap();
    new_agent(&w, lead, &format!("--in t{}", tab.0));
    assert_ne!(last_split(&w).0, c, "not the dock's pick");
    // --to new-tab: a tab of its own
    let tabs = || w.with(|m, _| m.nodes.values().filter(|n| n.kind == itermkit::app::Kind::Tab).count());
    let before = tabs();
    new_agent(&w, lead, "--to new-tab");
    assert_eq!(tabs(), before + 1);
}

#[test]
fn a_queued_agent_starts_in_the_dock() {
    let w = world("dock-queue");
    let (lead, c) = lead_and_companion(&w, None);
    // --max 1 counts agents only (not the companion): one runs and one waits
    let (code, rows) = run(w.env(Some(lead)), "agent new --task a --task b --max 1 --read-only");
    assert_eq!((code, codes(&rows)), (0, vec!["ok created".to_string(), "ok queued".to_string()]), "{rows:?}");
    let a = pid(&rows[0]);
    w.exit(a, 0);
    let (_, rows) = run(w.env(Some(lead)), "children");
    let b = pid(&rows[0]);
    assert_eq!(s(&rows[0], "code"), "created", "{rows:?}");
    assert_eq!(w.shape_of(lead), format!("[{}(0.50) | [{}(0.50) / {}(0.50)](0.50)]", lead.0, c.0, b.0));
}

#[test]
fn max_counts_agents_not_the_companion() {
    let w = world("dock-max");
    let (lead, _) = lead_and_companion(&w, None);
    let (_, rows) = run(w.env(Some(lead)), "agent new --task a --task b --max 1 --read-only");
    assert_eq!(codes(&rows), ["ok created", "ok queued"], "{rows:?}");
}

#[test]
fn agents_with_a_command_grow_a_row_too() {
    let w = world("dock-cmd");
    let (lead, c) = lead_and_companion(&w, None);
    let a: Vec<Id> = (0..3).map(|_| new_agent(&w, lead, "--cmd worker")).collect();
    let row: Vec<String> = a.iter().map(|x| format!("{}(0.33)", x.0)).collect();
    assert_eq!(w.shape_of(lead), format!("[{}(0.50) | [{}(0.50) / [{}](0.50)](0.50)]", lead.0, c.0, row.join(" | ")));
}
