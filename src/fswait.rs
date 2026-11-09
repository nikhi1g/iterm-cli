//! Block until a file or directory changes, without polling: one kqueue with an `EVFILT_VNODE`
//! filter per path (write, extend, rename, delete). Not attribute changes: a chmod or touch
//! by another waiter would wake this one, and two waiters woke each other forever (a busy spin). Used by `task watch|wait` (the board
//! is rewritten by rename, so its directory is watched) and `agent inbox --wait` (appends).
//!
//! Register first, then check the condition, then wait: a change between the check and the wait
//! is still delivered, so no wakeup is lost.

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

pub struct Watcher {
    kq: i32,
    _files: Vec<std::fs::File>,
}

impl Watcher {
    /// Paths that do not exist are skipped (watch their directory instead).
    pub fn new(paths: &[&Path]) -> std::io::Result<Watcher> {
        // SAFETY: kqueue has no preconditions; Drop closes the fd.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut files = Vec::new();
        let mut changes = Vec::new();
        for p in paths {
            let Ok(f) = std::fs::File::open(p) else { continue };
            changes.push(libc::kevent {
                ident: f.as_raw_fd() as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_RENAME | libc::NOTE_DELETE,
                data: 0,
                udata: std::ptr::null_mut(),
            });
            files.push(f);
        }
        let w = Watcher { kq, _files: files };
        // SAFETY: a valid change list; no events are returned by a registration-only call.
        let n = unsafe { libc::kevent(kq, changes.as_ptr(), changes.len() as i32, std::ptr::null_mut(), 0, std::ptr::null()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(w)
    }

    /// True when something changed, false when `timeout` passed.
    pub fn wait(&self, timeout: Option<Duration>) -> std::io::Result<bool> {
        let ts = timeout.map(|d| libc::timespec { tv_sec: d.as_secs() as libc::time_t, tv_nsec: d.subsec_nanos() as libc::c_long });
        // SAFETY: zeroed kevent is a valid out-buffer.
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: pointers to live locals.
        let n = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut out, 1, ts.as_ref().map_or(std::ptr::null(), |t| t as *const _)) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            return if err.kind() == std::io::ErrorKind::Interrupted { Ok(true) } else { Err(err) };
        }
        Ok(n > 0)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: our own fd.
        unsafe { libc::close(self.kq) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_and_chmods_do_not_wake_the_watcher() {
        let p = std::env::temp_dir().join(format!("iterm-fswait-{}", std::process::id()));
        std::fs::write(&p, "x\n").unwrap();
        let w = Watcher::new(&[p.as_path()]).unwrap();
        std::thread::sleep(Duration::from_millis(1100)); // past atime granularity
        let _ = std::fs::read(&p).unwrap();
        assert!(!w.wait(Some(Duration::from_millis(200))).unwrap(), "a read woke the watcher");
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        assert!(!w.wait(Some(Duration::from_millis(200))).unwrap(), "a chmod woke the watcher");
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"y\n").unwrap();
        assert!(w.wait(Some(Duration::from_millis(200))).unwrap(), "an append did not wake it");
        let _ = std::fs::remove_file(&p);
    }
}
