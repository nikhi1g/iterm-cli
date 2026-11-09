//! One WebSocket connection to iTerm2's API server over its private unix socket (TCP 1912 as
//! the legacy fallback), speaking the official protobuf protocol. Requests can be pipelined:
//! `call_many` writes them all, then collects the responses by id, so a snapshot of every pane's
//! variables costs one round trip instead of one per pane. Notifications that arrive meanwhile
//! are queued for `wait_notification`.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use prost::Message as _;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

use crate::iterm2::auth;
use crate::iterm2::error::{Error, Result};
use crate::iterm2::proto::{
    client_originated_message as cm, server_originated_message as sm, ClientOriginatedMessage,
    Notification, ServerOriginatedMessage,
};

pub enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn set_timeouts(&self, d: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(d)?;
        self.set_write_timeout(d)
    }
    fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.set_read_timeout(d),
            Stream::Tcp(s) => s.set_read_timeout(d),
        }
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.set_write_timeout(d),
            Stream::Tcp(s) => s.set_write_timeout(d),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.read(buf),
            Stream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.write(buf),
            Stream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.flush(),
            Stream::Tcp(s) => s.flush(),
        }
    }
}

pub const LIBRARY: &str = concat!("rust ", env!("CARGO_PKG_VERSION"));
/// Every request's deadline unless the caller passes its own.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(20);

fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("ITERM_API_TRACE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

fn stamp() -> String {
    format!("{:.3}", (crate::app::now_ms() as f64 / 1000.0) % 1000.0)
}

pub struct Conn {
    ws: WebSocket<Stream>,
    next_id: i64,
    responses: HashMap<i64, sm::Submessage>,
    events: VecDeque<Notification>,
    pub protocol: (i64, i64),
    /// How long a request waits for its answer now (the command's own bound, at most [`RPC_TIMEOUT`]).
    pub timeout: Duration,
}

pub fn socket_path() -> std::path::PathBuf {
    crate::iterm2::target::current().socket(&crate::conn::home())
}

/// The stream, plus the PID of the app serving it when it is the unix socket (checked to be the
/// target app; see `auth::verify_server`).
fn open_stream() -> Result<(Stream, Option<i32>)> {
    use std::os::unix::io::AsRawFd;
    let t = crate::iterm2::target::current();
    let app = t.app.display();
    let path = socket_path();
    if path.exists() {
        let s = UnixStream::connect(&path).map_err(|e| {
            Error::failed(format!("cannot reach {app}'s API socket {} ({e}); start {app} and enable Settings → General → Magic → Enable Python API", path.display()))
        })?;
        let server = auth::verify_server(s.as_raw_fd(), t)?.map(|(pid, _)| pid);
        return Ok((Stream::Unix(s), server));
    }
    let missing = || {
        Error::failed(crate::iterm2::target::unreachable_message(
            t,
            &path,
            t.api_server_enabled(),
        ))
    };
    if t.app != crate::iterm2::target::App::Iterm2 || t.suite != crate::iterm2::target::App::Iterm2.default_suite()
    {
        // TCP 1912 is whichever app holds it: never a fallback for a named target
        return Err(missing());
    }
    TcpStream::connect(("127.0.0.1", 1912))
        .map(|s| (Stream::Tcp(s), None))
        .map_err(|_| missing())
}

enum Handshake {
    Ok(WebSocket<Stream>, (i64, i64)),
    Unauthorized,
}

fn handshake(stream: Stream, cookie: Option<&(String, String)>) -> Result<Handshake> {
    stream.set_timeouts(Some(Duration::from_secs(10)))?;
    let mut req = "ws://localhost/"
        .into_client_request()
        .map_err(|e| Error::failed(e.to_string()))?;
    let headers = req.headers_mut();
    let mut put = |k: &'static str, v: &str| {
        if let Ok(v) = HeaderValue::from_str(v) {
            headers.insert(k, v);
        }
    };
    put("origin", "ws://localhost/");
    put("x-iterm2-library-version", LIBRARY);
    put("x-iterm2-disable-auth-ui", "true");
    put("x-iterm2-advisory-name", "iterm");
    put("sec-websocket-protocol", "api.iterm2.com");
    if let Some((c, k)) = cookie {
        put("x-iterm2-cookie", c);
        if !k.is_empty() {
            put("x-iterm2-key", k);
        }
    }
    match tungstenite::client::client(req, stream) {
        Ok((ws, resp)) => {
            let protocol = resp
                .headers()
                .get("x-iterm2-protocol-version")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split_once('.'))
                .map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(0)))
                .unwrap_or((0, 0));
            Ok(Handshake::Ok(ws, protocol))
        }
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
            match resp.status().as_u16() {
                401 => Ok(Handshake::Unauthorized),
                406 => Err(Error::failed("iTerm2 says this client is too old for it")),
                code => Err(Error::failed(format!(
                    "iTerm2 refused the API connection (HTTP {code})"
                ))),
            }
        }
        Err(e) => Err(Error::failed(format!("iTerm2 API handshake failed: {e}"))),
    }
}

/// The cookie a session's own app gave it, good only inside that app. Sessions get ITERM2_COOKIE alone
/// (the SetCookie setting); scripts the app launches also get ITERM2_KEY, which is optional.
pub fn inherited_cookie(
    cookie: Option<String>,
    key: Option<String>,
    inside: bool,
) -> Option<(String, String)> {
    match cookie {
        Some(c) if !c.is_empty() && inside => Some((c, key.unwrap_or_default())),
        _ => None,
    }
}

impl Conn {
    /// Connect with an inherited cookie, no cookie when the human allowed all apps, or a fresh
    /// cookie from iTerm2's own permission-gated scripting command.
    pub fn connect() -> Result<Conn> {
        // a cookie the caller's own app put in its environment is good only for that app
        let inherited = inherited_cookie(
            std::env::var("ITERM2_COOKIE").ok(),
            std::env::var("ITERM2_KEY").ok(),
            crate::iterm2::target::current().inside,
        );
        use crate::conn::timing::{time, Phase};
        let t0 = std::time::Instant::now();
        let (stream, server) = time(Phase::Socket, open_stream)?;
        let first = if inherited.is_some() {
            inherited.clone()
        } else if auth::automation_auth_disabled() {
            None
        } else {
            Some(time(Phase::Cookie, || auth::request_cookie("iterm", server))?)
        };
        let t1 = std::time::Instant::now();
        let mut attempt = time(Phase::Handshake, || handshake(stream, first.as_ref()))?;
        if trace() {
            eprintln!(
                "iterm-trace cookie {:.1} ms, handshake {:.1} ms",
                (t1 - t0).as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }
        if matches!(attempt, Handshake::Unauthorized) && (inherited.is_some() || first.is_none()) {
            let (stream, server) = time(Phase::Socket, open_stream)?;
            let fresh = time(Phase::Cookie, || auth::request_cookie("iterm", server))?;
            attempt = time(Phase::Handshake, || handshake(stream, Some(&fresh)))?;
        }
        match attempt {
            Handshake::Ok(ws, protocol) => {
                // writes keep this bound for the connection's life; each read sets its own (see `pump`)
                ws.get_ref().set_write_timeout(Some(RPC_TIMEOUT))?;
                Ok(Conn { ws, next_id: 0, responses: HashMap::new(), events: VecDeque::new(), protocol, timeout: RPC_TIMEOUT })
            }
            Handshake::Unauthorized => Err(Error::failed(format!(
                "{} rejected the API cookie; approve this CLI's scripting request (Settings → General → Magic)",
                crate::iterm2::target::current().reached()
            ))),
        }
    }

    fn write(&mut self, sub: cm::Submessage) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;
        if trace() {
            eprintln!(
                "iterm-trace {} → {id} {}",
                stamp(),
                format!("{sub:?}").chars().take(160).collect::<String>()
            );
        }
        let msg = ClientOriginatedMessage {
            id: Some(id),
            submessage: Some(sub),
        };
        self.ws
            .write(Message::Binary(msg.encode_to_vec()))
            .map_err(ws_err)?;
        Ok(id)
    }

    fn flush(&mut self) -> Result<()> {
        self.ws.flush().map_err(ws_err)
    }

    /// Read one message (a response or a notification) before `deadline`; false on timeout.
    fn pump(&mut self, deadline: Instant) -> Result<bool> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        self.ws
            .get_ref()
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
        match self.ws.read() {
            Ok(Message::Binary(data)) => {
                let msg = ServerOriginatedMessage::decode(data.as_slice())
                    .map_err(|e| Error::failed(format!("bad message from iTerm2: {e}")))?;
                if trace() {
                    eprintln!(
                        "iterm-trace {} ← {:?} {}",
                        stamp(),
                        msg.id,
                        format!("{:?}", msg.submessage)
                            .chars()
                            .take(160)
                            .collect::<String>()
                    );
                }
                match (msg.id, msg.submessage) {
                    (_, Some(sm::Submessage::Notification(n))) => self.events.push_back(n),
                    (Some(id), Some(sub)) => {
                        self.responses.insert(id, sub);
                    }
                    _ => {}
                }
                Ok(true)
            }
            Ok(Message::Close(_)) => Err(Error::failed("iTerm2 closed the API connection")),
            Ok(_) => Ok(true), // ping/pong/text: tungstenite answers pings itself
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(Instant::now() < deadline)
            }
            Err(e) => Err(ws_err(e)),
        }
    }

    fn take(&mut self, id: i64, deadline: Instant) -> Result<sm::Submessage> {
        loop {
            if let Some(sub) = self.responses.remove(&id) {
                if let sm::Submessage::Error(e) = &sub {
                    return Err(Error::failed(format!("iTerm2 rejected the request: {e}")));
                }
                return Ok(sub);
            }
            if !self.pump(deadline)? && Instant::now() >= deadline {
                return Err(Error::timeout(
                    "iTerm2 did not answer in time; check its API permission prompt",
                ));
            }
        }
    }

    pub fn call(&mut self, sub: cm::Submessage) -> Result<sm::Submessage> {
        let id = self.write(sub)?;
        self.flush()?;
        self.take(id, Instant::now() + self.timeout)
    }

    /// Several requests in flight at once; responses in request order (each may be an error).
    pub fn call_many(&mut self, subs: Vec<cm::Submessage>) -> Result<Vec<Result<sm::Submessage>>> {
        let mut ids = Vec::with_capacity(subs.len());
        for sub in subs {
            ids.push(self.write(sub)?);
        }
        self.flush()?;
        let deadline = Instant::now() + self.timeout;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(match self.take(id, deadline) {
                Err(e @ Error::Timeout(_)) => return Err(e),
                other => other,
            });
        }
        Ok(out)
    }

    /// The socket's descriptor, for a wait that blocks on it with other sources (kqueue). Data
    /// already read and buffered is not visible on it: drain `wait_notification(ZERO)` first.
    pub fn raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        match self.ws.get_ref() {
            Stream::Unix(s) => s.as_raw_fd(),
            Stream::Tcp(s) => s.as_raw_fd(),
        }
    }

    /// The next notification, waiting up to `timeout`.
    pub fn wait_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(n) = self.events.pop_front() {
                return Ok(Some(n));
            }
            if !self.pump(deadline)? {
                return Ok(None);
            }
        }
    }
}

fn ws_err(e: tungstenite::Error) -> Error {
    match e {
        tungstenite::Error::Io(io)
            if matches!(
                io.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Error::timeout("iTerm2 API connection timed out")
        }
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            Error::failed("iTerm2 closed the API connection")
        }
        other => Error::failed(format!("iTerm2 API connection failed: {other}")),
    }
}
