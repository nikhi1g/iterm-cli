//! The caller's context (which pane it is, where the CLI's state lives) and its one connection to the app.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::app::{App, Done, Events, Fail, Id, Op, Pushed, SettingOp, Snapshot};
use crate::out::{Error, Result};

/// How long a command waits for the app's answer by default.
pub const CALL_WAIT: Duration = Duration::from_secs(30);

/// Environment variables an agent's tool shell carries (Claude Code sets the first three; Codex its own).
pub const AGENT_ENV: [&str; 5] = ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT", "OMP_AGENT", "CODEX_SANDBOX"];

/// The CLI's state folder: `ITERM_STATE`, else `~/Library/Application Support/iterm-cli/state`.
pub const ENV_STATE: &str = "ITERM_STATE";

/// Opens a connection to the app (the live iTerm2 adapter, or a test's fake).
pub type Connector = Arc<dyn Fn(&Env) -> std::result::Result<Box<dyn App>, Fail> + Send + Sync>;

#[derive(Clone)]
pub struct Env {
    /// Where the app listens (shown in messages; the connector decides how to reach it).
    pub socket: PathBuf,
    /// The caller's pane: `ITERM_SESSION_ID`, mapped to its stable ID.
    pub me: Option<Id>,
    /// Durable CLI state: the ID map, mailboxes, task boards, queues, paused agents.
    pub state: PathBuf,
    /// The calling process runs inside an agent (its environment says so).
    pub agent: bool,
    pub cwd: PathBuf,
    /// How long a command waits for an answer ([`CALL_WAIT`]).
    pub call_timeout: Duration,
    pub connect: Connector,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env").field("socket", &self.socket).field("me", &self.me).field("state", &self.state).field("agent", &self.agent).finish()
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_state() -> PathBuf {
    std::env::var_os(ENV_STATE)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Library/Application Support/iterm-cli/state"))
}

impl Env {
    /// The live environment: iTerm2 through its official API, the caller's pane from `ITERM_SESSION_ID`.
    pub fn from_process() -> Env {
        timing::install();
        let t0 = std::time::Instant::now();
        let env = Env::from_process_untimed();
        timing::add(timing::Phase::Env, t0.elapsed());
        env
    }

    fn from_process_untimed() -> Env {
        let state = default_state();
        let me = std::env::var("ITERM_SESSION_ID").ok().and_then(|v| crate::iterm2::ids::session_of_env(&v)).and_then(|uuid| crate::iterm2::ids::peek(&state, crate::app::Kind::Pane, &uuid).or_else(|| crate::iterm2::ids::IdMap::open(&state).ok()?.id(crate::app::Kind::Pane, &uuid).ok()));
        Env {
            socket: crate::iterm2::socket_path(),
            me,
            agent: AGENT_ENV.iter().any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty())),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            state,
            call_timeout: CALL_WAIT,
            connect: Arc::new(|env: &Env| crate::iterm2::Live::connect(env).map(|l| Box::new(l) as Box<dyn App>)),
        }
    }
}

/// One connection per command, opened on first use; one snapshot, fetched again after a write.
pub struct Conn {
    env: Env,
    app: Option<Box<dyn App>>,
    snap: Option<Snapshot>,
}

impl Conn {
    pub fn new(env: Env) -> Conn {
        Conn { env, app: None, snap: None }
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    fn app(&mut self) -> Result<&mut Box<dyn App>> {
        if self.app.is_none() {
            let t0 = std::time::Instant::now();
            let app = (self.env.connect)(&self.env);
            timing::add(timing::Phase::Open, t0.elapsed());
            self.app = Some(app.map_err(Error::from)?);
        }
        Ok(self.app.as_mut().expect("connected"))
    }

    /// How long `op` may take: a delivery waits for its text to land behind other senders' messages, more for a
    /// long text.
    fn wait_for(&self, op: &Op) -> Duration {
        match op {
            Op::Deliver { text, .. } => self.env.call_timeout * 4 + Duration::from_millis(text.len() as u64 / 16),
            _ => self.env.call_timeout,
        }
    }

    pub fn snapshot(&mut self) -> Result<&Snapshot> {
        if self.snap.is_none() {
            let app = self.app()?;
            let s = timing::time(timing::Phase::Snapshot, || app.snapshot()).map_err(Error::from)?;
            self.snap = Some(s);
        }
        Ok(self.snap.as_ref().expect("fetched"))
    }

    /// Forget the snapshot (the model changed); the next read fetches a new one.
    pub fn invalidate(&mut self) {
        self.snap = None;
        if let Some(a) = self.app.as_mut() {
            a.invalidate();
        }
    }

    /// Run one operation. Any write invalidates the snapshot.
    pub fn call(&mut self, op: Op) -> Result<Done> {
        let wait = self.wait_for(&op);
        self.call_with(op, wait)
    }

    /// `call`, waiting at most `wait` for the answer.
    pub fn call_within(&mut self, op: Op, wait: Duration) -> Result<Done> {
        self.call_with(op, wait)
    }

    fn call_with(&mut self, op: Op, wait: Duration) -> Result<Done> {
        let read = matches!(op, Op::Read { .. });
        let app = self.app()?;
        let r = timing::time(timing::Phase::Calls, || app.call(op, Some(wait)));
        if !read {
            self.snap = None;
        }
        r.map_err(Error::from)
    }

    pub fn setting(&mut self, op: SettingOp) -> Result<Done> {
        let r = self.app()?.setting(op);
        self.invalidate();
        r.map_err(Error::from)
    }

    pub fn exits(&mut self, ids: Vec<Id>) -> Result<Vec<(Id, Option<i32>)>> {
        self.app()?.exits(ids).map_err(Error::from)
    }

    /// Start a watch (its own stream; the command's connection stays usable for reads). `timeout` bounds each
    /// wait for the next event until `set_deadline` changes it.
    pub fn watch(&mut self, since: Option<u64>, timeout: Option<Duration>) -> Result<(Watch, Snapshot)> {
        let (events, snap) = self.app()?.watch(since).map_err(Error::from)?;
        Ok((Watch { events, timeout }, snap))
    }
}

pub struct Watch {
    events: Box<dyn Events>,
    timeout: Option<Duration>,
}

impl Watch {
    /// The next event; `Ok(None)` when the deadline passed.
    pub fn next(&mut self) -> Result<Option<Pushed>> {
        self.events.next(self.timeout).map_err(Error::from)
    }
    pub fn set_deadline(&mut self, left: Option<Duration>) -> Result<()> {
        self.timeout = left;
        Ok(())
    }
}

/// `ITERM_TIMING=1`: one stderr line at exit saying where the command's wall time went, in ms:
///
/// `iterm-timing total=… startup=… env=… open=… cookie=… ae=… socket=… handshake=… ids=… snapshot=… calls=… other=…`
///
/// `total` runs from the kernel's start of the process to `exit`; `startup` from there to `main` building its
/// [`Env`] (exec, dyld, the runtime). `open` is the connection (`cookie` + `socket` + `handshake` + `ids`, the ID
/// map); `ae` is the part of `cookie` spent waiting for the app's Apple Event reply. `snapshot` and `calls` are the
/// [`Conn`] methods; `other` is the rest (parsing, logic, rendering, output). Costs nothing when off.
pub mod timing {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Clone, Copy)]
    pub enum Phase {
        Env,
        Open,
        Cookie,
        AppleEvent,
        Socket,
        Handshake,
        Snapshot,
        Calls,
    }
    const PHASES: usize = 8;

    static NS: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];
    static STARTUP_NS: AtomicU64 = AtomicU64::new(0);

    pub fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("ITERM_TIMING").is_some_and(|v| !v.is_empty() && v != "0"))
    }

    pub fn add(p: Phase, d: Duration) {
        if on() {
            NS[p as usize].fetch_add(d.as_nanos() as u64, Relaxed);
        }
    }

    pub fn time<T>(p: Phase, f: impl FnOnce() -> T) -> T {
        if !on() {
            return f();
        }
        let t0 = std::time::Instant::now();
        let r = f();
        add(p, t0.elapsed());
        r
    }

    /// Wall-clock nanoseconds since the kernel started this process (0 when unknown).
    fn since_start_ns() -> u64 {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo writes at most `size` bytes into our zeroed struct.
        let n = unsafe { libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        let start = info.pbi_start_tvsec * 1_000_000_000 + info.pbi_start_tvusec * 1_000;
        if n != size || start == 0 {
            return 0;
        }
        now.saturating_sub(start)
    }

    /// Called once by [`super::Env::from_process`]: notes the startup cost and reports at exit.
    pub fn install() {
        if !on() {
            return;
        }
        STARTUP_NS.store(since_start_ns(), Relaxed);
        // SAFETY: `report` is a plain extern "C" fn that only reads atomics and writes stderr.
        unsafe { libc::atexit(report) };
    }

    /// The report line from the phase totals (nanoseconds) and the process's total and startup.
    pub fn line(total: u64, startup: u64, ns: [u64; PHASES]) -> String {
        let ms = |n: u64| format!("{:.2}", n as f64 / 1e6);
        let [env, open, cookie, ae, socket, handshake, snapshot, calls] = ns;
        let ids = open.saturating_sub(cookie + socket + handshake);
        let other = total.saturating_sub(startup + env + open + snapshot + calls);
        format!(
            "iterm-timing total={} startup={} env={} open={} cookie={} ae={} socket={} handshake={} ids={} snapshot={} calls={} other={}",
            ms(total), ms(startup), ms(env), ms(open), ms(cookie), ms(ae), ms(socket), ms(handshake), ms(ids), ms(snapshot), ms(calls), ms(other)
        )
    }

    extern "C" fn report() {
        let ns = std::array::from_fn(|i| NS[i].load(Relaxed));
        eprintln!("{}", line(since_start_ns(), STARTUP_NS.load(Relaxed), ns));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Id, Kind, Node};

    /// Caches its snapshot as the iTerm2 adapter does; each fetch has a new app_pid.
    struct Caching {
        fetched: i32,
        cache: Option<Snapshot>,
    }

    impl App for Caching {
        fn snapshot(&mut self) -> std::result::Result<Snapshot, Fail> {
            if let Some(s) = &self.cache {
                return Ok(s.clone());
            }
            self.fetched += 1;
            let s = Snapshot { app_pid: self.fetched, nodes: vec![Node { id: Id(1), kind: Kind::App, parent: None, name: String::new(), rect: None, created: 0, vars: vec![], pane: None, layout: None }], ..Snapshot::default() };
            self.cache = Some(s.clone());
            Ok(s)
        }
        fn invalidate(&mut self) {
            self.cache = None;
        }
        fn call(&mut self, _: Op, _: Option<Duration>) -> std::result::Result<Done, Fail> {
            unimplemented!()
        }
        fn exits(&mut self, _: Vec<Id>) -> std::result::Result<Vec<(Id, Option<i32>)>, Fail> {
            unimplemented!()
        }
        fn watch(&mut self, _: Option<u64>) -> std::result::Result<(Box<dyn Events>, Snapshot), Fail> {
            unimplemented!()
        }
    }

    // regression (live, 2026-11-01): `invalidate` cleared only this cache, so the adapter answered every later read
    // from its own: `pane wait --until idle`, `children --wait` and `plan run` never saw a state change
    #[test]
    fn invalidate_reaches_the_apps_own_cache() {
        let env = Env {
            socket: PathBuf::new(),
            me: None,
            state: PathBuf::new(),
            agent: false,
            cwd: PathBuf::new(),
            call_timeout: CALL_WAIT,
            connect: Arc::new(|_: &Env| Ok(Box::new(Caching { fetched: 0, cache: None }) as Box<dyn App>)),
        };
        let mut c = Conn::new(env);
        assert_eq!(c.snapshot().unwrap().app_pid, 1);
        assert_eq!(c.snapshot().unwrap().app_pid, 1, "cached within a command");
        c.invalidate();
        assert_eq!(c.snapshot().unwrap().app_pid, 2, "fresh after an invalidate");
    }

    #[test]
    fn the_timing_line_splits_the_total_and_derives_ids_and_other() {
        let ms = 1_000_000;
        // env open cookie ae socket handshake snapshot calls
        let l = timing::line(100 * ms, 3 * ms, [ms, 50 * ms, 40 * ms, 38 * ms, ms, 2 * ms, 10 * ms, 5 * ms]);
        assert_eq!(l, "iterm-timing total=100.00 startup=3.00 env=1.00 open=50.00 cookie=40.00 ae=38.00 socket=1.00 handshake=2.00 ids=7.00 snapshot=10.00 calls=5.00 other=31.00");
    }
}
