//! iTerm2: send, key, read, clear, and the delivery engine (`Op::Deliver`). Owner: the `talk` agent.
//!
//! The delivery engine is perf term's (`perfterm/app/src/api/deliver.rs`), rebuilt on iTerm2's `SendText`, with the
//! CLI's state folder standing in for the app's memory, because every `iterm` is a short-lived process:
//! - one writer per pane: an exclusive `flock` on `STATE/talk/<session>.lock` from the first byte to the Enter, so
//!   messages from many senders arrive whole and in order;
//! - one bracketed paste for a program reading its terminal raw (shells' line editors, Claude Code, Codex, OMP);
//!   lines under the kernel's 1 KiB limit for a canonical-mode reader (termios of the pane's tty); never truncated,
//!   any size (SendText in pipelined chunks, which iTerm2 writes to the PTY in order);
//! - Enter is a separate send, once the text shows (the screen changed: iTerm2's screen-update notifications, a
//!   bounded wait) and the screen then went quiet for 150 ms (at most 1 s);
//! - the hard rail: a pane showing a permission or trust prompt (`app::agent::classify`) gets nothing. Its message is
//!   held in `STATE/talk/<session>.held` (one JSON line each, in arrival order) and the reply says
//!   `queued reason=prompt`; one detached drainer per pane (this binary, started with [`DRAIN_ENV`]) delivers them
//!   once the prompt is gone, the prompt checked again under the lock before each. Only the human answers a prompt.
//!
//! `Send` and `Key` keep the same rail: refused while a prompt shows (Esc and Ctrl-C, which only cancel, pass).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::app::agent::{self, Screen};
use crate::app::wire::{Key, KeyCode, Lines};
use crate::app::{Done, Fail, FailCode, Op};

use super::proto::{self as p, client_originated_message::Submessage as Req, server_originated_message::Submessage as Resp};
use super::shape::session_of;
use super::{unsupported, Live};

/// The kernel keeps at most MAX_CANON (1024) bytes of one line for a canonical-mode reader; lines stay under it.
pub const LINE_MAX: usize = 1000;
/// One SendText request at most this many bytes (pipelined; iTerm2 queues them to the PTY in order).
const CHUNK: usize = 16 * 1024;
/// How long a held message waits for its prompt to go.
const HOLD: Duration = Duration::from_secs(30 * 60);
/// The screen counts as quiet after this long without an update.
const QUIET: Duration = Duration::from_millis(150);
/// The longest settle before Enter.
const SETTLE_MAX: Duration = Duration::from_secs(1);
/// A screen re-read when no update notification came for this long (a backstop, not the signal).
const RECHECK: Duration = Duration::from_millis(100);
/// The reply while a message is held.
pub const QUEUED: &str = "queued reason=prompt";
/// The environment variable that turns this binary into a pane's drainer (its value: the session).
pub const DRAIN_ENV: &str = "ITERM_TALK_DRAIN";

fn fail(code: FailCode, m: impl Into<String>) -> Fail {
    Fail::new(code, m)
}

fn internal(m: impl Into<String>) -> Fail {
    Fail::new(FailCode::Internal, m)
}

pub fn call(live: &mut Live, op: Op, wait: Option<Duration>) -> Result<Done, Fail> {
    match op {
        Op::Send { pane, bytes, paste } => {
            let uuid = session_of(live, pane)?;
            let text = String::from_utf8(bytes).map_err(|_| fail(FailCode::Invalid, "iTerm2 takes text: the bytes are not UTF-8"))?;
            send(live, &uuid, &text, paste)?;
            Ok(Done { ids: vec![pane], lines: vec![] })
        }
        Op::Key { pane, key } => {
            let uuid = session_of(live, pane)?;
            let bytes = key_bytes(key)?;
            if !cancels(key) {
                refuse_prompt(live, &uuid)?;
            }
            send_text(live, &uuid, &String::from_utf8_lossy(&bytes))?;
            Ok(Done { ids: vec![pane], lines: vec![] })
        }
        Op::Read { pane, lines } => {
            let uuid = session_of(live, pane)?;
            Ok(Done { ids: vec![], lines: read(live, &uuid, lines)? })
        }
        Op::Clear { pane, scrollback } => {
            let uuid = session_of(live, pane)?;
            inject(live, &uuid, clear_bytes(scrollback))?;
            Ok(Done { ids: vec![pane], lines: vec![] })
        }
        Op::Scroll { .. } => Err(unsupported("scroll (iTerm2's API cannot move a pane's view; `content select` scrolls a match into view)")),
        Op::Deliver { pane, text, enter } => {
            let uuid = session_of(live, pane)?;
            let line = deliver(live, &uuid, &text, enter, wait.unwrap_or(Duration::from_secs(120)))?;
            Ok(Done { ids: vec![pane], lines: vec![line] })
        }
        other => Err(unsupported(format!("{other:?}").split([' ', '{', '(']).next().unwrap_or("op"))),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Pure parts: key encoding, paste plans, screen fingerprints (tested without iTerm2).

/// Esc and Ctrl-C only cancel: they pass the prompt rail.
pub fn cancels(key: Key) -> bool {
    let plain = !key.mods.shift && !key.mods.alt && !key.mods.cmd;
    plain && (key.code == KeyCode::Esc && !key.mods.ctrl || key.code == KeyCode::Char('c') && key.mods.ctrl || key.code == KeyCode::Char('C') && key.mods.ctrl)
}

/// The control byte for Ctrl+<c>.
pub fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// The bytes iTerm2 sends for a key press (xterm's legacy encoding, normal cursor keys, Option as Esc+): what
/// shells, Claude Code, Codex, OMP and vim read with the kitty keyboard protocol off. Cmd keys are menu shortcuts.
pub fn key_bytes(key: Key) -> Result<Vec<u8>, Fail> {
    let m = key.mods;
    if m.cmd {
        return Err(fail(FailCode::Invalid, "cmd keys are menu shortcuts, not input: use `app menu` or `action run`"));
    }
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let modified = param > 1;
    let mut out = Vec::with_capacity(8);
    let letter = |out: &mut Vec<u8>, l: u8, ss3: bool| {
        if modified {
            out.extend_from_slice(format!("\x1b[1;{param}").as_bytes());
            out.push(l);
        } else if ss3 {
            out.extend_from_slice(b"\x1bO");
            out.push(l);
        } else {
            out.extend_from_slice(b"\x1b[");
            out.push(l);
        }
    };
    let tilde = |out: &mut Vec<u8>, n: u8| {
        out.extend_from_slice(format!("\x1b[{n}").as_bytes());
        if modified {
            out.extend_from_slice(format!(";{param}").as_bytes());
        }
        out.push(b'~');
    };
    let alt = |out: &mut Vec<u8>| {
        if m.alt {
            out.push(0x1b)
        }
    };
    match key.code {
        KeyCode::Char(c) => {
            alt(&mut out);
            if m.ctrl {
                out.push(ctrl_byte(c).ok_or_else(|| fail(FailCode::Invalid, format!("ctrl-{c} has no control byte")))?);
            } else {
                let c = if m.shift { c.to_uppercase().next().unwrap_or(c) } else { c };
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
        KeyCode::Enter => {
            alt(&mut out);
            out.push(b'\r');
        }
        KeyCode::Tab if m.shift => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Tab => {
            alt(&mut out);
            out.push(b'\t');
        }
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Backspace => {
            alt(&mut out);
            out.push(if m.ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Delete => tilde(&mut out, 3),
        KeyCode::Up => letter(&mut out, b'A', false),
        KeyCode::Down => letter(&mut out, b'B', false),
        KeyCode::Right => letter(&mut out, b'C', false),
        KeyCode::Left => letter(&mut out, b'D', false),
        KeyCode::Home => letter(&mut out, b'H', false),
        KeyCode::End => letter(&mut out, b'F', false),
        KeyCode::PageUp => tilde(&mut out, 5),
        KeyCode::PageDown => tilde(&mut out, 6),
        KeyCode::F(n @ 1..=4) => letter(&mut out, b'P' + (n - 1), true),
        KeyCode::F(n @ 5..=20) => {
            const CODES: [u8; 16] = [15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34];
            tilde(&mut out, CODES[(n - 5) as usize]);
        }
        KeyCode::F(n) => return Err(fail(FailCode::Invalid, format!("f{n}: F1 to F20"))),
    }
    Ok(out)
}

/// Pasted text: bracketed for a program that reads raw (an embedded end marker removed so the paste can't escape
/// the bracket), else newlines as carriage returns, as a keyboard sends them.
pub fn paste_text(text: &str, bracketed: bool) -> String {
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

/// What a delivery writes.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub text: String,
    /// Lines longer than a canonical-mode reader takes, broken in two (counted, so the sender knows).
    pub wrapped: usize,
}

/// The text for a delivery: one bracketed paste for a raw reader; for a line reader, lines under its limit (broken
/// at a space where there is one), nothing lost.
pub fn plan(text: &str, canonical: bool) -> Plan {
    if !canonical {
        return Plan { text: paste_text(text, true), wrapped: 0 };
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut wrapped = 0;
    for (k, line) in text.split('\n').enumerate() {
        if k > 0 {
            out.push('\n');
        }
        let mut rest = line;
        while rest.len() > LINE_MAX {
            let mut cut = LINE_MAX;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let cut = rest[..cut].rfind(' ').filter(|&i| i > LINE_MAX / 2).map_or(cut, |i| i + 1);
            out.push_str(&rest[..cut]);
            out.push('\n');
            wrapped += 1;
            rest = &rest[cut..];
        }
        out.push_str(rest);
    }
    Plan { text: out, wrapped }
}

/// `s` in pieces of at most `max` bytes, cut on character boundaries.
pub fn chunks(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::with_capacity(s.len() / max + 1);
    let mut rest = s;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest);
    }
    out
}

/// A fingerprint of the visible screen and cursor: it changes when the message shows.
pub fn fingerprint(lines: &[String], cursor: (i64, i64)) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for l in lines {
        for b in l.bytes().chain(std::iter::once(b'\n')) {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    (h ^ ((cursor.0 as u64) << 32 | (cursor.1 as u64 & 0xffff_ffff))).wrapping_mul(0x0100_0000_01b3)
}

/// OMP's dialog for a large paste ("Pasted N lines", "⎋ to paste inline").
pub fn paste_dialog(lines: &[String]) -> bool {
    lines.iter().any(|l| l.contains("to paste inline")) && lines.iter().any(|l| l.contains("Pasted "))
}

/// The last 16 non-blank characters of a message: what shows at the end of its echo, however the screen wraps it.
pub fn tail_of(text: &str) -> String {
    let t: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    t[t.len().saturating_sub(16)..].iter().collect()
}

/// A row that stands for a paste: Claude's "[Pasted text #1 +1558 lines]", OMP's "Pasted 1559 lines" dialog, its
/// "📄 #2" chip and "+1559 lines" preview.
fn paste_marker(l: &str) -> bool {
    if l.contains("[Pasted") || l.contains("📄 #") {
        return true;
    }
    // "+N lin" or "Pasted N lin"
    l.match_indices(" lin").any(|(i, _)| {
        let head = l[..i].trim_end_matches(|c: char| c.is_ascii_digit());
        head.len() < i && (head.ends_with('+') || head.ends_with("Pasted "))
    })
}

/// How many signs of a delivered message the screen shows: its tail (rows joined, blanks and borders off, so a
/// wrapped echo counts) and paste markers. More than before the send: the message landed.
pub fn signs(lines: &[String], tail: &str) -> usize {
    let joined: String = lines.iter().flat_map(|l| l.chars()).filter(|c| !c.is_whitespace() && !matches!(c, '│' | '┃')).collect();
    let echoed = if tail.is_empty() { 0 } else { joined.matches(tail).count() };
    echoed + lines.iter().filter(|l| paste_marker(l)).count()
}

/// Whether a screen line is the echo of text the delivery typed: a suffix of it (at least 8 characters and 3/5 of the
/// row, borders and blanks off) is in `typed`, as a shell prompt's command line or an agent's input box shows it.
fn ours(line: &str, typed: &str) -> bool {
    let l = line.trim_end_matches([' ', '│', '┃', '|']).trim_start();
    // most of the row: a real prompt whose words the message merely quotes stays a prompt
    let need = (l.chars().count() * 3 / 5).max(8);
    for (i, _) in l.char_indices() {
        let s = &l[i..];
        if s.chars().count() < need {
            break;
        }
        if typed.contains(s) {
            return true;
        }
    }
    false
}

/// Whether the screen shows a permission or trust prompt, not counting rows that only echo the delivery's own text
/// (a message that quotes "Allow command? [y/N]" must not hold itself).
pub fn prompt_except(lines: &[String], typed: &str) -> bool {
    let one = |l: &str| agent::classify(std::iter::once(l)) == Screen::Prompt;
    let kept = lines.iter().map(String::as_str).filter(|l| typed.is_empty() || !(one(l) && ours(l, typed)));
    agent::classify(kept) == Screen::Prompt
}

/// Erase the screen (and the scrollback), as the program would by printing it.
pub fn clear_bytes(scrollback: bool) -> &'static [u8] {
    if scrollback {
        b"\x1b[H\x1b[2J\x1b[3J\x1b]1337;ClearScrollback\x07"
    } else {
        b"\x1b[H\x1b[2J"
    }
}

/// Buffer rows as text: NUL padding removed, right-trimmed.
pub fn buffer_lines(contents: &[p::LineContents]) -> Vec<String> {
    contents.iter().map(|l| l.text.as_deref().unwrap_or("").replace('\0', "").trim_end().to_string()).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// iTerm2 requests.

fn send_req(uuid: &str, text: &str) -> Req {
    Req::SendTextRequest(p::SendTextRequest { session: Some(uuid.into()), text: Some(text.into()), suppress_broadcast: Some(true) })
}

fn check_sent(r: Result<Resp, crate::out::Error>) -> Result<(), Fail> {
    match r.map_err(Fail::from)? {
        Resp::SendTextResponse(r) if r.status.unwrap_or(0) == 0 => Ok(()),
        Resp::SendTextResponse(_) => Err(fail(FailCode::NotFound, "the pane is gone")),
        _ => Err(internal("unexpected reply to send text")),
    }
}

/// Text into the pane's PTY, in pipelined chunks (one round trip whatever the size).
fn send_text(live: &mut Live, uuid: &str, text: &str) -> Result<(), Fail> {
    if text.is_empty() {
        return Ok(());
    }
    let reqs: Vec<Req> = chunks(text, CHUNK).into_iter().map(|c| send_req(uuid, c)).collect();
    for r in live.api.call_many(reqs).map_err(Fail::from)? {
        check_sent(r)?;
    }
    Ok(())
}

fn screen_req(uuid: &str) -> Req {
    Req::GetBufferRequest(p::GetBufferRequest { session: Some(uuid.into()), line_range: Some(p::LineRange { screen_contents_only: Some(true), ..Default::default() }), include_styles: Some(false) })
}

/// The screen's rows and the cursor (column, absolute row).
struct Shot {
    lines: Vec<String>,
    cursor: (i64, i64),
}

impl Shot {
    fn screen(&self) -> Screen {
        agent::classify(self.lines.iter().map(String::as_str))
    }
    /// A prompt shows that isn't the delivery's own text (`typed`) echoed on the screen.
    fn prompt(&self, typed: &str) -> bool {
        prompt_except(&self.lines, typed)
    }
    fn fp(&self) -> u64 {
        fingerprint(&self.lines, self.cursor)
    }
}

fn shot_of(r: Result<Resp, crate::out::Error>) -> Result<Shot, Fail> {
    match r.map_err(Fail::from)? {
        Resp::GetBufferResponse(b) if b.status.unwrap_or(0) == 0 => {
            let c = b.cursor.unwrap_or_default();
            Ok(Shot { lines: buffer_lines(&b.contents), cursor: (c.x.unwrap_or(0) as i64, c.y.unwrap_or(0)) })
        }
        Resp::GetBufferResponse(_) => Err(fail(FailCode::NotFound, "the pane is gone")),
        _ => Err(internal("unexpected reply to a buffer read")),
    }
}

fn shot(live: &mut Live, uuid: &str) -> Result<Shot, Fail> {
    shot_of(live.api.call(screen_req(uuid)))
}

/// The prompt rail for raw input: refused while a permission or trust prompt shows.
fn refuse_prompt(live: &mut Live, uuid: &str) -> Result<(), Fail> {
    if shot(live, uuid)?.screen() == Screen::Prompt {
        return Err(fail(FailCode::Invalid, "the pane shows a permission or trust prompt: only the human answers it (esc or ctrl-c cancel it; `pane send --text` holds a message until it's gone)"));
    }
    Ok(())
}

fn tty_req(uuid: &str) -> Req {
    Req::VariableRequest(p::VariableRequest { scope: Some(p::variable_request::Scope::SessionId(uuid.into())), get: vec!["tty".into()], set: vec![] })
}

/// The pane's terminal modes (canonical, echo) from its tty's termios; None when unknown.
fn modes_of(r: Result<Resp, crate::out::Error>) -> Option<(bool, bool)> {
    match r.ok()? {
        Resp::VariableResponse(v) => {
            let tty: String = serde_json::from_str(v.values.first()?).ok()?;
            termios(&tty)
        }
        _ => None,
    }
}

/// Whether the program on terminal `tty` reads it in canonical (line) mode, and whether it echoes. Only reads its
/// termios: opened without becoming a controlling terminal, never read from.
pub fn termios(tty: &str) -> Option<(bool, bool)> {
    if !tty.starts_with("/dev/tty") || tty.contains("..") {
        return None;
    }
    let path = std::ffi::CString::new(tty).ok()?;
    // SAFETY: a NUL-terminated path; the descriptor is closed below.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NOCTTY | libc::O_NONBLOCK) };
    if fd < 0 {
        return None;
    }
    // SAFETY: tcgetattr fills a zeroed termios we own.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::tcgetattr(fd, &mut t) };
    unsafe { libc::close(fd) };
    (rc == 0).then_some((t.c_lflag & libc::ICANON != 0, t.c_lflag & libc::ECHO != 0))
}

fn subscribe_req(uuid: &str, on: bool) -> Req {
    Req::NotificationRequest(p::NotificationRequest {
        session: Some(uuid.into()),
        subscribe: Some(on),
        notification_type: Some(p::NotificationType::NotifyOnScreenUpdate as i32),
        arguments: None,
    })
}

/// Whether the pane's screen updated within `timeout` (other notifications are dropped: this connection is the
/// command's own, and nothing else on it subscribes).
fn updated(live: &mut Live, uuid: &str, timeout: Duration) -> Result<bool, Fail> {
    let end = Instant::now() + timeout;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match live.api.wait_notification(left).map_err(Fail::from)? {
            Some(n) if n.screen_update_notification.as_ref().and_then(|s| s.session.as_deref()) == Some(uuid) => return Ok(true),
            Some(_) => continue,
            None => return Ok(false),
        }
    }
}

/// Until the screen stops changing for `QUIET` (at most `SETTLE_MAX`).
fn settle(live: &mut Live, uuid: &str) -> Result<(), Fail> {
    let end = Instant::now() + SETTLE_MAX;
    while Instant::now() < end {
        if !updated(live, uuid, QUIET.min(end.saturating_duration_since(Instant::now())))? {
            return Ok(());
        }
    }
    Ok(())
}

fn inject(live: &mut Live, uuid: &str, data: &[u8]) -> Result<(), Fail> {
    match live.api.call(Req::InjectRequest(p::InjectRequest { session_id: vec![uuid.into()], data: Some(data.to_vec()) })).map_err(Fail::from)? {
        Resp::InjectResponse(r) if r.status.iter().all(|s| *s == 0) => Ok(()),
        Resp::InjectResponse(_) => Err(fail(FailCode::NotFound, "the pane is gone")),
        _ => Err(internal("unexpected reply to inject")),
    }
}

fn read(live: &mut Live, uuid: &str, lines: Lines) -> Result<Vec<String>, Fail> {
    let range = match lines {
        Lines::Screen => p::LineRange { screen_contents_only: Some(true), ..Default::default() },
        Lines::Tail(n) => p::LineRange { trailing_lines: Some(n.min(i32::MAX as u32) as i32), ..Default::default() },
        Lines::Range(a, b) if b <= a => return Ok(vec![]),
        Lines::Range(a, b) => p::LineRange {
            windowed_coord_range: Some(p::WindowedCoordRange {
                coord_range: Some(p::CoordRange { start: Some(p::Coord { x: Some(0), y: Some(a as i64) }), end: Some(p::Coord { x: Some(0), y: Some(b as i64) }) }),
                columns: None,
            }),
            ..Default::default()
        },
    };
    match live.api.call(Req::GetBufferRequest(p::GetBufferRequest { session: Some(uuid.into()), line_range: Some(range), include_styles: Some(false) })).map_err(Fail::from)? {
        Resp::GetBufferResponse(b) if b.status.unwrap_or(0) == 0 => Ok(buffer_lines(&b.contents)),
        Resp::GetBufferResponse(b) if b.status == Some(2) => Err(fail(FailCode::Invalid, "rows outside the pane's history")),
        Resp::GetBufferResponse(_) => Err(fail(FailCode::NotFound, "the pane is gone")),
        _ => Err(internal("unexpected reply to a buffer read")),
    }
}

/// Raw input: text as is, or as a paste (bracketed for a raw reader). Refused while a prompt shows.
fn send(live: &mut Live, uuid: &str, text: &str, paste: bool) -> Result<(), Fail> {
    let replies = live.api.call_many(vec![screen_req(uuid), tty_req(uuid)]).map_err(Fail::from)?;
    let mut it = replies.into_iter();
    let s = shot_of(it.next().unwrap_or_else(|| Err(crate::out::Error::failed("no reply"))))?;
    if s.screen() == Screen::Prompt {
        return Err(fail(FailCode::Invalid, "the pane shows a permission or trust prompt: only the human answers it (esc or ctrl-c cancel it; `pane send --text` holds a message until it's gone)"));
    }
    let text = if paste {
        let canonical = it.next().and_then(modes_of).is_some_and(|m| m.0);
        paste_text(text, !canonical)
    } else {
        text.to_string()
    };
    send_text(live, uuid, &text)
}

// ---------------------------------------------------------------------------------------------------------------
// The delivery engine.

/// The CLI's per-pane delivery files: `STATE/talk/<session>.{lock,held,drain}`.
fn talk_file(state: &Path, uuid: &str, ext: &str) -> PathBuf {
    let safe: String = uuid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    state.join("talk").join(format!("{safe}.{ext}"))
}

/// An exclusive `flock`, released on drop.
pub struct Flock(File);

impl Flock {
    fn open(path: &Path) -> std::io::Result<File> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)
    }

    /// Waits for the lock until `deadline`.
    pub fn take(path: &Path, deadline: Instant) -> std::io::Result<Option<Flock>> {
        let f = Flock::open(path)?;
        let mut nap = Duration::from_millis(1);
        loop {
            // SAFETY: a valid fd owned by `f`.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(Flock(f)));
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(e);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(nap.min(deadline.saturating_duration_since(Instant::now())));
            nap = (nap * 2).min(Duration::from_millis(20));
        }
    }

    pub fn try_take(path: &Path) -> std::io::Result<Option<Flock>> {
        Flock::take(path, Instant::now())
    }
}

impl Drop for Flock {
    fn drop(&mut self) {
        // SAFETY: a valid fd.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A message held behind a prompt, until `until` (ms since the epoch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Held {
    pub text: String,
    pub enter: bool,
    pub until: u64,
    /// Text of this message already typed in (only its Enter is left): its echo on the screen is no prompt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub typed: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A pane's held messages, oldest first: one JSON line each, rewritten under the file's own lock.
pub struct HeldQueue {
    path: PathBuf,
}

impl HeldQueue {
    pub fn new(state: &Path, uuid: &str) -> HeldQueue {
        HeldQueue { path: talk_file(state, uuid, "held") }
    }

    fn edit<T>(&self, f: impl FnOnce(&mut Vec<Held>) -> T) -> std::io::Result<T> {
        let lock = Flock::take(&self.path, Instant::now() + Duration::from_secs(10))?.ok_or_else(|| std::io::Error::other("held queue busy"))?;
        let mut file = &lock.0;
        let mut text = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut text)?;
        let mut items: Vec<Held> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let before = items.clone();
        let out = f(&mut items);
        if items != before {
            let mut body = String::new();
            for m in &items {
                body.push_str(&serde_json::to_string(m).map_err(std::io::Error::other)?);
                body.push('\n');
            }
            lock.0.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(body.as_bytes())?;
        }
        Ok(out)
    }

    pub fn push(&self, m: Held) -> std::io::Result<usize> {
        self.edit(|q| {
            q.push(m);
            q.len()
        })
    }

    /// The oldest message still in time (expired ones are dropped).
    pub fn front(&self, now: u64) -> std::io::Result<Option<Held>> {
        self.edit(|q| {
            q.retain(|m| m.until > now);
            q.first().cloned()
        })
    }

    /// Removes the oldest message if it is still `m` (a message delivered).
    pub fn pop(&self, m: &Held) -> std::io::Result<()> {
        self.edit(|q| {
            if q.first() == Some(m) {
                q.remove(0);
            }
        })
    }

    pub fn len(&self) -> usize {
        std::fs::read_to_string(&self.path).map_or(0, |t| t.lines().filter(|l| !l.trim().is_empty()).count())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) -> std::io::Result<()> {
        self.edit(|q| q.clear())
    }
}

/// Holds a message behind a prompt and makes sure the pane's drainer runs.
fn hold(live: &Live, uuid: &str, text: &str, enter: bool, typed: &str) -> Result<String, Fail> {
    let q = HeldQueue::new(&live.env.state, uuid);
    q.push(Held { text: text.into(), enter, until: now_ms() + HOLD.as_millis() as u64, typed: typed.into() }).map_err(|e| internal(format!("holding the message: {e}")))?;
    start_drainer(&live.env.state, uuid);
    Ok(QUEUED.into())
}

/// Starts the pane's drainer unless one runs (its lock is taken). A drainer that finds the queue empty lets go of
/// its lock before it looks a last time, so a message pushed meanwhile is never stranded.
fn start_drainer(state: &Path, uuid: &str) {
    match Flock::try_take(&talk_file(state, uuid, "drain")) {
        Ok(Some(l)) => drop(l),
        _ => return,
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut c = std::process::Command::new(exe);
    c.env(DRAIN_ENV, uuid)
        .env(crate::conn::ENV_STATE, state)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe; the drainer outlives the command in its own session.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = c.spawn();
}

/// Delivers `text` (then Enter if asked) into the pane. The line: `delivered bytes=N [wrapped=N] ms=T`, or
/// `queued reason=prompt` while a prompt shows or earlier messages to the pane still wait behind one.
pub fn deliver(live: &mut Live, uuid: &str, text: &str, enter: bool, wait: Duration) -> Result<String, Fail> {
    let t0 = Instant::now();
    if !HeldQueue::new(&live.env.state, uuid).is_empty() {
        return hold(live, uuid, text, enter, "");
    }
    let lock_path = talk_file(&live.env.state, uuid, "lock");
    let _one = Flock::take(&lock_path, t0 + wait)
        .map_err(|e| internal(format!("delivery lock: {e}")))?
        .ok_or_else(|| fail(FailCode::Timeout, "not delivered: another message to the pane is still landing"))?;
    // under the lock: the message before this one may have brought a prompt up, or been held
    if !HeldQueue::new(&live.env.state, uuid).is_empty() {
        return hold(live, uuid, text, enter, "");
    }
    match land(live, uuid, text, "", enter, t0 + wait)? {
        Landed::Done(wrapped) => {
            let mut line = format!("delivered bytes={}", text.len());
            if wrapped > 0 {
                line.push_str(&format!(" wrapped={wrapped}"));
            }
            line.push_str(&format!(" ms={}", t0.elapsed().as_millis()));
            Ok(line)
        }
        Landed::Prompt => hold(live, uuid, text, enter, ""),
        // the text went in; a prompt came up before its Enter: the Enter waits for the prompt to go
        Landed::EnterHeld => hold(live, uuid, "", true, text),
    }
}

enum Landed {
    Done(usize),
    /// A prompt shows: nothing was written.
    Prompt,
    /// The text landed, then a prompt came up: Enter not sent.
    EnterHeld,
}

/// Writes the message and its Enter; the caller holds the pane's delivery lock.
/// `typed`: this message's text typed in by an earlier try (only its Enter is left).
fn land(live: &mut Live, uuid: &str, text: &str, typed: &str, enter: bool, deadline: Instant) -> Result<Landed, Fail> {
    // one round trip: the screen (the prompt rail, the before-fingerprint), the tty's modes, screen updates on
    let mut it = live.api.call_many(vec![screen_req(uuid), tty_req(uuid), subscribe_req(uuid, true)]).map_err(Fail::from)?.into_iter();
    let before = shot_of(it.next().unwrap_or_else(|| Err(crate::out::Error::failed("no reply"))))?;
    let (canonical, echo) = it.next().and_then(modes_of).unwrap_or((false, true));
    let _ = it.next();
    let r = land_on(live, uuid, text, typed, enter, deadline, &before, canonical, echo);
    let _ = live.api.call(subscribe_req(uuid, false));
    // drop what this delivery's subscription queued
    while live.api.wait_notification(Duration::ZERO).ok().flatten().is_some() {}
    r
}

#[allow(clippy::too_many_arguments)]
fn land_on(live: &mut Live, uuid: &str, text: &str, typed: &str, enter: bool, deadline: Instant, before: &Shot, canonical: bool, echo: bool) -> Result<Landed, Fail> {
    if before.prompt(typed) {
        return Ok(Landed::Prompt);
    }
    let mut wrapped = 0;
    if !text.is_empty() {
        let p = plan(text, canonical);
        wrapped = p.wrapped;
        send_text(live, uuid, &p.text)?;
        // landed: it shows (a line reader that doesn't echo can't show it)
        if !canonical || echo {
            let budget = (Instant::now() + Duration::from_secs(3) + Duration::from_millis((p.text.len() / 32) as u64)).min(deadline);
            // the message's own signs (its tail, a paste marker), not any change: an agent's spinner changes the
            // screen all the time. Past the budget, a changed screen counts (a program that redraws the text its way).
            let (fp, tail) = (before.fp(), tail_of(text));
            let was = signs(&before.lines, &tail);
            let mut changed = false;
            let shown = loop {
                let left = budget.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break changed;
                }
                updated(live, uuid, left.min(RECHECK))?;
                let now = shot(live, uuid)?;
                changed |= now.fp() != fp;
                if signs(&now.lines, &tail) > was {
                    break true;
                }
            };
            if !shown {
                return Err(fail(FailCode::Timeout, "not delivered: the text didn't show on screen; Enter not sent"));
            }
        }
    }
    if enter {
        // the program reacts to the paste first (OMP draws a dialog for a large one): decide on what it settled on
        settle(live, uuid)?;
        let mut now = shot(live, uuid)?;
        // OMP asks how to attach a large paste: its default (Esc, "paste inline") first, so Enter submits
        if paste_dialog(&now.lines) {
            send_text(live, uuid, "\x1b")?;
            let end = Instant::now() + Duration::from_secs(2);
            while paste_dialog(&now.lines) && Instant::now() < end {
                updated(live, uuid, RECHECK)?;
                now = shot(live, uuid)?;
            }
            settle(live, uuid)?;
            now = shot(live, uuid)?;
        }
        // never an Enter into a prompt that came up meanwhile
        let mine = format!("{typed}{text}");
        if now.prompt(&mine) {
            return Ok(Landed::EnterHeld);
        }
        send_text(live, uuid, "\r")?;
    }
    Ok(Landed::Done(wrapped))
}

/// The pane's drainer (this binary started with [`DRAIN_ENV`] set to the session): delivers its held messages in
/// order once the prompt is gone, the prompt checked again under the delivery lock before each; ends when the queue
/// is empty or the pane closed. The exit code is the process's.
pub fn drain_main(uuid: &str) -> i32 {
    let env = crate::conn::Env::from_process();
    let state = env.state.clone();
    let q = HeldQueue::new(&state, uuid);
    let drain_path = talk_file(&state, uuid, "drain");
    let Ok(Some(mut mine)) = Flock::try_take(&drain_path) else { return 0 };
    let Ok(mut live) = Live::connect(&env) else { return 1 };
    loop {
        let front = match q.front(now_ms()) {
            Ok(f) => f,
            Err(_) => return 1,
        };
        let Some(m) = front else {
            // let go first, then look once more: a sender that pushed meanwhile found the lock taken and counts on us
            drop(mine);
            match (q.is_empty(), Flock::try_take(&drain_path)) {
                (false, Ok(Some(l))) => {
                    mine = l;
                    continue;
                }
                _ => return 0,
            }
        };
        // until the prompt goes: woken by the pane's screen updates
        if live.api.call(subscribe_req(uuid, true)).is_err() {
            return 1;
        }
        let until = Instant::now() + Duration::from_millis(m.until.saturating_sub(now_ms()));
        let clear = loop {
            match shot(&mut live, uuid) {
                Ok(s) if !s.prompt(&m.typed) => break true,
                Ok(_) => {}
                Err(e) if e.code == FailCode::NotFound => {
                    // the pane closed: its messages go with it (the mailbox keeps every message)
                    let _ = q.clear();
                    return 0;
                }
                Err(_) => return 1,
            }
            if Instant::now() >= until {
                break false;
            }
            if updated(&mut live, uuid, Duration::from_secs(1)).is_err() {
                return 1;
            }
        };
        let _ = live.api.call(subscribe_req(uuid, false));
        if !clear {
            let _ = q.pop(&m);
            continue;
        }
        let Ok(Some(_one)) = Flock::take(&talk_file(&state, uuid, "lock"), Instant::now() + Duration::from_secs(600)) else { continue };
        match land(&mut live, uuid, &m.text, &m.typed, m.enter, Instant::now() + Duration::from_secs(120)) {
            Ok(Landed::Done(_)) => {
                let _ = q.pop(&m);
            }
            Ok(Landed::Prompt) => {}
            Ok(Landed::EnterHeld) => {
                // the text is in: what's left of the message is its Enter
                let _ = q.edit(|items| {
                    if items.first() == Some(&m) {
                        let t = std::mem::take(&mut items[0].text);
                        items[0].typed.push_str(&t);
                    }
                });
            }
            Err(e) if e.code == FailCode::NotFound => {
                let _ = q.clear();
                return 0;
            }
            Err(_) => {
                let _ = q.pop(&m);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Vec<u8> {
        key_bytes(Key::parse(s).unwrap()).unwrap()
    }

    #[test]
    fn keys_encode_as_xterm_sends_them() {
        assert_eq!(k("enter"), b"\r");
        assert_eq!(k("alt-enter"), b"\x1b\r");
        assert_eq!(k("esc"), b"\x1b");
        assert_eq!(k("tab"), b"\t");
        assert_eq!(k("shift-tab"), b"\x1b[Z");
        assert_eq!(k("backspace"), b"\x7f");
        assert_eq!(k("ctrl-c"), b"\x03");
        assert_eq!(k("C-d"), b"\x04");
        assert_eq!(k("ctrl-["), b"\x1b");
        assert_eq!(k("alt-x"), b"\x1bx");
        assert_eq!(k("shift-a"), b"A");
        assert_eq!(k("a"), b"a");
        assert_eq!(k("é"), "é".as_bytes());
        assert_eq!(k("up"), b"\x1b[A");
        assert_eq!(k("ctrl-down"), b"\x1b[1;5B");
        assert_eq!(k("shift-left"), b"\x1b[1;2D");
        assert_eq!(k("alt-right"), b"\x1b[1;3C");
        assert_eq!(k("home"), b"\x1b[H");
        assert_eq!(k("end"), b"\x1b[F");
        assert_eq!(k("delete"), b"\x1b[3~");
        assert_eq!(k("pageup"), b"\x1b[5~");
        assert_eq!(k("ctrl-pagedown"), b"\x1b[6;5~");
        assert_eq!(k("f1"), b"\x1bOP");
        assert_eq!(k("shift-f4"), b"\x1b[1;2S");
        assert_eq!(k("f5"), b"\x1b[15~");
        assert_eq!(k("f12"), b"\x1b[24~");
        assert_eq!(k("f20"), b"\x1b[34~");
        assert!(key_bytes(Key::parse("cmd-c").unwrap()).is_err(), "cmd keys are menu shortcuts");
        assert!(key_bytes(Key::parse("ctrl-é").unwrap()).is_err());
    }

    #[test]
    fn only_cancel_keys_pass_the_prompt_rail() {
        for c in ["esc", "ctrl-c"] {
            assert!(cancels(Key::parse(c).unwrap()), "{c}");
        }
        for c in ["enter", "y", "1", "down", "tab", "ctrl-d", "alt-esc"] {
            assert!(!cancels(Key::parse(c).unwrap()), "{c}");
        }
    }

    #[test]
    fn pastes_are_bracketed_for_raw_readers_and_cannot_escape() {
        assert_eq!(paste_text("a\nb", false), "a\rb");
        assert_eq!(paste_text("a\r\nb", false), "a\rb");
        assert_eq!(paste_text("x\x1b[201~y", true), "\x1b[200~xy\x1b[201~");
        let p = plan("a\nb", false);
        assert!(p.text.starts_with("\x1b[200~") && p.text.ends_with("\x1b[201~"), "one bracketed paste");
    }

    #[test]
    fn a_line_reader_gets_lines_under_its_limit_nothing_lost() {
        let long = format!("{} {}", "x".repeat(700), "y".repeat(700));
        let p = plan(&format!("start\n{long}\nend"), true);
        let lines: Vec<&str> = p.text.split('\n').collect();
        assert_eq!(p.wrapped, 1);
        assert!(lines.iter().all(|l| l.len() <= LINE_MAX));
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].len(), 701, "broken after the space");
        let p = plan(&"é".repeat(800), true);
        assert!(p.text.split('\n').all(|l| l.len() <= LINE_MAX));
        assert_eq!(p.text.replace('\n', ""), "é".repeat(800), "nothing lost");
        assert_eq!(plan("short line", true), Plan { text: "short line".into(), wrapped: 0 });
    }

    #[test]
    fn chunks_cut_on_character_boundaries() {
        let s = "aé".repeat(10_000);
        let c = chunks(&s, 1001);
        assert!(c.iter().all(|x| x.len() <= 1001));
        assert_eq!(c.concat(), s);
        assert_eq!(chunks("", 10), vec![""]);
        assert_eq!(chunks("abc", 3), vec!["abc"]);
    }

    #[test]
    fn fingerprints_see_text_and_cursor() {
        let a = vec!["$ ".to_string()];
        let b = vec!["$ hi".to_string()];
        assert_ne!(fingerprint(&a, (2, 0)), fingerprint(&b, (2, 0)));
        assert_ne!(fingerprint(&a, (2, 0)), fingerprint(&a, (3, 0)));
        assert_eq!(fingerprint(&a, (2, 0)), fingerprint(&a.clone(), (2, 0)));
    }

    #[test]
    fn omp_paste_dialog() {
        let s = |t: &str| t.lines().map(String::from).collect::<Vec<_>>();
        assert!(paste_dialog(&s("╭─ Pasted 1024 lines ─╮\n│  ❯ Attach as a wrapped block │\n│ ⎋ to paste inline │")));
        assert!(!paste_dialog(&s("❯ hello")));
    }

    #[test]
    fn a_message_that_quotes_a_prompt_does_not_hold_itself() {
        let typed = "clear; printf 'Allow command? [y/N]'; sleep 4";
        let screen = |rows: &[&str]| rows.iter().map(|r| r.to_string()).collect::<Vec<_>>();
        // the shell echoes the command line after its prompt
        assert!(!prompt_except(&screen(&["host% clear; printf 'Allow command? [y/N]'; sleep 4"]), typed));
        // an agent's input box, bordered
        assert!(!prompt_except(&screen(&["│ > clear; printf 'Allow command? [y/N]'; sleep 4 │"]), typed));
        // a real prompt still counts, beside the echo or alone
        assert!(prompt_except(&screen(&["host% clear; printf 'Allow command? [y/N]'; sleep 4", "Do you want to proceed?", "❯ 1. Yes"]), typed));
        assert!(prompt_except(&screen(&["Allow command? [y/N]"]), ""));
        assert!(prompt_except(&screen(&["Allow command? [y/N]"]), "something else entirely"));
        // a message quoting only the prompt's tail doesn't hide a real prompt
        assert!(prompt_except(&screen(&["Run rm -rf build in the repo? Allow command? [y/N]"]), "answer [y/N] for me"));
    }

    #[test]
    fn spawned_panes_drop_the_callers_claude_session() {
        // every launch path (split, tab, window) builds its command with shape::spawn_props: a Claude started with the
        // caller's CLAUDE_CODE_CHILD_SESSION saves no transcript
        let props = super::super::shape::spawn_props(&crate::app::wire::Spawn { cmd: Some("claude".into()), env: vec![("A".into(), "1".into())], ..Default::default() }, None);
        let cmd = props.iter().find(|p| p.key.as_deref() == Some("Command")).and_then(|p| p.json_value.clone()).unwrap();
        for v in super::super::shape::CALLER_SESSION_VARS {
            assert!(cmd.contains(v), "{v} unset");
        }
        assert!(cmd.find("unset").unwrap() < cmd.find("export A=1").unwrap() && cmd.find("export A=1").unwrap() < cmd.find("claude'").unwrap(), "{cmd}");
        for v in ["CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDECODE", "CLAUDE_PID"] {
            assert!(super::super::shape::CALLER_SESSION_VARS.contains(&v));
        }
    }

    #[test]
    fn a_message_shows_by_its_tail_or_a_paste_marker() {
        let rows = |t: &str| t.lines().map(String::from).collect::<Vec<_>>();
        let msg = "please review the parser change in src/grammar.rs and reply";
        assert_eq!(tail_of(msg), "ammar.rsandreply");
        let tail = tail_of(msg);
        // wrapped across rows, indented by an input box
        assert_eq!(signs(&rows("❯ please review the parser change in src/gram\n  mar.rs and reply"), &tail), 1);
        assert_eq!(signs(&rows("❯ "), &tail), 0);
        // paste markers: Claude's, OMP's dialog, chip and preview; not a plain mention of lines
        assert_eq!(signs(&rows("❯ [Pasted text #1 +1558 lines]"), "zzzz"), 1);
        assert_eq!(signs(&rows("╭─ Pasted 1559 lines ─╮"), "zzzz"), 1);
        assert_eq!(signs(&rows("❯ 📄 #2"), "zzzz"), 1);
        assert_eq!(signs(&rows("╰ +1559 lin… ╯"), "zzzz"), 1);
        assert_eq!(signs(&rows("read 20 lines of the file"), "zzzz"), 0);
    }

    #[test]
    fn buffer_rows_lose_padding() {
        let l = |t: &str| p::LineContents { text: Some(t.into()), ..Default::default() };
        assert_eq!(buffer_lines(&[l("a\0\0  "), l(""), l("b")]), ["a", "", "b"]);
    }

    #[test]
    fn held_messages_keep_their_order_and_expire() {
        let dir = std::env::temp_dir().join(format!("iterm-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let q = HeldQueue::new(&dir, "ABC-123");
        assert!(q.is_empty());
        let m = |t: &str, until: u64| Held { text: t.into(), enter: true, until, typed: String::new() };
        assert_eq!(q.push(m("one", 100)).unwrap(), 1);
        assert_eq!(q.push(m("two", 2_000)).unwrap(), 2);
        assert_eq!(q.push(m("three", 3_000)).unwrap(), 3);
        // "one" ran out of time: dropped, the next one waits on
        assert_eq!(q.front(500).unwrap(), Some(m("two", 2_000)));
        assert_eq!(q.len(), 2);
        // a pop removes only the message it delivered
        q.pop(&m("three", 3_000)).unwrap();
        assert_eq!(q.len(), 2);
        q.pop(&m("two", 2_000)).unwrap();
        assert_eq!(q.front(500).unwrap(), Some(m("three", 3_000)));
        q.clear().unwrap();
        assert!(q.is_empty());
        assert!(talk_file(&dir, "w0t0p0:../x", "held").file_name().unwrap().to_str().unwrap() == "w0t0p0____x.held");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_delivery_lock_per_pane() {
        let dir = std::env::temp_dir().join(format!("iterm-lock-{}", std::process::id()));
        let path = talk_file(&dir, "P", "lock");
        let a = Flock::try_take(&path).unwrap();
        assert!(a.is_some());
        // a second open file description: flock excludes it, as it would another process
        assert!(Flock::take(&path, Instant::now() + Duration::from_millis(30)).unwrap().is_none());
        drop(a);
        assert!(Flock::try_take(&path).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
