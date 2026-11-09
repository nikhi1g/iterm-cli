//! The iTerm2 adapter: [`Live`] implements [`crate::app::App`] over iTerm2's official, permission-gated API
//! (`conn`, `auth`, `target`, `proto`: the transport kept from the v2 CLI). Each operation group has its own
//! module, so work on one never touches another:
//!
//! - [`snapshot`]: the model (windows → vertical tabs → tabs → split tree → panes, variables, agent info), the
//!   ID map, and events (`watch`, `exits`).
//! - [`shape`]: create, split, close, move, order, merge, layout, set (name, color, var, frame, window state),
//!   select, window cells, broadcast.
//! - [`input`]: send, key, read, scroll, clear, and the delivery engine (`Op::Deliver`).
//! - [`appctl`]: content (selection, marks, find), menus, actions, ui dialogs, search, settings, quit.
//!
//! A module answers `Fail::Unsupported` for what it has not built, with the reason in one line.

pub mod appctl;
pub mod auth;
pub mod conn;
pub mod error;
pub mod events;
pub mod ids;
pub mod input;
pub mod proto;
pub mod sense;
pub mod shape;
pub mod snapshot;
pub mod target;

use std::time::Duration;

use crate::app::{App, Done, Events, Fail, FailCode, Id, Op, SettingOp, Snapshot};
use crate::conn::Env;

pub use conn::socket_path;

/// The live connection: one API connection, the ID map, and the caller's context.
pub struct Live {
    pub api: conn::Conn,
    pub ids: ids::IdMap,
    pub env: Env,
    /// The last snapshot this command built, with iTerm2's raw answers it came from.
    pub cache: Option<snapshot::Built>,
}

pub fn unsupported(what: &str) -> Fail {
    Fail::new(FailCode::Unsupported, format!("{what}: not on iTerm2 yet"))
}

impl From<error::Error> for Fail {
    fn from(e: error::Error) -> Fail {
        use error::Error as E;
        match e {
            E::Policy { code, msg } => Fail::new(if code == "auth" { FailCode::Auth } else { FailCode::Invalid }, msg),
            E::Timeout(m) => Fail::new(FailCode::Timeout, m),
            E::Invalid(m) => Fail::new(FailCode::Invalid, m),
            E::Failed(m) => Fail::new(FailCode::Internal, m),
        }
    }
}

impl Live {
    pub fn connect(env: &Env) -> Result<Live, Fail> {
        let api = conn::Conn::connect().map_err(Fail::from)?;
        let ids = ids::IdMap::open(&env.state).map_err(|e| Fail::new(FailCode::Internal, format!("state {}: {e}", env.state.display())))?;
        Ok(Live { api, ids, env: env.clone(), cache: None })
    }
}

impl App for Live {
    fn snapshot(&mut self) -> Result<Snapshot, Fail> {
        snapshot::snapshot(self)
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }

    fn call(&mut self, op: Op, wait: Option<Duration>) -> Result<Done, Fail> {
        // the caller's bound holds for each request of the operation too
        self.api.timeout = wait.map_or(conn::RPC_TIMEOUT, |w| w.min(conn::RPC_TIMEOUT));
        let r = match op {
            Op::NewWindow { .. }
            | Op::NewVTab { .. }
            | Op::NewTab { .. }
            | Op::Split { .. }
            | Op::Close { .. }
            | Op::Set { .. }
            | Op::Select { .. }
            | Op::Move { .. }
            | Op::Order { .. }
            | Op::SetLayout { .. }
            | Op::Merge { .. }
            | Op::WindowCells { .. }
            | Op::Broadcast { .. } => shape::call(self, op, wait),
            Op::Send { .. } | Op::Key { .. } | Op::Read { .. } | Op::Scroll { .. } | Op::Clear { .. } | Op::Deliver { .. } => {
                input::call(self, op, wait)
            }
            Op::Quit | Op::Search { .. } | Op::Actions | Op::Action { .. } | Op::Content { .. } | Op::Menu { .. } | Op::UiList | Op::UiAnswer { .. } => {
                appctl::call(self, op, wait)
            }
        };
        // a write changes the model: the next snapshot is built again
        self.cache = None;
        self.api.timeout = conn::RPC_TIMEOUT;
        r
    }

    fn setting(&mut self, op: SettingOp) -> Result<Done, Fail> {
        appctl::setting(self, op)
    }

    fn exits(&mut self, ids: Vec<Id>) -> Result<Vec<(Id, Option<i32>)>, Fail> {
        snapshot::exits(self, ids)
    }

    fn watch(&mut self, since: Option<u64>) -> Result<(Box<dyn Events>, Snapshot), Fail> {
        snapshot::watch(self, since)
    }
}
