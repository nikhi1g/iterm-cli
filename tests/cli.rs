//! The CLI end to end in process against the fake app: protocol, grammar, selectors,
//! placement and geometry, the CRUD gate, batch bindings, watch, and the agent primitives.

mod common;

use common::*;
use itermkit::app::{Event, Id, Layout};
use serde_json::json;

#[test]
fn pane_new_splits_beside_me_and_writes_run_open_with_plan_as_the_dry_run() {
    let w = world("gate");
    let human = w.window();
    let me = w.with(|m, ev| m.apply(itermkit::app::Op::Split { at: human, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0]);
    // inside my scope: no --apply needed; the child records me as its parent
    let (code, rows) = run(w.env(Some(me)), "pane new --cmd 'make test' --side down");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows), ["ok created"]);
    let child = pid(&rows[0]);
    let n = w.node(child).unwrap();
    assert_eq!(n.vars["agent.parent"], format!("p{}", me.0));
    assert_eq!(n.vars["agent.kind"], "cmd");
    assert_eq!(n.cmd.as_deref(), Some("make test"));
    // beside me, on the side asked
    let split = w.with(|m, _| m.ops.iter().rev().find(|o| matches!(o, itermkit::app::Op::Split { .. })).cloned());
    assert!(matches!(split, Some(itermkit::app::Op::Split { at, side: itermkit::app::wire::Side::Down, .. }) if at == me), "{split:?}");
    // open by default: beside the human's pane, renaming it, closing others' panes all run; --plan is the dry run
    let (code, rows) = run(w.env(Some(me)), &format!("pane new --at p{} --plan", human.0));
    assert_eq!((code, codes(&rows)), (0, vec!["planned new".to_string()]));
    // --plan never writes, even inside my own scope
    let before = w.with(|m, _| m.snapshot().nodes.len());
    let (_, rows) = run(w.env(Some(me)), "pane new --cmd x --side down --plan");
    assert_eq!(codes(&rows), ["planned new"]);
    assert_eq!(w.with(|m, _| m.snapshot().nodes.len()), before);
    let (_, rows) = run(w.env(Some(me)), &format!("pane new --at p{}", human.0));
    assert_eq!(codes(&rows), ["ok created"]);
    let (_, rows) = run(w.env(Some(me)), &format!("pane set p{} --name x --apply", human.0));
    assert_eq!(codes(&rows), ["ok set"], "--apply is accepted and ignored");
    let (_, rows) = run(w.env(Some(me)), &format!("pane set p{} --name build", child.0));
    assert_eq!(codes(&rows), ["ok set"]);
    assert_eq!(w.node(child).unwrap().name, "build");
    // the rails: your own pane takes --force; a non-agent pane anywhere closes
    let (_, rows) = run(w.env(Some(me)), "pane close build --plan");
    assert_eq!(codes(&rows), ["planned close"]);
    let (code, rows) = run(w.env(Some(me)), "pane close @me");
    assert_eq!((code, s(&rows[0], "code")), (2, "self"));
    let (code, rows) = run(w.env(Some(me)), "pane close build");
    assert_eq!((code, codes(&rows)), (0, vec!["ok closed".to_string()]));
    assert!(w.node(child).is_none());
}

#[test]
fn selectors_resolve_over_the_snapshot() {
    let w = world("sel");
    let me = w.window();
    let env = || {
        let mut e = w.env(Some(me));
        e.agent = false;
        e
    };
    let (_, rows) = run(env(), "pane new --count 3 --stack down --name 'w{i}' --cmd sleep");
    assert_eq!(codes(&rows), ["ok created"; 3]);
    let ids: Vec<Id> = rows.iter().map(pid).collect();
    // @right of me is the stack's first pane; names, globs, indices, paths, booleans
    let one = |sel: &str| run(env(), &format!("pane list {sel}")).1.iter().map(|r| s(r, "id").to_string()).collect::<Vec<_>>();
    let h = |i: usize| format!("p{}", ids[i].0);
    assert_eq!(one("@right"), [h(0)]);
    assert_eq!(one("@below:w1"), [h(1)]);
    assert_eq!(one("w2"), [h(1)]);
    assert_eq!(one("'w*' and not w3"), [h(0), h(1)]);
    assert_eq!(one("2-3"), [h(0), h(1)]);
    assert_eq!(one("last"), [h(2)]);
    assert_eq!(one("/w/1/v/1/t/1/p/1"), [format!("p{}", me.0)]);
    assert_eq!(one("kind=cmd").len(), 3);
    assert_eq!(one("recent:1"), [h(2)]);
    assert_eq!(run(env(), "pane list --where kind=shell").1.len(), 1);
    // the stack shares one split with equal weights
    let tab = w.m.lock().unwrap().nodes[&ids[0]].parent.unwrap();
    let Some(Layout::Split { children, .. }) = w.node(tab).unwrap().layout else { panic!() };
    let Layout::Split { children: stack, .. } = &children[1].1 else { panic!("{children:?}") };
    assert_eq!(stack.len(), 3);
    assert!(stack.iter().all(|(f, _)| (f - 1.0 / 3.0).abs() < 1e-4), "{stack:?}");
    // resize with weights, then read it back in tab get
    let (_, rows) = run(env(), "pane resize w1 w2 w3 --weights 1,2,1");
    assert_eq!(codes(&rows), ["ok resized"]);
    let (_, rows) = run(env(), "tab get @me");
    assert_eq!(rows[0]["layout"]["children"][1]["fractions"], json!([0.25, 0.5, 0.25]));
    // a selector that names nothing, an n/a cell and a slot that doesn't apply
    assert_eq!(run(env(), "pane close nobody --apply").0, 1);
    let (_, rows) = run(env(), "content move");
    assert!(s(&rows[0], "reason").contains("n/a"), "{rows:?}");
    let (_, rows) = run(env(), "window list --weights 1,2");
    assert!(s(&rows[0], "reason").contains("geometry"), "{rows:?}");
}

#[test]
fn batch_binds_what_a_line_created_and_checks_every_line_first() {
    let w = world("batch");
    let me = w.window();
    let script = "tab new --as t1 --cmd top\npane new --at $t1 --side right --cmd htop --as h\npane set $h --name mon\n";
    let (code, rows) = run_in(w.env(Some(me)), "batch", script);
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows), ["ok created", "ok created", "ok set"]);
    let mon = w.m.lock().unwrap().nodes.iter().find(|(_, n)| n.name == "mon").map(|(i, _)| *i).unwrap();
    assert_eq!(w.node(mon).unwrap().cmd.as_deref(), Some("htop"));
    // an unbound $NAME stops the batch before anything runs
    let before = w.panes().len();
    let (code, rows) = run_in(w.env(Some(me)), "batch", "pane new --cmd a\npane close $nope --apply\n");
    assert_eq!(code, 1);
    assert!(s(&rows[0], "reason").contains("$nope"), "{rows:?}");
    assert_eq!(w.panes().len(), before);
}

#[test]
fn agents_spawn_with_model_effort_task_and_queue_beyond_max() {
    let w = world("agents");
    let me = w.window();
    let (code, rows) = run(w.env(Some(me)), "pane new --agent --model opus --effort high --task 'fix the parser' --task 'write docs' --max 1 --read-only");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows), ["ok created", "ok queued"]);
    let a = pid(&rows[0]);
    let n = w.node(a).unwrap();
    let cmd = n.cmd.unwrap();
    assert!(cmd.starts_with("claude --model claude-opus-5-5 --effort high --append-system-prompt"), "{cmd}");
    assert!(cmd.contains("--name w1 -- \"$(cat "), "{cmd}");
    assert_eq!(n.vars["agent.kind"], "claude");
    assert_eq!(n.vars["agent.task"], "fix the parser");
    // the task file holds the whole task, private
    let f = cmd.split("cat ").nth(1).unwrap().split(" &&").next().unwrap().trim_matches('\'');
    assert_eq!(std::fs::read_to_string(f).unwrap(), "fix the parser");
    // children: one running, one queued; when it exits, the queue starts the next
    let (_, rows) = run(w.env(Some(me)), "children");
    assert_eq!(codes(&rows), ["ok running", "ok queued"]);
    w.exit(a, 0);
    let (_, rows) = run(w.env(Some(me)), "children");
    assert_eq!(codes(&rows), ["ok created", "ok exited", "ok running"], "{rows:?}");
    assert_eq!(rows[1]["exit"], json!(0));
    let b = pid(&rows[0]);
    assert_eq!(w.node(b).unwrap().vars["agent.task"], "write docs");
    // children --wait returns when the last one exits (pushed events, no polling)
    let wid = w.env(Some(me));
    let t = std::thread::spawn(move || run(wid, "children --wait --timeout 10"));
    std::thread::sleep(std::time::Duration::from_millis(100));
    w.exit(b, 3);
    let (_, rows) = t.join().unwrap();
    assert_eq!(codes(&rows), ["ok exited", "ok exited"], "{rows:?}");
    assert_eq!(rows[1]["exit"], json!(3));
    // a live agent is closed only with --force
    let (_, rows) = run(w.env(Some(me)), "agent new --cmd claude");
    let c = pid(&rows[0]);
    let (code, rows) = run(w.env(Some(me)), &format!("pane close p{} --apply", c.0));
    assert_eq!((code, s(&rows[0], "code")), (2, "live-agent"));
    assert_eq!(run(w.env(Some(me)), &format!("pane close p{} --apply --force", c.0)).0, 0);
}

#[test]
fn text_goes_through_the_delivery_engine() {
    let w = world("send");
    let me = w.window();
    let (_, rows) = run(w.env(Some(me)), "pane new --cmd bash");
    let sh = pid(&rows[0]);
    // the app delivers the text, then Enter once it shows (held behind prompts: the app's engine, api/deliver.rs)
    let (_, rows) = run(w.env(Some(me)), &format!("pane send p{} --text 'ls' --enter", sh.0));
    assert_eq!(codes(&rows), ["ok delivered"]);
    assert_eq!((rows[0]["bytes"].clone(), w.node(sh).unwrap().sent.clone()), (json!(2), b"ls\r".to_vec()));
    // keys go through as keys
    let (_, rows) = run(w.env(Some(me)), &format!("pane key p{} --key esc", sh.0));
    assert_eq!(codes(&rows), ["ok keyed"]);
    let (_, rows) = run(w.env(Some(me)), "agent new");
    let cl = pid(&rows[0]);
    // regression (live run 2026-10-26): quitting with live agents is refused without --force
    let (code, rows) = run(w.env(Some(me)), "app quit");
    assert_eq!((code, s(&rows[0], "code")), (2, "live-agent"), "{rows:?}");
    // states come from the screen
    w.screen(cl, &["─────────────────", "❯ "]);
    let (_, rows) = run(w.env(Some(me)), "agent list");
    assert_eq!(s(&rows[0], "state"), "ready");
    let (_, rows) = run(w.env(Some(me)), &format!("pane read p{} --tail 1", cl.0));
    assert_eq!(rows[0]["lines"], json!(["❯ "]));
}

#[test]
fn a_permission_prompt_is_never_answered_input_to_it_is_held() {
    let w = world("prompt");
    let me = w.window();
    let (_, rows) = run(w.env(Some(me)), "pane new --cmd codex");
    let cx = pid(&rows[0]);
    w.screen(cx, &["Allow command? [y/N]", "  1. Yes", "  2. No"]);
    // messages are held in order, never typed into the prompt
    for t in ["one", "two"] {
        let (_, rows) = run(w.env(Some(me)), &format!("pane send p{} --text {t} --enter", cx.0));
        assert_eq!(codes(&rows), ["ok queued"], "{rows:?}");
        assert_eq!(s(&rows[0], "reason"), "prompt");
    }
    // raw keys and text are refused; esc and ctrl-c only cancel and pass
    let (_, rows) = run(w.env(Some(me)), &format!("pane key p{} --key enter", cx.0));
    assert!(s(&rows[0], "reason").contains("permission or trust prompt"), "{rows:?}");
    let (_, rows) = run(w.env(Some(me)), &format!("pane send p{} --path x", cx.0));
    assert!(s(&rows[0], "reason").contains("permission or trust prompt"), "{rows:?}");
    assert!(w.node(cx).unwrap().sent.is_empty(), "nothing reached the prompt");
    let (_, rows) = run(w.env(Some(me)), &format!("pane key p{} --key esc", cx.0));
    assert_eq!(codes(&rows), ["ok keyed"]);
    // the human answers: the held messages land, in order
    w.screen(cx, &["› "]);
    assert_eq!(w.node(cx).unwrap().sent, b"<Key { code: Esc, mods: Mods { shift: false, ctrl: false, alt: false, cmd: false } }>one\rtwo\r");
}

#[test]
fn watch_and_wait_block_on_pushed_events() {
    let w = world("watch");
    let me = w.window();
    let (_, rows) = run(w.env(Some(me)), "pane new --cmd sleep");
    let p = pid(&rows[0]);
    let env = w.env(Some(me));
    let t = std::thread::spawn(move || run(env, &format!("pane wait p{} --until exit --timeout 10", p.0)));
    std::thread::sleep(std::time::Duration::from_millis(100));
    w.exit(p, 7);
    let (code, rows) = t.join().unwrap();
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows), ["ok until"]);
    assert_eq!(rows[0]["exit"], json!(7));
    // streamed events (filtered by --kind) until the timeout
    let env = w.env(Some(me));
    let t = std::thread::spawn(move || run(env, "app watch --kind bell --timeout 0.5"));
    std::thread::sleep(std::time::Duration::from_millis(100));
    w.with(|_, ev| {
        ev.push(Event::Title { pane: me, title: "x".into() });
        ev.push(Event::Bell { pane: me });
    });
    let (_, rows) = t.join().unwrap();
    assert_eq!(codes(&rows), ["ok event", "ok watched"], "{rows:?}");
    assert_eq!(s(&rows[0], "event"), "bell");
    // a wait that is never met times out with exit code 3
    let (code, _) = run(w.env(Some(me)), &format!("pane wait p{} --until exit --timeout 0.2", me.0));
    assert_eq!(code, 3);
}

#[test]
fn mail_and_the_task_board_between_parent_and_children() {
    let w = world("team");
    let lead = w.window();
    let (_, rows) = run(w.env(Some(lead)), "pane new --count 2 --cmd worker --name 'k{i}'");
    let (k1, k2) = (pid(&rows[0]), pid(&rows[1]));
    // non-Claude panes: kept in the mailbox and typed in by the app's delivery engine
    let (_, rows) = run(w.env(Some(k1)), "agent msg @parent --text 'done with parsing'");
    assert_eq!(codes(&rows), ["ok delivered"]);
    let (_, rows) = run(w.env(Some(k1)), "agent msg k2 --text 'hi sibling'");
    assert_eq!(codes(&rows), ["ok delivered"]);
    assert_eq!(w.node(k2).unwrap().sent, b"hi sibling\r");
    let (_, rows) = run(w.env(Some(lead)), "agent inbox --mark-read");
    assert_eq!(codes(&rows), ["ok message", "ok inbox"]);
    assert_eq!(s(&rows[0], "text"), "done with parsing");
    assert_eq!(run(w.env(Some(lead)), "agent inbox").1.len(), 1);
    // the human reads every message between any panes, from anywhere
    let (_, rows) = run(w.env(None), "agent inbox --all-panes");
    assert_eq!(codes(&rows), ["ok message", "ok message", "ok inbox"], "{rows:?}");
    let by = |t: &str| rows.iter().find(|r| s(r, "text") == t).unwrap();
    assert_eq!((s(by("done with parsing"), "to"), by("done with parsing")["read"].clone()), (format!("p{}", lead.0).as_str(), json!(true)));
    assert_eq!((s(by("hi sibling"), "to"), by("hi sibling")["read"].clone()), (format!("p{}", k2.0).as_str(), json!(false)));
    assert_eq!(rows[2]["unread"], json!(1));
    // inbox --wait wakes on the append
    let env = w.env(Some(k2));
    let t = std::thread::spawn(move || run(env, "agent inbox --since 0 --wait 10"));
    std::thread::sleep(std::time::Duration::from_millis(100));
    run(w.env(Some(lead)), "agent msg k2 --text 'from lead'");
    let (_, rows) = t.join().unwrap();
    assert!(rows.iter().any(|r| s(r, "text") == "from lead"), "{rows:?}");
    // the board: one claim wins, deps gate
    run(w.env(Some(lead)), "task new build the thing");
    run(w.env(Some(lead)), "task new test it --after 1");
    let (_, rows) = run(w.env(Some(k1)), "task claim next");
    assert_eq!((codes(&rows), rows[0]["id"].clone()), (vec!["ok claimed".to_string()], json!(1)));
    let (code, rows) = run(w.env(Some(k2)), "task claim 1");
    assert_eq!((code, s(&rows[0], "code")), (2, "taken"));
    let (code, rows) = run(w.env(Some(k2)), "task claim next");
    assert_eq!((code, s(&rows[0], "code")), (2, "none-ready"));
    run(w.env(Some(k1)), "task done 1 built it");
    assert_eq!(run(w.env(Some(k2)), "task claim next").1[0]["id"], json!(2));
    let (_, rows) = run(w.env(Some(lead)), "task list");
    assert_eq!(codes(&rows), ["ok done", "ok claimed"]);
}

#[test]
fn worktree_agents_checkpoint_and_merge_into_the_parent() {
    let w = world("wt");
    let repo = w.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap().status.success(), "git {args:?}");
    git(&["init", "-q", "-b", "main"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
    let lead = w.window();
    w.with(|m, _| m.nodes.get_mut(&lead).unwrap().cwd = repo.display().to_string());
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --name fix");
    assert_eq!(code, 0, "{rows:?}");
    let wt = repo.join(".worktrees/fix");
    assert_eq!(s(&rows[0], "cwd"), wt.display().to_string());
    std::fs::write(wt.join("a.txt"), "x").unwrap();
    std::process::Command::new("git").arg("-C").arg(&wt).args(["config", "user.email", "t@t"]).output().unwrap();
    std::process::Command::new("git").arg("-C").arg(&wt).args(["config", "user.name", "t"]).output().unwrap();
    let (code, rows) = run(w.env(Some(lead)), "agent checkpoint fix");
    assert_eq!((code, codes(&rows)), (0, vec!["ok checkpointed".to_string()]), "{rows:?}");
    assert_eq!(rows[0]["ahead"], json!(1));
    let (code, rows) = run(w.env(Some(lead)), "agent merge fix");
    assert_eq!((code, codes(&rows)), (0, vec!["ok merged".to_string()]), "{rows:?}");
    assert!(repo.join("a.txt").exists());
}

#[test]
fn coverage_help_and_containers() {
    let w = world("misc");
    let me = w.window();
    let (_, out) = text(w.env(Some(me)), "coverage --json");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["schema_version"], json!(2));
    assert_eq!(v["grammar"]["pane"]["lend"], json!("U"));
    assert_eq!(v["grammar"]["profile"]["set"], json!("U"));
    assert_eq!(v["grammar"]["profile"]["new"], json!("C"));
    assert!(text(w.env(Some(me)), "help pane").1.contains("iterm pane <verb>"));
    let (_, rows) = run(w.env(Some(me)), "vtab new api --cmd top");
    assert_eq!(codes(&rows), ["ok created"]);
    let (_, rows) = run(w.env(Some(me)), "vtab list");
    assert_eq!(rows.len(), 2);
    assert_eq!(s(&rows[1], "name"), "api");
    let (code, rows) = run(w.env(Some(me)), "layout list");
    assert_eq!((code, rows.len()), (0, 0), "wired: no layouts saved yet");
    let (_, rows) = run(w.env(Some(me)), "app get");
    assert_eq!((rows[0]["panes"].clone(), rows[0]["vtabs"].clone()), (json!(2), json!(2)));
    // old commands still work
    assert_eq!(codes(&run(w.env(Some(me)), "panes").1).len(), 2);
}

#[test]
fn regress_live_run_2026_10_26() {
    // bugs the first live run found
    let w = world("regress");
    let me = w.window();
    // a caller outside iTerm2: app get needs no selector, @focused no identity
    let (code, rows) = run(w.human(), "app get");
    assert_eq!((code, codes(&rows)), (0, vec!["ok app".to_string()]), "{rows:?}");
    let (_, rows) = run(w.human(), "pane list @focused");
    assert_eq!(s(&rows[0], "id"), format!("p{}", me.0));
    // a read drops the screen's blank rows below the text
    w.screen(me, &["$ echo hi", "hi", "$ ", "", ""]);
    let (_, rows) = run(w.human(), &format!("pane read p{}", me.0));
    assert_eq!(rows[0]["lines"], json!(["$ echo hi", "hi", "$ "]));
    let (_, rows) = run(w.human(), &format!("pane read p{} --tail 2", me.0));
    assert_eq!(rows[0]["lines"], json!(["hi", "$ "]));
    // a container create names the container, and its pane
    let (_, rows) = run(w.env(Some(me)), "vtab new api");
    assert!(s(&rows[0], "id").starts_with('v') && s(&rows[0], "pane").starts_with('p'), "{rows:?}");
    // plain --cmd children are supervised too: children --wait sees them end
    let (_, rows) = run(w.env(Some(me)), "pane new --cmd 'sleep 1'");
    let kid = pid(&rows[0]);
    let env = w.env(Some(me));
    let t = std::thread::spawn(move || run(env, "children --wait --timeout 10"));
    std::thread::sleep(std::time::Duration::from_millis(100));
    w.exit(kid, 4);
    let (code, rows) = t.join().unwrap();
    assert_eq!(code, 0, "{rows:?}");
    assert!(rows.iter().any(|r| s(r, "code") == "exited" && r["exit"] == json!(4)), "{rows:?}");
}

#[test]
fn settings_profile_and_keys_over_the_apps_settings() {
    // one test touches the fake's settings (a process-wide store)
    let w = world("settings");
    let me = w.window();
    let e = || w.env(Some(me));
    // setting: keys, a planned set, an applied set, get, a refused unknown key, reset
    assert_eq!(run(e(), "setting keys --grep font").1.len(), 2);
    let (_, rows) = run(e(), "setting set --set font_size=13 --plan");
    assert_eq!(codes(&rows), ["planned set"]);
    let (code, rows) = run(e(), "setting set --set font_size=13 --apply");
    assert_eq!((code, codes(&rows)), (0, vec!["ok set".to_string()]));
    let (_, rows) = run(e(), "setting get font_size");
    assert_eq!(rows[0]["value"], json!("13"));
    let (code, rows) = run(e(), "setting set --set fnot=x --apply");
    assert_eq!(code, 1);
    assert!(s(&rows[0], "reason").contains("closest: font"), "{rows:?}");
    // profile: the one in use takes get/set; new switches to a copy; delete falls back
    let (_, rows) = run(e(), "profile list");
    assert_eq!((s(&rows[0], "name"), rows[0]["active"].clone()), ("Default", json!(true)));
    let (_, rows) = run(e(), "profile set Default --set font=Monaco --apply");
    assert_eq!(codes(&rows), ["ok set"]);
    let (_, rows) = run(e(), "profile get Default --grep font");
    assert!(rows.iter().any(|r| s(r, "key") == "font" && r["value"] == json!("Monaco")), "{rows:?}");
    let (code, rows) = run(e(), "profile get Work");
    assert_eq!(code, 1, "{rows:?}");
    let (_, rows) = run(e(), "profile new Work --apply");
    assert_eq!(codes(&rows), ["ok set"], "{rows:?}");
    let (_, rows) = run(e(), "profile list");
    assert!(rows.iter().any(|r| s(r, "name") == "Work" && r["active"] == json!(false)), "made, not switched to: {rows:?}");
    // another profile's keys travel as KEY@NAME
    let (code, rows) = run(e(), "profile get Work");
    assert_eq!(code, 0, "{rows:?}");
    let (code, rows) = run(e(), "profile set Work --set font=Menlo --apply");
    assert_eq!((code, s(&rows[0], "key")), (0, "font@Work"), "{rows:?}");
    run(e(), "setting set --set profile=Work --apply");
    let (_, rows) = run(e(), "profile list");
    assert!(rows.iter().any(|r| s(r, "name") == "Work" && r["active"] == json!(true)), "{rows:?}");
    // delete: the one in use falls back to another; an unknown name is refused
    let (code, rows) = run(e(), "profile delete Work");
    assert_eq!((code, codes(&rows)), (0, vec!["ok set".to_string()]), "{rows:?}");
    let (_, rows) = run(e(), "profile list");
    assert!(!rows.iter().any(|r| s(r, "name") == "Work") && rows.iter().any(|r| s(r, "name") == "Default" && r["active"] == json!(true)), "{rows:?}");
    assert_eq!(run(e(), "profile delete Work").0, 1);
    let (code, rows) = run(e(), "profile delete Default");
    assert!(code == 1 && s(&rows[0], "reason").contains("built in"), "{rows:?}");
    // key: list, set by action name, reset --all touches only key overrides
    let (_, rows) = run(e(), "key set new_vtab --key ctrl+cmd+t --apply");
    assert_eq!(codes(&rows), ["ok set"], "{rows:?}");
    let (_, rows) = run(e(), "key list");
    assert_eq!((s(&rows[0], "action"), s(&rows[0], "combo")), ("new_vtab", "ctrl+cmd+t"));
    run(e(), "key reset --all --apply");
    assert_eq!(s(&run(e(), "key get new_vtab").1[0], "combo"), "default");
    assert_eq!(run(e(), "setting get font").1[0]["value"], json!("Monaco"));
    run(e(), "setting reset --all --apply");
    assert_eq!(run(e(), "setting get font").1[0]["value"], json!("MesloLGS NF"));
}

#[test]
fn search_sends_the_query_with_its_kind_and_limit() {
    let w = world("search");
    let (code, rows) = run(w.human(), "search split pane --kind actions --limit 1");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows), ["ok found"]);
    assert_eq!((s(&rows[0], "group"), s(&rows[0], "name"), s(&rows[0], "detail")), ("Actions", ">split pane", "⌘X"));
    let (code, rows) = run(w.human(), "search x --kind sideways");
    assert_eq!((code, s(&rows[0], "status")), (1, "failed"));
}

#[test]
fn broadcast_set_runs_and_sends_the_pick_set() {
    let w = world("bcast");
    let human = w.window();
    let (_, rows) = run(w.human(), &format!("broadcast set --add p{} --mode pick --plan", human.0));
    assert_eq!(codes(&rows), ["planned set"]);
    let (code, rows) = run(w.human(), &format!("broadcast set --add p{} --mode pick --apply", human.0));
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows), ["ok broadcast"]);
    assert_eq!(s(&rows[0], "mode"), "pick");
    assert_eq!(rows[0]["targets"], serde_json::json!([format!("p{}", human.0)]));
    // a repeated --add keeps every value, not just the last
    let other = w.with(|m, ev| m.apply(itermkit::app::Op::Split { at: human, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0]);
    let (code, rows) = run(w.human(), &format!("broadcast set --add p{} --add p{} --mode pick", human.0, other.0));
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(rows[0]["targets"], serde_json::json!([format!("p{}", human.0), format!("p{}", other.0)]));
    let (code, rows) = run(w.human(), "broadcast set --mode sideways --apply");
    assert_eq!((code, s(&rows[0], "status")), (1, "failed"));
    let (code, rows) = run(w.human(), "broadcast get");
    assert_eq!((code, codes(&rows)), (0, vec!["ok broadcast".to_string()]));
}

#[test]
fn regress_live_run_2026_10_29() {
    // bugs the live run with real Claude Code, Codex and OMP agents found
    let w = world("live29");
    let lead = w.window();
    let (_, rows) = run(w.env(Some(lead)), "pane new --count 2 --cmd worker --name 'k{i}'");
    let (k1, k2) = (pid(&rows[0]), pid(&rows[1]));
    run(w.env(Some(k1)), "agent msg k2 --text 'hi sibling'");
    // an inbox row shows the message's text (the one-line form hid it: an agent couldn't read its mail)
    let (_, out) = text(w.env(Some(k2)), "agent inbox --mark-read");
    assert!(out.contains("text=\"hi sibling\""), "{out}");
    // a dry run says how to run it (writes run at once; --apply is a no-op)
    let (_, out) = text(w.env(Some(lead)), "pane new --plan");
    assert!(out.trim_end().ends_with("(drop --plan to run it)"), "{out}");
    // mailboxes are per app instance: a restarted app's pane with the same ID starts empty
    let w2 = world("live29b");
    w2.with(|m, _| m.pid = Some(1));
    let lead2 = w2.window();
    run(w2.env(Some(lead2)), "pane new --count 2 --cmd worker --name 'k{i}'");
    let mut env = w2.env(Some(k2));
    env.state = w.dir.join("state");
    let (_, rows) = run(env, "agent inbox --all");
    assert_eq!(codes(&rows), ["ok inbox"], "{rows:?}");
    // and so are default names: a new instance's first agent is w1 again (not w486 after many runs)
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker");
    assert_eq!(s(&rows[0], "name"), "w3", "after k1 and k2");
    let mut env = w2.env(Some(lead2));
    env.state = w.dir.join("state");
    let (_, rows) = run(env, "pane new --agent --cmd worker");
    assert_eq!(s(&rows[0], "name"), "w1", "{rows:?}");

    // children --wait ends when an agent child's turn is done (idle), not only when it exits
    let (_, rows) = run(w.env(Some(lead)), "pane close k1 k2 w3 --force");
    assert_eq!(codes(&rows).len(), 3, "{rows:?}");
    let (_, rows) = run(w.env(Some(lead)), "pane new --agent --name a1");
    let a1 = pid(&rows[0]);
    run(w.human(), &format!("pane set p{} --set agent.state=idle", a1.0));
    let (code, rows) = run(w.env(Some(lead)), "children --wait --timeout 2");
    assert_eq!(code, 0, "{rows:?}");
    // a rename moves the agent's name (rows, mail) and its parent's ledger (merge after it closed)
    run(w.env(Some(lead)), "pane set a1 --name a2");
    assert_eq!(w.node(a1).unwrap().vars.get("agent.name").map(String::as_str), Some("a2"));
    let (_, rows) = run(w.env(Some(lead)), "pane close a2 --force");
    assert_eq!(codes(&rows), ["ok closed"], "{rows:?}");
    let (_, rows) = run(w.env(Some(lead)), "agent merge a2");
    // (and `a2` reads as the app's handle too: a name wins over a handle, and a handle counts only as a pane)
    assert_eq!(s(&rows[0], "code"), "not-worktree", "found by its new name after it closed: {rows:?}");
    // a message to an agent named like a window (default names w1, w2, …) goes to the agent
    let win = w.with(|m, _| m.up(lead, itermkit::app::Kind::Window)).unwrap();
    let (_, rows) = run(w.env(Some(lead)), &format!("pane new --cmd worker --name w{}", win.0));
    let named_w1 = pid(&rows[0]);
    let (_, rows) = run(w.env(Some(lead)), &format!("agent msg w{} --text 'to the agent'", win.0));
    assert_eq!(s(&rows[0], "pane"), format!("p{}", named_w1.0), "{rows:?}");
}

#[test]
fn agent_list_reads_no_shell_screen() {
    // perf (2026-10-29): agent list read every pane's screen, one main-thread hop each (92 ms with 10 shells)
    let w = world("alist");
    let me = w.window();
    run(w.env(Some(me)), "pane new --count 5");
    let (_, rows) = run(w.env(Some(me)), "pane new --agent --cmd claude --name a1");
    let a1 = pid(&rows[0]);
    let reads = || w.with(|m, _| m.ops.iter().filter(|o| matches!(o, itermkit::app::Op::Read { .. })).count());
    let before = reads();
    let (_, rows) = run(w.human(), "agent list");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(reads() - before, 1, "only the agent's screen (it has no app state yet)");
    run(w.human(), &format!("pane set p{} --set agent.state=idle", a1.0));
    let before = reads();
    let (_, rows) = run(w.human(), "agent list");
    assert_eq!((reads() - before, s(&rows[0], "state")), (0, "idle"), "the app's state: no read at all");
    // a state selector still observes every pane
    let before = reads();
    run(w.human(), "pane list --where state=ready");
    assert!(reads() - before >= 6);
}

#[test]
fn merge_goes_into_the_checkout_the_worktree_was_made_in() {
    // regression (live run 2026-10-29): the parent's folder moved (or the human ran the CLI elsewhere), and
    // merge/wt-remove looked for the parent's branch in the wrong repository
    let w = world("wtmoved");
    let repo = w.dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let git = |dir: &std::path::Path, args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap().status.success(), "git {args:?}");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let lead = w.window();
    w.with(|m, _| m.nodes.get_mut(&lead).unwrap().cwd = repo.display().to_string());
    let (code, rows) = run(w.env(Some(lead)), "pane new --agent --cmd worker --worktree --name fix");
    assert_eq!(code, 0, "{rows:?}");
    let wt = repo.join(".worktrees/fix");
    std::fs::write(wt.join("a.txt"), "x").unwrap();
    git(&wt, &["add", "a.txt"]);
    git(&wt, &["commit", "-q", "-m", "a"]);
    // the lead cd'd out of the repository
    w.with(|m, _| m.nodes.get_mut(&lead).unwrap().cwd = w.dir.display().to_string());
    let (code, rows) = run(w.env(Some(lead)), "agent merge fix");
    assert_eq!((code, codes(&rows)), (0, vec!["ok merged".to_string()]), "{rows:?}");
    assert!(repo.join("a.txt").exists());
}

#[test]
fn a_stuck_app_answers_not_responding_instead_of_hanging_the_cli() {
    // regression (2026-10-29 freeze): a command connection had no read timeout, so every CLI call that needed the
    // app's main thread hung with it (agent msg, pane set, children --wait's screen reads)
    let w = world("stuck");
    let me = w.window();
    let mut env = w.env(Some(me));
    env.call_timeout = std::time::Duration::from_millis(300);
    // the fake's operations wait on its model: holding it is a main thread that never answers
    let stuck = w.m.lock().unwrap();
    let cmd = format!("pane set p{} --name x", me.0);
    let (code, rows) = within(10, move || run(env, &cmd)).expect("the CLI came back while the app was stuck");
    drop(stuck);
    assert_eq!(code, 1, "{rows:?}");
    assert!(s(&rows[0], "reason").contains("not responding"), "{rows:?}");
    // the app comes back: the next command works (its stale connection was dropped, not reused)
    let (code, rows) = run(w.env(Some(me)), &format!("pane set p{} --name y", me.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok set".to_string()]), "{rows:?}");
}

#[test]
fn durations_are_validated_never_a_panic_or_forever() {
    // regression (2026-10-29 audit A5): negative, NaN and infinite waits panicked the CLI (Duration::from_secs_f64),
    // `--wait abc` silently meant forever, and `--timeout 0` was a zero socket timeout the kernel API refuses
    let w = world("durations");
    let me = w.window();
    let go = |cmd: String| {
        let env = w.env(Some(me));
        let c = cmd.clone();
        within(10, move || run(env, &c)).unwrap_or_else(|| panic!("{cmd}: hung or panicked"))
    };
    for cmd in [
        "agent inbox --wait=-1",
        "agent inbox --wait=nan",
        "agent inbox --wait=abc",
        "agent inbox --wait abc",
        "agent inbox --wait -1",
        "children --wait --timeout -1",
        "children --wait --timeout nan",
        "ui wait --timeout -1",
        "ui wait --timeout inf",
        "pane wait @me --timeout=-2",
    ] {
        let (code, rows) = go(cmd.to_string());
        assert_eq!(code, 1, "{cmd}: {rows:?}");
        assert!(s(&rows[0], "reason").contains("not a duration"), "{cmd}: {rows:?}");
    }
    // 0 checks once: an unmet condition is a timeout at once, a mailbox is read once
    let (code, rows) = go("pane wait @me --until exit --timeout 0".into());
    assert_eq!((code, codes(&rows)), (3, vec!["timeout timeout".to_string()]), "{rows:?}");
    let (code, rows) = go("children --wait --timeout 0".into());
    assert_eq!(code, 0, "{rows:?}");
    for cmd in ["agent inbox --wait 0", "agent inbox --wait=0"] {
        let (code, rows) = go(cmd.into());
        assert_eq!((code, codes(&rows)), (0, vec!["ok inbox".to_string()]), "{cmd}: {rows:?}");
    }
    // `--wait S`, space-separated as the help gives it, is S seconds (it was a selector word, ignored: forever)
    let t = std::time::Instant::now();
    let (code, rows) = go("agent inbox --wait 0.3".into());
    assert_eq!((code, codes(&rows)), (0, vec!["ok inbox".to_string()]), "{rows:?}");
    assert!(t.elapsed() >= std::time::Duration::from_millis(250), "{:?}", t.elapsed());
    // a huge wait is "forever" (clamped), never an overflow panic
    let (code, rows) = go("pane wait @me --until count>=1 --timeout 1e30".into());
    assert_eq!((code, codes(&rows)), (0, vec!["ok until".to_string()]), "{rows:?}");
}

/// A pane split beside `at` (the human's), opened as the app would.
fn split(w: &World, at: Id) -> Id {
    w.with(|m, ev| m.apply(itermkit::app::Op::Split { at, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0])
}

/// Runs `cmd` on its own thread, then `meanwhile` once its watch is up; the CLI's rows, or a panic if it hung.
fn run_while(env: itermkit::conn::Env, cmd: String, meanwhile: impl FnOnce()) -> (i32, Vec<serde_json::Value>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run(env, &cmd));
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    meanwhile();
    rx.recv_timeout(std::time::Duration::from_secs(20)).expect("the CLI came back")
}

/// A burst far past a watcher's queue (1024): the events after it are dropped and the watcher gets a gap.
const BURST: usize = 50_000;

#[test]
fn waits_read_the_present_after_a_gap_and_end_by_one_deadline() {
    // regression (2026-10-29 audit A6): `pane send --wait` and `wait --until gone|exit|new` ignored a gap, so the
    // event they waited for, dropped in it, never came; send --wait's timeout restarted with every event
    let w = world("gap");
    let lead = w.window();
    // send --wait: the agent's whole turn (busy … idle) lands in one burst; its idle is dropped
    let agent = split(&w, lead);
    w.with(|m, _| {
        m.nodes.get_mut(&agent).unwrap().vars.insert("agent.state".into(), "idle".into());
        m.turn_burst = BURST;
    });
    let (code, rows) = within(20, {
        let env = w.env(Some(lead));
        move || run(env, &format!("pane send p{} --text go --enter --wait --timeout 8", agent.0))
    })
    .expect("send --wait came back");
    assert_eq!((code, codes(&rows)), (0, vec!["ok delivered".to_string()]), "{rows:?}");
    // gone, exit and new: the event that meets each is the last of a burst
    let p = split(&w, lead);
    let (code, rows) = run_while(w.env(Some(lead)), format!("pane wait p{} --until gone --timeout 8", p.0), || {
        w.with(|m, ev| {
            ev.extend(std::iter::repeat_n(Event::Bell { pane: p }, BURST));
            m.close(p, ev);
        })
    });
    assert_eq!((code, codes(&rows)), (0, vec!["ok until".to_string()]), "gone: {rows:?}");
    let p = split(&w, lead);
    let (code, rows) = run_while(w.env(Some(lead)), format!("pane wait p{} --until exit --timeout 8", p.0), || {
        w.with(|m, ev| {
            ev.extend(std::iter::repeat_n(Event::Bell { pane: p }, BURST));
            m.nodes.get_mut(&p).unwrap().exit = Some(0);
            ev.push(Event::Exited { pane: p, status: 0 });
        })
    });
    assert_eq!((code, codes(&rows)), (0, vec!["ok until".to_string()]), "exit: {rows:?}");
    let win = w.with(|m, _| m.up(lead, itermkit::app::Kind::Window).unwrap());
    let (code, rows) = run_while(w.env(Some(lead)), format!("window wait w{} --until new --timeout 8", win.0), || {
        w.with(|m, ev| {
            ev.extend(std::iter::repeat_n(Event::Bell { pane: lead }, BURST));
            m.apply(itermkit::app::Op::Split { at: lead, side: itermkit::app::wire::Side::Down, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap();
        })
    });
    assert_eq!((code, codes(&rows)), (0, vec!["ok until".to_string()]), "new: {rows:?}");
    // one deadline for send --wait, however many events come: a busy pane printing forever still times out
    w.with(|m, _| m.turn_burst = 0);
    let busy = split(&w, lead);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let chatter = {
        let (stop, m, hub) = (stop.clone(), w.m.clone(), w.hub.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let snap = m.lock().unwrap().snapshot();
                hub.publish(snap, vec![Event::Bell { pane: busy }]);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        })
    };
    let t = std::time::Instant::now();
    let (code, rows) = within(20, {
        let env = w.env(Some(lead));
        move || run(env, &format!("pane send p{} --text go --enter --wait --timeout 1", busy.0))
    })
    .expect("send --wait came back");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    chatter.join().unwrap();
    assert_eq!(code, 3, "{rows:?}");
    assert!(t.elapsed() < std::time::Duration::from_secs(5), "the deadline is overall, not per event: {:?}", t.elapsed());
}

#[test]
fn claude_to_claude_mail_reports_only_a_confirmed_notice() {
    // regression (review of the hang fix, and backlog claude-to-claude-msg-cost (a)): a notice still waiting when the
    // CLI stopped waiting read `notified=pending` though the app may give it up, and `notified=yes` read as if the
    // cross-session push had landed when only the typed notice had
    let w = world("notice");
    let human = w.window();
    let claude = |name: &str, stall: Option<&str>| {
        let mut vars = vec![("agent.provider".to_string(), "claude".to_string()), ("agent.name".to_string(), name.to_string())];
        if let Some(ms) = stall {
            vars.push(("fake.stall".into(), ms.into()));
        }
        let spawn = itermkit::app::wire::Spawn { name: Some(name.into()), vars, ..Default::default() };
        w.with(|m, ev| m.apply(itermkit::app::Op::Split { at: human, side: itermkit::app::wire::Side::Right, ratio: None, spawn, focus: false }, ev).unwrap().ids[0])
    };
    let me = claude("lead", None);
    claude("quick", None);
    claude("stuck", Some("7000"));
    let (code, rows) = run(w.env(Some(me)), "agent msg quick --text hello");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!((s(&rows[0], "route"), &rows[0]["notice_typed"]), ("sendmessage", &json!(true)), "{rows:?}");
    assert!(rows[0].get("notified").is_none() && rows[0].get("sendmessage_to").is_some(), "{rows:?}");
    let t = std::time::Instant::now();
    let (_, rows) = run(w.env(Some(me)), "agent msg stuck --text hello");
    assert!(t.elapsed() < std::time::Duration::from_millis(6500), "{:?}", t.elapsed());
    assert_eq!((&rows[0]["notice_typed"], s(&rows[0], "notice")), (&json!(false), "unconfirmed"), "{rows:?}");
}

#[test]
fn a_timeout_after_a_reconnect_still_holds() {
    // regression (review of the hang fix): after a timed-out request the connection is dropped, and the next one
    // connected again without arming its read timeout (the old value was remembered), so it blocked unbounded
    let w = world("rearm");
    let p = w.window();
    w.with(|m, _| m.nodes.get_mut(&p).unwrap().vars.insert("fake.stall".into(), "3000".into()));
    let mut c = itermkit::conn::Conn::new(w.human());
    let read = || itermkit::app::Op::Read { pane: p, lines: itermkit::app::wire::Lines::Screen };
    let wait = std::time::Duration::from_millis(200);
    for _ in 0..2 {
        let t = std::time::Instant::now();
        assert!(c.call_within(read(), wait).is_err());
        assert!(t.elapsed() < std::time::Duration::from_millis(1500), "unbounded after a reconnect: {:?}", t.elapsed());
    }
}

#[test]
fn reads_stay_bounded_with_a_stuck_main_thread_and_a_gone_parent() {
    // regression (2026-10-29 CLI hang): `agent list` read each untracked agent pane's screen in turn, and each read
    // waits on the app's main thread (10–20 s apiece while it is stuck), so 14 panes held the reply past 120 s; the
    // orchestrator's pane had closed (resumed as a new pane), so its children named a parent that is gone
    let w = world("stuck");
    let human = w.window();
    for k in 0..4 {
        let name = if k == 0 { "redteam".to_string() } else { format!("a{k}") };
        let spawn = itermkit::app::wire::Spawn {
            name: Some(name.clone()),
            vars: vec![("agent.kind".into(), "claude".into()), ("agent.name".into(), name), ("agent.parent".into(), "p9999".into()), ("fake.stall".into(), "4000".into())],
            ..Default::default()
        };
        w.with(|m, ev| m.apply(itermkit::app::Op::Split { at: human, side: itermkit::app::wire::Side::Right, ratio: None, spawn, focus: false }, ev).unwrap());
    }
    let secs = |t: std::time::Instant| t.elapsed().as_secs_f32();
    // a name selector: the snapshot alone, at once; the gone parent is no parent
    let t = std::time::Instant::now();
    let (code, rows) = run(w.human(), "pane get redteam");
    assert!(secs(t) < 1.0, "pane get NAME took {} s", secs(t));
    assert_eq!((code, rows.len()), (0, 1), "{rows:?}");
    assert!(rows[0].get("agent_parent").is_none() && s(&rows[0], "name") == "redteam", "{rows:?}");
    // agent list: one bounded read, then the rest are unknown (4 reads of 4 s each before)
    let t = std::time::Instant::now();
    let (code, rows) = run(w.human(), "agent list");
    assert!(secs(t) < 3.0, "agent list took {} s", secs(t));
    assert_eq!((code, rows.len()), (0, 4), "{rows:?}");
    assert!(rows.iter().all(|r| s(r, "state") == "unknown"), "{rows:?}");
    // adopt by name: a head (its parent is gone) is adopted
    let t = std::time::Instant::now();
    let (code, rows) = run(w.human(), "agent adopt redteam --to human");
    assert!(secs(t) < 1.0, "agent adopt took {} s", secs(t));
    assert_eq!((code, codes(&rows)), (0, vec!["ok adopted".to_string()]), "{rows:?}");
}

#[test]
fn an_agent_the_human_started_is_listed_by_its_provider() {
    // the app's agent table names the provider of any pane (iTerm2: from its foreground process), not only the
    // panes the CLI spawned with agent.kind
    let w = world("humanagent");
    let me = w.window();
    let (_, rows) = run(w.env(Some(me)), "pane new --count 2");
    let p = pid(&rows[0]);
    w.with(|m, _| {
        let n = m.nodes.get_mut(&p).unwrap();
        n.vars.insert("agent.provider".into(), "codex".into());
        n.vars.insert("agent.state".into(), "busy".into());
    });
    let (_, rows) = run(w.human(), "agent list");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((s(&rows[0], "kind"), s(&rows[0], "state")), ("codex", "busy"));
}

#[test]
fn a_starting_agent_is_never_typed_into_by_a_rename() {
    use itermkit::app::Op;
    // keys sent while an agent starts wait in its tty and land on its first screen, a folder-trust prompt included
    let w = world("rename-starting");
    let human = w.window();
    let a = w.with(|m, ev| m.apply(Op::Split { at: human, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0]);
    w.with(|m, _| {
        let n = m.nodes.get_mut(&a).unwrap();
        n.vars.insert("agent.provider".into(), "claude".into());
        n.vars.insert("agent.state".into(), "starting".into());
    });
    let (code, rows) = run(w.human(), &format!("pane set p{} --name proj", a.0));
    assert_eq!(code, 0, "{rows:?}");
    assert!(rows.iter().any(|r| s(r, "renamed") == "later"), "{rows:?}");
    assert!(!w.with(|m, _| m.ops.iter().any(|o| matches!(o, Op::Deliver { .. }))), "nothing typed while it starts");
    // once its input box is up, the rename goes in
    w.with(|m, _| { m.nodes.get_mut(&a).unwrap().vars.insert("agent.state".into(), "ready".into()); });
    run(w.human(), &format!("pane set p{} --name proj2", a.0));
    assert!(w.with(|m, _| m.ops.iter().any(|o| matches!(o, Op::Deliver { text, .. } if text == "/rename proj2"))));
}

#[test]
fn color_is_claudes_own_slash_color_for_a_claude_agent_else_the_tab_color() {
    use itermkit::app::{wire::Field, Op};
    let w = world("color-route");
    let human = w.window();
    let a = w.with(|m, ev| m.apply(Op::Split { at: human, side: itermkit::app::wire::Side::Right, ratio: None, spawn: Default::default(), focus: false }, ev).unwrap().ids[0]);
    w.with(|m, _| {
        let n = m.nodes.get_mut(&a).unwrap();
        n.vars.insert("agent.provider".into(), "claude".into());
        n.vars.insert("agent.state".into(), "ready".into());
    });
    let (code, rows) = run(w.human(), &format!("pane set p{} --color teal", a.0));
    assert_eq!(code, 0, "{rows:?}");
    assert!(w.with(|m, _| m.ops.iter().any(|o| matches!(o, Op::Deliver { text, .. } if text == "/color cyan"))), "{rows:?}");
    assert!(!w.with(|m, _| m.ops.iter().any(|o| matches!(o, Op::Set { id, field: Field::Color(_) } if *id == a))), "no tab color for a Claude agent");
    // a shell pane: the tab color, nothing typed
    let (code, _) = run(w.human(), &format!("pane set p{} --color red", human.0));
    assert_eq!(code, 0);
    assert!(w.with(|m, _| m.ops.iter().any(|o| matches!(o, Op::Set { id, field: Field::Color(Some(_)) } if *id == human))));
    assert!(!w.with(|m, _| m.ops.iter().any(|o| matches!(o, Op::Deliver { pane, .. } if *pane == human))));
}

// regression: a non-UTF-8 argument panicked in std::env::args
#[test]
fn a_non_utf8_argument_is_one_error_line() {
    use std::os::unix::ffi::OsStringExt;
    let bad = vec![std::ffi::OsString::from("pane"), std::ffi::OsString::from_vec(vec![0xff])];
    let line = itermkit::cli::text_args(bad.into_iter()).unwrap_err();
    assert!(line.starts_with("failed error") && line.contains("not UTF-8") && !line.contains('\n'), "{line}");
}

// regression: a regex that doesn't compile selected nothing, silently
#[test]
fn a_bad_regex_is_an_error_not_an_empty_answer() {
    let w = world("badre");
    w.window();
    let (code, rows) = run(w.human(), "pane list --where cwd~/x(/");
    assert_eq!(code, 1, "{rows:?}");
    assert_eq!(s(&rows[0], "status"), "failed");
}

// the promise in AGENT_USAGE.md: an old name and a new one write at once, --apply changes nothing, --plan touches nothing
#[test]
fn writes_run_at_once_and_only_plan_is_dry() {
    let w = world("gate");
    let me = w.window();
    let dir = |n: &str| {
        let d = std::env::temp_dir().join(format!("iterm-gate-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.display().to_string()
    };
    let (a, b) = (dir("a"), dir("b"));
    let env = || w.env(Some(me));
    let tabs = || w.with(|m, _| m.nodes.values().filter(|n| n.kind == itermkit::app::Kind::Tab).count());
    let start = tabs();
    let (code, rows) = run(env(), &format!("tab-create --cwd {a} --cwd {b} --agent --plan"));
    assert_eq!((code, s(&rows[0], "status")), (0, "planned"), "{rows:?}");
    assert_eq!(tabs(), start, "--plan makes nothing");
    let (code, rows) = run(env(), &format!("tab-create --cwd {a} --cwd {b} --agent"));
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(tabs(), start + 2, "no flag: real tabs, one per folder: {rows:?}");
    assert_eq!(rows.iter().map(|r| s(r, "name")).collect::<Vec<_>>(), ["w1", "w2"], "named in spawn order, as documented");
    let (code, rows) = run(env(), &format!("tab new --cwd {a} --apply"));
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(tabs(), start + 3, "--apply is accepted and changes nothing");
}

#[test]
fn agents_fill_the_spawners_dock_a_row_under_its_companion() {
    let w = world("dock");
    let lead = w.window();
    let me = || w.env(Some(lead));
    // the screenshot's tab: the lead, and `st -l` to its right
    let (_, rows) = run(me(), "pane new --at @me --side right --cmd 'st -l'");
    let st = pid(&rows[0]);
    let (l, c) = (lead.0, st.0);
    // --plan says where the first agent goes: below the companion
    let (_, rows) = run(me(), "agent new --plan");
    assert_eq!((s(&rows[0], "beside"), s(&rows[0], "side")), (format!("p{c}").as_str(), "down"), "{rows:?}");
    let (_, rows) = run(me(), "agent new");
    let a1 = pid(&rows[0]).0;
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{c}(0.50) / {a1}(0.50)](0.50)]"));
    // the next ones grow the row to the right, all equal; the lead and the companion keep their shares
    let (_, rows) = run(me(), "agent new");
    let a2 = pid(&rows[0]).0;
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{c}(0.50) / [{a1}(0.50) | {a2}(0.50)](0.50)](0.50)]"));
    let more: Vec<u64> = (0..3).map(|_| pid(&run(me(), "agent new").1[0]).0).collect();
    let row: Vec<String> = [a1, a2].iter().chain(&more).map(|a| format!("{a}(0.20)")).collect();
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{c}(0.50) / [{}](0.50)](0.50)]", row.join(" | ")));
    // several at once continue the row
    let (_, rows) = run(me(), "agent new --task one --task two --task three --read-only");
    assert_eq!(codes(&rows), ["ok created", "ok created", "ok created"], "{rows:?}");
    let shape = w.shape_of(lead);
    assert_eq!(shape.matches("(0.12)").count(), 8, "eight equal columns: {shape}");
    assert!(shape.starts_with(&format!("[{l}(0.50) | [{c}(0.50) / [{a1}(0.12)")), "{shape}");
    // an explicit placement is kept as given
    let (_, rows) = run(me(), "agent new --at @me --side down");
    let b = pid(&rows[0]).0;
    assert!(w.shape_of(lead).starts_with(&format!("[[{l}(0.50) / {b}(0.50)](0.50) | ")), "{}", w.shape_of(lead));
}

#[test]
fn a_lone_spawner_docks_beside_itself_and_a_child_under_itself() {
    let w = world("dock-lone");
    let lead = w.window();
    let l = lead.0;
    // alone and wide: right, then the row grows rightwards; the lead keeps its half
    let a1 = pid(&run(w.env(Some(lead)), "agent new").1[0]);
    let a2 = pid(&run(w.env(Some(lead)), "agent new").1[0]).0;
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | {}(0.25) | {a2}(0.25)]", a1.0));
    // a child in its parent's dock row starts its own row under itself, then grows it rightwards
    let c1 = pid(&run(w.env(Some(a1)), "agent new").1[0]).0;
    let c2 = pid(&run(w.env(Some(a1)), "agent new").1[0]).0;
    assert_eq!(w.shape_of(lead), format!("[{l}(0.50) | [{}(0.50) / [{c1}(0.50) | {c2}(0.50)](0.50)](0.25) | {a2}(0.25)]", a1.0));
    // a plain pane (no --agent) splits as before: the caller, the wider way
    let (_, rows) = run(w.env(Some(lead)), "pane new --plan");
    assert!(rows[0].get("beside").is_none(), "{rows:?}");
}

#[test]
fn a_task_file_is_one_task_unless_split_and_plan_checks_it() {
    let w = world("taskfile");
    let lead = w.window();
    // a brief with a heading, two ## sections and bullets (field report from fsearch-lead, 2026-11-07)
    std::fs::write(w.dir.join("brief.md"), "# Learn\n\n## Goal\n- one\n- two\n\n## Rules\n1. a\n2. b\n").unwrap();
    let (code, rows) = run(w.env(Some(lead)), "agent new --name fs-learn --task-file brief.md --plan");
    assert_eq!((code, rows[0]["items"].clone()), (0, json!(1)), "{rows:?}");
    let (code, rows) = run(w.env(Some(lead)), "agent new --name fs-learn --task-file brief.md");
    assert_eq!((code, codes(&rows)), (0, vec!["ok created".to_string()]), "{rows:?}");
    assert!(w.node(pid(&rows[0])).unwrap().vars["agent.task"].starts_with("# Learn"));
    // --split: one per ## section; --plan says how many, and refuses what the run would refuse
    let (_, rows) = run(w.env(Some(lead)), "agent new --task-file brief.md --split --read-only --plan");
    assert_eq!(rows[0]["items"], json!(2), "{rows:?}");
    let (code, rows) = run(w.env(Some(lead)), "agent new --name x --task-file brief.md --split --read-only --plan");
    assert_eq!(code, 1, "{rows:?}");
    assert!(s(&rows[0], "reason").contains("--name for several items"), "{rows:?}");
}

#[test]
fn a_message_finds_an_agent_by_its_name_when_no_pane_has_it() {
    let w = world("msgname");
    let lead = w.window();
    let (_, rows) = run(w.env(Some(lead)), "agent new --name helper");
    let a = pid(&rows[0]);
    // the agent's name lives in agent.name (tree's as=), not the pane's own name
    w.with(|m, _| m.nodes.get_mut(&a).unwrap().name = "something else".into());
    let (code, rows) = run(w.env(Some(lead)), "agent msg helper --text hi");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(s(&rows[0], "pane"), format!("p{}", a.0));
    let (code, rows) = run(w.env(Some(lead)), "agent msg nobody --text hi");
    assert_eq!(code, 1);
    assert!(s(&rows[0], "reason").contains("no agent or pane nobody"), "{rows:?}");
}

#[test]
fn a_relative_cwd_is_the_callers_and_a_missing_one_is_refused() {
    // field report from status-lead (2026-11-07): a relative --cwd started the agent in whichever project iTerm2 had
    // active, since the launch's `cd` ran from there and failed quietly
    let w = world("cwd");
    let lead = w.window();
    std::fs::create_dir_all(w.dir.join("worktrees/feat")).unwrap();
    let (code, rows) = run(w.env(Some(lead)), "agent new --cwd worktrees/feat --at @me --side right");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(w.node(pid(&rows[0])).unwrap().cwd, w.dir.join("worktrees/feat").display().to_string());
    let before = w.panes().len();
    let (code, rows) = run(w.env(Some(lead)), "agent new --cwd worktrees/nope --at @me --side right");
    assert_eq!(code, 1, "{rows:?}");
    assert!(s(&rows[0], "reason").contains("--cwd worktrees/nope: no such folder"), "{rows:?}");
    assert_eq!(w.panes().len(), before, "nothing opened");
}

#[test]
fn a_selector_finds_an_agent_by_its_name() {
    // field reports (2026-11-07): `pane get land`, `agent close complexity` failed while tree showed as=land
    let w = world("selname");
    let lead = w.window();
    let a = pid(&run(w.env(Some(lead)), "agent new --name land").1[0]);
    w.with(|m, _| m.nodes.get_mut(&a).unwrap().name = "✳ Claude Code".into());
    let (code, rows) = run(w.env(Some(lead)), "pane get land");
    assert_eq!((code, s(&rows[0], "id")), (0, format!("p{}", a.0).as_str()), "{rows:?}");
    let (code, rows) = run(w.env(Some(lead)), "agent close land --force");
    assert_eq!((code, codes(&rows)), (0, vec!["ok closed".to_string()]), "{rows:?}");
}

#[test]
fn a_subcommand_help_flag_shows_the_nouns_help() {
    // field report from perf_usage-lead (2026-11-07): `iterm pane new --help` was an unrecognized argument
    let w = world("help");
    let (code, text) = text(w.human(), "pane new --help");
    assert_eq!(code, 0, "{text}");
    assert!(text.starts_with("iterm pane <verb>"), "{text}");
    assert_eq!(text, common::text(w.human(), "help pane").1);
}
