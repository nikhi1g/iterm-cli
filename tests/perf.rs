//! Complexity tests for the core algorithms: each runs at two sizes and the time may not grow worse than its
//! bound allows; the numbers print with `cargo test --release --test perf -- --nocapture --test-threads=1`.
//! Pure code only (no app, no fake), so they measure the algorithms and not a transport.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use itermkit::app::{Id, Kind as PKind, Node, PaneInfo, Snapshot};
use itermkit::geometry::Frame;
use itermkit::grammar::{self, Kind};
use itermkit::iterm2::ids::IdMap;
use itermkit::plan::{self, PlanFile, Task};
use itermkit::select::{self, Ctx, MNode, Model};

/// The best of `runs`: the least disturbed by other work on the machine.
fn best(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap_or_default()
}

/// Time at `n` and at 10·`n`; the ratio must stay under `limit` (linear is about 10, quadratic about 100). A busy
/// machine only makes a measurement slower at random, so a failure is measured again before it counts.
fn scales(what: &str, n: usize, limit: f64, mut at: impl FnMut(usize) -> Box<dyn FnMut()>) {
    let mut ratio = f64::MAX;
    for _ in 0..4 {
        let (small, big) = (best(5, at(n)), best(3, at(n * 10)));
        ratio = big.as_secs_f64() / small.as_secs_f64().max(1e-7);
        println!("{what:<34} n={n:<6} {small:>12.2?}   n={:<7} {big:>12.2?}   ×{ratio:.1}", n * 10);
        if ratio < limit {
            return;
        }
    }
    panic!("{what}: ×{ratio:.1} for 10× the input, limit ×{limit}");
}

fn node(kind: Kind, id: String, parent: Option<usize>, rect: Option<Frame>) -> MNode {
    MNode { kind, raw: id.clone(), id, name: String::new(), parent, rect, rank: 0.0, fields: BTreeMap::new() }
}

/// One window with `tabs` tabs of `per` panes side by side, each pane with a cwd.
fn model(tabs: usize, per: usize) -> Model {
    let mut m = Model::default();
    m.nodes.push(node(Kind::App, "app".into(), None, None));
    m.nodes.push(node(Kind::Window, "w1".into(), Some(0), None));
    m.nodes.push(node(Kind::VTab, "v2".into(), Some(1), None));
    for t in 0..tabs {
        let ti = m.nodes.len();
        m.nodes.push(node(Kind::Tab, format!("t{ti}"), Some(2), None));
        for p in 0..per {
            let i = m.nodes.len();
            let mut n = node(Kind::Pane, format!("p{i}"), Some(ti), Some(Frame { x: p as f64 * 10.0, y: 0.0, width: 10.0, height: 10.0 }));
            n.fields.insert("cwd".into(), format!("/work/{t}/{p}"));
            m.nodes.push(n);
        }
    }
    m.me = m.nodes.iter().position(|n| n.kind == Kind::Pane);
    m.index();
    m
}

fn eval(m: &Model, sel: &str) -> usize {
    select::eval(m, &grammar::selector(sel).unwrap(), Some(Kind::Pane), &Ctx::default()).map(|v| v.len()).unwrap_or(0)
}

#[test]
fn index_selector_counts_containers_in_linear_time() {
    scales("select: index, one pane per tab", 2000, 40.0, |n| {
        let m = model(n, 1);
        Box::new(move || assert_eq!(eval(&m, "1"), 1))
    });
}

#[test]
fn a_regex_selector_compiles_once() {
    scales("select: cwd~/re/ over every pane", 2000, 40.0, |n| {
        let m = model(1, n);
        Box::new(move || assert_eq!(eval(&m, r"cwd~/\/0\/1$/"), 1))
    });
}

#[test]
fn neighbors_of_every_pane_stay_in_their_tab() {
    scales("select: @right of every pane", 2000, 40.0, |n| {
        let m = model(n / 2, 2);
        Box::new(move || assert_eq!(eval(&m, "@right:p*"), n / 2))
    });
}

#[test]
fn an_in_filter_is_linear_in_panes() {
    scales("--in every tab: index in each", 2000, 40.0, |n| {
        let m = model(n, 1);
        let within: Vec<usize> = (0..m.nodes.len()).filter(|i| m.nodes[*i].kind == Kind::Tab).collect();
        Box::new(move || {
            let ctx = Ctx { within: Some(within.clone()) };
            assert_eq!(select::eval(&m, &grammar::selector("1").unwrap(), Some(Kind::Pane), &ctx).unwrap().len(), n);
        })
    });
}

#[test]
fn a_huge_index_span_costs_nothing() {
    let m = model(1, 10);
    let t = Instant::now();
    assert_eq!(eval(&m, "1-99999999999"), 10);
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[test]
fn a_glob_never_backtracks_exponentially() {
    let (pat, text) = ("a*".repeat(14) + "b", "a".repeat(40));
    let t = Instant::now();
    assert!(!grammar::glob(&pat, &text));
    println!("glob: 14 stars, 40 chars           {:>12.2?}", t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

fn snap(panes: usize) -> Snapshot {
    let node = |id: u64, kind, parent| Node { id: Id(id), kind, parent, name: String::new(), rect: None, created: id, vars: vec![], pane: None, layout: None };
    let mut nodes = vec![node(0, PKind::App, None), node(1, PKind::Window, Some(0)), node(2, PKind::VTab, Some(1)), node(3, PKind::Tab, Some(2))];
    nodes.extend((0..panes as u64).map(|i| Node { pane: Some(PaneInfo { cwd: "/tmp".into(), cols: 80, rows: 24, ..PaneInfo::default() }), ..node(4 + i, PKind::Pane, Some(3)) }));
    Snapshot { nodes, ..Snapshot::default() }
}

#[test]
fn tree_json_folds_a_full_tab_in_linear_time() {
    scales("tree --json: one tab of n panes", 2000, 40.0, |n| {
        let s = snap(n);
        Box::new(move || {
            let o = itermkit::tree::tree_json(&s, None, None, "");
            assert_eq!(o["windows"][0]["vtabs"][0]["tabs"][0]["panes"].as_array().unwrap().len(), n);
        })
    });
}

fn chain(n: usize) -> PlanFile {
    // the last task first: the worst order for a scheduler that sweeps the list
    let tasks = (0..n)
        .rev()
        .map(|i| Task { id: format!("t{i}"), task: "x".into(), deps: if i == 0 { vec![] } else { vec![format!("t{}", i - 1)] }, provider: None, model: None, effort: None, worktree: false, gate: None, retries: 0 })
        .collect();
    PlanFile { name: None, max: None, tasks }
}

#[test]
fn a_plan_check_is_linear_in_tasks_and_edges() {
    scales("plan check: chain, worst order", 500, 40.0, |n| {
        let p = chain(n);
        Box::new(move || plan::check(&p).unwrap())
    });
}

#[cfg(target_os = "macos")]
#[test]
fn one_process_read() {
    let pid = std::process::id() as i32;
    let t = best(5, || {
        for _ in 0..100 {
            assert!(itermkit::iterm2::sense::proc_of(pid).is_some_and(|p| p.provider().is_none()));
        }
    });
    println!("proc_of + provider, per pane       {:>12.2?}", t / 100);
}

fn id_lines(n: usize) -> String {
    (1..=n).map(|i| format!("{i}\tp\t{:08X}-AAAA-BBBB-CCCC-DDDDEEEEFFFF\n", i)).collect()
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("iterm-perf-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn id_map_cost_per_command() {
    let d = temp("ids");
    std::fs::write(d.join("ids.tsv"), id_lines(100_000)).unwrap();
    let open = best(5, || {
        IdMap::open(&d).unwrap();
    });
    let m = IdMap::open(&d).unwrap();
    let u = format!("{:08X}-AAAA-BBBB-CCCC-DDDDEEEEFFFF", 50_000);
    let get = best(5, || {
        for _ in 0..1000 {
            assert!(m.get(PKind::Pane, &u).is_some());
        }
    });
    let peek = best(5, || {
        assert!(itermkit::iterm2::ids::peek(&d, PKind::Pane, &u).is_some());
    });
    println!("ids.tsv, 100k lines: open {open:>10.2?}   1000 lookups {get:>10.2?}   peek {peek:>10.2?}");
    let _ = std::fs::remove_dir_all(&d);
}
