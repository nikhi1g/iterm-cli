//! The iTerm2 API cookie, through iTerm2's own permission gate: the `request cookie and key`
//! scripting command, sent as one in-process Apple Event (no `osascript` process, no AppleScript
//! compile), with `osascript` as the fallback. A root-owned `disable-automation-auth` file (the
//! human's "allow all apps" setting) and an inherited ITERM2_COOKIE are honored as the official
//! Python client does.

use std::ffi::c_void;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::iterm2::error::{Error, Result};

#[repr(C)]
struct AEDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

impl AEDesc {
    fn null() -> AEDesc {
        AEDesc {
            descriptor_type: fourcc(b"null"),
            data_handle: std::ptr::null_mut(),
        }
    }
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn AECreateDesc(type_code: u32, data: *const c_void, size: isize, result: *mut AEDesc) -> i16;
    fn AECreateAppleEvent(
        class: u32,
        id: u32,
        target: *const AEDesc,
        return_id: i16,
        transaction: i32,
        result: *mut AEDesc,
    ) -> i16;
    fn AEPutParamPtr(
        event: *mut AEDesc,
        key: u32,
        type_code: u32,
        data: *const c_void,
        size: isize,
    ) -> i16;
    fn AESendMessage(
        event: *const AEDesc,
        reply: *mut AEDesc,
        mode: i32,
        ticks: libc::c_long,
    ) -> i32;
    fn AEGetParamPtr(
        event: *const AEDesc,
        key: u32,
        desired: u32,
        actual: *mut u32,
        data: *mut c_void,
        max: isize,
        size: *mut isize,
    ) -> i16;
    fn AEDisposeDesc(desc: *mut AEDesc) -> i16;
    fn AEDeterminePermissionToAutomateTarget(
        target: *const AEDesc,
        class: u32,
        id: u32,
        ask: u8,
    ) -> i32;
}

type CFTypeRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        alloc: CFTypeRef,
        buf: *const u8,
        len: isize,
        is_dir: u8,
    ) -> CFTypeRef;
    fn CFBundleCreate(alloc: CFTypeRef, url: CFTypeRef) -> CFTypeRef;
    fn CFBundleGetIdentifier(bundle: CFTypeRef) -> CFTypeRef;
    fn CFStringGetCString(s: CFTypeRef, buf: *mut u8, size: isize, encoding: u32) -> u8;
    fn CFRelease(cf: CFTypeRef);
}

/// CFBundleIdentifier of an app bundle (None when it is not one): read from an XML `Contents/Info.plist`
/// directly (one small file read; CoreFoundation's bundle machinery costs about 1.3 ms cold), else by
/// CoreFoundation (binary plists, other layouts).
pub fn bundle_identifier(app: &std::path::Path) -> Option<String> {
    std::fs::read(app.join("Contents/Info.plist"))
        .ok()
        .and_then(|b| plist_bundle_id(&b))
        .or_else(|| cf_bundle_identifier(app))
}

/// The CFBundleIdentifier of an XML property list's top-level dict: the `<string>` right after the first
/// `<key>CFBundleIdentifier</key>` (Info.plist has one dict; nested dicts never come before it in Apple's or
/// iTerm2's). None for a binary plist or anything unexpected, so the caller asks CoreFoundation.
pub fn plist_bundle_id(bytes: &[u8]) -> Option<String> {
    if !bytes.starts_with(b"<?xml") {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let at = text.find("<key>CFBundleIdentifier</key>")? + "<key>CFBundleIdentifier</key>".len();
    let rest = text[at..].trim_start();
    let value = rest.strip_prefix("<string>")?;
    let end = value.find("</string>")?;
    let id = value[..end].trim();
    // entities or markup inside the value: let CoreFoundation decode it
    if id.is_empty() || id.contains(['&', '<', '>']) {
        return None;
    }
    Some(id.to_string())
}

fn cf_bundle_identifier(app: &std::path::Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = app.as_os_str().as_bytes();
    // SAFETY: CF objects created here are released here; the identifier is borrowed (Get rule).
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as isize,
            1,
        );
        if url.is_null() {
            return None;
        }
        let bundle = CFBundleCreate(std::ptr::null(), url);
        CFRelease(url);
        if bundle.is_null() {
            return None;
        }
        let id = CFBundleGetIdentifier(bundle);
        let mut buf = [0u8; 512];
        let ok = !id.is_null()
            && CFStringGetCString(id, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100) != 0;
        CFRelease(bundle);
        if !ok {
            return None;
        }
        let n = buf.iter().position(|b| *b == 0).unwrap_or(0);
        String::from_utf8(buf[..n].to_vec()).ok()
    }
}

/// The PID of the process serving a connected unix socket, and its uid.
pub fn peer_of(fd: std::os::unix::io::RawFd) -> Option<(i32, u32)> {
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt/getpeereid write into our own locals.
    unsafe {
        if libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut pid as *mut _ as *mut c_void,
            &mut len,
        ) != 0
            || pid <= 0
        {
            return None;
        }
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        if libc::getpeereid(fd, &mut uid, &mut gid) != 0 {
            return None;
        }
        Some((pid, uid))
    }
}

/// The app bundle a process runs from: the `.app` enclosing its executable.
pub fn app_of_pid(pid: i32) -> Option<std::path::PathBuf> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: proc_pidpath writes at most buf.len() bytes into our buffer.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let exe = std::path::PathBuf::from(String::from_utf8(buf).ok()?);
    exe.ancestors()
        .find(|a| a.extension().map(|e| e == "app").unwrap_or(false))
        .map(|a| a.to_path_buf())
}

/// The process serving the target's socket, checked against the target: (pid, its app bundle).
/// A socket served by another user, or by an app with another bundle ID, is refused.
pub fn verify_server(
    fd: std::os::unix::io::RawFd,
    t: &crate::iterm2::target::Target,
) -> Result<Option<(i32, std::path::PathBuf)>> {
    let Some((pid, uid)) = peer_of(fd) else {
        return Ok(None);
    };
    // SAFETY: getuid has no preconditions.
    if uid != unsafe { libc::getuid() } {
        return Err(Error::failed(format!(
            "the {} API socket is served by another user (uid {uid}); refusing it",
            t.app.display()
        )));
    }
    let Some(app) = app_of_pid(pid) else {
        return Ok(None);
    };
    let id = bundle_identifier(&app).unwrap_or_default();
    if id != t.bundle_id {
        return Err(Error::failed(format!(
            "the socket for suite {} is served by {} ({id}), not {} ({}); pass --app to pick the app you mean",
            t.suite,
            app.display(),
            t.app.display(),
            t.bundle_id
        )));
    }
    Ok(Some((pid, app)))
}

/// Whether sending this target the cookie request needs the human's Automation consent:
/// granted, would-ask, denied, not-running or unknown. Never shows a dialog.
pub fn automation_permission(pid: Option<i32>, bundle: &str) -> &'static str {
    // SAFETY: the target descriptor is created and disposed here.
    unsafe {
        let mut target = AEDesc::null();
        let rc = match pid {
            Some(p) => AECreateDesc(
                fourcc(b"kpid"),
                &p as *const i32 as *const c_void,
                4,
                &mut target,
            ),
            None => AECreateDesc(
                fourcc(b"bund"),
                bundle.as_ptr() as *const c_void,
                bundle.len() as isize,
                &mut target,
            ),
        };
        if rc != 0 {
            return "unknown";
        }
        let r = AEDeterminePermissionToAutomateTarget(&target, fourcc(b"Itrm"), fourcc(b"rqck"), 0);
        AEDisposeDesc(&mut target);
        match r {
            0 => "granted",
            -1744 => "would-ask",
            -1743 => "denied",
            -600 => "not-running",
            _ => "unknown",
        }
    }
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const AE_WAIT_REPLY: i32 = 0x3;
const AE_CAN_INTERACT: i32 = 0x20;
const PROC_NOT_FOUND: i32 = -600;

pub const BUNDLE_ID: &str = crate::iterm2::target::ITERM2_BUNDLE_ID;

/// The target's settings suite (see `target`).
pub fn suite() -> String {
    crate::iterm2::target::current().suite.clone()
}

/// ITERM_API_NO_PROMPT=1: fail instead of letting macOS ask the human for Automation consent.
fn no_prompt() -> bool {
    std::env::var("ITERM_API_NO_PROMPT")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// The human's "allow all apps to connect" setting: a root-owned file holding a magic value.
pub fn automation_auth_disabled() -> bool {
    let path = crate::conn::home()
        .join("Library/Application Support")
        .join(suite())
        .join("disable-automation-auth");
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    let hex: String = path
        .to_string_lossy()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let expected = format!("{hex} 61DF88DC-3423-4823-B725-22570E01C027");
    meta.uid() == 0
        && meta.len() as usize == expected.len()
        && std::fs::read_to_string(&path)
            .map(|t| t == expected)
            .unwrap_or(false)
}

/// (cookie, key) for one connection, from the process `server` (the socket's peer) when known,
/// else from the target's bundle ID. Addressed by PID there is no fallback: an osascript by
/// name, path or bundle could reach another instance of the same app.
pub fn request_cookie(app_name: &str, server: Option<i32>) -> Result<(String, String)> {
    let t = crate::iterm2::target::current();
    if no_prompt() {
        match automation_permission(server, &t.bundle_id) {
            "granted" => {}
            other => {
                return Err(Error::failed(format!(
                    "Automation of {} is {other}; ITERM_API_NO_PROMPT forbids asking the human (System Settings → Privacy & Security → Automation)",
                    t.app.display()
                )))
            }
        }
    }
    let text = match server {
        Some(pid) => apple_event(app_name, Some(pid))?,
        None => match apple_event(app_name, None) {
            Ok(text) => text,
            Err(e) if e.message().contains("not running") => return Err(e),
            Err(_) => osascript(app_name)?,
        },
    };
    let mut parts = text.trim().splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some(cookie), Some(key)) if !cookie.is_empty() && !key.is_empty() => {
            Ok((cookie.to_string(), key.trim().to_string()))
        }
        _ => Err(Error::failed(format!(
            "{} returned no API cookie; check Settings → General → Magic → Enable Python API",
            t.reached()
        ))),
    }
}

/// A refused cookie request, naming the app instance that refused it, e.g. iTerm2's "The Python
/// API is not enabled" from a vTerm build answering for the wrong suite.
pub fn cookie_refused(t: &crate::iterm2::target::Target, detail: &str) -> String {
    let detail = detail.trim();
    let mut msg = format!("{} refused the API cookie request", t.reached());
    if !detail.is_empty() {
        msg.push_str(&format!(": {detail}"));
    }
    if detail.to_ascii_lowercase().contains("not enabled") {
        msg.push_str(&format!(" (enable it in that app: Settings → General → Magic → Enable Python API; defaults domain {})", t.prefs_domain()));
    }
    msg
}

fn apple_event(app_name: &str, pid: Option<i32>) -> Result<String> {
    let bundle = crate::iterm2::target::current().bundle_id.clone();
    // SAFETY: each AEDesc is created, used and disposed here; buffers outlive the calls using them.
    unsafe {
        let mut target = AEDesc::null();
        let mut event = AEDesc::null();
        let mut reply = AEDesc::null();
        let guard = |rc: i32, what: &str| -> Result<()> {
            if rc == 0 {
                Ok(())
            } else {
                Err(Error::failed(format!("{what} failed ({rc})")))
            }
        };
        let result = (|| {
            let rc = match pid {
                Some(p) => AECreateDesc(
                    fourcc(b"kpid"),
                    &p as *const i32 as *const c_void,
                    4,
                    &mut target,
                ),
                None => AECreateDesc(
                    fourcc(b"bund"),
                    bundle.as_ptr() as *const c_void,
                    bundle.len() as isize,
                    &mut target,
                ),
            };
            guard(rc as i32, "AECreateDesc")?;
            guard(
                AECreateAppleEvent(fourcc(b"Itrm"), fourcc(b"rqck"), &target, -1, 0, &mut event)
                    as i32,
                "AECreateAppleEvent",
            )?;
            guard(
                AEPutParamPtr(
                    &mut event,
                    fourcc(b"Rcsn"),
                    fourcc(b"utf8"),
                    app_name.as_ptr() as *const c_void,
                    app_name.len() as isize,
                ) as i32,
                "AEPutParamPtr",
            )?;
            let rc = crate::conn::timing::time(crate::conn::timing::Phase::AppleEvent, || {
                AESendMessage(&event, &mut reply, AE_WAIT_REPLY | AE_CAN_INTERACT, 600)
            });
            if rc == PROC_NOT_FOUND {
                return Err(Error::failed(format!(
                    "{} not running",
                    crate::iterm2::target::current().app.display()
                )));
            }
            guard(rc, "AESendMessage")?;
            let mut errn: i32 = 0;
            let mut actual: u32 = 0;
            let mut size: isize = 0;
            if AEGetParamPtr(
                &reply,
                fourcc(b"errn"),
                fourcc(b"long"),
                &mut actual,
                &mut errn as *mut i32 as *mut c_void,
                4,
                &mut size,
            ) == 0
                && errn != 0
            {
                // the app's own reason, when it sent one (errs)
                let mut text = vec![0u8; 512];
                let mut tsize: isize = 0;
                let reason = if AEGetParamPtr(
                    &reply,
                    fourcc(b"errs"),
                    fourcc(b"utf8"),
                    &mut actual,
                    text.as_mut_ptr() as *mut c_void,
                    text.len() as isize,
                    &mut tsize,
                ) == 0
                {
                    text.truncate((tsize.max(0) as usize).min(512));
                    String::from_utf8_lossy(&text).into_owned()
                } else {
                    String::new()
                };
                return Err(Error::failed(cookie_refused(
                    crate::iterm2::target::current(),
                    &format!("{reason} ({errn})"),
                )));
            }
            let mut buf = vec![0u8; 1024];
            let rc = AEGetParamPtr(
                &reply,
                fourcc(b"----"),
                fourcc(b"utf8"),
                &mut actual,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as isize,
                &mut size,
            );
            guard(rc as i32, "reading the cookie")?;
            buf.truncate((size.max(0) as usize).min(1024));
            String::from_utf8(buf).map_err(|_| Error::failed("cookie was not UTF-8"))
        })();
        AEDisposeDesc(&mut reply);
        AEDisposeDesc(&mut event);
        AEDisposeDesc(&mut target);
        result
    }
}

fn osascript(app_name: &str) -> Result<String> {
    // by bundle ID, never by name: vTerm's development builds are also called iTerm2.app
    let target = format!("application id \"{}\"", crate::iterm2::target::current().bundle_id);
    let name = app_name.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "if {target} is running then\n tell {target} to request cookie and key for app named \"{name}\"\nelse\n return \"not-running\"\nend if"
    );
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::failed(format!("osascript: {e}")))?;
    child
        .stdin
        .take()
        .map(|mut s| s.write_all(script.as_bytes()));
    let out = child
        .wait_with_output()
        .map_err(|e| Error::failed(format!("osascript: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(Error::failed(cookie_refused(
            crate::iterm2::target::current(),
            &err,
        )));
    }
    if text == "not-running" {
        return Err(Error::failed(format!(
            "{} not running",
            crate::iterm2::target::current().app.display()
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use crate::iterm2::target::{resolve, App};

    #[test]
    fn the_bundle_id_is_read_from_an_xml_plist_and_anything_else_goes_to_corefoundation() {
        let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>CFBundleExecutable</key>\n\t<string>iTerm2</string>\n\t<key>CFBundleIdentifier</key>\n\t<string>com.googlecode.iterm2</string>\n</dict>\n</plist>\n";
        assert_eq!(super::plist_bundle_id(xml).as_deref(), Some("com.googlecode.iterm2"));
        assert_eq!(super::plist_bundle_id(b"bplist00\x01\x02"), None, "binary plist");
        assert_eq!(super::plist_bundle_id(b"<?xml?><dict><key>CFBundleIdentifier</key><integer>1</integer></dict>"), None);
        assert_eq!(super::plist_bundle_id(b"<?xml?><dict><key>CFBundleIdentifier</key><string>a&amp;b</string></dict>"), None, "entities: CF decodes them");
        assert_eq!(super::plist_bundle_id(b"<?xml?><dict><key>CFBundleName</key><string>x</string></dict>"), None);
    }

    // the fast path and CoreFoundation agree on every app installed here (a fixed list: no scan of /Applications)
    #[test]
    fn the_fast_bundle_id_matches_corefoundation_on_installed_apps() {
        for app in ["/Applications/iTerm.app", "/System/Applications/Calculator.app", "/System/Applications/Utilities/Terminal.app"] {
            let p = std::path::Path::new(app);
            if p.exists() {
                assert_eq!(super::bundle_identifier(p), super::cf_bundle_identifier(p), "{app}");
            }
        }
    }

    #[test]
    fn a_refused_cookie_names_the_app_and_suite_it_reached() {
        let none = |_: &str| None;
        let vterm = resolve(Some("vterm"), None, &none).unwrap();
        let m = super::cookie_refused(&vterm, "The Python API is not enabled. (-2700)");
        assert!(m.starts_with("vTerm (suite vTerm) refused the API cookie request: The Python API is not enabled."), "{m}");
        assert!(m.contains("defaults domain vTerm"), "{m}");
        let iterm = resolve(Some("iterm2"), None, &none).unwrap();
        assert_eq!(iterm.app, App::Iterm2);
        assert_eq!(
            super::cookie_refused(&iterm, " "),
            "iTerm2 (suite iTerm2) refused the API cookie request"
        );
        assert!(!super::cookie_refused(&iterm, "denied (-1743)").contains("Enable Python API"));
    }
}
