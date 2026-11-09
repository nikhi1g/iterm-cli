//! One geometry language for panes in a tab and windows on a screen (ported from iterm_api's
//! `geometry.rs`): shares along a split's axis (`--weights`, `--ratio`, `--equalize`) computed over
//! the app's split tree ([`Layout`]), which the CLI sends back whole with `Op::SetLayout`; and the
//! tiling of rectangles (`--tile columns|rows|grid`). Pure.

use crate::app::{Id, Layout};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// How windows (or any rectangles) tile an area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tile {
    Columns,
    Rows,
    Grid,
}

impl Tile {
    pub fn parse(s: &str) -> Option<Tile> {
        Some(match s {
            "columns" | "cols" => Tile::Columns,
            "rows" => Tile::Rows,
            "grid" => Tile::Grid,
            _ => return None,
        })
    }
}

/// `--weights 1,1,2` / `--frame x,y,w,h` number lists.
pub fn numbers(s: &str) -> Result<Vec<f64>, String> {
    s.split(',')
        .map(|x| x.trim().parse::<f64>().ok().filter(|f| f.is_finite()).ok_or_else(|| format!("'{x}' is not a number")))
        .collect()
}

/// The split whose direct children include every pane of `ids`.
fn parent_of_all<'a>(l: &'a mut Layout, ids: &[Id]) -> Option<&'a mut Layout> {
    let direct = match &*l {
        Layout::Split { children, .. } => {
            ids.iter().all(|i| children.iter().any(|(_, c)| matches!(c, Layout::Pane(x) if x == i)))
        }
        Layout::Pane(_) => false,
    };
    if direct {
        return Some(l);
    }
    match l {
        Layout::Split { children, .. } => children.iter_mut().find_map(|(_, c)| parent_of_all(c, ids)),
        Layout::Pane(_) => None,
    }
}

/// Give the panes `ids` (direct children of one split) shares proportional to `weights`, within
/// the share they hold together; siblings outside keep theirs.
pub fn set_weights(l: &mut Layout, ids: &[Id], weights: &[f64]) -> Result<(), String> {
    if ids.len() != weights.len() || weights.iter().any(|w| !w.is_finite() || *w <= 0.0) {
        return Err(format!("--weights gives {} positive numbers for {} panes", weights.len(), ids.len()));
    }
    let Some(Layout::Split { children, .. }) = parent_of_all(l, ids) else {
        return Err("--weights: those panes do not share one split; place them with --stack first".into());
    };
    let held: f64 = children
        .iter()
        .filter(|(_, c)| matches!(c, Layout::Pane(x) if ids.contains(x)))
        .map(|(f, _)| *f as f64)
        .sum();
    let total: f64 = weights.iter().sum();
    for (f, c) in children.iter_mut() {
        if let Layout::Pane(x) = c {
            if let Some(i) = ids.iter().position(|i| i == x) {
                *f = (held * weights[i] / total) as f32;
            }
        }
    }
    Ok(())
}

/// The pane's share of its split becomes `ratio`; its siblings share the rest in proportion.
pub fn set_ratio(l: &mut Layout, id: Id, ratio: f64) -> Result<(), String> {
    let Some(Layout::Split { children, .. }) = parent_of_all(l, &[id]) else {
        return Err("--ratio: the pane is alone in its tab".into());
    };
    let others: f64 = children.iter().filter(|(_, c)| *c != Layout::Pane(id)).map(|(f, _)| *f as f64).sum();
    for (f, c) in children.iter_mut() {
        *f = if *c == Layout::Pane(id) {
            ratio as f32
        } else if others > 0.0 {
            (*f as f64 / others * (1.0 - ratio)) as f32
        } else {
            0.0
        };
    }
    Ok(())
}

/// Equal shares among the pane's siblings in its own split (not deeper or higher); false when
/// the pane is alone in its tab.
pub fn equalize_around(l: &mut Layout, id: Id) -> bool {
    let Some(Layout::Split { children, .. }) = parent_of_all(l, &[id]) else { return false };
    let k = children.len() as f32;
    children.iter_mut().for_each(|(f, _)| *f = 1.0 / k);
    true
}

/// A tab's panes (in reading order) as columns side by side, rows stacked, or a grid: ⌈√n⌉ columns, filled
/// left to right, top to bottom, the last column holding what's left. Equal shares throughout.
pub fn tiled(ids: &[Id], mode: Tile) -> Option<Layout> {
    let split = |vertical: bool, parts: Vec<Layout>| {
        let k = parts.len() as f32;
        if parts.len() == 1 { parts.into_iter().next().expect("one") } else { Layout::Split { vertical, children: parts.into_iter().map(|p| (1.0 / k, p)).collect() } }
    };
    let panes = || ids.iter().map(|i| Layout::Pane(*i)).collect::<Vec<_>>();
    Some(match (ids.len(), mode) {
        (0, _) => return None,
        (_, Tile::Columns) => split(true, panes()),
        (_, Tile::Rows) => split(false, panes()),
        (n, Tile::Grid) => {
            let cols = (n as f64).sqrt().ceil() as usize;
            let rows = n.div_ceil(cols);
            // row-major reading order into columns: column c holds panes c, c+cols, …
            let columns = (0..cols)
                .map(|c| (0..rows).filter_map(|r| ids.get(r * cols + c)).map(|i| Layout::Pane(*i)).collect::<Vec<_>>())
                .filter(|col| !col.is_empty())
                .map(|col| split(false, col))
                .collect();
            split(true, columns)
        }
    })
}

/// Where a spawner's next agent goes when no placement is given: its dock. The spawner keeps its
/// pane, a companion to its right (a status view, a shell) keeps the top, and its agents fill a row
/// of columns below the companion:
/// 1. the spawner has agent children in this tab (`kids`): right of the last one (reading order);
/// 2. else its right neighbour holds no agent (`agent(id)`): below that neighbour's last pane;
/// 3. else the spawner itself, to the right when `wide`, else down (the caller passes false for a
///    child in its parent's dock row, which starts its own row under itself).
///
/// Returns the pane to split and whether to split it to the right (false: down). O(panes).
pub fn dock(l: &Layout, me: Id, kids: &[Id], agent: &dyn Fn(Id) -> bool, wide: bool) -> (Id, bool) {
    let mut order = Vec::new();
    l.panes(&mut order);
    if let Some(last) = order.iter().rev().find(|p| kids.contains(p)) {
        return (*last, true);
    }
    if let Some(right) = right_of(l, me) {
        let mut there = Vec::new();
        right.panes(&mut there);
        if let (false, Some(last)) = (there.iter().any(|p| agent(*p)), there.last()) {
            return (*last, false);
        }
    }
    (me, wide)
}

/// The subtree right of `id`: the next child of the nearest side-by-side split above it that has
/// one.
fn right_of(l: &Layout, id: Id) -> Option<&Layout> {
    fn walk<'a>(l: &'a Layout, id: Id, found: &mut Option<&'a Layout>) -> bool {
        let Layout::Split { vertical, children } = l else { return *l == Layout::Pane(id) };
        let Some(i) = children.iter().position(|(_, c)| walk(c, id, found)) else { return false };
        if found.is_none() && *vertical {
            *found = children.get(i + 1).map(|(_, c)| c);
        }
        true
    }
    let mut found = None;
    walk(l, id, &mut found);
    found
}

/// The panes of `members` that are direct children of the split holding `pane` (in order), `pane`
/// among them when it is a member: a dock row.
pub fn row_of(l: &Layout, pane: Id, members: &[Id]) -> Vec<Id> {
    fn holder(l: &Layout, id: Id) -> Option<&[(f32, Layout)]> {
        let Layout::Split { children, .. } = l else { return None };
        if children.iter().any(|(_, c)| *c == Layout::Pane(id)) {
            return Some(children);
        }
        children.iter().find_map(|(_, c)| holder(c, id))
    }
    holder(l, pane)
        .map(|cs| cs.iter().filter_map(|(_, c)| match c {
            Layout::Pane(x) if members.contains(x) => Some(*x),
            _ => None,
        }).collect())
        .unwrap_or_default()
}

/// Equal shares in every split.
pub fn equalize(l: &mut Layout) {
    if let Layout::Split { children, .. } = l {
        let k = children.len() as f32;
        for (f, c) in children.iter_mut() {
            *f = 1.0 / k;
            equalize(c);
        }
    }
}

/// `n` rectangles tiling `area`, in reading order; `weights` (one per rectangle) apply along the
/// axis for columns and rows.
pub fn tile(area: Frame, n: usize, mode: Tile, weights: Option<&[f64]>) -> Vec<Frame> {
    if n == 0 {
        return vec![];
    }
    let (cols, rows) = match mode {
        Tile::Columns => (n, 1),
        Tile::Rows => (1, n),
        Tile::Grid => {
            let c = (n as f64).sqrt().ceil() as usize;
            (c, n.div_ceil(c))
        }
    };
    let shares = |k: usize| -> Vec<f64> {
        match weights.filter(|w| w.len() == k && mode != Tile::Grid) {
            Some(w) => {
                let s: f64 = w.iter().sum();
                w.iter().map(|x| x / s).collect()
            }
            None => vec![1.0 / k as f64; k],
        }
    };
    let mut out = Vec::with_capacity(n);
    let (cs, rs) = (shares(cols), shares(rows));
    let mut y = area.y;
    for rh in &rs {
        let h = rh * area.height;
        let mut x = area.x;
        for cw in &cs {
            let w = cw * area.width;
            if out.len() < n {
                out.push(Frame { x: x.round(), y: y.round(), width: w.round(), height: h.round() });
            }
            x += w;
        }
        y += h;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fr(l: &Layout) -> Vec<f32> {
        match l {
            Layout::Split { children, .. } => children.iter().map(|(f, _)| (f * 1000.0).round() / 1000.0).collect(),
            Layout::Pane(_) => vec![],
        }
    }

    #[test]
    fn weights_ratio_equalize_and_tiles() {
        let ids: Vec<Id> = (1..=7).map(Id).collect();
        let mut t = Layout::Split { vertical: false, children: ids.iter().map(|i| (1.0 / 7.0, Layout::Pane(*i))).collect() };
        set_weights(&mut t, &ids, &[1.0, 1.0, 2.0, 2.0, 2.0, 1.0, 1.0]).unwrap();
        assert_eq!(fr(&t), vec![0.1, 0.1, 0.2, 0.2, 0.2, 0.1, 0.1]);
        assert!(set_weights(&mut t, &ids, &[1.0]).is_err());
        set_ratio(&mut t, Id(1), 0.4).unwrap();
        assert_eq!(fr(&t)[0], 0.4);
        assert!((fr(&t).iter().sum::<f32>() - 1.0).abs() < 0.01);
        equalize(&mut t);
        assert_eq!(fr(&t)[3], 0.143);
        assert!(set_ratio(&mut Layout::Pane(Id(1)), Id(1), 0.5).is_err());
        let f = tile(Frame { x: 0.0, y: 25.0, width: 1440.0, height: 875.0 }, 2, Tile::Columns, None);
        assert_eq!(f[1], Frame { x: 720.0, y: 25.0, width: 720.0, height: 875.0 });
        assert_eq!(tile(Frame { x: 0.0, y: 0.0, width: 900.0, height: 900.0 }, 5, Tile::Grid, None).len(), 5);
    }

    fn p(i: u64) -> Layout {
        Layout::Pane(Id(i))
    }
    fn cols(cs: Vec<Layout>) -> Layout {
        Layout::Split { vertical: true, children: cs.into_iter().map(|c| (0.5, c)).collect() }
    }
    fn rows(cs: Vec<Layout>) -> Layout {
        Layout::Split { vertical: false, children: cs.into_iter().map(|c| (0.5, c)).collect() }
    }

    #[test]
    fn the_dock_fills_a_row_under_the_companion() {
        let none = |_: Id| false;
        let agents = |id: Id| id.0 >= 10;
        // [lead 1 | status 2]: the first agent goes below the status pane
        let t = cols(vec![p(1), p(2)]);
        assert_eq!(dock(&t, Id(1), &[], &none, true), (Id(2), false));
        // [lead | [status / a10]]: the next one right of the last child, and so on along the row
        let t = cols(vec![p(1), rows(vec![p(2), p(10)])]);
        assert_eq!(dock(&t, Id(1), &[Id(10)], &agents, true), (Id(10), true));
        let t = cols(vec![p(1), rows(vec![p(2), cols(vec![p(10), p(11), p(12)])])]);
        assert_eq!(dock(&t, Id(1), &[Id(10), Id(11), Id(12)], &agents, true), (Id(12), true));
        assert_eq!(row_of(&t, Id(12), &[Id(10), Id(11), Id(12)]), [Id(10), Id(11), Id(12)]);
        assert_eq!(row_of(&t, Id(10), &[Id(10), Id(12)]), [Id(10), Id(12)], "members only");
        assert_eq!(row_of(&t, Id(2), &[Id(10)]), Vec::<Id>::new(), "the status pane's split holds no member pane");
        // a right neighbour that runs an agent is not a companion: split the spawner, the wider way
        let t = cols(vec![p(1), p(10)]);
        assert_eq!(dock(&t, Id(1), &[], &agents, true), (Id(1), true));
        assert_eq!(dock(&t, Id(1), &[], &agents, false), (Id(1), false));
        // alone in its tab
        assert_eq!(dock(&p(1), Id(1), &[], &none, true), (Id(1), true));
        // a child in a dock row (the rightmost: no right neighbour) starts its own row under itself
        let t = cols(vec![p(1), rows(vec![p(2), cols(vec![p(10), p(11)])])]);
        assert_eq!(dock(&t, Id(11), &[], &agents, false), (Id(11), false));
        // the nearest side-by-side split decides: a10's right neighbour is a11, an agent
        assert_eq!(dock(&t, Id(10), &[], &agents, false), (Id(10), false));
        // the companion is a column of panes: below its last (bottom-right) pane
        let t = cols(vec![p(1), rows(vec![p(2), p(3)])]);
        assert_eq!(dock(&t, Id(1), &[], &none, true), (Id(3), false));
    }
}
