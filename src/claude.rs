//! A Claude agent's usage, from Claude Code's own records (as the vterm CLI's sense.rs reads them): the session
//! record of the Claude process running under the pane (`<claude home>/sessions/<pid>.json`), then the context
//! size of its transcript's last turn (input + cache reads + cache writes), read from the file's tail. Read when
//! asked (`agent get`), never polled.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

pub fn home() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude"),
    }
}

/// The parent of process `pid` (0 when unknown).
fn parent(pid: i32) -> i32 {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    if n == size { info.pbi_ppid as i32 } else { 0 }
}

/// Whether `pid` is `root` or runs under it.
fn under(mut pid: i32, root: i32) -> bool {
    for _ in 0..16 {
        if pid == root {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        pid = parent(pid);
    }
    false
}

fn dashed(path: &str) -> String {
    path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The Claude session running under process `root` (the pane's shell): its id and working directory, from
/// Claude Code's session records.
pub fn session_under(root: i32) -> Option<(String, String)> {
    let r = record_under(root)?;
    let sid = r.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))?;
    let cwd = r.get("cwd").and_then(Value::as_str).unwrap_or("");
    Some((sid.to_owned(), cwd.to_owned()))
}

/// The session's name (`name` in its record: what SendMessage's `to` takes) under process `root`.
pub fn session_name_under(root: i32) -> Option<String> {
    record_under(root)?.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_owned)
}

/// The session record (`<claude home>/sessions/<pid>.json`) of the Claude process running under `root`.
fn record_under(root: i32) -> Option<serde_json::Map<String, Value>> {
    for e in std::fs::read_dir(home().join("sessions")).ok()?.flatten() {
        let Ok(Value::Object(r)) = serde_json::from_slice::<Value>(&std::fs::read(e.path()).unwrap_or_default()) else { continue };
        let pid = r.get("pid").and_then(Value::as_i64).unwrap_or(0) as i32;
        if pid > 0 && under(pid, root) {
            return Some(r);
        }
    }
    None
}

/// The transcript of the Claude session running under process `root`.
fn transcript_under(root: i32) -> Option<PathBuf> {
    let (sid, cwd) = session_under(root)?;
    let path = home().join("projects").join(dashed(&cwd)).join(format!("{sid}.jsonl"));
    path.is_file().then_some(path)
}

#[derive(Debug, Default, PartialEq)]
pub struct Usage {
    /// The context its last turn held.
    pub tokens: i64,
    pub model: Option<String>,
}

impl Usage {
    /// The share of the model's window (1M for a `[1m]` model, else 200k), in percent.
    pub fn percent(&self) -> i64 {
        let window = if self.model.as_deref().is_some_and(|m| m.contains("[1m]")) { 1_000_000 } else { 200_000 };
        self.tokens * 100 / window
    }
}

const USAGE_TAIL_MAX: u64 = 64 << 20;

/// The last assistant turn's usage in a transcript, from its tail (256 KiB, more only if needed).
pub fn usage_of(path: &Path) -> Option<Usage> {
    let mut f = File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let mut chunk: u64 = 262_144;
    loop {
        let start = size.saturating_sub(chunk);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::with_capacity((size - start) as usize);
        (&mut f).take(size - start).read_to_end(&mut buf).ok()?;
        for line in buf.split(|b| *b == b'\n').rev() {
            // only assistant turns carry usage
            if !line.windows(7).any(|w| w == b"\"usage\"") {
                continue;
            }
            let Ok(Value::Object(e)) = serde_json::from_slice::<Value>(line) else { continue };
            if e.get("type").and_then(Value::as_str) != Some("assistant") || e.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(m) = e.get("message").and_then(Value::as_object) else { continue };
            let Some(u) = m.get("usage").and_then(Value::as_object) else { continue };
            let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
            return Some(Usage {
                tokens: n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens"),
                model: m.get("model").and_then(Value::as_str).map(str::to_string),
            });
        }
        // bounded: `agent get` reads at most the last 64 MiB of a transcript (they reach hundreds of MiB)
        if start == 0 || chunk >= USAGE_TAIL_MAX {
            return None;
        }
        chunk = (chunk * 4).min(USAGE_TAIL_MAX);
    }
}

/// The usage of the Claude agent under the pane whose process is `pane_pid`.
pub fn usage(pane_pid: i32) -> Option<Usage> {
    usage_of(&transcript_under(pane_pid)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_main_turn_counts() {
        let dir = std::env::temp_dir().join(format!("pt-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let text = [
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":5,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0}}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_read_input_tokens":40000,"cache_creation_input_tokens":8}}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":999999}}}"#,
            r#"{"type":"user","message":{"content":"hi"}}"#,
        ]
        .join("\n");
        std::fs::write(&path, text).unwrap();
        let u = usage_of(&path).unwrap();
        assert_eq!(u, Usage { tokens: 40010, model: Some("claude-opus-5-5".into()) });
        assert_eq!(u.percent(), 20);
        assert!(under(std::process::id() as i32, std::process::id() as i32));
        assert!(under(std::process::id() as i32, parent(std::process::id() as i32)));
    }
}
