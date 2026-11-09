//! Which app a command talks to: iTerm2 or vTerm (our iTerm2 build with vertical tabs). Both
//! speak the same protocol; they differ in settings suite (hence API socket), bundle ID and app
//! path. Every command addresses exactly one of them:
//!
//! 1. `--app iterm2|vterm`;
//! 2. the binary's name: `vterm` means vTerm;
//! 3. `ITERM_APP=iterm2|vterm`;
//! 4. the app the caller runs in: vTerm when `IT2_SUITE` names a vTerm suite (`vTerm`, `vterm-…`)
//!    or `IT2_APP_PATH` is a vTerm bundle; iTerm2 otherwise.
//!
//! The socket is `~/Library/Application Support/<suite>/private/socket`. The suite is `IT2_SUITE`
//! when it belongs to the target's family (so a vTerm instance started with `-suite vterm-dev`
//! is reachable), else the target's default (`iTerm2`, `vTerm`). The cookie is requested from the
//! process serving that socket (by PID, see `auth`), so two apps or two instances of one app
//! never answer for each other. The caller's identity (`ITERM_SESSION_ID`) and an inherited
//! cookie count only when the caller runs in the target itself.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    Iterm2,
    Vterm,
}

pub const VTERM_BUNDLE_ID: &str = "com.nikhilgupta.vterm";
pub const ITERM2_BUNDLE_ID: &str = "com.googlecode.iterm2";

impl App {
    pub fn parse(s: &str) -> Option<App> {
        match s.trim().to_ascii_lowercase().as_str() {
            "iterm2" | "iterm" => Some(App::Iterm2),
            "vterm" => Some(App::Vterm),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            App::Iterm2 => "iterm2",
            App::Vterm => "vterm",
        }
    }
    /// The name people see in messages.
    pub fn display(self) -> &'static str {
        match self {
            App::Iterm2 => "iTerm2",
            App::Vterm => "vTerm",
        }
    }
    pub fn bundle_id(self) -> &'static str {
        match self {
            App::Iterm2 => ITERM2_BUNDLE_ID,
            App::Vterm => VTERM_BUNDLE_ID,
        }
    }
    pub fn default_suite(self) -> &'static str {
        match self {
            App::Iterm2 => "iTerm2",
            App::Vterm => "vTerm",
        }
    }
    pub fn default_path(self) -> &'static str {
        match self {
            App::Iterm2 => "/Applications/iTerm.app",
            App::Vterm => "/Applications/vTerm.app",
        }
    }
    /// The family a settings suite belongs to: vTerm's start with "vterm" (any case).
    pub fn of_suite(suite: &str) -> App {
        if suite.to_ascii_lowercase().starts_with("vterm") {
            App::Vterm
        } else {
            App::Iterm2
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub app: App,
    /// The settings suite: names the API socket directory.
    pub suite: String,
    pub bundle_id: String,
    /// The app bundle, for messages and the osascript fallback.
    pub app_path: PathBuf,
    /// The caller runs in this very app instance: its ITERM_SESSION_ID and ITERM2_COOKIE count.
    pub inside: bool,
    /// What chose the app: flag, binary, env or caller.
    pub source: &'static str,
}

impl Target {
    pub fn socket(&self, home: &Path) -> PathBuf {
        home.join("Library/Application Support")
            .join(&self.suite)
            .join("private/socket")
    }

    /// The defaults domain holding the target's settings: the suite, except iTerm2 run without
    /// `-suite`, which keeps its bundle's own domain.
    pub fn prefs_domain(&self) -> String {
        if self.app == App::Iterm2 && self.suite == App::Iterm2.default_suite() {
            ITERM2_BUNDLE_ID.to_string()
        } else {
            self.suite.clone()
        }
    }

    /// The app instance a command reached, for messages: `vTerm (suite vTerm)`.
    pub fn reached(&self) -> String {
        format!("{} (suite {})", self.app.display(), self.suite)
    }

    /// The target's EnableAPIServer setting, read (never written) with `defaults read`: None
    /// when unset or unreadable.
    pub fn api_server_enabled(&self) -> Option<bool> {
        let out = std::process::Command::new("/usr/bin/defaults")
            .args(["read", &self.prefs_domain(), "EnableAPIServer"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        parse_defaults_bool(out.status.success(), &String::from_utf8_lossy(&out.stdout))
    }
}

/// A `defaults read` boolean: Some(true/false) when set, None when unset (non-zero exit) or not
/// a boolean.
pub fn parse_defaults_bool(success: bool, stdout: &str) -> Option<bool> {
    if !success {
        return None;
    }
    match stdout.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Why the target's API socket is missing, from its EnableAPIServer setting: disabled (or unset on
/// iTerm2, where it defaults off) says where to enable it; enabled means the app is not running
/// there. vTerm 0.3.0+ defaults it on, so unset there means not running, or an older vTerm.
pub fn unreachable_message(t: &Target, socket: &Path, api_enabled: Option<bool>) -> String {
    let app = t.app.display();
    match (api_enabled, t.app) {
        (Some(true), _) => format!(
            "{app} API is not reachable: {app} is not running in suite {} (its API server is enabled, but there is no socket at {}); start {app}",
            t.suite,
            socket.display()
        ),
        (None, App::Vterm) => format!(
            "{app} API is not reachable: {app} is not running in suite {}, or it is older than 0.3.0, whose API server is off until Settings → General → Magic → Enable Python API (EnableAPIServer is unset in defaults domain {}; expected socket {})",
            t.suite,
            t.prefs_domain(),
            socket.display()
        ),
        _ => format!(
            "{app} API is not reachable: API server disabled in suite {}: Settings → General → Magic → Enable Python API (EnableAPIServer is {} in defaults domain {}; expected socket {})",
            t.suite,
            if api_enabled == Some(false) { "false" } else { "unset" },
            t.prefs_domain(),
            socket.display()
        ),
    }
}

/// The doctor line for the API server setting.
pub fn api_server_line(t: &Target, api_enabled: Option<bool>) -> String {
    let state = match (api_enabled, t.app) {
        (Some(true), _) => "enabled",
        (Some(false), _) => "disabled",
        (None, App::Vterm) => "unset (on by default since vTerm 0.3.0)",
        (None, _) => "unset (disabled)",
    };
    format!(
        "api server: {state} in suite {} (defaults read {} EnableAPIServer)",
        t.suite,
        t.prefs_domain()
    )
}

/// The app the caller's environment names: vTerm when IT2_SUITE or IT2_APP_PATH says so (set by
/// vTerm in its sessions, or by hand outside), iTerm2 in any other iTerm-family pane, else None.
pub fn caller_app(env: &dyn Fn(&str) -> Option<String>) -> Option<App> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(suite) = get("IT2_SUITE") {
        if App::of_suite(&suite) == App::Vterm {
            return Some(App::Vterm);
        }
    }
    if let Some(path) = get("IT2_APP_PATH") {
        if bundle_id_of(Path::new(&path)).as_deref() == Some(VTERM_BUNDLE_ID) {
            return Some(App::Vterm);
        }
    }
    get("ITERM_SESSION_ID").map(|_| App::Iterm2)
}

/// Choose the target. `flag` is `--app`; `binary` is the app the binary's name implies.
pub fn resolve(
    flag: Option<&str>,
    binary: Option<App>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Target, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    let bad =
        |what: &str, v: &str| format!("{what}: invalid choice: '{v}' (choose from iterm2, vterm)");
    let caller = caller_app(env);
    let (app, source) = if let Some(f) = flag {
        (
            App::parse(f).ok_or_else(|| bad("argument --app", f))?,
            "flag",
        )
    } else if let Some(b) = binary {
        (b, "binary")
    } else if let Some(e) = get("ITERM_APP") {
        (App::parse(&e).ok_or_else(|| bad("ITERM_APP", &e))?, "env")
    } else {
        (caller.unwrap_or(App::Iterm2), "caller")
    };
    let env_suite = get("IT2_SUITE");
    let suite = match &env_suite {
        Some(s) if App::of_suite(s) == app => s.clone(),
        _ => app.default_suite().to_string(),
    };
    let caller_suite = env_suite
        .clone()
        .unwrap_or_else(|| App::Iterm2.default_suite().to_string());
    let inside = get("ITERM_SESSION_ID").is_some() && caller == Some(app) && caller_suite == suite;
    let app_path = match get("IT2_APP_PATH") {
        Some(p)
            if bundle_id_of(Path::new(&p))
                .map(|b| b == app.bundle_id())
                .unwrap_or(App::of_path(&p) == app) =>
        {
            PathBuf::from(p)
        }
        _ => PathBuf::from(app.default_path()),
    };
    let bundle_id = match (app, get("ITERM_API_BUNDLE_ID")) {
        (App::Iterm2, Some(b)) => b,
        _ => app.bundle_id().to_string(),
    };
    Ok(Target {
        app,
        suite,
        bundle_id,
        app_path,
        inside,
        source,
    })
}

impl App {
    fn of_path(p: &str) -> App {
        if p.to_ascii_lowercase().contains("vterm") {
            App::Vterm
        } else {
            App::Iterm2
        }
    }
}

/// CFBundleIdentifier of an app bundle, read from its Info.plist (XML or binary) by CoreFoundation.
pub fn bundle_id_of(app: &Path) -> Option<String> {
    crate::iterm2::auth::bundle_identifier(app)
}

static CURRENT: OnceLock<Target> = OnceLock::new();

/// Fix the process's target (the CLI does this once, before connecting).
pub fn set(t: Target) {
    let _ = CURRENT.set(t);
}

/// The process's target: as set, else resolved from the environment alone.
pub fn current() -> &'static Target {
    CURRENT.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok();
        resolve(None, None, &env).unwrap_or_else(|_| {
            resolve(Some("iterm2"), None, &|_: &str| None).expect("default target")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn the_caller_app_is_the_default_and_every_switch_overrides_it() {
        let in_vterm = env(&[
            ("ITERM_SESSION_ID", "w0t0p0:AAAA"),
            ("IT2_SUITE", "vTerm"),
            ("IT2_APP_PATH", "/nonexistent/vTerm.app"),
        ]);
        let t = resolve(None, None, &in_vterm).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside, t.source),
            (App::Vterm, "vTerm", true, "caller")
        );
        assert_eq!(t.bundle_id, VTERM_BUNDLE_ID);
        // from inside vTerm, --app iterm2 reaches iTerm2's own socket and is not "inside"
        let t = resolve(Some("iterm2"), None, &in_vterm).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside),
            (App::Iterm2, "iTerm2", false)
        );
        assert_eq!(t.app_path, PathBuf::from("/Applications/iTerm.app")); // IT2_APP_PATH is vTerm's: ignored
        let in_iterm = env(&[("ITERM_SESSION_ID", "w0t0p0:BBBB")]);
        let t = resolve(None, None, &in_iterm).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside),
            (App::Iterm2, "iTerm2", true)
        );
        let t = resolve(None, Some(App::Vterm), &in_iterm).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside, t.source),
            (App::Vterm, "vTerm", false, "binary")
        );
        assert_eq!(t.app_path, PathBuf::from("/Applications/vTerm.app"));
        // the flag beats the binary name, the binary name beats ITERM_APP
        let with_env = env(&[("ITERM_SESSION_ID", "w0t0p0:BBBB"), ("ITERM_APP", "vterm")]);
        assert_eq!(resolve(None, None, &with_env).unwrap().source, "env");
        assert_eq!(resolve(None, None, &with_env).unwrap().app, App::Vterm);
        assert_eq!(
            resolve(None, Some(App::Iterm2), &with_env).unwrap().app,
            App::Iterm2
        );
        assert_eq!(
            resolve(Some("vterm"), Some(App::Iterm2), &with_env)
                .unwrap()
                .app,
            App::Vterm
        );
        assert!(resolve(Some("kitty"), None, &with_env)
            .unwrap_err()
            .contains("invalid choice"));
        assert!(resolve(None, None, &env(&[("ITERM_APP", "x")])).is_err());
        // outside any terminal: iTerm2, and nobody is inside
        let t = resolve(None, None, &env(&[])).unwrap();
        assert_eq!((t.app, t.inside), (App::Iterm2, false));
    }

    #[test]
    fn a_suite_counts_only_for_its_own_family() {
        // a vTerm dev instance (-suite vterm-dev): its sessions reach it, not the installed vTerm
        let dev = env(&[
            ("ITERM_SESSION_ID", "w0t0p0:CCCC"),
            ("IT2_SUITE", "vterm-dev"),
        ]);
        let t = resolve(None, None, &dev).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside),
            (App::Vterm, "vterm-dev", true)
        );
        assert!(t
            .socket(Path::new("/h"))
            .ends_with("Application Support/vterm-dev/private/socket"));
        // the documented outside switch
        let outside = env(&[
            ("IT2_SUITE", "vTerm"),
            ("IT2_APP_PATH", "/Applications/vTerm.app"),
        ]);
        let t = resolve(None, None, &outside).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside),
            (App::Vterm, "vTerm", false)
        ); // no pane: no identity
        let t = resolve(Some("iterm2"), None, &outside).unwrap();
        assert_eq!((t.app, t.suite.as_str()), (App::Iterm2, "iTerm2"));
        // an iTerm2 suite never redirects vTerm, and vice versa
        let it = env(&[
            ("ITERM_SESSION_ID", "w0t0p0:DDDD"),
            ("IT2_SUITE", "iterm-test"),
        ]);
        assert_eq!(resolve(Some("vterm"), None, &it).unwrap().suite, "vTerm");
        let t = resolve(None, None, &it).unwrap();
        assert_eq!(
            (t.app, t.suite.as_str(), t.inside),
            (App::Iterm2, "iterm-test", true)
        );
        assert_eq!(App::of_suite("VTERM-x"), App::Vterm);
    }

    #[test]
    fn a_missing_socket_says_whether_the_api_server_is_disabled_or_the_app_is_not_running() {
        let vterm = resolve(Some("vterm"), None, &env(&[])).unwrap();
        let sock = vterm.socket(Path::new("/h"));
        assert_eq!(vterm.prefs_domain(), "vTerm");
        let m = unreachable_message(&vterm, &sock, Some(false));
        assert!(m.contains("API server disabled in suite vTerm: Settings → General → Magic → Enable Python API"), "{m}");
        assert!(m.starts_with("vTerm API is not reachable"), "{m}");
        // vTerm 0.3.0+ defaults the server on: unset means not running, or an older vTerm
        let m = unreachable_message(&vterm, &sock, None);
        assert!(
            m.contains("vTerm is not running in suite vTerm, or it is older than 0.3.0"),
            "{m}"
        );
        assert!(
            m.contains("EnableAPIServer is unset in defaults domain vTerm"),
            "{m}"
        );
        // iTerm2 defaults it off, so unset there is disabled
        let it = resolve(Some("iterm2"), None, &env(&[])).unwrap();
        assert!(unreachable_message(&it, &it.socket(Path::new("/h")), None)
            .contains("API server disabled in suite"));
        assert_eq!(
            api_server_line(&it, None).split(" in suite").next(),
            Some("api server: unset (disabled)")
        );
        let m = unreachable_message(&vterm, &sock, Some(true));
        assert!(
            m.contains("vTerm is not running in suite vTerm")
                && m.contains("/h/Library/Application Support/vTerm/private/socket"),
            "{m}"
        );
        assert!(!m.contains("disabled"), "{m}");
        // iTerm2 without -suite keeps its bundle's domain; a named suite is its own domain
        assert_eq!(
            resolve(Some("iterm2"), None, &env(&[]))
                .unwrap()
                .prefs_domain(),
            ITERM2_BUNDLE_ID
        );
        let dev = resolve(None, None, &env(&[("IT2_SUITE", "vterm-dev")])).unwrap();
        assert_eq!(
            (dev.prefs_domain(), dev.reached()),
            (
                "vterm-dev".to_string(),
                "vTerm (suite vterm-dev)".to_string()
            )
        );
        assert!(
            unreachable_message(&dev, &dev.socket(Path::new("/h")), Some(false))
                .contains("disabled in suite vterm-dev")
        );
        assert_eq!(api_server_line(&vterm, None), "api server: unset (on by default since vTerm 0.3.0) in suite vTerm (defaults read vTerm EnableAPIServer)");
        assert!(
            api_server_line(&vterm, Some(true)).starts_with("api server: enabled in suite vTerm")
        );
    }

    #[test]
    fn defaults_booleans() {
        assert_eq!(parse_defaults_bool(true, "1\n"), Some(true));
        assert_eq!(parse_defaults_bool(true, "0\n"), Some(false));
        assert_eq!(parse_defaults_bool(true, "YES"), Some(true));
        assert_eq!(parse_defaults_bool(false, ""), None); // unset: defaults exits non-zero
        assert_eq!(parse_defaults_bool(true, "{ x = 1; }"), None);
    }
}
