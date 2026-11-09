//! Selector evaluation over one snapshot: the app's nodes as a flat list in tree order with
//! parents, cell rectangles for neighbors, focus, the agent overlay and the fields `--where`
//! filters on. Pure: `exec` builds the Model from one snapshot; this resolves a typed selector
//! (`grammar::Expr`) to node indices in model order, so every result is deterministic. Ported from
//! iterm_api's `select.rs`, with the scans it did per atom replaced by indexes built once per
//! command: ID → node, name → nodes, kind → nodes, and each node's subtree as an index range (the
//! list is in pre-order), so descendants are a binary search.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::geometry::Frame;
use crate::grammar::{Atom, Dir, Expr, Kind, Match, Pick, Rel};

#[derive(Debug, Clone)]
pub struct MNode {
    pub kind: Kind,
    /// The display handle (`p12`).
    pub id: String,
    /// The bare ID (`12`), which operations take.
    pub raw: String,
    pub name: String,
    pub parent: Option<usize>,
    /// Panes: cells within their tab; windows: points on screen.
    pub rect: Option<Frame>,
    /// Creation rank for `recent:N` (higher is newer).
    pub rank: f64,
    /// `--where` fields: state, kind, cwd, group, title, job, exit, and var:K.
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    /// In pre-order (every subtree is contiguous), the app first.
    pub nodes: Vec<MNode>,
    /// The caller's pane.
    pub me: Option<usize>,
    /// Focused window, vtab, tab and pane.
    pub focused: Vec<usize>,
    /// Agent parent of each pane (by node index).
    pub agent_parent: HashMap<usize, usize>,
    /// Batch bindings: name → raw IDs.
    pub binds: HashMap<String, Vec<String>>,
    ix: Ix,
}

#[derive(Debug, Clone, Default)]
struct Ix {
    by_id: HashMap<String, usize>,
    by_name: HashMap<String, Vec<usize>>,
    kinds: HashMap<Kind, Vec<usize>>,
    /// One past the last node of each node's subtree.
    end: Vec<usize>,
    children: Vec<Vec<usize>>,
}

type R<T> = std::result::Result<T, String>;

impl Model {
    /// Builds the indexes; call once after `nodes` is filled (and after any change to them).
    pub fn index(&mut self) {
        let n = self.nodes.len();
        let mut ix = Ix { end: (0..n).map(|i| i + 1).collect(), children: vec![Vec::new(); n], ..Ix::default() };
        for (i, node) in self.nodes.iter().enumerate() {
            ix.by_id.insert(node.id.clone(), i);
            ix.by_id.insert(node.raw.clone(), i);
            if !node.name.is_empty() {
                ix.by_name.entry(node.name.clone()).or_default().push(i);
            }
            // an agent answers to its own name too (tree's `as=`), whatever its pane is called by now
            if let Some(a) = node.fields.get("var:agent.name").filter(|a| !a.is_empty() && **a != node.name) {
                ix.by_name.entry(a.clone()).or_default().push(i);
            }
            ix.kinds.entry(node.kind).or_default().push(i);
            if let Some(p) = node.parent {
                ix.children[p].push(i);
            }
        }
        // pre-order: a node's subtree ends where its last descendant ends
        for i in (0..n).rev() {
            if let Some(p) = self.nodes[i].parent {
                ix.end[p] = ix.end[p].max(ix.end[i]);
            }
        }
        self.ix = ix;
    }

    pub fn by_raw(&self, raw: &str) -> Option<usize> {
        self.ix.by_id.get(raw).copied()
    }

    fn of_kind(&self, k: Option<Kind>) -> Vec<usize> {
        match k {
            Some(k) => self.ix.kinds.get(&k).cloned().unwrap_or_default(),
            None => (1..self.nodes.len()).filter(|i| self.nodes[*i].kind != Kind::App).collect(),
        }
    }

    /// The node itself or its nearest ancestor of kind `k` (the tree is 5 levels deep).
    pub fn up(&self, i: usize, k: Kind) -> Option<usize> {
        let mut cur = Some(i);
        while let Some(c) = cur {
            if self.nodes[c].kind == k {
                return Some(c);
            }
            cur = self.nodes[c].parent;
        }
        None
    }

    /// `i` is `anc` or inside its subtree: O(1).
    pub fn is_under(&self, i: usize, anc: usize) -> bool {
        anc <= i && i < self.ix.end[anc]
    }

    /// The node and everything under it, as indices.
    pub fn subtree(&self, i: usize) -> std::ops::Range<usize> {
        i..self.ix.end[i]
    }

    /// Which nodes lie under one of `roots` (`strict`: a root counts only when it is under another): O(nodes).
    pub fn covered(&self, roots: &[usize], strict: bool) -> Vec<bool> {
        let mut sorted = roots.to_vec();
        sorted.sort_unstable();
        let mut out = vec![false; self.nodes.len()];
        let mut upto = 0;
        for r in sorted {
            let end = self.ix.end[r];
            out[(r + usize::from(strict)).max(upto).min(end)..end].fill(true);
            upto = upto.max(end);
        }
        out
    }

    /// Some index of the sorted `list` is `i` or under it: a binary search.
    pub fn any_under(&self, list: &[usize], i: usize) -> bool {
        list.get(list.partition_point(|x| *x < i)).is_some_and(|x| *x < self.ix.end[i])
    }

    /// Nodes of kind `k` strictly inside `i`'s subtree, in order: a binary search.
    fn descendants(&self, i: usize, k: Kind) -> &[usize] {
        let Some(list) = self.ix.kinds.get(&k) else { return &[] };
        let lo = list.partition_point(|x| *x <= i);
        let hi = list.partition_point(|x| *x < self.ix.end[i]);
        &list[lo..hi.max(lo)]
    }

    /// Nodes of kind `k` related to `i`: itself, its ancestor of that kind, or its descendants.
    pub fn project(&self, i: usize, k: Option<Kind>) -> Vec<usize> {
        let Some(k) = k else { return vec![i] };
        if self.nodes[i].kind == k {
            return vec![i];
        }
        if let Some(u) = self.up(i, k) {
            return vec![u];
        }
        self.descendants(i, k).to_vec()
    }

    pub fn children(&self, i: usize) -> &[usize] {
        &self.ix.children[i]
    }

    /// The container indices count within: a pane's tab, a tab's vtab, a vtab's window.
    fn container(&self, i: usize) -> Option<usize> {
        let k = self.nodes[i].kind.parent()?;
        self.nodes[i].parent.and_then(|p| self.up(p, k))
    }

    fn sorted(&self, mut v: Vec<usize>) -> Vec<usize> {
        v.sort_unstable();
        v.dedup();
        v
    }
}

/// What a selector's indices count within: `--in`, else the caller's own container.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub within: Option<Vec<usize>>,
}

pub fn eval(m: &Model, e: &Expr, k: Option<Kind>, ctx: &Ctx) -> R<Vec<usize>> {
    Ok(match e {
        Expr::Atom(a) => m.sorted(atom(m, a, k, ctx)?),
        Expr::Or(v) => {
            let mut out = Vec::new();
            for x in v {
                out.extend(eval(m, x, k, ctx)?);
            }
            m.sorted(out)
        }
        Expr::And(v) => {
            let mut out: Option<HashSet<usize>> = None;
            for x in v {
                let s: HashSet<usize> = eval(m, x, k, ctx)?.into_iter().collect();
                out = Some(match out {
                    None => s,
                    Some(o) => o.intersection(&s).copied().collect(),
                });
            }
            m.sorted(out.unwrap_or_default().into_iter().collect())
        }
        Expr::Not(x) => {
            // a negated word that names nothing excludes nothing
            let minus: HashSet<usize> = match eval(m, x, k, ctx) {
                Ok(v) => v.into_iter().collect(),
                Err(_) if matches!(**x, Expr::Atom(Atom::Word(_))) => HashSet::new(),
                Err(e) => return Err(e),
            };
            m.of_kind(k)
                .into_iter()
                .filter(|i| !minus.contains(i))
                .collect()
        }
    })
}

fn field_of<'a>(n: &'a MNode, key: &str) -> Option<&'a str> {
    match key {
        "id" => Some(&n.id),
        "name" => Some(&n.name),
        "group" => n.fields.get("groups").map(String::as_str),
        _ => n.fields.get(key).map(String::as_str),
    }
}

fn test_field(n: &MNode, key: &str, mat: &Match) -> bool {
    match (field_of(n, key), mat) {
        (Some(v), m) if key == "group" || key == "groups" => {
            let groups: Vec<&str> = v.split(',').filter(|g| !g.is_empty()).collect();
            match m {
                Match::Ne(x) => !groups.iter().any(|g| g.eq_ignore_ascii_case(x)),
                _ => groups.iter().any(|g| m.test(g)),
            }
        }
        (Some(v), m) => m.test(v),
        (None, Match::Ne(_)) => true,
        (None, _) => false,
    }
}

fn word(m: &Model, w: &str, k: Option<Kind>) -> R<Vec<usize>> {
    let fits = |i: &usize| k.map_or(m.nodes[*i].kind != Kind::App, |k| m.nodes[*i].kind == k);
    if let Some(i) = m.ix.by_id.get(w).copied().filter(fits) {
        return Ok(vec![i]);
    }
    let named: Vec<usize> = m.ix.by_name.get(w).map(|v| v.iter().copied().filter(fits).collect()).unwrap_or_default();
    if !named.is_empty() {
        // a name your own subtree uses wins
        if named.len() > 1 {
            if let Some(me) = m.me {
                let mine: Vec<usize> = named
                    .iter()
                    .copied()
                    .filter(|i| is_agent_desc(m, *i, me))
                    .collect();
                if mine.len() == 1 {
                    return Ok(mine);
                }
            }
        }
        return Ok(named);
    }
    let min = if k == Some(Kind::Pane) { 4 } else { 2 };
    let up = w.to_uppercase();
    // the rare miss: an ID prefix, a scan of one kind
    let pre: Vec<usize> = m
        .of_kind(k)
        .into_iter()
        .filter(|i| {
            w.chars().count() >= min
                && (m.nodes[*i].id.to_uppercase().starts_with(&up)
                    || m.nodes[*i].raw.to_uppercase().starts_with(&up))
        })
        .collect();
    match pre.len() {
        1 => Ok(pre),
        0 => Err(format!(
            "no {} '{w}' (an ID, a unique prefix or a name)",
            kind_name(k)
        )),
        n => Err(format!(
            "'{w}' matches {n} {}s; pass more characters",
            kind_name(k)
        )),
    }
}

fn is_agent_desc(m: &Model, i: usize, anc: usize) -> bool {
    let mut cur = m.agent_parent.get(&i).copied();
    let mut steps = 0;
    while let Some(c) = cur {
        if c == anc {
            return true;
        }
        steps += 1;
        if steps > 64 {
            return false;
        }
        cur = m.agent_parent.get(&c).copied();
    }
    false
}

pub fn kind_name(k: Option<Kind>) -> &'static str {
    match k {
        Some(Kind::App) => "app",
        Some(Kind::Window) => "window",
        Some(Kind::VTab) => "vertical tab",
        Some(Kind::Tab) => "tab",
        Some(Kind::Pane) => "pane",
        None => "node",
    }
}

fn pick(list: &[usize], p: &Pick) -> Vec<usize> {
    let n = list.len() as i64;
    let at = |i: i64| -> Option<usize> {
        let ix = if i < 0 { n + i } else { i - 1 };
        (0..n).contains(&ix).then(|| list[ix as usize])
    };
    match p {
        Pick::One(i) => at(*i).into_iter().collect(),
        // only what the list can hold: a span of 99999999999 costs what the list costs
        Pick::Span(a, b) => ((*a).max(-n)..=(*b).min(n)).filter_map(at).collect(),
        Pick::Odd => list.iter().step_by(2).copied().collect(),
        Pick::Even => list.iter().skip(1).step_by(2).copied().collect(),
        Pick::Last => list.last().copied().into_iter().collect(),
        Pick::Part(k, parts) => {
            // equal contiguous parts; the earlier parts take the remainder
            let len = list.len();
            let (base, extra) = (len / parts, len % parts);
            let start = (0..k - 1)
                .map(|j| base + usize::from(j < extra))
                .sum::<usize>();
            let size = base + usize::from(k - 1 < extra);
            list[start.min(len)..(start + size).min(len)].to_vec()
        }
    }
}

/// Split `list` into `parts` contiguous equal parts (the `--groups N` partition).
pub fn parts(list: &[usize], n: usize) -> Vec<Vec<usize>> {
    (1..=n)
        .map(|k| pick(list, &Pick::Part(k, n)))
        .filter(|p| !p.is_empty())
        .collect()
}

/// The nearest node of the same kind on side `d` of `x` that overlaps it across the axis.
pub fn neighbor(m: &Model, x: usize, d: Dir, all: bool) -> Vec<usize> {
    let Some(r) = m.nodes[x].rect else {
        return vec![];
    };
    // a pane's neighbors are in its tab: that tab's panes, never every pane
    let same: Vec<usize> = match m.nodes[x].parent.filter(|_| m.nodes[x].kind == Kind::Pane) {
        Some(tab) => m.children(tab).iter().copied().filter(|i| *i != x && m.nodes[*i].kind == Kind::Pane).collect(),
        None => m.of_kind(Some(m.nodes[x].kind)).into_iter().filter(|i| *i != x).collect(),
    };
    let eps = 0.5;
    let mut hits: Vec<(f64, usize)> = Vec::new();
    for i in same {
        let Some(o) = m.nodes[i].rect else { continue };
        let overlap_y = o.y < r.y + r.height - eps && o.y + o.height > r.y + eps;
        let overlap_x = o.x < r.x + r.width - eps && o.x + o.width > r.x + eps;
        let dist = match d {
            Dir::Right if overlap_y && o.x >= r.x + r.width - eps => o.x - (r.x + r.width),
            Dir::Left if overlap_y && o.x + o.width <= r.x + eps => r.x - (o.x + o.width),
            Dir::Down if overlap_x && o.y >= r.y + r.height - eps => o.y - (r.y + r.height),
            Dir::Up if overlap_x && o.y + o.height <= r.y + eps => r.y - (o.y + o.height),
            _ => continue,
        };
        hits.push((dist, i));
    }
    hits.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    if all {
        let best = hits.first().map(|h| h.0).unwrap_or(0.0);
        return hits
            .into_iter()
            .filter(|h| h.0 <= best + eps)
            .map(|h| h.1)
            .collect();
    }
    hits.into_iter().take(1).map(|h| h.1).collect()
}

fn rel_base(m: &Model, of: &Option<Box<Atom>>, k: Option<Kind>, ctx: &Ctx) -> R<Vec<usize>> {
    match of {
        Some(a) => atom(m, a, k.or(Some(Kind::Pane)), ctx),
        None => {
            m.me.map(|i| vec![i])
                .ok_or_else(|| "no caller identity (@ needs your pane)".to_string())
        }
    }
}

fn atom(m: &Model, a: &Atom, k: Option<Kind>, ctx: &Ctx) -> R<Vec<usize>> {
    let cands = || m.of_kind(k);
    Ok(match a {
        Atom::All => cands(),
        Atom::Word(w) => word(m, w, k)?,
        Atom::Glob(g) => cands()
            .into_iter()
            .filter(|i| {
                crate::grammar::glob(g, &m.nodes[*i].name)
                    || crate::grammar::glob(g, &m.nodes[*i].id)
            })
            .collect(),
        Atom::Field(key, mat) => cands()
            .into_iter()
            .filter(|i| test_field(&m.nodes[*i], key, mat))
            .collect(),
        Atom::Var(key, mat) => {
            let key = format!("var:{key}");
            cands().into_iter().filter(|i| test_field(&m.nodes[*i], &key, mat)).collect()
        }
        Atom::Env(key, mat) => {
            let key = format!("env:{key}");
            cands().into_iter().filter(|i| test_field(&m.nodes[*i], &key, mat)).collect()
        }
        Atom::State(s) => {
            let s = s.to_lowercase();
            cands()
                .into_iter()
                .filter(|i| {
                    let n = &m.nodes[*i];
                    match s.as_str() {
                        "focused" => m.focused.contains(i),
                        "active" => {
                            n.fields.get("active").map(|v| v == "yes").unwrap_or(false)
                                || m.focused.contains(i)
                        }
                        "attention" => matches!(
                            n.fields.get("state").map(String::as_str),
                            Some("MENU") | Some("TRUST")
                        ),
                        other => n
                            .fields
                            .get("state")
                            .map(|v| v.eq_ignore_ascii_case(other))
                            .unwrap_or(false),
                    }
                })
                .collect()
        }
        Atom::Recent(n) => {
            let mut v = cands();
            v.sort_by(|a, b| {
                m.nodes[*b]
                    .rank
                    .partial_cmp(&m.nodes[*a].rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            v.truncate(*n);
            v
        }
        Atom::Bind(name) => {
            let raws = m.binds.get(name).ok_or_else(|| {
                format!("${name} is not bound (an earlier batch line needs --as {name})")
            })?;
            raws.iter()
                .filter_map(|r| m.by_raw(r))
                .flat_map(|i| m.project(i, k))
                .collect()
        }
        Atom::Path(steps) => {
            let mut cur: Vec<usize> = m.of_kind(Some(Kind::App));
            for (sk, v) in steps {
                let mut next = Vec::new();
                for c in &cur {
                    let kids: Vec<usize> = m.descendants(*c, *sk).to_vec();
                    // indices count among the direct members of this container
                    let direct: Vec<usize> = kids
                        .iter()
                        .copied()
                        .filter(|i| {
                            m.nodes[*i]
                                .parent
                                .map(|p| m.up(p, m.nodes[*c].kind) == Some(*c))
                                .unwrap_or(false)
                        })
                        .collect();
                    let list = if direct.is_empty() {
                        kids.clone()
                    } else {
                        direct
                    };
                    match v.parse::<i64>() {
                        Ok(ix) => next.extend(pick(&list, &Pick::One(ix))),
                        Err(_) => next.extend(kids.iter().copied().filter(|i| {
                            m.nodes[*i].id == *v || m.nodes[*i].raw == *v || m.nodes[*i].name == *v
                        })),
                    }
                }
                if next.is_empty() {
                    return Err(format!("path step {}/{v} names nothing", sk.letter()));
                }
                cur = next;
            }
            cur.into_iter().flat_map(|i| m.project(i, k)).collect()
        }
        Atom::Index(p) => {
            let cands = cands();
            // group by container; count within --in, else within your own container
            let mut groups: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
            let mut at: HashMap<Option<usize>, usize> = HashMap::new();
            for i in cands {
                let c = m.container(i);
                let g = *at.entry(c).or_insert_with(|| {
                    groups.push((c, Vec::new()));
                    groups.len() - 1
                });
                groups[g].1.push(i);
            }
            // a node is under itself and kinds never nest, so this is every container inside --in or equal to it
            let inside = ctx.within.as_ref().map(|w| m.covered(w, false));
            let keep = |c: Option<usize>| -> bool {
                match (&inside, c, m.me) {
                    (Some(w), Some(c), _) => w[c],
                    (Some(_), None, _) => true,
                    (None, Some(c), Some(me)) => m.is_under(me, c),
                    _ => true,
                }
            };
            let chosen: Vec<&(Option<usize>, Vec<usize>)> =
                groups.iter().filter(|g| keep(g.0)).collect();
            let chosen = if chosen.is_empty() {
                groups.iter().collect()
            } else {
                chosen
            };
            chosen.into_iter().flat_map(|g| pick(&g.1, p)).collect()
        }
        Atom::Neighbor(d) => {
            let me = m.me.ok_or("neighbor: needs your pane")?;
            let base = m.project(me, Some(k.unwrap_or(Kind::Pane)));
            base.into_iter()
                .flat_map(|b| neighbor(m, b, *d, true))
                .collect()
        }
        Atom::Rel(rel, of) => {
            let want = k.unwrap_or(Kind::Pane);
            // @focused is the human's focus: it needs no caller identity
            if *rel == Rel::Focused && of.is_none() {
                return Ok(m.focused.iter().copied().filter(|i| m.nodes[*i].kind == want).collect());
            }
            let base = rel_base(m, of, k, ctx)?;
            match rel {
                Rel::Me => base
                    .into_iter()
                    .flat_map(|b| m.project(b, Some(want)))
                    .collect(),
                Rel::Focused => m
                    .focused
                    .iter()
                    .copied()
                    .filter(|i| m.nodes[*i].kind == want)
                    .collect(),
                Rel::Parent => base
                    .into_iter()
                    .filter_map(|b| m.agent_parent.get(&b).copied())
                    .flat_map(|p| m.project(p, Some(want)))
                    .collect(),
                Rel::Right | Rel::Left | Rel::Above | Rel::Below => {
                    let d = match rel {
                        Rel::Right => Dir::Right,
                        Rel::Left => Dir::Left,
                        Rel::Above => Dir::Up,
                        _ => Dir::Down,
                    };
                    let geo = if want == Kind::Window {
                        Kind::Window
                    } else {
                        Kind::Pane
                    };
                    let out: Vec<usize> = base
                        .into_iter()
                        .flat_map(|b| m.project(b, Some(geo)))
                        .flat_map(|b| neighbor(m, b, d, false))
                        .flat_map(|n| m.project(n, Some(want)))
                        .collect();
                    if out.is_empty() {
                        return Err(format!(
                            "nothing {} of {}",
                            match d {
                                Dir::Right => "right",
                                Dir::Left => "left",
                                Dir::Up => "above",
                                Dir::Down => "below",
                            },
                            if of.is_some() { "it" } else { "you" }
                        ));
                    }
                    out
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::selector;

    fn node(kind: Kind, id: &str, name: &str, parent: Option<usize>, rect: Option<Frame>) -> MNode {
        MNode {
            kind,
            id: id.into(),
            raw: id.into(),
            name: name.into(),
            parent,
            rect,
            rank: 0.0,
            fields: BTreeMap::new(),
        }
    }

    fn model() -> Model {
        let f = |x: f64, w: f64| {
            Some(Frame {
                x,
                y: 0.0,
                width: w,
                height: 50.0,
            })
        };
        let mut m = Model::default();
        m.nodes.push(node(Kind::App, "app", "", None, None));
        m.nodes.push(node(Kind::Window, "w1", "", Some(0), None));
        m.nodes.push(node(Kind::VTab, "v1", "", Some(1), None));
        m.nodes.push(node(Kind::Tab, "t1", "", Some(2), None));
        m.nodes
            .push(node(Kind::Pane, "AAAA1111", "lead", Some(3), f(0.0, 100.0)));
        m.nodes
            .push(node(Kind::Pane, "BBBB2222", "w1", Some(3), f(100.0, 50.0)));
        m.nodes
            .push(node(Kind::Pane, "CCCC3333", "w2", Some(3), f(150.0, 50.0)));
        m.nodes.push(node(Kind::Tab, "t2", "", Some(2), None));
        m.nodes.push(node(Kind::Pane, "DDDD4444", "other", Some(7), f(0.0, 100.0)));
        m.nodes[5].fields.insert("state".into(), "BUSY".into());
        m.nodes[6].fields.insert("state".into(), "READY".into());
        m.nodes[6].rank = 2.0;
        m.me = Some(4);
        m.agent_parent.insert(5, 4);
        m.agent_parent.insert(6, 4);
        m.index();
        m
    }

    fn run(m: &Model, s: &str, k: Kind) -> Vec<String> {
        eval(m, &selector(s).unwrap(), Some(k), &Ctx::default())
            .unwrap()
            .into_iter()
            .map(|i| m.nodes[i].id.clone())
            .collect()
    }

    #[test]
    fn atoms_and_operators_resolve_against_one_model() {
        let m = model();
        assert_eq!(run(&m, "@right", Kind::Pane), vec!["BBBB2222"]);
        assert_eq!(run(&m, "@right:w1", Kind::Pane), vec!["CCCC3333"]);
        assert_eq!(run(&m, "@me", Kind::Tab), vec!["t1"]);
        assert_eq!(run(&m, "@parent:w2", Kind::Pane), vec!["AAAA1111"]);
        assert_eq!(run(&m, "2-3", Kind::Pane), vec!["BBBB2222", "CCCC3333"]);
        assert_eq!(run(&m, "half:1", Kind::Pane), vec!["AAAA1111", "BBBB2222"]);
        assert_eq!(
            run(&m, "w* and not state:busy", Kind::Pane),
            vec!["CCCC3333"]
        );
        assert_eq!(
            run(&m, "lead, w2", Kind::Pane),
            vec!["AAAA1111", "CCCC3333"]
        );
        assert_eq!(run(&m, "BBBB", Kind::Pane), vec!["BBBB2222"]);
        assert_eq!(run(&m, "recent:1", Kind::Pane), vec!["CCCC3333"]);
        assert_eq!(run(&m, "/w/1/t/1/p/3", Kind::Pane), vec!["CCCC3333"]);
        assert_eq!(run(&m, "/w/1/v/1/t/2/p/1", Kind::Pane), vec!["DDDD4444"]);
        // tabs count within the vertical tab
        assert_eq!(run(&m, "2", Kind::Tab), vec!["t2"]);
        assert_eq!(run(&m, "@me", Kind::VTab), vec!["v1"]);
        assert_eq!(run(&m, "neighbor:right", Kind::Pane), vec!["BBBB2222"]);
        assert_eq!(
            run(&m, "state!=BUSY", Kind::Pane),
            vec!["AAAA1111", "CCCC3333", "DDDD4444"]
        );
        assert!(eval(
            &m,
            &selector("nobody").unwrap(),
            Some(Kind::Pane),
            &Ctx::default()
        )
        .is_err());
        assert!(eval(
            &m,
            &selector("@left").unwrap(),
            Some(Kind::Pane),
            &Ctx::default()
        )
        .is_err());
        assert_eq!(parts(&[1, 2, 3, 4, 5], 2), vec![vec![1, 2, 3], vec![4, 5]]);
    }

    // regression: `1-99999999999` walked the whole span (24 s for 10 panes)
    #[test]
    fn a_span_past_the_list_stops_at_the_list() {
        assert_eq!(pick(&[7, 8, 9], &Pick::Span(2, 99_999_999_999)), vec![8, 9]);
        assert_eq!(pick(&[7, 8, 9], &Pick::Span(5, i64::MAX)), Vec::<usize>::new());
    }

    #[test]
    fn covered_marks_subtrees() {
        let m = model();
        let names = |v: Vec<bool>| (0..v.len()).filter(|i| v[*i]).collect::<Vec<_>>();
        assert_eq!(names(m.covered(&[3], false)), [3, 4, 5, 6]);
        assert_eq!(names(m.covered(&[3], true)), [4, 5, 6]);
        // a root inside another root counts as under it, strict or not
        assert_eq!(names(m.covered(&[4, 3], true)), [4, 5, 6]);
        assert_eq!(names(m.covered(&[8, 3], true)), [4, 5, 6]);
        assert!(m.any_under(&[5, 8], 3) && !m.any_under(&[8], 3) && m.any_under(&[3], 3));
    }
}
