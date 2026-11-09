//! The guide's `shape` section, command by command, against the fake: what each does to the split tree and the
//! focus. (Before these, `pane move`, `pane resize --ratio`, `tab set --equalize`, `--tile`, `--frame`, `select`
//! and `--focus` ran only in `control.rs`'s bare sweep, which checks a handler answers, not what it does.)

mod common;

use common::*;
use itermkit::app::{Id, Kind, Op};

/// [lead | [b / c]]: (lead, b, c).
fn three(w: &World) -> (Id, Id, Id) {
    let lead = w.window();
    let b = pid(&run(w.env(Some(lead)), "pane new --cmd b --side right").1[0]);
    let c = pid(&run(w.env(Some(lead)), &format!("pane new --at p{} --side down --cmd c", b.0)).1[0]);
    assert_eq!(w.shape_of(lead), format!("[{}(0.50) | [{}(0.50) / {}(0.50)](0.50)]", lead.0, b.0, c.0));
    (lead, b, c)
}

#[test]
fn resize_ratio_then_equalize() {
    let w = world("shape-ratio");
    let (lead, b, c) = three(&w);
    let (code, rows) = run(w.human(), &format!("pane resize p{} --ratio 0.7", lead.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok resized".to_string()]), "{rows:?}");
    assert_eq!(w.shape_of(lead), format!("[{}(0.70) | [{}(0.50) / {}(0.50)](0.30)]", lead.0, b.0, c.0));
    run(w.human(), &format!("pane resize p{} --ratio 0.25", c.0));
    assert_eq!(w.shape_of(lead), format!("[{}(0.70) | [{}(0.75) / {}(0.25)](0.30)]", lead.0, b.0, c.0));
    let tab = w.node(lead).unwrap().parent.unwrap();
    let (code, rows) = run(w.human(), &format!("tab set t{} --equalize", tab.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok equalized".to_string()]), "{rows:?}");
    assert_eq!(w.shape_of(lead), format!("[{}(0.50) | [{}(0.50) / {}(0.50)](0.50)]", lead.0, b.0, c.0));
    assert_eq!(run(w.human(), &format!("pane resize p{} --ratio 1.5", lead.0)).0, 1, "a ratio is inside (0, 1)");
}

#[test]
fn move_a_pane_beside_another_or_to_a_tab_of_its_own() {
    let w = world("shape-move");
    let (lead, b, c) = three(&w);
    let (code, rows) = run(w.human(), &format!("pane move p{} --at p{} --side down", c.0, lead.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok moved".to_string()]), "{rows:?}");
    assert_eq!(w.shape_of(lead), format!("[[{}(0.50) / {}(0.50)](0.50) | {}(0.50)]", lead.0, c.0, b.0));
    let tab = w.node(lead).unwrap().parent;
    run(w.human(), &format!("pane move p{} --to new-tab", b.0));
    assert_ne!(w.node(b).unwrap().parent, tab);
    assert_eq!(w.shape_of(b), b.0.to_string());
    assert_eq!(w.shape_of(lead), format!("[{}(0.50) / {}(0.50)]", lead.0, c.0));
    let (code, _) = run(w.human(), &format!("pane move p{} --at p{} --side down", c.0, c.0));
    assert_eq!(code, 1, "not beside itself");
}

#[test]
fn tile_grid_fills_rows_in_reading_order() {
    let w = world("shape-tile");
    let lead = w.window();
    let (code, rows) = run(w.env(Some(lead)), "pane new --count 4 --stack down --tile grid");
    assert_eq!(code, 0, "{rows:?}");
    assert_eq!(codes(&rows).last().map(String::as_str), Some("ok tiled"), "{rows:?}");
    // five panes: ⌈√5⌉ = 3 columns of rows: (1 4) (2 5) (3)
    let order: Vec<u64> = std::iter::once(lead.0).chain(rows.iter().filter(|r| s(r, "code") == "created").map(|r| pid(r).0)).collect();
    let [a, b, c, d, e] = order[..] else { panic!("{order:?}") };
    assert_eq!(w.shape_of(lead), format!("[[{a}(0.50) / {d}(0.50)](0.33) | [{b}(0.50) / {e}(0.50)](0.33) | {c}(0.33)]"));
}

#[test]
fn window_frame_is_set_as_given() {
    let w = world("shape-frame");
    let p = w.window();
    let win = w.with(|m, _| m.up(p, Kind::Window).unwrap());
    let (code, rows) = run(w.human(), &format!("window set w{} --frame 10,25,800,600", win.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok framed".to_string()]), "{rows:?}");
    let f = w.node(win).unwrap().frame.unwrap();
    assert_eq!((f.x, f.y, f.w, f.h), (10.0, 25.0, 800.0, 600.0));
    assert_eq!(run(w.human(), &format!("window set w{} --frame 1,2,3", win.0)).0, 1, "four numbers");
}

#[test]
fn only_select_and_focus_move_the_humans_focus() {
    // the rule in help: no write moves the human's focus unless select or --focus
    let w = world("shape-focus");
    let lead = w.window();
    let focus = || w.m.lock().unwrap().focus;
    let b = pid(&run(w.env(Some(lead)), "pane new --cmd b").1[0]);
    run(w.env(Some(lead)), &format!("pane set p{} --name x", b.0));
    run(w.env(Some(lead)), &format!("pane resize p{} --ratio 0.3", b.0));
    assert_eq!(focus(), Some(lead));
    assert!(w.with(|m, _| m.ops.iter().all(|o| !matches!(o, Op::Split { focus: true, .. }))));
    let c = pid(&run(w.env(Some(lead)), "pane new --cmd c --focus").1[0]);
    assert_eq!(focus(), Some(c));
    let (code, rows) = run(w.env(Some(lead)), &format!("pane select p{}", b.0));
    assert_eq!((code, codes(&rows)), (0, vec!["ok selected".to_string()]), "{rows:?}");
    assert_eq!(focus(), Some(b));
}

#[test]
fn only_a_plain_esc_or_ctrl_c_passes_a_prompt() {
    // `input::cancels`: shift, alt or cmd make it another key (the fake once let any esc or ctrl-c through)
    let w = world("shape-cancel");
    let lead = w.window();
    let p = pid(&run(w.env(Some(lead)), "pane new --cmd codex").1[0]);
    w.screen(p, &["Allow command? [y/N]"]);
    for k in ["esc", "ctrl-c", "C-c"] {
        assert_eq!(run(w.human(), &format!("pane key p{} --key {k}", p.0)).0, 0, "{k}");
    }
    for k in ["shift-esc", "cmd-ctrl-c", "alt-esc", "enter", "y"] {
        let (code, rows) = run(w.human(), &format!("pane key p{} --key {k}", p.0));
        assert!(code != 0 && s(&rows[0], "reason").contains("prompt"), "{k}: {rows:?}");
    }
}

#[test]
fn pane_set_bury_does_what_the_guide_says() {
    let w = world("shape-bury");
    let (lead, _, c) = three(&w);
    let (code, rows) = run(w.env(Some(lead)), &format!("pane set p{} --bury", c.0));
    assert_eq!(code, 0, "{rows:?}");
}
