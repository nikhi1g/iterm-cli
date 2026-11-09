//! Stable short IDs for iTerm2's objects. iTerm2 names a session by UUID, a tab by a number and a window by
//! `pty-…`; the CLI shows one counter across kinds (`w1`, `t4`, `p12`), as perf term did. The map is one
//! file in the state folder (`ids.tsv`: `ID\tKIND\tFOREIGN`), appended to and compacted (below), read once per
//! command; a new object gets the next number under an exclusive lock, so two CLIs never hand out the same ID. An
//! ID is never reused.
//!
//! Cost: open reads the file, O(lines); a lookup is O(1); an assign reads only the lines added since. Lines of dead
//! objects would make the file O(objects ever seen), so a snapshot compacts it when [`due`] (live objects plus the
//! newest [`KEEP_DEAD`] dead ones, by rename): the file stays O(live + KEEP_DEAD) lines.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::app::{Id, Kind};

/// The session UUID in `ITERM_SESSION_ID` (`w0t2p0:UUID`).
pub fn session_of_env(v: &str) -> Option<String> {
    let uuid = v.rsplit(':').next()?.trim();
    (!uuid.is_empty()).then(|| uuid.to_string())
}

pub struct IdMap {
    path: PathBuf,
    /// Foreign name → ID, one map per kind: a lookup allocates nothing.
    by_foreign: [HashMap<Arc<str>, Id>; 5],
    by_id: HashMap<Id, (Kind, Arc<str>)>,
    next: u64,
    /// The bytes of the file read so far (whole lines): a reload reads only what was added since.
    loaded: u64,
    /// Whole lines read so far, dead objects' included: what a compaction would shrink.
    lines: u64,
    /// The file (device, inode) the map was read from. A compaction renames a new file over the old one; a map read
    /// from the old one starts over from the new one.
    file: Option<(u64, u64)>,
}

/// Dead objects a compaction keeps, the newest (highest IDs): a recently closed pane's exit status, a node iTerm2
/// is re-creating, a snapshot taken while the app restores its windows.
pub const KEEP_DEAD: usize = 512;

/// Whether a file of `lines` lines for `live` live objects is due for compaction: it holds three times what a
/// compaction keeps. So the file stays O(live + KEEP_DEAD) lines, and a compaction (O(lines)) removes at least two
/// thirds of what it reads: amortized O(1) per line ever appended.
pub fn due(lines: u64, live: usize) -> bool {
    lines > 3 * (live + KEEP_DEAD) as u64
}

fn slot(k: Kind) -> usize {
    match k {
        Kind::App => 0,
        Kind::Window => 1,
        Kind::VTab => 2,
        Kind::Tab => 3,
        Kind::Pane => 4,
    }
}

fn ident(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

/// The ID of one object without loading the map: the file's newest line for it. For the one lookup every command
/// makes before it knows it needs the app at all (the caller's own pane). None: not there yet, or unreadable.
pub fn peek(state: &Path, kind: Kind, foreign: &str) -> Option<Id> {
    let text = std::fs::read_to_string(state.join("ids.tsv")).ok()?;
    let tail = format!("\t{}\t{foreign}\n", kind.letter());
    let at = text.rfind(&tail)?;
    let start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    text[start..at].parse().ok().map(Id)
}

struct Lock(File);

impl Lock {
    fn take(f: File) -> std::io::Result<Lock> {
        // SAFETY: a valid fd owned by `f`.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: a valid fd.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl IdMap {
    pub fn open(state: &Path) -> std::io::Result<IdMap> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(state)?;
        crate::store::private_default(state);
        let path = state.join("ids.tsv");
        let mut m = IdMap { path, by_foreign: Default::default(), by_id: HashMap::new(), next: 0, loaded: 0, lines: 0, file: None };
        if let Ok(mut f) = File::open(&m.path) {
            let mut bytes = Vec::new();
            if let (Ok(md), Ok(_)) = (f.metadata(), f.read_to_end(&mut bytes)) {
                m.file = Some(ident(&md));
                m.load(&bytes);
            }
        }
        Ok(m)
    }

    /// Whole lines of the file this map has read: O(live + KEEP_DEAD) after a compaction, whatever was ever seen.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Takes in the whole lines of `bytes`, the file from `self.loaded` on. A line another command is still writing
    /// waits for the next read; a line that isn't text (a damaged file) is skipped, the rest still count.
    fn load(&mut self, bytes: &[u8]) {
        let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
        self.loaded += whole as u64;
        for line in bytes[..whole].split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            self.lines += 1;
            let Ok(line) = std::str::from_utf8(line) else { continue };
            let mut f = line.splitn(3, '\t');
            let (Some(n), Some(k), Some(foreign)) = (f.next(), f.next(), f.next()) else { continue };
            let (Ok(n), Some(kind)) = (n.parse::<u64>(), k.chars().next().and_then(Kind::from_letter)) else { continue };
            self.next = self.next.max(n);
            self.put(kind, foreign, Id(n));
        }
    }

    /// Forgets what was read (the file was replaced), keeping the highest ID seen: it is never handed out again.
    fn reset(&mut self, file: (u64, u64)) {
        self.by_foreign = Default::default();
        self.by_id.clear();
        (self.loaded, self.lines, self.file) = (0, 0, Some(file));
    }

    /// The file under its exclusive lock, with what was added since this map read it taken in; whether its last
    /// line is torn (a crash mid-write: the next line written must close it first). A compaction that replaced the
    /// file while this waited is followed to the new file.
    fn locked(&mut self) -> std::io::Result<(Lock, bool)> {
        loop {
            let f = OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(&self.path)?;
            let mut lock = Lock::take(f)?;
            let held = ident(&lock.0.metadata()?);
            if std::fs::metadata(&self.path).map(|m| ident(&m)).ok() != Some(held) {
                continue;
            }
            if self.file != Some(held) {
                self.reset(held);
            }
            let mut bytes = Vec::new();
            lock.0.seek(SeekFrom::Start(self.loaded))?;
            lock.0.read_to_end(&mut bytes)?;
            self.load(&bytes);
            let torn = bytes.last().is_some_and(|b| *b != b'\n');
            return Ok((lock, torn));
        }
    }

    fn put(&mut self, kind: Kind, foreign: &str, id: Id) {
        let foreign: Arc<str> = foreign.into();
        self.by_foreign[slot(kind)].insert(foreign.clone(), id);
        self.by_id.insert(id, (kind, foreign));
    }

    pub fn get(&self, kind: Kind, foreign: &str) -> Option<Id> {
        self.by_foreign[slot(kind)].get(foreign).copied()
    }

    /// The ID of `foreign`, assigning the next one when it has none.
    pub fn id(&mut self, kind: Kind, foreign: &str) -> std::io::Result<Id> {
        if let Some(id) = self.get(kind, foreign) {
            return Ok(id);
        }
        Ok(self.assign(&[(kind, foreign)])?[0])
    }

    /// IDs for many objects with one lock and one write (a snapshot's new objects).
    pub fn assign(&mut self, items: &[(Kind, &str)]) -> std::io::Result<Vec<Id>> {
        if let Some(known) = items.iter().map(|(k, f)| self.get(*k, f)).collect::<Option<Vec<Id>>>() {
            return Ok(known);
        }
        let (mut lock, torn) = self.locked()?;
        let mut out = Vec::with_capacity(items.len());
        let mut add = if torn { String::from("\n") } else { String::new() };
        for (kind, foreign) in items {
            if let Some(id) = self.get(*kind, foreign) {
                out.push(id);
                continue;
            }
            self.next += 1;
            let id = Id(self.next);
            add.push_str(&format!("{}\t{}\t{}\n", id.0, kind.letter(), foreign));
            self.put(*kind, foreign, id);
            out.push(id);
        }
        if !add.is_empty() {
            lock.0.write_all(add.as_bytes())?;
        }
        Ok(out)
    }

    /// Gives `foreign` the existing ID `id` (a node iTerm2 re-created under a new name, as a tab moved into
    /// another window): the ID never changes when a node moves. The newest line for an ID wins on load.
    pub fn alias(&mut self, kind: Kind, foreign: &str, id: Id) -> std::io::Result<()> {
        if self.get(kind, foreign) == Some(id) {
            return Ok(());
        }
        let (mut lock, torn) = self.locked()?;
        lock.0.write_all(format!("{}{}\t{}\t{}\n", if torn { "\n" } else { "" }, id.0, kind.letter(), foreign).as_bytes())?;
        self.put(kind, foreign, id);
        Ok(())
    }

    /// Compacts the file when [`due`]: what a snapshot sees (`live`) plus the newest [`KEEP_DEAD`] dead objects.
    pub fn compact_if_due(&mut self, live: &[(Kind, &str)]) -> std::io::Result<bool> {
        if !due(self.lines, live.len()) {
            return Ok(false);
        }
        self.compact(live, KEEP_DEAD, |lines| due(lines, live.len()))
    }

    /// Rewrites the file to `live`'s lines (their IDs unchanged) and the `keep_dead` newest dead objects' (the
    /// highest ID always among them, so no opener hands it out again), when `worth(lines)` still holds under the
    /// lock (another command may have compacted first). Temp file, fsync, rename under the old file's lock: a crash
    /// leaves the old file or the new one, never a part. O(lines).
    pub fn compact(&mut self, live: &[(Kind, &str)], keep_dead: usize, worth: impl Fn(u64) -> bool) -> std::io::Result<bool> {
        let (lock, _) = self.locked()?;
        if !worth(self.lines) {
            return Ok(false);
        }
        let mut keep: Vec<(Id, Kind, Arc<str>)> = Vec::with_capacity(live.len() + keep_dead + 1);
        let mut live_ids = std::collections::HashSet::with_capacity(live.len());
        for (kind, foreign) in live {
            if let Some(id) = self.get(*kind, foreign) {
                if live_ids.insert(id) {
                    keep.push((id, *kind, (*foreign).into()));
                }
            }
        }
        let mut dead: Vec<Id> = self.by_id.keys().copied().filter(|id| !live_ids.contains(id)).collect();
        if dead.len() > keep_dead {
            // the newest: an O(n) selection, not a sort
            dead.select_nth_unstable_by(keep_dead, |a, b| b.cmp(a));
            dead.truncate(keep_dead);
        }
        keep.extend(dead.into_iter().map(|id| {
            let (k, f) = &self.by_id[&id];
            (id, *k, f.clone())
        }));
        let top = keep.iter().map(|k| k.0 .0).max().unwrap_or(0);
        if self.next > top {
            // the highest ID ever handed out has no line left (keep_dead 0, or a line lost): a marker keeps it
            keep.push((Id(self.next), Kind::App, "#next".into()));
        }
        keep.sort_by_key(|k| k.0);
        let mut text = String::with_capacity(keep.len() * 48);
        for (id, kind, foreign) in &keep {
            text.push_str(&format!("{}\t{}\t{}\n", id.0, kind.letter(), foreign));
        }
        let tmp = self.path.with_file_name(format!("ids.tsv.{}.tmp", std::process::id()));
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        let file = ident(&f.metadata()?);
        std::fs::rename(&tmp, &self.path)?;
        drop(lock);
        self.reset(file);
        self.load(text.as_bytes());
        Ok(true)
    }

    /// iTerm2's name for an ID.
    pub fn foreign(&self, id: Id) -> Option<(Kind, &str)> {
        self.by_id.get(&id).map(|(k, f)| (*k, &**f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_shared() {
        let dir = std::env::temp_dir().join(format!("iterm-ids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = IdMap::open(&dir).unwrap();
        let mut b = IdMap::open(&dir).unwrap();
        let p = a.id(Kind::Pane, "UUID-1").unwrap();
        let t = b.id(Kind::Tab, "7").unwrap();
        assert_ne!(p, t, "two maps never hand out one ID");
        assert_eq!(b.id(Kind::Pane, "UUID-1").unwrap(), p);
        let c = IdMap::open(&dir).unwrap();
        assert_eq!(c.foreign(t), Some((Kind::Tab, "7")));
        assert_eq!(session_of_env("w0t2p0:ABC-DEF"), Some("ABC-DEF".into()));
        // a tab re-created under a new iTerm2 ID keeps its short ID, for this map and the next one
        a.alias(Kind::Tab, "9", t).unwrap();
        assert_eq!((a.get(Kind::Tab, "9"), a.foreign(t)), (Some(t), Some((Kind::Tab, "9"))));
        let d = IdMap::open(&dir).unwrap();
        assert_eq!(d.foreign(t), Some((Kind::Tab, "9")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_or_damaged_file_never_reissues_or_joins_lines() {
        let dir = std::env::temp_dir().join(format!("iterm-ids-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ids.tsv");
        // regression: a byte that isn't UTF-8 emptied the map on open and failed every later assign; a line still
        // being written read as a real (truncated) name
        let mut bytes = b"7\tp\tOLD\n8\tp\t\xff\xfe\n9\tt\t3\n12\tp\tTORN-UU".to_vec();
        std::fs::write(&file, &bytes).unwrap();
        let mut m = IdMap::open(&dir).unwrap();
        assert_eq!(m.get(Kind::Pane, "OLD"), Some(Id(7)), "lines around a damaged one still count");
        assert_eq!(m.get(Kind::Pane, "TORN-UU"), None, "an unfinished line is not a name");
        let new = m.id(Kind::Pane, "NEW").unwrap();
        assert_eq!(new, Id(10), "the next ID, read past the damage");
        bytes = std::fs::read(&file).unwrap();
        assert!(bytes.ends_with(b"TORN-UU\n10\tp\tNEW\n"), "the torn line was closed first");
        // the next opener counts the closed torn line's number too: never handed out again
        let mut m = IdMap::open(&dir).unwrap();
        assert_eq!(m.id(Kind::Pane, "NEXT").unwrap(), Id(13));
        // an append by another command is picked up from where this one stopped
        let mut other = IdMap::open(&dir).unwrap();
        let x = other.id(Kind::Tab, "T").unwrap();
        assert_eq!(m.assign(&[(Kind::Tab, "T"), (Kind::Tab, "U")]).unwrap(), vec![x, Id(x.0 + 1)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_reads_the_newest_line_for_an_object() {
        let dir = std::env::temp_dir().join(format!("iterm-ids-peek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ids.tsv"), "1\tp\tA\n2\tt\tA\n3\tp\tA\n4\tp\tXA\n").unwrap();
        assert_eq!(peek(&dir, Kind::Pane, "A"), Some(Id(3)));
        assert_eq!(peek(&dir, Kind::Tab, "A"), Some(Id(2)));
        assert_eq!(peek(&dir, Kind::Pane, "B"), None);
        assert_eq!(peek(&dir, Kind::Pane, "A").unwrap(), IdMap::open(&dir).unwrap().get(Kind::Pane, "A").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn lines_on_disk(dir: &Path) -> u64 {
        std::fs::read_to_string(dir.join("ids.tsv")).unwrap().lines().count() as u64
    }

    // regression: the file only grew (one line per object ever seen), so every command's open was O(all objects
    // ever); it now stays O(live + KEEP_DEAD) lines, and compacting costs amortized O(1) per line appended
    #[test]
    fn churn_keeps_the_file_bounded_and_ids_unique() {
        let dir = std::env::temp_dir().join(format!("iterm-ids-churn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = IdMap::open(&dir).unwrap();
        const LIVE: usize = 8;
        let bound = 3 * (LIVE + 1 + KEEP_DEAD) as u64 + LIVE as u64 + 1;
        let mut live: Vec<String> = (0..LIVE).map(|i| format!("U{i}")).collect();
        let mut first: HashMap<String, Id> = HashMap::new();
        let (mut last, mut appended, mut compactions, mut compacted_lines) = (0u64, 0u64, 0u64, 0u64);
        for round in 0..20 * KEEP_DEAD {
            // one pane closes, one opens: the window and tab stay
            live[round % LIVE] = format!("U{}", round + LIVE);
            let mut want: Vec<(Kind, &str)> = vec![(Kind::App, "app"), (Kind::Window, "pty-1")];
            want.extend(live.iter().map(|u| (Kind::Pane, u.as_str())));
            let before = m.next;
            let got = m.assign(&want).unwrap();
            appended += m.next - before;
            for ((_, f), id) in want.iter().zip(&got) {
                assert_eq!(*first.entry(f.to_string()).or_insert(*id), *id, "{f} kept its ID");
                if id.0 > last {
                    assert_eq!(id.0, last + 1, "IDs are handed out in order, none twice");
                    last = id.0;
                }
            }
            let lines = m.lines();
            if m.compact_if_due(&want).unwrap() {
                compactions += 1;
                compacted_lines += lines;
                assert!(m.lines() <= (LIVE + 2 + KEEP_DEAD) as u64, "{} lines kept", m.lines());
            }
            assert!(lines_on_disk(&dir) <= bound, "round {round}: {} lines", lines_on_disk(&dir));
        }
        assert!(compactions >= 5, "{compactions} compactions");
        // each compaction reads about 3x what it keeps, so the work is a fixed multiple of the lines appended
        assert!(compacted_lines <= 2 * appended, "{compacted_lines} lines compacted for {appended} appended");
        // a new command opens the small file, sees every live object under its ID and hands out the next one
        let mut fresh = IdMap::open(&dir).unwrap();
        assert!(fresh.lines() <= bound);
        for u in &live {
            assert_eq!(fresh.get(Kind::Pane, u), Some(first[u]));
        }
        assert_eq!(fresh.id(Kind::Pane, "NEW").unwrap(), Id(last + 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_map_read_before_a_compaction_follows_the_new_file() {
        let dir = std::env::temp_dir().join(format!("iterm-ids-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut a = IdMap::open(&dir).unwrap();
        let names: Vec<String> = (0..50).map(|i| format!("S{i}")).collect();
        let refs: Vec<(Kind, &str)> = names.iter().map(|n| (Kind::Pane, n.as_str())).collect();
        a.assign(&refs).unwrap();
        let mut stale = IdMap::open(&dir).unwrap();
        // only S0 lives, nothing dead is kept: the highest ID (S49's) survives as a marker line
        assert!(a.compact(&[(Kind::Pane, "S0")], 0, |_| true).unwrap());
        assert_eq!(lines_on_disk(&dir), 2);
        assert_eq!(a.foreign(Id(1)), Some((Kind::Pane, "S0")));
        assert_eq!(a.get(Kind::Pane, "S7"), None, "a dead object is forgotten");
        // the stale map (read the old file) and a fresh one never hand out an ID twice
        assert_eq!(stale.id(Kind::Pane, "NEW").unwrap(), Id(51));
        assert_eq!(IdMap::open(&dir).unwrap().id(Kind::Pane, "NEWER").unwrap(), Id(52));
        // the compacting map takes in what the others appended to the new file
        assert_eq!(a.id(Kind::Pane, "NEW").unwrap(), Id(51));
        stale.alias(Kind::Pane, "S0-moved", Id(1)).unwrap();
        let c = IdMap::open(&dir).unwrap();
        assert_eq!((c.get(Kind::Pane, "S0-moved"), c.get(Kind::Pane, "NEWER")), (Some(Id(1)), Some(Id(52))));
        // not worth it under the lock: nothing changes
        assert!(!a.compact(&[], 0, |_| false).unwrap());
        assert_eq!(lines_on_disk(&dir), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
