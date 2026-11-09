//! The CLI's contract with a terminal app: one [`App`] answers snapshots, operations, settings, exit statuses and
//! event streams. perf term's protocol types ([`model`], [`wire`]) are the vocabulary; the iTerm2 adapter
//! (`crate::iterm2::Live`) and the in-memory fake (tests) implement it. [`Hub`] is the publisher both can use:
//! the last snapshot, a replay ring and bounded watcher queues, in process.

pub mod agent;
pub mod model;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use model::{Broadcast, BroadcastMode, Id, Kind, Layout, Node, PaneInfo, Rect, Snapshot};
pub use wire::{Done, Event, Fail, FailCode, Op, Pushed, SettingOp};

/// The grammar's version: `--json` rows carry it.
pub const SCHEMA_VERSION: u32 = 2;

/// A terminal app the CLI drives. Every method may block on the app; `wait` bounds an operation that may be slow.
pub trait App: Send {
    /// The whole model now: windows → vertical tabs → tabs → panes, with variables and agent info.
    fn snapshot(&mut self) -> Result<Snapshot, Fail>;
    /// The model moved on (an event said so): the next `snapshot` must not answer from any cache.
    fn invalidate(&mut self) {}
    /// One operation. `wait` bounds it (None: the backend's default).
    fn call(&mut self, op: Op, wait: Option<Duration>) -> Result<Done, Fail>;
    /// Settings, profiles and key bindings.
    fn setting(&mut self, op: SettingOp) -> Result<Done, Fail> {
        let _ = op;
        Err(Fail::new(FailCode::Unsupported, "this app has no settings"))
    }
    /// Exit statuses recorded for these panes (they may be closed by now); None: unknown or still running.
    fn exits(&mut self, ids: Vec<Id>) -> Result<Vec<(Id, Option<i32>)>, Fail>;
    /// Events from `since` (exclusive; None: from now), with the snapshot the stream starts from.
    fn watch(&mut self, since: Option<u64>) -> Result<(Box<dyn Events>, Snapshot), Fail>;
}

/// A stream of pushed events.
pub trait Events: Send {
    /// The next event; `Ok(None)` when `timeout` passed first (None: wait without a bound).
    fn next(&mut self, timeout: Option<Duration>) -> Result<Option<Pushed>, Fail>;
}

/// Events kept for `--since` replay.
const RING: usize = 4096;
/// Events a watcher may lag behind before it gets a gap.
const QUEUE: usize = 1024;
/// Exit statuses kept for closed panes.
const EXITS: usize = 4096;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct Sub {
    tx: SyncSender<Pushed>,
    missed: Arc<AtomicU64>,
}

struct HubState {
    seq: u64,
    snap: Snapshot,
    ring: VecDeque<Pushed>,
    subs: Vec<Sub>,
    exits: HashMap<Id, i32>,
    exit_order: VecDeque<Id>,
}

impl HubState {
    fn push(&mut self, ev: Event) {
        self.seq += 1;
        if let Event::Exited { pane, status } = ev {
            if self.exits.insert(pane, status).is_none() {
                self.exit_order.push_back(pane);
                if self.exit_order.len() > EXITS {
                    if let Some(old) = self.exit_order.pop_front() {
                        self.exits.remove(&old);
                    }
                }
            }
        }
        let p = Pushed { seq: self.seq, ms: now_ms(), ev };
        self.subs.retain(|s| match s.tx.try_send(p.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                s.missed.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        if self.ring.len() == RING {
            self.ring.pop_front();
        }
        self.ring.push_back(p);
    }
}

/// The publisher: a snapshot and the events that describe each change, atomically, so a watcher never sees one
/// without the other. Cheap to clone (shared).
#[derive(Clone)]
pub struct Hub {
    st: Arc<Mutex<HubState>>,
}

impl Hub {
    pub fn new(snap: Snapshot) -> Hub {
        Hub {
            st: Arc::new(Mutex::new(HubState {
                seq: 0,
                snap,
                ring: VecDeque::new(),
                subs: Vec::new(),
                exits: HashMap::new(),
                exit_order: VecDeque::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A model change: the new snapshot and its events.
    pub fn publish(&self, mut snap: Snapshot, events: Vec<Event>) {
        let mut st = self.lock();
        for ev in events {
            st.push(ev);
        }
        snap.seq = st.seq;
        st.snap = snap;
    }

    /// An event with no model change (bell, quiet, prompt mark).
    pub fn emit(&self, ev: Event) {
        self.lock().push(ev);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().snap.clone()
    }

    pub fn seq(&self) -> u64 {
        self.lock().seq
    }

    pub fn exits(&self, ids: &[Id]) -> Vec<(Id, Option<i32>)> {
        let st = self.lock();
        ids.iter().map(|id| (*id, st.exits.get(id).copied())).collect()
    }

    /// A watcher from `since` (exclusive): the ring's later events first (a gap when `since` fell out of it).
    pub fn watch(&self, since: Option<u64>) -> (HubEvents, Snapshot) {
        let mut st = self.lock();
        let (tx, rx) = sync_channel(QUEUE + RING);
        let missed = Arc::new(AtomicU64::new(0));
        if let Some(since) = since {
            let oldest = st.ring.front().map(|p| p.seq).unwrap_or(st.seq + 1);
            if since + 1 < oldest && since < st.seq {
                let _ = tx.try_send(Pushed { seq: since, ms: now_ms(), ev: Event::Gap { missed: oldest - since - 1 } });
            }
            for p in st.ring.iter().filter(|p| p.seq > since) {
                let _ = tx.try_send(p.clone());
            }
        }
        st.subs.push(Sub { tx, missed: missed.clone() });
        (HubEvents { rx, missed }, st.snap.clone())
    }
}

pub struct HubEvents {
    rx: Receiver<Pushed>,
    missed: Arc<AtomicU64>,
}

impl Events for HubEvents {
    fn next(&mut self, timeout: Option<Duration>) -> Result<Option<Pushed>, Fail> {
        let n = self.missed.swap(0, Ordering::Relaxed);
        if n > 0 {
            return Ok(Some(Pushed { seq: 0, ms: now_ms(), ev: Event::Gap { missed: n } }));
        }
        let r = match timeout {
            Some(t) => self.rx.recv_timeout(t),
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match r {
            Ok(p) => Ok(Some(p)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(Fail::new(FailCode::Internal, "the event stream ended")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_replays_since_and_streams() {
        let hub = Hub::new(Snapshot::default());
        hub.emit(Event::Bell { pane: Id(1) });
        hub.emit(Event::Bell { pane: Id(2) });
        let (mut w, _) = hub.watch(Some(1));
        assert_eq!(w.next(Some(Duration::ZERO)).unwrap().unwrap().ev, Event::Bell { pane: Id(2) });
        hub.publish(Snapshot::default(), vec![Event::Quiet { pane: Id(3) }]);
        assert_eq!(w.next(Some(Duration::from_millis(50))).unwrap().unwrap().seq, 3);
        assert!(w.next(Some(Duration::from_millis(1))).unwrap().is_none());
        assert_eq!(hub.snapshot().seq, 3);
    }

    #[test]
    fn hub_keeps_exits() {
        let hub = Hub::new(Snapshot::default());
        hub.emit(Event::Exited { pane: Id(4), status: 2 });
        assert_eq!(hub.exits(&[Id(4), Id(5)]), vec![(Id(4), Some(2)), (Id(5), None)]);
    }
}
