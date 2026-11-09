//! The selector model built from one snapshot, the agent overlay read from pane variables, and the
//! caller's scope (the CRUD gate's one question: is this node mine?).
//!
//! The agent tree lives on the panes themselves, so it dies with them and has one source of truth:
//! every pane the CLI creates gets `agent.parent` (the creator's handle, or `-` when the creator
//! runs outside iTerm2) and `agent.kind` (`claude`, `cmd` or `shell`). A pane is **mine** when
//! following `agent.parent` from it reaches me; for a caller outside iTerm2, when the chain ends
//! at a pane an outside caller created.

use std::collections::{BTreeMap, HashMap};

use crate::app::model::{handle, parse_handle};
use crate::app::{Id, Kind as PKind, Snapshot};
use serde_json::json;

use crate::geometry::Frame;
use crate::grammar::Kind;
use crate::out::Obj;
use crate::select::{MNode, Model};

pub const V_PARENT: &str = "agent.parent";
pub const V_KIND: &str = "agent.kind";
/// Set by the app (its agent table): the provider running in the pane and its state.
pub const V_PROVIDER: &str = "agent.provider";
pub const V_STATE: &str = "agent.state";
pub const V_NAME: &str = "agent.name";
pub const V_MODEL: &str = "agent.model";
pub const V_EFFORT: &str = "agent.effort";
pub const V_TASK: &str = "agent.task";
pub const V_CMD: &str = "agent.cmd";
pub const V_WORKTREE: &str = "agent.worktree";
pub const V_PORTS: &str = "agent.ports";
pub const V_LEND: &str = "lend";
pub const V_GROUPS: &str = "groups";
/// `agent.parent` of a pane created from outside iTerm2.
pub const OUTSIDE: &str = "-";

pub fn kind_of(k: PKind) -> Kind {
    match k {
        PKind::App => Kind::App,
        PKind::Window => Kind::Window,
        PKind::VTab => Kind::VTab,
        PKind::Tab => Kind::Tab,
        PKind::Pane => Kind::Pane,
    }
}

pub fn is_agent_kind(k: Option<&str>) -> bool {
    matches!(k, Some("claude") | Some("codex") | Some("omp"))
}

/// One command's view: the selector model plus each node's ID.
pub struct View {
    pub m: Model,
    pub ids: Vec<Id>,
    /// The caller has no pane (runs outside iTerm2).
    pub outside: bool,
}

impl View {
    pub fn new(s: &Snapshot, me: Option<Id>, binds: &HashMap<String, Vec<String>>) -> View {
        let mut m = Model::default();
        let mut ids = Vec::with_capacity(s.nodes.len());
        m.nodes.reserve(s.nodes.len());
        for n in &s.nodes {
            let mut fields = BTreeMap::new();
            for (k, v) in &n.vars {
                fields.insert(format!("var:{k}"), v.clone());
            }
            if let Some(g) = n.var(V_GROUPS) {
                fields.insert("groups".into(), g.to_string());
            }
            if let Some(p) = &n.pane {
                fields.insert("cwd".into(), p.cwd.clone());
                fields.insert("title".into(), p.title.clone());
                fields.insert("job".into(), p.job.clone());
                fields.insert("pid".into(), p.pid.to_string());
                // an agent the human started (no agent.kind, or a shell's) still has the provider the app detected
                let kind = n.var(V_KIND).filter(|k| is_agent_kind(Some(k))).or(n.var(V_PROVIDER)).or(n.var(V_KIND));
                fields.insert("kind".into(), kind.unwrap_or("shell").to_string());
                if let Some(x) = p.exit {
                    fields.insert("exit".into(), x.to_string());
                    fields.insert("state".into(), "EXITED".into());
                }
            }
            let rect = n.rect.map(|r| Frame { x: r.x as f64, y: r.y as f64, width: r.w as f64, height: r.h as f64 });
            m.nodes.push(MNode {
                kind: kind_of(n.kind),
                id: handle(n.kind, n.id),
                raw: n.id.0.to_string(),
                name: n.name.clone(),
                parent: n.parent.map(|p| p as usize),
                rect,
                rank: n.created as f64,
                fields,
            });
            ids.push(n.id);
        }
        m.index();
        m.me = me.and_then(|id| m.by_raw(&id.0.to_string())).filter(|i| m.nodes[*i].kind == Kind::Pane);
        for f in &s.focus {
            if let Some(i) = m.by_raw(&f.0.to_string()) {
                m.focused.push(i);
            }
        }
        for (i, n) in s.nodes.iter().enumerate() {
            if let Some(p) = n.var(V_PARENT).and_then(parse_handle).and_then(|(_, id)| m.by_raw(&id.0.to_string())) {
                m.agent_parent.insert(i, p);
            }
        }
        m.binds = binds.clone();
        View { outside: me.is_none(), m, ids }
    }

    pub fn var<'a>(&self, s: &'a Snapshot, i: usize, key: &str) -> Option<&'a str> {
        s.nodes[i].var(key)
    }

    /// `p` is the caller's pane or in its agent subtree (`strict`: not the caller itself).
    pub fn mine(&self, s: &Snapshot, p: usize, strict: bool) -> bool {
        if let Some(me) = self.m.me {
            if p == me {
                return !strict;
            }
            let mut cur = self.m.agent_parent.get(&p).copied();
            for _ in 0..64 {
                match cur {
                    Some(c) if c == me => return true,
                    Some(c) => cur = self.m.agent_parent.get(&c).copied(),
                    None => return false,
                }
            }
            return false;
        }
        // outside iTerm2: the chains that start at a pane an outside caller created
        let mut cur = p;
        for _ in 0..64 {
            match self.m.agent_parent.get(&cur) {
                Some(&up) => cur = up,
                None => return s.nodes[cur].var(V_PARENT) == Some(OUTSIDE),
            }
        }
        false
    }

    /// Every pane under these nodes is mine (and there is at least one).
    pub fn inside(&self, s: &Snapshot, nodes: &[usize], strict: bool) -> bool {
        let mut any = false;
        for n in nodes {
            for p in self.m.project(*n, Some(Kind::Pane)) {
                any = true;
                if !self.mine(s, p, strict) {
                    return false;
                }
            }
        }
        any
    }

    /// One node as a row (`ok node …`).
    pub fn row(&self, s: &Snapshot, i: usize) -> Obj {
        let n = &self.m.nodes[i];
        let node = &s.nodes[i];
        let mut o = crate::obj! {"status" => "ok", "code" => "node", "node" => node.kind.name(), "id" => n.id};
        if !n.name.is_empty() {
            o.insert("name".into(), json!(n.name));
        }
        if let Some(p) = n.parent.filter(|p| self.m.nodes[*p].kind != Kind::App) {
            o.insert("parent".into(), json!(self.m.nodes[p].id));
        }
        if self.m.focused.contains(&i) {
            o.insert("focused".into(), json!(true));
        }
        if self.m.me == Some(i) {
            o.insert("me".into(), json!(true));
        }
        if let Some(p) = &node.pane {
            o.insert("size".into(), json!(format!("{}x{}", p.cols, p.rows)));
            for k in ["kind", "cwd", "title", "job", "exit"] {
                if let Some(v) = n.fields.get(k) {
                    o.insert(k.into(), json!(v));
                }
            }
            if let Some(&pp) = self.m.agent_parent.get(&i) {
                o.insert("agent_parent".into(), json!(self.m.nodes[pp].id));
            }
            for (k, key) in [("model", V_MODEL), ("effort", V_EFFORT), ("worktree", V_WORKTREE), ("ports", V_PORTS), ("groups", V_GROUPS)] {
                if let Some(v) = node.var(key) {
                    o.insert(k.into(), json!(v));
                }
            }
        }
        if let Some(st) = n.fields.get("state") {
            o.insert("state".into(), json!(st));
        }
        o
    }

    pub fn id(&self, i: usize) -> Id {
        self.ids[i]
    }

    pub fn handle(&self, i: usize) -> &str {
        &self.m.nodes[i].id
    }
}

/// `p12` for a pane ID.
pub fn pane_handle(id: Id) -> String {
    handle(PKind::Pane, id)
}

#[cfg(test)]
mod tests {
    // regression: Codex panes were left out of agent lists (live run, 2026-10-26)
    #[test]
    fn every_provider_is_an_agent_kind() {
        for k in ["claude", "codex", "omp"] {
            assert!(super::is_agent_kind(Some(k)), "{k}");
        }
        assert!(!super::is_agent_kind(Some("cmd")) && !super::is_agent_kind(None));
    }
}
