//! Named things end to end against the fake app: groups, layouts, snippets, scripts, presets, and agents'
//! spawn budgets.

mod common;

use common::*;
use itermkit::app::{Layout, Op};

fn split(env: itermkit::conn::Env, args: &str) -> itermkit::app::Id {
    let (code, rows) = run(env, args);
    assert_eq!(code, 0, "{args}: {rows:?}");
    pid(&rows[0])
}

#[test]
fn groups_are_named_pane_sets_usable_as_selectors() {
    let w = world("group");
    let me = w.window();
    let a = split(w.env(Some(me)), "pane new --cmd a");
    let b = split(w.env(Some(me)), "pane new --cmd b");
    let (code, rows) = run(w.human(), &format!("group new web --add 'p{} p{}'", a.0, b.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok created".to_string()]), "{rows:?}");
    assert_eq!(w.node(a).unwrap().vars["groups"], "web");
    // the group selects: --group and group=
    let (_, rows) = run(w.human(), "pane list --group web");
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (_, rows) = run(w.human(), "pane list group=web");
    assert_eq!(rows.len(), 2);
    let (_, rows) = run(w.human(), "group list");
    assert_eq!((s(&rows[0], "name"), s(&rows[0], "kind"), rows[0]["members"].as_array().unwrap().len()), ("web", "label", 2));
    let (code, rows) = run(w.human(), "group new web --add @focused");
    assert_eq!((code, s(&rows[0], "status")), (1, "failed"), "a name is made once");
    // set: remove one, switch the kind to broadcast (its members receive broadcast input)
    let (_, rows) = run(w.human(), &format!("group set web --remove p{} --kind broadcast", a.0));
    assert_eq!(codes(&rows), ["ok set"], "{rows:?}");
    assert!(!w.node(a).unwrap().vars.contains_key("groups"));
    assert!(w.m.lock().unwrap().ops.iter().any(|o| matches!(o, Op::Broadcast { add, .. } if add == &vec![b])));
    let (_, rows) = run(w.human(), "group get web");
    assert_eq!((s(&rows[0], "kind"), rows[0]["members"].clone()), ("broadcast", serde_json::json!([format!("p{}", b.0)])));
    // --plan changes nothing; delete clears the members and the record
    let (_, rows) = run(w.human(), "group delete web --plan");
    assert_eq!(codes(&rows), ["planned delete"]);
    assert_eq!(w.node(b).unwrap().vars["groups"], "web");
    let (_, rows) = run(w.human(), "group delete web");
    assert_eq!(codes(&rows), ["ok deleted"]);
    assert!(!w.node(b).unwrap().vars.contains_key("groups"));
    assert!(w.m.lock().unwrap().ops.iter().any(|o| matches!(o, Op::Broadcast { remove, .. } if remove == &vec![b])));
    let (code, _) = run(w.human(), "group get web");
    assert_eq!(code, 1);
}

#[test]
fn a_layout_saves_a_tab_and_recreates_it_in_a_new_tab() {
    let w = world("layout");
    let me = w.window();
    let a = split(w.env(Some(me)), "pane new --cmd 'make test' --side right --name build");
    split(w.env(Some(me)), &format!("pane new --at p{} --side down --cwd /var", a.0));
    let (code, rows) = run(w.env(Some(me)), "layout new dev");
    assert_eq!((code, codes(&rows)), (0, vec!["ok saved".to_string()]), "{rows:?}");
    assert_eq!(rows[0]["panes"], 3);
    let tree = s(&rows[0], "tree").to_string();
    let (code, rows) = run(w.env(Some(me)), "layout new dev");
    assert_eq!(code, 1, "an existing name needs --force: {rows:?}");
    let (_, rows) = run(w.human(), "layout get dev");
    assert_eq!(rows.len(), 4);
    assert_eq!((s(&rows[2], "cmd"), s(&rows[2], "name")), ("make test", "build"));
    assert_eq!(s(&rows[3], "cwd"), "/var");
    let (_, rows) = run(w.human(), "layout list");
    assert_eq!((s(&rows[0], "name"), s(&rows[0], "tree")), ("dev", tree.as_str()));
    // run: a new tab with the same tree, commands and folders
    let before = w.panes().len();
    let (code, rows) = run(w.env(Some(me)), "layout run dev");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows).last().unwrap(), "ok layout");
    assert_eq!(w.panes().len(), before + 3);
    let new: Vec<_> = rows.iter().filter(|r| s(r, "code") == "created").map(pid).collect();
    let n = w.node(new[1]).unwrap();
    assert_eq!((n.cmd.as_deref(), n.cwd.as_str(), n.name.as_str()), (Some("make test"), "/tmp", "build"));
    assert_eq!(w.node(new[2]).unwrap().cwd, "/var");
    let tab = n.parent.unwrap();
    let old_tab = w.node(me).unwrap().parent.unwrap();
    assert_ne!(tab, old_tab);
    let shape = |l: &Layout| format!("{:?}", l).chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    assert_eq!(shape(w.node(tab).unwrap().layout.as_ref().unwrap()), shape(w.node(old_tab).unwrap().layout.as_ref().unwrap()));
    let (_, rows) = run(w.human(), "layout delete dev");
    assert_eq!(codes(&rows), ["ok deleted"]);
    assert_eq!(run(w.human(), "layout list").1.len(), 0);
}

#[test]
fn snippets_are_delivered_through_the_delivery_engine() {
    let w = world("snip");
    let me = w.window();
    let a = split(w.env(Some(me)), "pane new --cmd a");
    let (code, rows) = run(w.human(), "snippet new hi --text 'echo hi'");
    assert_eq!((code, codes(&rows)), (0, vec!["ok created".to_string()]), "{rows:?}");
    let (_, rows) = run(w.human(), "snippet get hi");
    assert_eq!(s(&rows[0], "value"), "echo hi");
    let (_, rows) = run(w.human(), "snippet list");
    assert_eq!((s(&rows[0], "name"), rows[0]["bytes"].clone()), ("hi", serde_json::json!(7)));
    let (code, rows) = run(w.human(), &format!("snippet run hi --in p{} --enter", a.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok delivered".to_string()]), "{rows:?}");
    assert_eq!(w.node(a).unwrap().sent, b"echo hi\r");
    // the caller's own pane by default
    run(w.env(Some(me)), "snippet run hi");
    assert_eq!(w.node(me).unwrap().sent, b"echo hi");
    let (_, rows) = run(w.human(), "snippet delete hi");
    assert_eq!(codes(&rows), ["ok deleted"]);
    assert_eq!(run(w.human(), "snippet run hi").0, 1);
}

#[test]
fn scripts_are_batch_files_by_name() {
    let w = world("script");
    let me = w.window();
    let dir = w.dir.join("state/scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pair.batch"), "# two panes\npane new --cmd a --as x\npane new --at $x --cmd b --side down\n").unwrap();
    let (_, rows) = run(w.env(Some(me)), "script list");
    assert_eq!((s(&rows[0], "name"), rows[0]["lines"].clone()), ("pair", serde_json::json!(2)));
    let (code, rows) = run(w.env(Some(me)), "script run pair");
    assert_eq!((code, codes(&rows)), (0, vec!["ok created".to_string(); 2]), "{rows:?}");
    assert_eq!(w.panes().len(), 3);
    // a script that runs itself stops at the depth limit, not forever
    std::fs::write(dir.join("loop"), "script run loop\n").unwrap();
    let (code, rows) = run(w.env(Some(me)), "script run loop");
    assert_eq!(code, 2, "{rows:?}");
    assert!(rows.iter().any(|r| s(r, "code") == "depth"));
    assert_eq!(run(w.env(Some(me)), "script run nope").0, 1);
}

#[test]
fn presets_list_the_apps_presets_and_the_profiles() {
    let w = world("preset");
    let (_, rows) = run(w.human(), "preset list --grep solarized");
    assert_eq!((rows.len(), s(&rows[0], "name"), s(&rows[0], "source")), (1, "Solarized Dark", "preset"), "{rows:?}");
    let (_, rows) = run(w.human(), "preset list");
    assert!(rows.iter().any(|r| s(r, "name") == "Default" && s(r, "source") == "profile" && r["active"] == true), "{rows:?}");
    let (code, rows) = run(w.human(), "preset get solarized dark");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!((s(&rows[0], "background"), s(&rows[0], "ansi.1"), s(&rows[0], "kind")), ("#002b36", "#dc322f", "dark"));
    let (_, rows) = run(w.human(), "preset get tango light");
    assert_eq!(s(&rows[0], "kind"), "light");
    // a profile's colors, by their CLI names
    let (code, rows) = run(w.human(), "preset get Default");
    assert_eq!((code, s(&rows[0], "source"), s(&rows[0], "background")), (0, "profile", "#000000"), "{rows:?}");
    assert_eq!(run(w.human(), "preset get Nope").0, 1);
    // import hands the app the file (the fake has no profile_import key: the refusal names it)
    let f = w.dir.join("x.itermcolors");
    std::fs::write(&f, "<plist><dict></dict></plist>").unwrap();
    let (_, rows) = run(w.human(), &format!("preset import {}", f.display()));
    assert!(s(&rows[0], "reason").contains("profile_import"), "{rows:?}");
    let (_, rows) = run(w.human(), &format!("preset import {} --plan", f.display()));
    assert_eq!(codes(&rows), ["planned import"]);
    assert_eq!(run(w.human(), "preset import /nope.itermcolors").0, 1);
}

#[test]
fn agent_budget_is_not_in_the_cli() {
    let w = world("budget");
    let human = w.window();
    let lead = split(w.human(), &format!("pane new --at p{} --cmd lead", human.0));
    // the verb is gone: refused as unknown, nothing written
    let (code, rows) = run(w.human(), &format!("agent budget p{} 2", lead.0));
    assert_eq!(code, 1);
    assert!(s(&rows[0], "reason").contains("no verb 'budget'"), "{rows:?}");
    assert!(!w.node(lead).unwrap().vars.contains_key("agent.budget"));
    assert!(!run(w.human(), "agent --help").1.iter().any(|r| r.to_string().contains("budget")));
}

#[test]
fn spawns_are_never_capped_even_by_a_leftover_budget() {
    let w = world("nobudget");
    let human = w.window();
    let lead = split(w.human(), &format!("pane new --at p{} --cmd lead", human.0));
    // what an older CLI left behind: the budget folder and a budget of 1 on the lead
    std::fs::create_dir_all(w.dir.join("state").join("budget")).unwrap();
    w.with(|m, _| m.nodes.get_mut(&lead).unwrap().vars.insert("agent.budget".into(), "1".into()));
    let child = split(w.env(Some(lead)), "pane new --cmd c");
    split(w.env(Some(child)), "pane new --cmd g");
    for i in 0..4 {
        let (code, rows) = run(w.env(Some(lead)), &format!("pane new --cmd c{i}"));
        assert_eq!(code, 0, "{rows:?}");
    }
    assert_eq!(run(w.env(Some(child)), "pane new --cmd g2").0, 0);
}
