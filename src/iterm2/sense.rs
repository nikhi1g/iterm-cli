//! What runs in a pane, read-only and native: a process's exec path, arguments and environment (one
//! `sysctl(KERN_PROCARGS2)`, no `ps`), and Claude Code's own session record (`~/.claude/sessions/<pid>.json`: its
//! session ID, name and busy/idle status). Nothing here writes under `~/.claude` or reads its `.key` files.

use std::path::PathBuf;

use serde_json::Value;

use crate::app::agent::{provider_of, Provider};

/// A process as the kernel kept it at exec.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Proc {
    pub exec: Vec<u8>,
    pub argv: Vec<Vec<u8>>,
    /// The `KEY=VALUE` strings, NUL-separated: parsed only when asked, and a pane's shell never is.
    pub env: Vec<u8>,
}

impl Proc {
    pub fn provider(&self) -> Option<Provider> {
        provider_of(&self.exec, self.argv.iter().map(Vec::as_slice))
    }

    pub fn env(&self, key: &str) -> Option<String> {
        self.env.split(|b| *b == 0).find_map(|s| s.strip_prefix(key.as_bytes())?.strip_prefix(b"=")).map(|v| String::from_utf8_lossy(v).into_owned())
    }

    /// `--flag VALUE` or `--flag=VALUE` in the arguments.
    pub fn flag(&self, flag: &str) -> Option<String> {
        let mut it = self.argv.iter().skip(1);
        while let Some(a) = it.next() {
            if a == flag.as_bytes() {
                return it.next().map(|v| String::from_utf8_lossy(v).into_owned());
            }
            if let Some(v) = a.strip_prefix(flag.as_bytes()).and_then(|r| r.strip_prefix(b"=")) {
                return Some(String::from_utf8_lossy(v).into_owned());
            }
        }
        None
    }
}

/// Parses a KERN_PROCARGS2 buffer: argc (i32), the exec path, NUL padding, argc arguments, then `KEY=VALUE` strings
/// until an empty one.
pub fn parse_procargs(buf: &[u8]) -> Option<Proc> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let rest = &buf[4..];
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    let exec = rest[..end].to_vec();
    let mut i = end;
    while i < rest.len() && rest[i] == 0 {
        i += 1;
    }
    let mut strings = rest[i..].split(|b| *b == 0);
    // never trust argc for a reservation: each argument takes at least its NUL
    let mut argv = Vec::with_capacity(argc.min(rest.len()));
    let mut used = 0;
    for _ in 0..argc {
        let a = strings.next()?;
        used += a.len() + 1;
        argv.push(a.to_vec());
    }
    let rest = rest.get(i + used..).unwrap_or_default();
    let end = if rest.first() == Some(&0) { 0 } else { rest.windows(2).position(|w| w == [0, 0]).map_or(rest.len(), |n| n + 1) };
    Some(Proc { exec, argv, env: rest[..end].to_vec() })
}

/// The process `pid` (same user only), or None when it is gone or not readable.
#[cfg(target_os = "macos")]
pub fn proc_of(pid: i32) -> Option<Proc> {
    if pid <= 0 {
        return None;
    }
    // KERN_ARGMAX is 1 MiB on macOS; a pane's job rarely needs more than a few KiB, so try small first
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    BUF.with(|b| read_procargs(pid, &mut b.borrow_mut()))
}

/// One sysctl into a buffer kept between panes (a snapshot reads every pane's process).
#[cfg(target_os = "macos")]
fn read_procargs(pid: i32, buf: &mut Vec<u8>) -> Option<Proc> {
    for cap in [16 * 1024usize, 1024 * 1024] {
        if buf.len() < cap {
            buf.resize(cap, 0);
        }
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: buf has `size` writable bytes; the kernel writes at most that and updates size.
        let ok = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) };
        if ok == 0 {
            return parse_procargs(&buf[..size]);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return None;
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
pub fn proc_of(_pid: i32) -> Option<Proc> {
    None
}

/// Claude Code's record of a running session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeRecord {
    pub session: String,
    pub name: String,
    /// `busy`, `idle`, or another word a later Claude Code adds.
    pub status: String,
}

pub fn claude_home() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base),
        None => crate::conn::home().join(".claude"),
    }
}

pub fn parse_record(text: &str) -> Option<ClaudeRecord> {
    let v: Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    Some(ClaudeRecord { session: s("sessionId"), name: s("name"), status: s("status") })
}

/// The record of the Claude process `pid` (one small file read).
pub fn claude_record(pid: i32) -> Option<ClaudeRecord> {
    if pid <= 0 {
        return None;
    }
    let text = std::fs::read_to_string(claude_home().join("sessions").join(format!("{pid}.json"))).ok()?;
    parse_record(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procargs_parse_into_exec_argv_and_env() {
        let mut b = 3i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/Users/x/.local/share/claude/versions/2.1.316\0\0\0claude\0--model\0opus\0CLAUDE_EFFORT=high\0A=b=c\0\0junk=1\0");
        let p = parse_procargs(&b).unwrap();
        assert_eq!(p.provider(), Some(Provider::Claude));
        assert_eq!(p.flag("--model").as_deref(), Some("opus"));
        assert_eq!(p.env("CLAUDE_EFFORT").as_deref(), Some("high"));
        assert_eq!(p.env("A").as_deref(), Some("b=c"));
        assert_eq!(p.env("junk"), None);
        let mut b = 2i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/opt/homebrew/bin/node\0node\0/opt/homebrew/lib/node_modules/@openai/codex/bin/codex.js\0\0");
        assert_eq!(parse_procargs(&b).unwrap().provider(), Some(Provider::Codex));
        let mut b = 1i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/bin/zsh\0-zsh\0");
        assert_eq!(parse_procargs(&b).unwrap().provider(), None);
        // a variable's prefix isn't the variable; the junk past the empty string isn't one
        let mut b = 1i32.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/bin/x\0x\0AB=1\0A=2\0\0J=3\0");
        let p = parse_procargs(&b).unwrap();
        assert_eq!((p.env("A").as_deref(), p.env("AB").as_deref(), p.env("J")), (Some("2"), Some("1"), None));
    }

    // regression: argc sized a reservation (48 GiB here) before anything was checked
    #[test]
    fn a_bad_argc_is_not_a_reservation() {
        let mut b = i32::MAX.to_ne_bytes().to_vec();
        b.extend_from_slice(b"/bin/x\0x\0");
        assert_eq!(parse_procargs(&b), None);
    }

    #[test]
    fn claude_records_parse() {
        let r = parse_record(r#"{"pid":1,"sessionId":"24c1","name":"iterm-cli-a5","status":"busy"}"#).unwrap();
        assert_eq!(r, ClaudeRecord { session: "24c1".into(), name: "iterm-cli-a5".into(), status: "busy".into() });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_is_readable() {
        let p = proc_of(std::process::id() as i32).unwrap();
        assert!(!p.exec.is_empty() && !p.argv.is_empty());
    }
}
