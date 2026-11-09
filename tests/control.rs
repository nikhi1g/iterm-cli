//! The control plane end to end against the fake app: tree, actions, window cells, content, menus, dialogs,
//! and that none of these slots is "(not wired yet)" any more.

mod common;

use common::*;
use itermkit::app::Op;

#[test]
fn tree_is_one_line_per_node_from_one_snapshot() {
    let w = world("tree");
    let p = w.window();
    w.with(|m, _| {
        let n = m.nodes.get_mut(&p).unwrap();
        n.vars.insert("agent.provider".into(), "claude".into());
        n.vars.insert("agent.state".into(), "idle".into());
    });
    let before = w.with(|m, _| m.ops.len());
    let (code, out) = text(w.human(), "tree");
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with('w') && lines[0].ends_with('*'), "{out}");
    assert!(lines[1].starts_with("  v") && lines[2].starts_with("    t"), "{out}");
    assert!(lines[3].starts_with(&format!("      p{}* ", p.0)) && lines[3].contains("agent=claude:idle") && lines[3].contains("/tmp"), "{out}");
    // a read: no operation reached the app
    assert_eq!(w.with(|m, _| m.ops.len()), before);
    let (_, rows) = run(w.human(), "tree");
    assert_eq!(rows[0]["windows"][0]["vtabs"][0]["tabs"][0]["panes"][0]["state"], "idle");
    let (_, out) = text(w.human(), &format!("tree p{}", p.0));
    assert_eq!(out.lines().count(), 4);
    let (code, _) = text(w.human(), "tree nothing-named-this");
    assert_eq!(code, 1);
}

#[test]
fn actions_list_get_run_in_a_pane() {
    let w = world("action");
    let p = w.window();
    let (code, rows) = run(w.human(), "action list");
    assert_eq!((code, rows.len()), (0, 3), "{rows:?}");
    assert_eq!(s(&rows[1], "name"), "zoom");
    assert_eq!(s(&rows[1], "key"), "shift+cmd+return");
    let (_, rows) = run(w.human(), "action get vtab 3");
    assert_eq!(s(&rows[0], "name"), "vtab N");
    let (code, _) = run(w.human(), "action get nope");
    assert_eq!(code, 1);
    let (code, rows) = run(w.human(), &format!("action run zoom --in p{}", p.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok ran".to_string()]), "{rows:?}");
    assert!(w.with(|m, _| m.ops.iter().any(|o| *o == Op::Action { name: "zoom".into(), pane: Some(p) })));
    // the human's focused pane when no --in
    let (_, rows) = run(w.human(), "action run vtab 2");
    assert_eq!(s(&rows[0], "in"), format!("p{}", p.0));
    let (_, rows) = run(w.human(), "action run zoom --plan");
    assert_eq!(codes(&rows), ["planned run"]);
}

#[test]
fn zoom_sidebar_reopen_and_panels_have_verbs() {
    // regression (2026-10-29 CLI parity): these keys had no command of their own
    let w = world("keyverbs");
    let p = w.window();
    let q = w.with(|m, ev| m.apply(Op::Split { at: p, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0]);
    let presses = |w: &World, name: &str| w.with(|m, _| m.ops.iter().filter(|o| matches!(o, Op::Action { name: n, .. } if n == name)).count());
    // zoom: pressed only when the state differs (the tab's `zoom` var)
    let (code, rows) = run(w.human(), &format!("pane set p{} --zoom on", q.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok set".to_string()]), "{rows:?}");
    assert_eq!((s(&rows[0], "zoom"), presses(&w, "zoom")), ("on", 1));
    let tab = w.node(q).unwrap().parent.unwrap();
    w.with(|m, _| m.nodes.get_mut(&tab).unwrap().vars.insert("zoom".into(), format!("p{}", q.0)));
    run(w.human(), &format!("pane set p{} --zoom on", q.0));
    assert_eq!(presses(&w, "zoom"), 1, "already zoomed: no press");
    run(w.human(), &format!("pane set p{} --zoom off", q.0));
    assert_eq!(presses(&w, "zoom"), 2);
    let (code, _) = run(w.human(), &format!("pane set p{} --zoom sideways", q.0));
    assert_eq!(code, 1);
    // two panes of one tab: refused before any press (each press changes the tab's zoom)
    let (code, rows) = run(w.human(), &format!("pane set p{},p{} --zoom on", p.0, q.0));
    assert_eq!((code, presses(&w, "zoom")), (1, 2), "{rows:?}");
    assert!(s(&rows[0], "reason").contains("one pane per tab"), "{rows:?}");
    // sidebar: the window's `sidebar` var (off when hidden)
    let win = w.with(|m, _| m.nodes.iter().find(|(_, n)| n.kind == itermkit::app::Kind::Window).map(|(i, _)| *i).unwrap());
    let (code, rows) = run(w.human(), &format!("window set w{} --sidebar on", win.0));
    assert_eq!((code, s(&rows[0], "sidebar"), presses(&w, "sidebar")), (0, "on", 0), "{rows:?}");
    let (_, rows) = run(w.human(), &format!("window set w{} --sidebar toggle", win.0));
    assert_eq!((s(&rows[0], "sidebar"), presses(&w, "sidebar")), ("off", 1), "{rows:?}");
    // the panels and reopen
    let (code, rows) = run(w.human(), "ui open settings");
    assert_eq!((code, codes(&rows), presses(&w, "settings")), (0, vec!["ok opened".to_string()], 1), "{rows:?}");
    assert_eq!(run(w.human(), "ui open nothing").0, 1);
    let (code, rows) = run(w.human(), "app reopen");
    assert_eq!((code, presses(&w, "reopen")), (1, 1), "the fake reopens nothing: {rows:?}");
    use itermkit::grammar::{noun_help, Noun};
    assert!(noun_help(Noun::Pane).contains("new-vtab") && noun_help(Noun::App).contains("reopen"));
}

#[test]
fn every_matrix_cell_reaches_a_handler() {
    // the grammar audit as a test: no cell `help` lists falls through to a dispatcher's catch-all
    use itermkit::grammar::{Kind, MATRIX};
    let unwired = ["is not implemented", "has no reader", "is not an agent verb", "is not a ui operation", "is n/a", "not wired"];
    let (mut cells, mut ran) = (0, 0);
    for (noun, verbs) in MATRIX {
        for (verb, _) in *verbs {
            // each cell for real (not --plan, which stops before the handler) in a world of its own
            let w = world("sweep");
            w.window();
            let sel = match noun.kind() {
                Some(Kind::App) | None => String::new(),
                Some(_) => "@focused".to_string(),
            };
            // watches and waits stream until a deadline: one that passes at once
            let cmd = format!("{noun} {verb} {sel} --force --timeout 0.01");
            let (_, rows) = run(w.human(), &cmd);
            for r in &rows {
                let reason = r.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                assert!(!unwired.iter().any(|u| reason.contains(u)), "{cmd}: {reason}");
            }
            cells += 1;
            ran += rows.iter().any(|r| r["status"] == "ok") as usize;
        }
    }
    // not vacuous: many cells run to an answer even with these bare arguments (the rest ask for theirs)
    assert!(ran * 3 > cells, "{ran} of {cells} cells answered ok");
}

#[test]
fn action_close_keeps_the_live_agent_rail() {
    let w = world("action-rail");
    let p = w.window();
    w.with(|m, _| {
        let n = m.nodes.get_mut(&p).unwrap();
        n.vars.insert("agent.kind".into(), "claude".into());
        n.vars.insert("agent.state".into(), "busy".into());
    });
    let (code, rows) = run(w.human(), &format!("action run close_pane --in p{}", p.0));
    assert_eq!((code, codes(&rows)), (2, vec!["refused live-agent".to_string()]), "{rows:?}");
    let (code, _) = run(w.human(), &format!("action run close_pane --in p{} --force", p.0));
    assert_eq!(code, 0);
}

#[test]
fn window_set_cols_rows_asks_the_app() {
    let w = world("cells");
    let p = w.window();
    let win = w.with(|m, _| m.up(p, itermkit::app::Kind::Window).unwrap());
    let (code, rows) = run(w.human(), &format!("window set w{} --cols 100 --rows 30", win.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok sized".to_string()]), "{rows:?}");
    assert_eq!(s(&rows[0], "size"), "100x30");
    let (code, _) = run(w.human(), &format!("window set w{} --cols 0", win.0));
    assert_eq!(code, 1);
}

#[test]
fn window_state_and_find_bar_ask_the_app() {
    let w = world("winstate");
    let p = w.window();
    let win = w.with(|m, _| m.up(p, itermkit::app::Kind::Window).unwrap());
    for st in ["zoomed", "minimized", "fullscreen", "normal"] {
        let (code, rows) = run(w.human(), &format!("window set w{} --state {st}", win.0));
        assert_eq!((code, codes(&rows)), (0, vec!["ok set".to_string()]), "{rows:?}");
        assert_eq!(w.with(|m, _| m.nodes[&win].vars.get("state").cloned()), Some(st.to_string()));
    }
    let (code, _) = run(w.human(), &format!("window set w{} --state huge", win.0));
    assert_eq!(code, 1);
    let (code, rows) = run(w.human(), &format!("content find p{} --match needle --show", p.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok shown".to_string()]), "{rows:?}");
}

#[test]
fn content_selection_marks_copy_open() {
    let w = world("content");
    let p = w.window();
    let (code, rows) = run(w.human(), &format!("content get p{}", p.0));
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(s(&rows[0], "selection"), "hello");
    assert_eq!(rows[0]["marks"], serde_json::json!(["2", "5:build"]));
    let (_, rows) = run(w.human(), &format!("content select p{} --line 1", p.0));
    assert_eq!((codes(&rows), s(&rows[0], "span")), (vec!["ok selected".to_string()], "1:0-1:4"));
    let (_, rows) = run(w.human(), &format!("content copy p{} --match hel+", p.0));
    assert_eq!(rows[0]["bytes"], 5);
    let (_, rows) = run(w.human(), &format!("content mark p{} --name build", p.0));
    assert_eq!(s(&rows[0], "row"), "7");
    let (_, rows) = run(w.human(), &format!("content open p{} --match https://", p.0));
    assert_eq!(s(&rows[0], "url"), "https://example.com/");
    let (code, rows) = run(w.human(), &format!("content scroll p{} --to mark build", p.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok scrolled".to_string()]), "{rows:?}");
    use itermkit::app::wire::{ContentAct, Grab};
    assert!(w.with(|m, _| m.ops.iter().any(|o| *o == Op::Content { pane: p, act: ContentAct::Copy(Some(Grab::Match("hel+".into()))) })));
    // `--in PANE` names the target, never the caller's own pane
    let me = w.with(|m, ev| m.apply(Op::Split { at: p, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0]);
    let (code, rows) = run(w.env(Some(me)), &format!("content mark --in p{} --name here", p.0));
    assert_eq!((code, s(&rows[0], "pane")), (0, format!("p{}", p.0).as_str()), "{rows:?}");
    let (_, rows) = run(w.env(Some(me)), &format!("content get --in p{}", p.0));
    assert_eq!(s(&rows[0], "pane"), format!("p{}", p.0).as_str(), "{rows:?}");
    // open model: clearing another pane's screen runs (no ownership check, no --apply)
    let (code, rows) = run(w.env(Some(me)), &format!("content clear p{}", p.0));
    assert_eq!(code, 0, "{rows:?}");
    // (this asserted the Jump of `content scroll` above, so it held whatever clear did)
    assert!(w.with(|m, _| matches!(m.ops.last(), Some(Op::Clear { pane, .. }) if *pane == p)), "{rows:?}");
    // scroll --to mark NAME is the jump to that mark
    assert!(w.with(|m, _| m.ops.iter().any(|o| *o == Op::Content { pane: p, act: ContentAct::Jump("build".into()) })));
}

#[test]
fn menus_and_dialogs() {
    let w = world("menus");
    let p = w.window();
    let (_, rows) = run(w.human(), "app menu");
    assert_eq!((s(&rows[0], "item"), s(&rows[0], "key")), ("Edit > Copy", "cmd+c"));
    let (_, rows) = run(w.human(), &format!("app menu --context pane p{}", p.0));
    assert!(s(&rows[0], "key").ends_with(&format!("@{}", p.0)), "{rows:?}");
    let (code, rows) = run(w.human(), "app menu Copy");
    assert_eq!((code, s(&rows[0], "item")), (0, "Edit > Copy"));
    let (code, rows) = run(w.human(), "app menu 'Quit iTerm2'");
    assert_eq!((code, codes(&rows)), (2, vec!["refused quit".to_string()]));
    let (_, rows) = run(w.human(), "ui list");
    assert_eq!((s(&rows[0], "kind"), rows[0]["buttons"].clone()), ("alert", serde_json::json!(["Close", "Cancel"])));
    let (_, rows) = run(w.human(), "ui get 812");
    assert_eq!(rows.len(), 1);
    let (code, rows) = run(w.human(), "ui answer 812 --button Cancel");
    assert_eq!((code, s(&rows[0], "button")), (0, "Cancel"));
    let (code, rows) = run(w.human(), "ui wait --until open --timeout 1");
    assert_eq!((code, codes(&rows)), (0, vec!["ok ui".to_string()]));
    let (code, _) = run(w.human(), "ui wait --until closed --timeout 0.2");
    assert_eq!(code, 3, "a dialog stays open in the fake: a timeout");
    // event-driven (live run 2026-10-29: it polled every 50 ms): nothing is listed again until a `ui` event
    w.with(|m, _| {
        m.ui = Some(vec![]);
        m.ui_lists = 0;
    });
    let env = w.human();
    let t = std::thread::spawn(move || run(env, "ui wait --until open --timeout 5"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(w.with(|m, _| m.ui_lists), 1, "listed once, then blocked: no polling");
    w.with(|m, ev| {
        m.ui = Some(vec!["90\tpanel\tSettings\tOK".into()]);
        ev.push(itermkit::app::Event::Ui { window: 90 });
    });
    let (code, rows) = t.join().unwrap();
    assert_eq!((code, codes(&rows)), (0, vec!["ok opened".to_string()]), "{rows:?}");
    assert_eq!(w.with(|m, _| m.ui_lists), 2);
    // app launch with the app up: nothing to start
    let (_, rows) = run(w.human(), "app launch");
    assert_eq!(codes(&rows), ["ok running"]);
}

#[test]
fn every_control_plane_slot_is_wired() {
    use itermkit::grammar::{coverage, noun_help, Noun};
    let c = coverage();
    for (noun, verbs) in [
        ("tree", &["list"][..]),
        ("action", &["list", "get", "run"]),
        ("content", &["get", "select", "copy", "mark", "open"]),
        ("app", &["launch", "menu"]),
        ("ui", &["list", "get", "watch", "wait", "answer"]),
    ] {
        for v in verbs {
            let cell = c["grammar"][noun][v].as_str().unwrap();
            assert!(!cell.ends_with('*') && cell != "n/a", "{noun} {v}: {cell}");
        }
    }
    for n in [Noun::Tree, Noun::Action, Noun::Content, Noun::App, Noun::Ui, Noun::Window] {
        assert!(!noun_help(n).contains("not wired"), "{}", noun_help(n));
    }
}
