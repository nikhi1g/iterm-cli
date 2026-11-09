//! Agent mail (ported from iterm_api's `mailbox.rs`): one durable, append-only mailbox per
//! recipient pane, `<state>/mail/<pane>.ndjson` (0600, one JSON message per line). A send appends
//! under the mailbox's lock and syncs; only then is it reported sent. A message's ID is the byte
//! offset of its line, so IDs grow, never repeat, and unread reads seek straight to the first new
//! byte: O(new messages), never O(box). The read mark (`<pane>.read`) only moves forward.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::out::{ok, Error, Obj, Result};
use crate::store::{self, Lock};

/// Bytes of text one message may carry; longer work goes in a file or a task body.
pub const MAIL_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub at: i64,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_name: Option<String>,
    pub to: String,
    pub text: String,
}

impl Message {
    pub fn row(&self) -> Obj {
        ok("message", crate::obj! {"id" => self.id, "at" => self.at, "from" => self.from, "from_name" => self.from_name, "who" => who(self.from_name.as_deref(), &self.from), "text" => self.text})
    }
}

/// How people see a pane: `name (p16)`, or the bare id when it has no name.
pub fn who(name: Option<&str>, id: &str) -> String {
    match name.filter(|n| !n.is_empty() && *n != id) {
        Some(n) => format!("{n} ({id})"),
        None => id.to_string(),
    }
}

/// Messages `from` sent, every mailbox, oldest first, each with whether its receiver has
/// marked it read. ponytail: scans every mailbox (O(all mail)); keep a per-sender index if
/// mail volume ever makes `inbox --sent` slow.
pub fn sent(root: &Path, from: &str) -> Result<Vec<(Message, bool)>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir(root)?).map_err(io)?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("ndjson") {
            continue;
        }
        let Some(pane) = p.file_stem().and_then(|x| x.to_str()).map(str::to_string) else { continue };
        let mark = read_mark(root, &pane);
        let (msgs, _) = read(root, &pane, Since::All)?;
        out.extend(msgs.into_iter().filter(|m| m.from == from).map(|m| {
            let seen = m.id < mark;
            (m, seen)
        }));
    }
    out.sort_by_key(|(m, _)| (m.at, m.id));
    Ok(out)
}

/// Every message in every mailbox, oldest first, each with whether its receiver has marked it read: the human's
/// view of all traffic between panes. ponytail: O(all mail), as `sent`.
pub fn all(root: &Path) -> Result<Vec<(Message, bool)>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir(root)?).map_err(io)?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("ndjson") {
            continue;
        }
        let Some(pane) = p.file_stem().and_then(|x| x.to_str()).map(str::to_string) else { continue };
        let mark = read_mark(root, &pane);
        let (msgs, _) = read(root, &pane, Since::All)?;
        out.extend(msgs.into_iter().map(|m| {
            let seen = m.id < mark;
            (m, seen)
        }));
    }
    out.sort_by_key(|(m, _)| (m.at, m.id));
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    Unread,
    After(u64),
    All,
}

fn dir(root: &Path) -> Result<PathBuf> {
    store::dir(root, &["mail"])
}

pub fn box_path(root: &Path, pane: &str) -> Result<PathBuf> {
    Ok(dir(root)?.join(store::sanitize(pane) + ".ndjson"))
}

fn mark_path(root: &Path, pane: &str) -> Result<PathBuf> {
    Ok(dir(root)?.join(store::sanitize(pane) + ".read"))
}

fn lock_path(root: &Path, pane: &str) -> Result<PathBuf> {
    Ok(dir(root)?.join(store::sanitize(pane) + ".lock"))
}

fn io(e: std::io::Error) -> Error {
    Error::failed(format!("mailbox: {e}"))
}

/// Append under the mailbox's lock, synced; the message's ID. A torn last line (a crash
/// mid-write) is closed with a newline first, so it never swallows this one.
pub fn append(root: &Path, to: &str, mut msg: Message) -> Result<u64> {
    let _held = Lock::acquire(&lock_path(root, to)?)?;
    let mut f = OpenOptions::new().create(true).append(true).read(true).mode(0o600).open(box_path(root, to)?).map_err(io)?;
    let mut len = f.metadata().map_err(io)?.len();
    if len > 0 {
        let mut last = [0u8; 1];
        f.read_exact_at(&mut last, len - 1).map_err(io)?;
        if last[0] != b'\n' {
            f.write_all(b"\n").map_err(io)?;
            len += 1;
        }
    }
    msg.id = len;
    let line = serde_json::to_string(&msg).map_err(|e| Error::failed(e.to_string()))? + "\n";
    f.write_all(line.as_bytes()).map_err(io)?;
    f.sync_data().map_err(io)?;
    Ok(len)
}

pub fn read_mark(root: &Path, pane: &str) -> u64 {
    mark_path(root, pane).ok().and_then(|p| fs::read_to_string(p).ok()).and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

/// Messages from `since`, and the offset just past the last whole line read.
pub fn read(root: &Path, pane: &str, since: Since) -> Result<(Vec<Message>, u64)> {
    let (start, skip_first) = match since {
        Since::All => (0, false),
        Since::Unread => (read_mark(root, pane), false),
        Since::After(id) => (id, true),
    };
    let mut f = match fs::File::open(box_path(root, pane)?) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], 0)),
        Err(e) => return Err(io(e)),
    };
    let len = f.metadata().map_err(io)?.len();
    if start >= len {
        return Ok((vec![], len));
    }
    f.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).map_err(io)?;
    let (mut out, mut at, mut first) = (Vec::new(), 0usize, true);
    // whole lines only: one still being written is left for the next read
    while let Some(nl) = buf[at..].iter().position(|b| *b == b'\n') {
        let line = &buf[at..at + nl];
        let skip = first && skip_first;
        first = false;
        at += nl + 1;
        if skip || line.is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_slice::<Message>(line) {
            out.push(m);
        }
    }
    Ok((out, start + at as u64))
}

/// Move the read mark forward to `to` (never back).
pub fn mark_read(root: &Path, pane: &str, to: u64) -> Result<()> {
    let _held = Lock::acquire(&lock_path(root, pane)?)?;
    if to > read_mark(root, pane) {
        store::atomic_write(&mark_path(root, pane)?, format!("{to}\n").as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_read_mark() {
        let d = std::env::temp_dir().join(format!("iterm-mail-{}", std::process::id()));
        let m = |t: &str| Message { id: 0, at: 1, from: "p1".into(), from_name: None, to: "p2".into(), text: t.into() };
        let a = append(&d, "p2", m("one")).unwrap();
        let b = append(&d, "p2", m("two")).unwrap();
        assert!(b > a);
        let (all, next) = read(&d, "p2", Since::Unread).unwrap();
        assert_eq!(all.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        mark_read(&d, "p2", next).unwrap();
        assert!(read(&d, "p2", Since::Unread).unwrap().0.is_empty());
        assert_eq!(read(&d, "p2", Since::After(a)).unwrap().0[0].text, "two");
        mark_read(&d, "p2", 0).unwrap(); // never back
        assert_eq!(read_mark(&d, "p2"), next);
        let c = append(&d, "p2", m("three")).unwrap();
        let sent = sent(&d, "p1").unwrap();
        assert_eq!(sent.iter().map(|(m, r)| (m.id, *r)).collect::<Vec<_>>(), [(a, true), (b, true), (c, false)]);
        assert_eq!(who(Some("stamp-golden"), "p16"), "stamp-golden (p16)");
        assert_eq!(who(None, "p16"), "p16");
    }
}
