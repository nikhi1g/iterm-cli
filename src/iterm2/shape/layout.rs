//! Pure geometry for the shape module: iTerm2's split tree with cell sizes ([`Tree`], from and to
//! `SplitTreeNode`), a wanted tree with fractions ([`Plan`], from the CLI's [`Layout`]), and the sizes, specs and
//! orders the writes send. No iTerm2 calls here, so every translation is unit-tested.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::super::proto as p;

/// A tab's split tree as iTerm2 reports it: leaves are session UUIDs with their grid sizes. `vertical`: the
/// children sit side by side (vertical dividers), as iTerm2 names it.
#[derive(Debug, Clone, PartialEq)]
pub enum Tree {
    Leaf { id: String, cols: i32, rows: i32 },
    Split { vertical: bool, kids: Vec<Tree> },
}

impl Tree {
    /// From iTerm2's tree; one-child nodes collapse (iTerm2 wraps a lone session in one).
    pub fn from_proto(n: &p::SplitTreeNode) -> Option<Tree> {
        use p::split_tree_node::split_tree_link::Child;
        let mut kids: Vec<Tree> = n
            .links
            .iter()
            .filter_map(|l| match &l.child {
                Some(Child::Session(s)) => {
                    let g = s.grid_size.unwrap_or_default();
                    Some(Tree::Leaf { id: s.unique_identifier.clone().unwrap_or_default(), cols: g.width.unwrap_or(0), rows: g.height.unwrap_or(0) })
                }
                Some(Child::Node(c)) => Tree::from_proto(c),
                None => None,
            })
            .collect();
        match kids.len() {
            0 => None,
            1 => kids.pop(),
            _ => Some(Tree::Split { vertical: n.vertical.unwrap_or(false), kids }),
        }
    }

    /// To iTerm2's tree (a lone session is wrapped in a node, as iTerm2 reports it).
    #[cfg(test)]
    pub fn to_proto(&self) -> p::SplitTreeNode {
        use p::split_tree_node::{split_tree_link::Child, SplitTreeLink};
        fn link(t: &Tree) -> SplitTreeLink {
            SplitTreeLink {
                child: Some(match t {
                    Tree::Leaf { id, cols, rows } => Child::Session(p::SessionSummary {
                        unique_identifier: Some(id.clone()),
                        grid_size: Some(p::Size { width: Some(*cols), height: Some(*rows) }),
                        ..Default::default()
                    }),
                    split => Child::Node(split.to_proto()),
                }),
            }
        }
        match self {
            Tree::Leaf { .. } => p::SplitTreeNode { vertical: Some(false), links: vec![link(self)] },
            Tree::Split { vertical, kids } => p::SplitTreeNode { vertical: Some(*vertical), links: kids.iter().map(link).collect() },
        }
    }

    /// Columns × rows the tree covers: a side-by-side split adds widths and shares the tallest height.
    pub fn extent(&self) -> (i32, i32) {
        match self {
            Tree::Leaf { cols, rows, .. } => (*cols, *rows),
            Tree::Split { vertical, kids } => {
                let e: Vec<(i32, i32)> = kids.iter().map(Tree::extent).collect();
                if *vertical {
                    (e.iter().map(|x| x.0).sum(), e.iter().map(|x| x.1).max().unwrap_or(0))
                } else {
                    (e.iter().map(|x| x.0).max().unwrap_or(0), e.iter().map(|x| x.1).sum())
                }
            }
        }
    }

    pub fn leaves(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |id, _, _| out.push(id.to_string()));
        out
    }

    fn walk(&self, f: &mut impl FnMut(&str, i32, i32)) {
        match self {
            Tree::Leaf { id, cols, rows } => f(id, *cols, *rows),
            Tree::Split { kids, .. } => kids.iter().for_each(|k| k.walk(f)),
        }
    }

    fn walk_mut(&mut self, f: &mut impl FnMut(&mut String, &mut i32, &mut i32)) {
        match self {
            Tree::Leaf { id, cols, rows } => f(id, cols, rows),
            Tree::Split { kids, .. } => kids.iter_mut().for_each(|k| k.walk_mut(f)),
        }
    }

    #[cfg(test)]
    pub fn sizes(&self) -> HashMap<String, (i32, i32)> {
        let mut m = HashMap::new();
        self.walk(&mut |id, c, r| {
            m.insert(id.to_string(), (c, r));
        });
        m
    }

    /// This tree's shape with the sizes from `sizes` (leaves not in it keep theirs).
    #[cfg(test)]
    pub fn with_sizes(&self, sizes: &HashMap<String, (i32, i32)>) -> Tree {
        let mut t = self.clone();
        t.walk_mut(&mut |id, c, r| {
            if let Some((nc, nr)) = sizes.get(id.as_str()) {
                *c = *nc;
                *r = *nr;
            }
        });
        t
    }

    /// The same slots (shape and sizes) holding `order`'s sessions, in leaf order.
    pub fn permuted(&self, order: &[String]) -> Tree {
        let mut t = self.clone();
        let mut it = order.iter();
        t.walk_mut(&mut |id, _, _| {
            if let Some(n) = it.next() {
                *id = n.clone();
            }
        });
        t
    }

    /// Fractions from the cell sizes (for a tab that moves and must keep its proportions).
    pub fn plan(&self) -> Plan {
        match self {
            Tree::Leaf { id, .. } => Plan::Leaf(id.clone()),
            Tree::Split { vertical, kids } => {
                let along: Vec<f32> = kids.iter().map(|k| k.extent()).map(|(c, r)| (if *vertical { c } else { r }).max(1) as f32).collect();
                let total: f32 = along.iter().sum();
                Plan::Split { vertical: *vertical, kids: kids.iter().zip(along).map(|(k, a)| (a / total, k.plan())).collect() }
            }
        }
    }

    /// After a split of `at` into `at` and `new` (siblings on the `vertical` axis): `new` takes `ratio` of the two
    /// panes' space along that axis. False when they are not siblings there.
    pub fn set_ratio(&mut self, at: &str, new: &str, vertical: bool, ratio: f32) -> bool {
        let Tree::Split { vertical: v, kids } = self else { return false };
        let pos = |id: &str| kids.iter().position(|k| matches!(k, Tree::Leaf { id: x, .. } if x == id));
        if *v == vertical {
            if let (Some(a), Some(n)) = (pos(at), pos(new)) {
                let get = |k: &Tree| if let Tree::Leaf { cols, rows, .. } = k { if vertical { *cols } else { *rows } } else { 0 };
                let total = get(&kids[a]) + get(&kids[n]);
                if total < 2 {
                    return false;
                }
                let nn = ((ratio.clamp(0.0, 1.0) * total as f32).round() as i32).clamp(1, total - 1);
                let set = |k: &mut Tree, x: i32| {
                    if let Tree::Leaf { cols, rows, .. } = k {
                        if vertical {
                            *cols = x
                        } else {
                            *rows = x
                        }
                    }
                };
                set(&mut kids[n], nn);
                set(&mut kids[a], total - nn);
                return true;
            }
        }
        kids.iter_mut().any(|k| k.set_ratio(at, new, vertical, ratio))
    }
}

/// iTerm2's own tree with new cell sizes for the sessions in `sizes`: `SetTabLayout` wants its structure back
/// exactly (a lone session's wrapper is `vertical: true`, for one), so only the sizes change.
pub fn resized(n: &p::SplitTreeNode, sizes: &HashMap<String, (i32, i32)>) -> p::SplitTreeNode {
    use p::split_tree_node::split_tree_link::Child;
    let mut out = n.clone();
    for l in &mut out.links {
        match &mut l.child {
            Some(Child::Session(s)) => {
                if let Some((c, r)) = s.unique_identifier.as_ref().and_then(|u| sizes.get(u)) {
                    s.grid_size = Some(p::Size { width: Some(*c), height: Some(*r) });
                }
            }
            Some(Child::Node(c)) => *c = resized(c, sizes),
            None => {}
        }
    }
    out
}

/// A wanted tree: leaves are session UUIDs, splits carry fractions along their axis.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Leaf(String),
    Split { vertical: bool, kids: Vec<(f32, Plan)> },
}

impl Plan {
    /// iTerm2's rules: no one-child splits, no split inside a split of the same axis (flattened, fractions
    /// multiplied); fractions that are missing or do not sum to 1 become proportional (equal if none).
    pub fn normalize(self) -> Plan {
        match self {
            Plan::Leaf(_) => self,
            Plan::Split { vertical, kids } => {
                let sum: f32 = kids.iter().map(|(f, _)| f.max(0.0)).sum();
                let n = kids.len().max(1) as f32;
                let mut flat: Vec<(f32, Plan)> = Vec::new();
                for (f, k) in kids {
                    let f = if sum > 0.0 && f > 0.0 { f / sum } else { 1.0 / n };
                    match k.normalize() {
                        Plan::Split { vertical: v, kids: inner } if v == vertical => flat.extend(inner.into_iter().map(|(g, c)| (f * g, c))),
                        other => flat.push((f, other)),
                    }
                }
                let s: f32 = flat.iter().map(|x| x.0).sum();
                if s > 0.0 {
                    flat.iter_mut().for_each(|x| x.0 /= s);
                }
                if flat.len() == 1 {
                    return flat.pop().map(|x| x.1).unwrap_or(Plan::Split { vertical, kids: vec![] });
                }
                Plan::Split { vertical, kids: flat }
            }
        }
    }

    pub fn leaves(&self) -> Vec<String> {
        match self {
            Plan::Leaf(id) => vec![id.clone()],
            Plan::Split { kids, .. } => kids.iter().flat_map(|(_, k)| k.leaves()).collect(),
        }
    }

    /// The node of `iterm2.apply_layout`'s spec.
    pub fn spec(&self) -> Value {
        match self {
            Plan::Leaf(id) => json!({ "session_id": id }),
            Plan::Split { vertical, kids } => json!({ "vertical": vertical, "children": kids.iter().map(|(_, k)| k.spec()).collect::<Vec<_>>() }),
        }
    }

    /// Same splits, axes and leaf order as `t` (fractions aside): then sizes alone reshape it.
    pub fn same_shape(&self, t: &Tree) -> bool {
        match (self, t) {
            (Plan::Leaf(a), Tree::Leaf { id, .. }) => a == id,
            (Plan::Split { vertical: a, kids: ka }, Tree::Split { vertical: b, kids: kb }) => {
                a == b && ka.len() == kb.len() && ka.iter().zip(kb).all(|((_, x), y)| x.same_shape(y))
            }
            _ => false,
        }
    }

    /// Every leaf's cell size when this plan fills `cols` × `rows`.
    pub fn sizes(&self, cols: i32, rows: i32, out: &mut HashMap<String, (i32, i32)>) {
        match self {
            Plan::Leaf(id) => {
                out.insert(id.clone(), (cols.max(1), rows.max(1)));
            }
            Plan::Split { vertical, kids } => {
                let fr: Vec<f32> = kids.iter().map(|k| k.0).collect();
                let shares = shares(if *vertical { cols } else { rows }, &fr);
                for ((_, k), s) in kids.iter().zip(shares) {
                    if *vertical {
                        k.sizes(s, rows, out)
                    } else {
                        k.sizes(cols, s, out)
                    }
                }
            }
        }
    }
}

/// `total` cells split by fractions, each at least 1, summing to `total` (largest remainder).
pub fn shares(total: i32, fr: &[f32]) -> Vec<i32> {
    let n = fr.len();
    if n == 0 {
        return vec![];
    }
    let sum: f32 = fr.iter().map(|f| f.max(0.0)).sum();
    let fr: Vec<f32> = if sum > 0.0 { fr.iter().map(|f| f.max(0.0) / sum).collect() } else { vec![1.0 / n as f32; n] };
    let exact: Vec<f32> = fr.iter().map(|f| f * total as f32).collect();
    let mut out: Vec<i32> = exact.iter().map(|x| (x.floor() as i32).max(1)).collect();
    let target = total.max(n as i32);
    let mut left = target - out.iter().sum::<i32>();
    // the biggest remainders take what is left; when the floors of 1 overshoot, the biggest give back
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| (exact[*b] - exact[*b].floor()).partial_cmp(&(exact[*a] - exact[*a].floor())).unwrap_or(std::cmp::Ordering::Equal));
    for i in order.iter().cycle().take(n * 4) {
        if left <= 0 {
            break;
        }
        out[*i] += 1;
        left -= 1;
    }
    while left < 0 {
        let Some(i) = (0..n).filter(|i| out[*i] > 1).max_by_key(|i| out[*i]) else { break };
        out[i] -= 1;
        left += 1;
    }
    out
}

/// `first` in their order, then the rest of `all` in theirs (an `Op::Order`).
pub fn reorder(all: &[String], first: &[String]) -> Vec<String> {
    let mut out: Vec<String> = first.iter().filter(|x| all.contains(x)).cloned().collect();
    out.extend(all.iter().filter(|x| !first.contains(x)).cloned());
    out
}

/// A window frame (points, as iTerm2 reports it: origin at the bottom left of the screen) resized so a pane of
/// `grid` cells drawn in `pane_pts` points holds `want` cells; the window's top-left corner stays put. iTerm2
/// spreads a window's growth over its panes in proportion, so the pane's share of the window scales the change.
pub fn frame_for_cells(win: (f64, f64, f64, f64), pane_pts: (f64, f64), grid: (i32, i32), want: (Option<u16>, Option<u16>)) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = win;
    let cw = if grid.0 > 0 { pane_pts.0 / grid.0 as f64 } else { 0.0 };
    let ch = if grid.1 > 0 { pane_pts.1 / grid.1 as f64 } else { 0.0 };
    let share = |part: f64, whole: f64| if whole > 0.0 && part > 0.0 { (part / whole).clamp(0.05, 1.0) } else { 1.0 };
    let dw = want.0.map(|c| (c as i32 - grid.0) as f64 * cw / share(pane_pts.0, w)).unwrap_or(0.0);
    let dh = want.1.map(|r| (r as i32 - grid.1) as f64 * ch / share(pane_pts.1, h)).unwrap_or(0.0);
    let (nw, nh) = ((w + dw).round().max(1.0), (h + dh).round().max(1.0));
    (x, y + h - nh, nw, nh)
}

/// Standard base64 (iTerm2's expression parser does not decode `\"` in string literals, so JSON travels so).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An iTerm2 expression string literal.
pub fn lit(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `#rrggbb` (or `rrggbb`) as the profile color dictionary iTerm2 stores (`Tab Color`).
pub fn color_dict(hex: &str) -> Option<Value> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map(|v| v as f64 / 255.0).ok();
    Some(json!({ "Red Component": c(0)?, "Green Component": c(2)?, "Blue Component": c(4)?, "Alpha Component": 1.0, "Color Space": "sRGB" }))
}

/// The picked set after an edit: `current` plus `add`, minus `remove`, without repeats.
pub fn pick(current: &[String], add: &[String], remove: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in current.iter().chain(add) {
        if !remove.contains(s) && !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str, c: i32, r: i32) -> Tree {
        Tree::Leaf { id: id.into(), cols: c, rows: r }
    }

    #[test]
    fn shares_sum_and_floor() {
        assert_eq!(shares(100, &[0.25, 0.5, 0.25]), vec![25, 50, 25]);
        assert_eq!(shares(10, &[1.0, 1.0, 1.0]).iter().sum::<i32>(), 10);
        assert_eq!(shares(3, &[0.0, 1.0, 0.0]), vec![1, 1, 1]);
        assert_eq!(shares(80, &[0.0, 0.0]), vec![40, 40]);
        let s = shares(81, &[1.0, 2.0, 1.0]);
        assert_eq!(s.iter().sum::<i32>(), 81);
        assert!((s[1] - 40).abs() <= 1, "{s:?}");
    }

    #[test]
    fn proto_round_trip_and_extent() {
        let t = Tree::Split { vertical: true, kids: vec![leaf("a", 40, 24), Tree::Split { vertical: false, kids: vec![leaf("b", 39, 12), leaf("c", 39, 11)] }] };
        assert_eq!(Tree::from_proto(&t.to_proto()), Some(t.clone()));
        assert_eq!(t.extent(), (79, 24));
        let mut m = HashMap::new();
        m.insert("b".to_string(), (39, 20));
        let r = resized(&t.to_proto(), &m);
        assert_eq!(Tree::from_proto(&r).unwrap().sizes()["b"], (39, 20));
        assert_eq!(r.vertical, Some(true));
        assert_eq!(t.leaves(), vec!["a", "b", "c"]);
        // a lone session comes wrapped and goes back wrapped
        let one = leaf("a", 80, 24);
        let pr = one.to_proto();
        assert_eq!(pr.links.len(), 1);
        assert_eq!(Tree::from_proto(&pr), Some(one));
    }

    #[test]
    fn plans_normalize_like_iterm2() {
        let nested = Plan::Split {
            vertical: true,
            kids: vec![(1.0, Plan::Leaf("a".into())), (1.0, Plan::Split { vertical: true, kids: vec![(1.0, Plan::Leaf("b".into())), (3.0, Plan::Leaf("c".into()))] })],
        }
        .normalize();
        let Plan::Split { vertical: true, kids } = &nested else { panic!("{nested:?}") };
        let fr: Vec<f32> = kids.iter().map(|k| k.0).collect();
        assert_eq!(kids.len(), 3);
        assert!((fr[0] - 0.5).abs() < 1e-6 && (fr[1] - 0.125).abs() < 1e-6 && (fr[2] - 0.375).abs() < 1e-6, "{fr:?}");
        let single = Plan::Split { vertical: false, kids: vec![(1.0, Plan::Leaf("a".into()))] }.normalize();
        assert_eq!(single, Plan::Leaf("a".into()));
        assert_eq!(nested.spec()["children"][2]["session_id"], "c");
    }

    #[test]
    fn plan_sizes_fill_the_tab() {
        let plan = Plan::Split { vertical: true, kids: vec![(0.25, Plan::Leaf("a".into())), (0.75, Plan::Split { vertical: false, kids: vec![(0.5, Plan::Leaf("b".into())), (0.5, Plan::Leaf("c".into()))] })] };
        let mut m = HashMap::new();
        plan.sizes(120, 40, &mut m);
        assert_eq!(m["a"], (30, 40));
        assert_eq!(m["b"], (90, 20));
        assert_eq!(m["c"], (90, 20));
        let t = Tree::Split { vertical: true, kids: vec![leaf("a", 60, 40), Tree::Split { vertical: false, kids: vec![leaf("b", 60, 20), leaf("c", 60, 20)] }] };
        assert!(plan.same_shape(&t));
        assert!(!Plan::Leaf("a".into()).same_shape(&t));
        let sized = t.with_sizes(&m);
        assert_eq!(sized.sizes()["a"], (30, 40));
        // a tree's own fractions come back as its plan
        let Plan::Split { kids, .. } = sized.plan() else { panic!() };
        assert!((kids[0].0 - 0.25).abs() < 1e-6);
    }

    #[test]
    fn ratio_after_a_split() {
        let mut t = Tree::Split { vertical: false, kids: vec![leaf("x", 80, 10), Tree::Split { vertical: true, kids: vec![leaf("a", 40, 14), leaf("n", 40, 14)] }] };
        assert!(t.set_ratio("a", "n", true, 0.25));
        assert_eq!(t.sizes()["n"], (20, 14));
        assert_eq!(t.sizes()["a"], (60, 14));
        assert!(!t.set_ratio("a", "n", false, 0.5), "not siblings on that axis");
    }

    #[test]
    fn orders_frames_and_picks() {
        let all: Vec<String> = ["1", "2", "3", "4"].map(String::from).to_vec();
        assert_eq!(reorder(&all, &["3".into(), "1".into()]), ["3", "1", "2", "4"]);
        let t = Tree::Split { vertical: true, kids: vec![leaf("a", 30, 10), leaf("b", 50, 10)] };
        assert_eq!(t.permuted(&["b".into(), "a".into()]).sizes()["b"], (30, 10));
        // 10×20-point cells, the pane the whole window: 80×24 → 100×30 grows it 200×120 and keeps its top
        assert_eq!(frame_for_cells((0.0, 500.0, 800.0, 480.0), (800.0, 480.0), (80, 24), (Some(100), Some(30))), (0.0, 380.0, 1000.0, 600.0));
        // a pane half the window's width gets half its growth: twice the change
        assert_eq!(frame_for_cells((0.0, 0.0, 800.0, 480.0), (400.0, 480.0), (40, 24), (Some(50), None)), (0.0, 0.0, 1000.0, 480.0));
        assert_eq!(frame_for_cells((5.0, 5.0, 100.0, 100.0), (80.0, 80.0), (8, 4), (None, None)), (5.0, 5.0, 100.0, 100.0));
        let cur: Vec<String> = vec!["a".into(), "b".into()];
        assert_eq!(pick(&cur, &["c".into(), "a".into()], &["b".into()]), ["a", "c"]);
    }

    #[test]
    fn encodings() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"{\"a\":1}"), "eyJhIjoxfQ==");
        assert_eq!(lit("a\"b\\c"), "\"a\\\"b\\\\c\"");
        let c = color_dict("#ff0080").unwrap();
        assert_eq!((c["Red Component"].as_f64(), c["Blue Component"].as_f64().map(|b| (b * 255.0).round())), (Some(1.0), Some(128.0)));
        assert!(color_dict("red").is_none());
    }
}
