//! Durable CLI state under `Env::state` (mailboxes, task boards, queues, paused agents): private
//! files (0600) in private directories (0700), written atomically (temp file + rename), with
//! `flock` locks where several writers meet.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::out::{Error, Result};

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn dir(root: &Path, parts: &[&str]) -> Result<PathBuf> {
    let mut p = root.to_path_buf();
    for x in parts {
        p.push(x);
    }
    // chmod only what this call creates: a chmod on every call woke every waiter watching the
    // directory, and two `inbox --wait`s then woke each other forever
    if !p.is_dir() {
        fs::create_dir_all(&p).map_err(|e| Error::failed(format!("{}: {e}", p.display())))?;
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
    }
    Ok(p)
}

/// The default state folder holds other agents' messages and tasks: private to this user (a folder made before this
/// rule was 0755). A folder the caller named with `ITERM_STATE` is the caller's to set.
pub fn private_default(state: &Path) {
    if state != crate::conn::default_state() || std::env::var_os(crate::conn::ENV_STATE).is_some_and(|s| !s.is_empty()) {
        return;
    }
    if fs::metadata(state).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
        let _ = fs::set_permissions(state, fs::Permissions::from_mode(0o700));
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let io = |e: std::io::Error| Error::failed(format!("{}: {e}", path.display()));
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    fs::rename(&tmp, path).map_err(io)
}

/// An exclusive `flock` on `path` (created 0600), released on drop. Waited for without a bound, on purpose: every
/// lock guards a write (a ledger record, a queue, a mailbox, a board, a budget), and a write must never be skipped
/// or half done because another command held the file (one that gave up would lose the record of panes it already
/// made, or let a budget through). Locks are held for a file update, except the launch queue, held while it starts
/// its queued panes.
pub struct Lock {
    _f: File,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Lock> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::failed(format!("{}: {e}", path.display())))?;
        // SAFETY: a valid open fd.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Error::failed(format!("lock {}: {}", path.display(), std::io::Error::last_os_error())));
        }
        Ok(Lock { _f: f })
    }
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// A file name from a pane handle or agent name (`/` and dots never reach the path).
pub fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_lock_is_waited_for_never_given_up() {
        // regression (review of the hang fix): a lock that gave up after 10 s made writes fail or be skipped (the
        // ledger of panes already made, the budget check failing open); a write waits for the lock instead
        let dir = std::env::temp_dir().join(format!("pt-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.lock");
        let held = Lock::acquire(&path).unwrap();
        let p2 = path.clone();
        let t = std::time::Instant::now();
        let waiter = std::thread::spawn(move || Lock::acquire(&p2).map(|_| ()).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(held);
        assert!(waiter.join().unwrap(), "the waiter got the lock");
        assert!(t.elapsed() >= std::time::Duration::from_millis(200));
    }
}
