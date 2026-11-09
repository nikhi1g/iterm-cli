//! Settings on iTerm2, as one key space for `SettingOp` (what `setting`, `profile`, `key` and `preset` use):
//!
//! - `profile`: the default profile (new windows, tabs and panes start with it). Setting a name that does not
//!   exist makes it, as a copy of the current default, and makes it the default. `profiles`: every profile's
//!   name (read only). `profile_new=NAME` makes one (a copy of the default, not switched to);
//!   `profile_delete=NAME` deletes a profile this CLI made; `profile_import=FILE` makes a
//!   profile from an `.itermcolors` file, named after it.
//! - Profile properties by iTerm2's own names (`Normal Font`, `Background Color`, …) or the short names in
//!   [`ALIASES`] (`font`, `font_size`, `bg`, …), on the default profile; `KEY@PROFILE` names another profile and
//!   `@PROFILE` (Get) all of its keys. Colors read and write as `#rrggbb`. `preset=NAME` applies a color preset.
//! - `presets`: the color presets' names; `preset:NAME`: one preset's colors (Get).
//! - `pref.KEY`: app preferences ([`pref_keys`], the API's documented keys).
//! - `keys.ACTION`: an action's key binding (macOS's per-app menu shortcuts, read by iTerm2 at launch).
//!
//! Profiles made here are iTerm2 dynamic profiles (`~/Library/Application Support/iTerm2/DynamicProfiles/
//! iterm-cli-*.json`, GUID `iterm-cli-…`): property changes are also written to the file, so they persist.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::app::{Done, Fail, SettingOp};

use super::super::proto::{self as p, client_originated_message::Submessage as Req, server_originated_message::Submessage as Resp};
use super::super::Live;
use super::{internal, invalid, menu, rpc};

static PREF_KEYS: &str = include_str!("pref_keys.tsv");

/// The API's documented app preference keys: (key, type, doc).
pub fn pref_keys() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    PREF_KEYS.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).filter_map(|l| {
        let mut f = l.split('\t');
        Some((f.next()?, f.next()?, f.next().unwrap_or("")))
    })
}

/// Short names for the profile keys set most: (alias, iTerm2's key).
pub const ALIASES: [(&str, &str); 14] = [
    ("font", "Normal Font"),
    ("font_size", "Normal Font"),
    ("bg", "Background Color"),
    ("background", "Background Color"),
    ("fg", "Foreground Color"),
    ("foreground", "Foreground Color"),
    ("cols", "Columns"),
    ("rows", "Rows"),
    ("scrollback", "Scrollback Lines"),
    ("cursor", "Cursor Type"),
    ("badge", "Badge Text"),
    ("cwd", "Working Directory"),
    ("tab_color", "Tab Color"),
    ("name", "Name"),
];

const DYN_PREFIX: &str = "iterm-cli-";

// ---- pure translation (unit tested) ---------------------------------------------------------------------

/// iTerm2's color dictionary as `#rrggbb` (None: not a color).
pub fn color_hex(v: &Value) -> Option<String> {
    let o = v.as_object()?;
    let c = |k: &str| o.get(k).and_then(Value::as_f64).map(|x| (x.clamp(0.0, 1.0) * 255.0).round() as u8);
    Some(format!("#{:02x}{:02x}{:02x}", c("Red Component")?, c("Green Component")?, c("Blue Component")?))
}

/// `#rrggbb` as iTerm2's color dictionary (sRGB).
pub fn hex_color(s: &str) -> Option<Value> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|x| x as f64 / 255.0);
    Some(json!({"Red Component": c(0)?, "Green Component": c(2)?, "Blue Component": c(4)?, "Alpha Component": 1.0, "Color Space": "sRGB"}))
}

/// A value as the CLI shows it: colors as `#rrggbb`, the rest as iTerm2 has it.
pub fn show(v: &Value) -> Value {
    color_hex(v).map(Value::from).unwrap_or_else(|| v.clone())
}

/// `Ansi 1 Color` → `ansi.1`, `Cursor Text Color` → `cursor_text`, `Background Color (Dark)` stays apart.
pub fn color_name(key: &str) -> String {
    let k = key.trim();
    if let Some(n) = k.strip_prefix("Ansi ").and_then(|r| r.strip_suffix(" Color")) {
        return format!("ansi.{n}");
    }
    k.trim_end_matches(" Color").to_lowercase().replace(' ', "_")
}

/// A string as the JSON value for a key whose current (or typed) value is `like`.
pub fn parse_as(key: &str, raw: &str, like: Option<&Value>, ty: Option<&str>) -> Result<Value, String> {
    let bad = |want: &str| format!("{key}: '{raw}' is not {want}");
    let t = raw.trim();
    let kind = match (ty, like) {
        (Some(t), _) => t,
        (None, Some(Value::Bool(_))) => "bool",
        (None, Some(Value::Number(n))) if n.is_i64() || n.is_u64() => "int",
        (None, Some(Value::Number(_))) => "float",
        (None, Some(Value::String(_))) => "string",
        (None, Some(v)) if color_hex(v).is_some() => "color",
        (None, Some(Value::Null)) | (None, None) => "auto",
        (None, Some(_)) => "json",
    };
    Ok(match kind {
        "bool" => json!(match t.to_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => true,
            "false" | "no" | "off" | "0" => false,
            _ => return Err(bad("true or false")),
        }),
        "int" => json!(t.parse::<i64>().map_err(|_| bad("a whole number"))?),
        "float" => json!(t.parse::<f64>().map_err(|_| bad("a number"))?),
        "string" => json!(raw),
        "color" => hex_color(t).ok_or_else(|| bad("a color (#rrggbb)"))?,
        "json" => serde_json::from_str(t).map_err(|_| bad("JSON"))?,
        // no type known: a color, JSON, else the text
        _ => hex_color(t).or_else(|| serde_json::from_str(t).ok()).unwrap_or_else(|| json!(raw)),
    })
}

/// `Menlo-Regular 12` → (`Menlo-Regular`, `12`).
pub fn split_font(f: &str) -> (&str, &str) {
    match f.rsplit_once(' ') {
        Some((name, size)) if size.parse::<f64>().is_ok() => (name, size),
        _ => (f, ""),
    }
}

fn slug(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest of `options` to `word`, when reasonably close.
pub fn closest<'a>(word: &str, options: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let w = slug(word);
    options
        .into_iter()
        .map(|o| (distance(&w, &slug(o)), o))
        .filter(|(d, o)| *d <= (slug(o).len().max(w.len()) / 3).max(2))
        .min_by_key(|(d, _)| *d)
        .map(|(_, o)| o)
}

/// A profile key as asked (`font`, `normal font`, `Normal Font`) → iTerm2's key, among `known`.
pub fn resolve_key(name: &str, known: &Map<String, Value>) -> Result<String, String> {
    if known.contains_key(name) {
        return Ok(name.to_string());
    }
    if let Some((_, real)) = ALIASES.iter().find(|(a, _)| *a == name) {
        return Ok(real.to_string());
    }
    let s = slug(name);
    if let Some(k) = known.keys().find(|k| slug(k) == s) {
        return Ok(k.clone());
    }
    let opts = known.keys().map(String::as_str).chain(ALIASES.iter().map(|(a, _)| *a)).chain(["profile", "profiles", "presets"]);
    Err(match closest(name, opts) {
        Some(c) => format!("no setting '{name}'; closest: {c} (iterm setting keys --grep)"),
        None => format!("no setting '{name}' (iterm setting keys)"),
    })
}

/// `cmd+shift+d` → macOS's menu shortcut form `@$d`.
pub fn combo_to_mac(c: &str) -> Option<String> {
    let mut out = String::new();
    let parts: Vec<&str> = c.split('+').collect();
    let (key, mods) = parts.split_last()?;
    let (mut cmd, mut opt, mut ctrl, mut shift) = (false, false, false, false);
    for m in mods {
        match m.to_lowercase().as_str() {
            "cmd" | "command" | "super" => cmd = true,
            "opt" | "option" | "alt" => opt = true,
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            _ => return None,
        }
    }
    for (on, c) in [(ctrl, '^'), (opt, '~'), (shift, '$'), (cmd, '@')] {
        if on {
            out.push(c);
        }
    }
    let k = match key.to_lowercase().as_str() {
        "up" => "\u{f700}".to_string(),
        "down" => "\u{f701}".to_string(),
        "left" => "\u{f702}".to_string(),
        "right" => "\u{f703}".to_string(),
        "return" | "enter" => "\r".to_string(),
        "tab" => "\t".to_string(),
        "space" => " ".to_string(),
        k if k.chars().count() == 1 => k.to_string(),
        k => {
            let n: u32 = k.strip_prefix('f')?.parse().ok().filter(|n| (1..=20).contains(n))?;
            char::from_u32(0xF704 + n - 1)?.to_string()
        }
    };
    out.push_str(&k);
    Some(out)
}

/// `@$d` → `shift+cmd+d`.
pub fn mac_to_combo(m: &str) -> String {
    let mut mods = (false, false, false, false);
    let mut rest = m;
    while let Some(c) = rest.chars().next().filter(|c| "^~$@".contains(*c)) {
        match c {
            '^' => mods.0 = true,
            '~' => mods.1 = true,
            '$' => mods.2 = true,
            _ => mods.3 = true,
        }
        rest = &rest[c.len_utf8()..];
        if rest.chars().count() <= 1 {
            break;
        }
    }
    let key = match rest {
        "\u{f700}" => "up".to_string(),
        "\u{f701}" => "down".to_string(),
        "\u{f702}" => "left".to_string(),
        "\u{f703}" => "right".to_string(),
        "\r" => "return".to_string(),
        "\t" => "tab".to_string(),
        " " => "space".to_string(),
        k => match k.chars().next().map(|c| c as u32) {
            Some(c @ 0xF704..=0xF717) if k.chars().count() == 1 => format!("f{}", c - 0xF704 + 1),
            _ => k.to_lowercase(),
        },
    };
    let mut parts: Vec<String> = [(mods.0, "ctrl"), (mods.1, "opt"), (mods.2, "shift"), (mods.3, "cmd")].iter().filter(|(on, _)| *on).map(|(_, n)| n.to_string()).collect();
    parts.push(key);
    parts.join("+")
}

/// A dynamic profile's file content.
pub fn dynamic_profile(name: &str, guid: &str, parent: &str, extra: &Map<String, Value>) -> Value {
    let mut p = Map::new();
    p.insert("Name".into(), json!(name));
    p.insert("Guid".into(), json!(guid));
    p.insert("Dynamic Profile Parent Name".into(), json!(parent));
    for (k, v) in extra {
        p.insert(k.clone(), v.clone());
    }
    json!({ "Profiles": [Value::Object(p)] })
}

// ---- iTerm2 ---------------------------------------------------------------------------------------------

struct Profile {
    guid: String,
    name: String,
    props: Map<String, Value>,
}

fn list_profiles(live: &mut Live, guids: Vec<String>, props: &[&str]) -> Result<Vec<Profile>, Fail> {
    let req = p::ListProfilesRequest { properties: props.iter().map(|s| s.to_string()).collect(), guids };
    let Resp::ListProfilesResponse(r) = rpc(live, Req::ListProfilesRequest(req))? else { return Err(internal("unexpected reply to list profiles")) };
    Ok(r
        .profiles
        .into_iter()
        .map(|pr| {
            let props: Map<String, Value> = pr
                .properties
                .into_iter()
                .filter_map(|x| Some((x.key?, x.json_value.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or(Value::Null))))
                .collect();
            let s = |k: &str| props.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            Profile { guid: s("Guid"), name: s("Name"), props }
        })
        .collect())
}

pub fn profile_names(live: &mut Live) -> Result<Vec<String>, Fail> {
    let mut n: Vec<String> = list_profiles(live, vec![], &["Name", "Guid"])?.into_iter().map(|p| p.name).collect();
    n.sort_by_key(|a| a.to_lowercase());
    Ok(n)
}

fn prefs_call(live: &mut Live, reqs: Vec<p::preferences_request::request::Request>) -> Result<Vec<p::preferences_response::result::Result>, Fail> {
    let req = p::PreferencesRequest { requests: reqs.into_iter().map(|r| p::preferences_request::Request { request: Some(r) }).collect() };
    let Resp::PreferencesResponse(r) = rpc(live, Req::PreferencesRequest(req))? else { return Err(internal("unexpected reply to preferences")) };
    Ok(r.results.into_iter().filter_map(|x| x.result).collect())
}

fn default_guid(live: &mut Live) -> Result<String, Fail> {
    use p::preferences_request::request::{GetDefaultProfile, Request as R};
    use p::preferences_response::result::Result as X;
    match prefs_call(live, vec![R::GetDefaultProfileRequest(GetDefaultProfile {})])?.into_iter().next() {
        Some(X::GetDefaultProfileResult(g)) => g.guid.ok_or_else(|| internal("iTerm2 named no default profile")),
        _ => Err(internal("unexpected reply to get default profile")),
    }
}

fn get_prefs(live: &mut Live, keys: &[&str]) -> Result<Vec<Value>, Fail> {
    use p::preferences_request::request::{GetPreference, Request as R};
    use p::preferences_response::result::Result as X;
    let res = prefs_call(live, keys.iter().map(|k| R::GetPreferenceRequest(GetPreference { key: Some(k.to_string()) })).collect())?;
    Ok(res
        .into_iter()
        .map(|r| match r {
            X::GetPreferenceResult(g) => g.json_value.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or(Value::Null),
            _ => Value::Null,
        })
        .collect())
}

fn set_prefs(live: &mut Live, pairs: Vec<(String, Value)>) -> Result<(), Fail> {
    use p::preferences_request::request::{Request as R, SetPreference};
    use p::preferences_response::result::Result as X;
    let keys: Vec<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
    let res = prefs_call(live, pairs.into_iter().map(|(k, v)| R::SetPreferenceRequest(SetPreference { key: Some(k), json_value: Some(v.to_string()) })).collect())?;
    for (k, r) in keys.iter().zip(res) {
        match r {
            X::SetPreferenceResult(s) if s.status.unwrap_or(0) == 0 => {}
            X::SetPreferenceResult(s) => return Err(invalid(format!("pref.{k}: iTerm2 refused the value (status {})", s.status.unwrap_or(0)))),
            _ => return Err(internal("unexpected reply to set preference")),
        }
    }
    Ok(())
}

fn set_default(live: &mut Live, guid: &str) -> Result<(), Fail> {
    use p::preferences_request::request::{Request as R, SetDefaultProfile};
    use p::preferences_response::result::Result as X;
    match prefs_call(live, vec![R::SetDefaultProfileRequest(SetDefaultProfile { guid: Some(guid.into()) })])?.into_iter().next() {
        Some(X::SetDefaultProfileResult(s)) if s.status.unwrap_or(0) == 0 => Ok(()),
        _ => Err(invalid("iTerm2 refused the default profile")),
    }
}

fn set_profile_props(live: &mut Live, guid: &str, props: &[(String, Value)]) -> Result<(), Fail> {
    use p::set_profile_property_request::{Assignment, GuidList, Target};
    let req = p::SetProfilePropertyRequest {
        key: None,
        json_value: None,
        assignments: props.iter().map(|(k, v)| Assignment { key: Some(k.clone()), json_value: Some(v.to_string()) }).collect(),
        target: Some(Target::GuidList(GuidList { guids: vec![guid.into()] })),
    };
    match rpc(live, Req::SetProfilePropertyRequest(req))? {
        Resp::SetProfilePropertyResponse(r) if r.status.unwrap_or(0) == 0 => {}
        Resp::SetProfilePropertyResponse(r) => return Err(invalid(format!("iTerm2 refused the profile change (status {})", r.status.unwrap_or(0)))),
        _ => return Err(internal("unexpected reply to set profile property")),
    }
    // a profile this CLI made keeps its changes in its file (iTerm2 reloads dynamic profiles from it)
    if guid.starts_with(DYN_PREFIX) {
        let f = dyn_file(guid);
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
                if let Some(Value::Object(o)) = v.get_mut("Profiles").and_then(|a| a.get_mut(0)) {
                    for (k, val) in props {
                        o.insert(k.clone(), val.clone());
                    }
                    write_atomic(&f, &v)?;
                    // iTerm2 reloads the file a moment later, briefly showing the profile without the change:
                    // done when it reads back
                    let keys: Vec<&str> = props.iter().map(|(k, _)| k.as_str()).collect();
                    let end = Instant::now() + Duration::from_secs(3);
                    while Instant::now() < end {
                        let now = list_profiles(live, vec![guid.into()], &keys)?;
                        if now.first().is_some_and(|p| props.iter().all(|(k, v)| p.props.get(k).map(show) == Some(show(v)))) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }
    Ok(())
}

fn presets(live: &mut Live) -> Result<Vec<String>, Fail> {
    use p::color_preset_request::{ListPresets, Request as R};
    match rpc(live, Req::ColorPresetRequest(p::ColorPresetRequest { request: Some(R::ListPresets(ListPresets {})) }))? {
        Resp::ColorPresetResponse(r) => match r.response {
            Some(p::color_preset_response::Response::ListPresets(l)) => Ok(l.name),
            _ => Err(internal("no preset list in the reply")),
        },
        _ => Err(internal("unexpected reply to color presets")),
    }
}

/// A preset's colors: (iTerm2's key, color dictionary).
fn preset(live: &mut Live, name: &str) -> Result<Vec<(String, Value)>, Fail> {
    use p::color_preset_request::{GetPreset, Request as R};
    // names match case-insensitively, as the CLI's others do
    let real = presets(live)?.into_iter().find(|n| n.eq_ignore_ascii_case(name)).ok_or_else(|| invalid(format!("no color preset {name:?} (iterm preset list)")))?;
    match rpc(live, Req::ColorPresetRequest(p::ColorPresetRequest { request: Some(R::GetPreset(GetPreset { name: Some(real) })) }))? {
        Resp::ColorPresetResponse(r) => match r.response {
            Some(p::color_preset_response::Response::GetPreset(g)) => Ok(g
                .color_settings
                .into_iter()
                .filter_map(|c| {
                    let v = json!({
                        "Red Component": c.red.unwrap_or(0.0), "Green Component": c.green.unwrap_or(0.0), "Blue Component": c.blue.unwrap_or(0.0),
                        "Alpha Component": c.alpha.unwrap_or(1.0), "Color Space": c.color_space.unwrap_or_else(|| "sRGB".into()),
                    });
                    Some((c.key?, v))
                })
                .collect()),
            _ => Err(invalid(format!("no color preset {name:?}"))),
        },
        _ => Err(internal("unexpected reply to color presets")),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn dyn_dir() -> PathBuf {
    home().join("Library/Application Support/iTerm2/DynamicProfiles")
}

fn dyn_file(guid: &str) -> PathBuf {
    dyn_dir().join(format!("{guid}.json"))
}

fn write_atomic(f: &std::path::Path, v: &Value) -> Result<(), Fail> {
    let tmp = f.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).unwrap_or_default()).map_err(|e| internal(format!("{}: {e}", tmp.display())))?;
    // iTerm2 watches the folder; a `.tmp` file is not a profile until renamed
    std::fs::rename(&tmp, f).map_err(|e| internal(format!("{}: {e}", f.display())))
}

/// A new profile (a dynamic profile, child of `parent`) with `extra` properties; waits until iTerm2 lists it.
fn make_profile(live: &mut Live, name: &str, parent: &str, extra: &Map<String, Value>) -> Result<String, Fail> {
    use sha2::Digest;
    let seed = format!("{name}{}{:?}", std::process::id(), std::time::SystemTime::now());
    let h = sha2::Sha256::digest(seed.as_bytes());
    let guid = format!("{DYN_PREFIX}{}", h.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>());
    std::fs::create_dir_all(dyn_dir()).map_err(|e| internal(format!("{}: {e}", dyn_dir().display())))?;
    write_atomic(&dyn_file(&guid), &dynamic_profile(name, &guid, parent, extra))?;
    // iTerm2 loads the folder on a file-system event: there is no API notification for a new profile
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if list_profiles(live, vec![guid.clone()], &["Name"])?.iter().any(|p| p.name == name) {
            return Ok(guid);
        }
        if Instant::now() >= end {
            return Err(Fail::new(crate::app::FailCode::Timeout, format!("iTerm2 did not load the new profile {name:?} ({})", dyn_file(&guid).display())));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The user's per-app menu shortcuts (`NSUserKeyEquivalents`): menu title → combo.
pub fn key_overrides(live: &mut Live) -> HashMap<String, String> {
    get_prefs(live, &["NSUserKeyEquivalents"])
        .ok()
        .and_then(|v| v.into_iter().next())
        .and_then(|v| v.as_object().cloned())
        .map(|o| o.into_iter().filter_map(|(k, v)| Some((k, mac_to_combo(v.as_str()?)))).collect())
        .unwrap_or_default()
}

fn title_of(path: &str) -> &str {
    path.rsplit(" > ").next().unwrap_or(path)
}

/// One key as asked, split by its namespace.
enum Key<'a> {
    Profile,
    Profiles,
    Presets,
    Preset(&'a str),
    Pref(&'a str),
    Bind(&'a str),
    /// A profile key (None: all), on a profile (None: the default).
    Prop(Option<&'a str>, Option<&'a str>),
}

fn parse_key(k: &str) -> Key<'_> {
    match k {
        "profile" => Key::Profile,
        "profiles" => Key::Profiles,
        "presets" => Key::Presets,
        _ => {
            if let Some(n) = k.strip_prefix("preset:") {
                Key::Preset(n)
            } else if let Some(n) = k.strip_prefix("pref.") {
                Key::Pref(n)
            } else if let Some(n) = k.strip_prefix("keys.") {
                Key::Bind(n)
            } else if let Some((key, prof)) = k.rsplit_once('@') {
                Key::Prop((!key.is_empty()).then_some(key), Some(prof))
            } else {
                Key::Prop(Some(k), None)
            }
        }
    }
}

/// A profile by name (case-insensitive), all its properties.
fn profile_named(live: &mut Live, name: &str) -> Result<Profile, Fail> {
    let all = list_profiles(live, vec![], &["Name", "Guid"])?;
    let p = all.iter().find(|p| p.name == name).or_else(|| all.iter().find(|p| p.name.eq_ignore_ascii_case(name)));
    let Some(p) = p else {
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        return Err(invalid(match closest(name, names) {
            Some(c) => format!("no profile '{name}'; closest: {c} (iterm profile list)"),
            None => format!("no profile '{name}' (iterm profile list)"),
        }));
    };
    let guid = p.guid.clone();
    list_profiles(live, vec![guid], &[])?.into_iter().next().ok_or_else(|| invalid(format!("no profile '{name}'")))
}

fn default_profile(live: &mut Live) -> Result<Profile, Fail> {
    let g = default_guid(live)?;
    list_profiles(live, vec![g], &[])?.into_iter().next().ok_or_else(|| internal("the default profile is not listed"))
}

/// A profile key's value as shown, aliases included.
fn prop_value(p: &Profile, asked: &str) -> Result<Value, String> {
    let real = resolve_key(asked, &p.props)?;
    let v = p.props.get(&real).cloned().unwrap_or(Value::Null);
    Ok(match asked {
        "font" => json!(split_font(v.as_str().unwrap_or("")).0),
        "font_size" => split_font(v.as_str().unwrap_or("")).1.parse::<f64>().map(|n| if n.fract() == 0.0 { json!(n as i64) } else { json!(n) }).unwrap_or(Value::Null),
        _ => show(&v),
    })
}

fn line(v: Value) -> Result<Done, Fail> {
    Ok(Done { ids: vec![], lines: vec![v.to_string()] })
}

pub fn setting(live: &mut Live, op: SettingOp) -> Result<Done, Fail> {
    match op {
        SettingOp::Keys => keys(live),
        SettingOp::Get { keys } => get(live, keys),
        SettingOp::Set { pairs } => set(live, pairs),
        SettingOp::Reset { keys } => reset(live, keys),
    }
}

fn keys(live: &mut Live) -> Result<Done, Fail> {
    let mut out = vec![
        json!({"key": "profile", "type": "string", "doc": "the default profile (new windows, tabs and panes); a new name makes it, as a copy"}),
        json!({"key": "profiles", "type": "array", "doc": "every profile's name (read only)"}),
        json!({"key": "profile_new", "type": "string", "doc": "make a profile, a copy of the default (not switched to)"}),
        json!({"key": "profile_delete", "type": "string", "doc": "delete a profile this CLI made"}),
        json!({"key": "profile_import", "type": "string", "doc": "make a profile from an .itermcolors file"}),
        json!({"key": "preset", "type": "string", "doc": "apply a color preset to the profile (KEY@PROFILE for another)"}),
        json!({"key": "presets", "type": "array", "doc": "the color presets' names (read only); preset:NAME its colors"}),
    ];
    for (a, real) in ALIASES {
        out.push(json!({"key": a, "type": "alias", "doc": format!("profile key '{real}'")}));
    }
    let d = default_profile(live)?;
    for (k, v) in &d.props {
        let ty = match v {
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            v if color_hex(v).is_some() => "color",
            Value::Array(_) => "array",
            _ => "object",
        };
        out.push(json!({"key": k, "type": ty, "default": show(v), "doc": "profile key (on the default profile; KEY@PROFILE for another)"}));
    }
    for (k, ty, doc) in pref_keys() {
        out.push(json!({"key": format!("pref.{k}"), "type": ty, "doc": doc}));
    }
    for x in menu::ACTIONS {
        out.push(json!({"key": format!("keys.{}", x.name), "type": "key", "default": menu::action_item(x).key, "doc": x.does}));
    }
    Ok(Done { ids: vec![], lines: out.into_iter().map(|v| v.to_string()).collect() })
}

fn get(live: &mut Live, keys: Vec<String>) -> Result<Done, Fail> {
    let all = keys.is_empty();
    let keys: Vec<String> = if all { vec!["profile".into(), "@".into(), "pref.*".into(), "keys.*".into()] } else { keys };
    let mut o = Map::new();
    let mut default: Option<Profile> = None;
    let mut named: HashMap<String, Profile> = HashMap::new();
    let mut overrides: Option<HashMap<String, String>> = None;
    for k in &keys {
        match parse_key(k) {
            Key::Profile => {
                if default.is_none() {
                    default = Some(default_profile(live)?);
                }
                o.insert(k.clone(), json!(default.as_ref().map(|d| d.name.clone())));
            }
            Key::Profiles => {
                o.insert(k.clone(), json!(profile_names(live)?));
            }
            Key::Presets => {
                o.insert(k.clone(), json!(presets(live)?));
            }
            Key::Preset(n) => {
                let colors: Map<String, Value> = preset(live, n)?.into_iter().map(|(k, v)| (color_name(&k), show(&v))).collect();
                o.insert(k.clone(), Value::Object(colors));
            }
            Key::Pref("*") => {
                let ks: Vec<&str> = pref_keys().map(|(k, _, _)| k).collect();
                for (key, v) in ks.iter().zip(get_prefs(live, &ks)?) {
                    o.insert(format!("pref.{key}"), v);
                }
            }
            Key::Pref(n) => {
                let v = get_prefs(live, &[n])?.into_iter().next().unwrap_or(Value::Null);
                o.insert(k.clone(), v);
            }
            Key::Bind(a) => {
                let ov = overrides.get_or_insert_with(|| key_overrides(live)).clone();
                let acts: Vec<&menu::Action> = if a == "*" { menu::ACTIONS.iter().collect() } else { menu::action(a).into_iter().collect() };
                if acts.is_empty() {
                    return Err(invalid(format!("no action '{a}' (iterm key keys)")));
                }
                for x in acts {
                    let item = menu::action_item(x);
                    o.insert(format!("keys.{}", x.name), json!(ov.get(title_of(item.path)).cloned().unwrap_or_else(|| "default".into())));
                }
            }
            Key::Prop(key, prof) => {
                let p = match prof.filter(|n| !n.is_empty()) {
                    None => {
                        if default.is_none() {
                            default = Some(default_profile(live)?);
                        }
                        default.as_ref().unwrap()
                    }
                    Some(n) => {
                        if !named.contains_key(n) {
                            let p = profile_named(live, n)?;
                            named.insert(n.to_string(), p);
                        }
                        &named[n]
                    }
                };
                match key {
                    Some(key) => {
                        o.insert(k.clone(), prop_value(p, key).map_err(invalid)?);
                    }
                    None => {
                        if all {
                            for a in ["font", "font_size"] {
                                o.insert(a.into(), prop_value(p, a).unwrap_or(Value::Null));
                            }
                        }
                        for (pk, v) in &p.props {
                            o.insert(pk.clone(), show(v));
                        }
                    }
                }
            }
        }
    }
    line(Value::Object(o))
}

/// What a `Set` will do, checked as a whole before anything runs.
enum Change {
    Default(String),
    New(String),
    Delete(String),
    Import(PathBuf),
    Pref(String, Value),
    Bind(String, Option<String>),
    /// Properties on a profile (None: the default).
    Props(Option<String>, Vec<(String, Value)>),
}

fn set(live: &mut Live, pairs: Vec<(String, String)>) -> Result<Done, Fail> {
    let mut changes: Vec<Change> = Vec::new();
    let mut shown = Map::new();
    let mut live_now = true;
    let mut cache: HashMap<String, Profile> = HashMap::new();
    for (k, v) in &pairs {
        shown.insert(k.clone(), json!(v));
        match parse_key(k) {
            Key::Profile => changes.push(Change::Default(v.clone())),
            Key::Profiles | Key::Presets | Key::Preset(_) => return Err(invalid(format!("{k} is read only"))),
            Key::Pref(n) => {
                let Some((_, ty, _)) = pref_keys().find(|(x, _, _)| *x == n) else {
                    let near = closest(n, pref_keys().map(|(x, _, _)| x));
                    return Err(invalid(format!("no setting 'pref.{n}'{} (iterm setting keys --grep pref.)", near.map(|c| format!("; closest: pref.{c}")).unwrap_or_default())));
                };
                changes.push(Change::Pref(n.to_string(), parse_as(k, v, None, Some(ty)).map_err(invalid)?));
            }
            Key::Bind(a) => {
                if menu::action(a).is_none() {
                    return Err(invalid(format!("no action '{a}' (iterm key keys)")));
                }
                let mac = if v == "default" { None } else { Some(combo_to_mac(v).ok_or_else(|| invalid(format!("keys.{a}: '{v}' is not a key (cmd+shift+d, opt+t, ctrl+f5)")))?) };
                live_now = false;
                changes.push(Change::Bind(a.to_string(), mac));
            }
            Key::Prop(Some("profile_delete"), None) => changes.push(Change::Delete(v.clone())),
            Key::Prop(Some("profile_new"), None) => changes.push(Change::New(v.clone())),
            Key::Prop(Some("profile_import"), None) => changes.push(Change::Import(PathBuf::from(v))),
            Key::Prop(None, _) => return Err(invalid(format!("'{k}': name a key (KEY@PROFILE)"))),
            Key::Prop(Some(key), prof) => {
                let pname = prof.map(str::to_string);
                let ck = pname.clone().unwrap_or_default();
                if !cache.contains_key(&ck) {
                    let p = match &pname {
                        Some(n) => profile_named(live, n)?,
                        None => default_profile(live)?,
                    };
                    cache.insert(ck.clone(), p);
                }
                let p = &cache[&ck];
                let vals: Vec<(String, Value)> = if key == "preset" {
                    preset(live, v)?
                } else {
                    let real = resolve_key(key, &p.props).map_err(invalid)?;
                    let cur = p.props.get(&real);
                    let val = match key {
                        "font" => json!(format!("{} {}", v.trim(), split_font(cur.and_then(Value::as_str).unwrap_or("")).1).trim().to_string()),
                        "font_size" => {
                            let n: f64 = v.trim().parse().map_err(|_| invalid(format!("font_size: '{v}' is not a number")))?;
                            json!(format!("{} {}", split_font(cur.and_then(Value::as_str).unwrap_or("Menlo-Regular")).0, n))
                        }
                        _ => parse_as(&real, v, cur, None).map_err(invalid)?,
                    };
                    vec![(real, val)]
                };
                match changes.iter_mut().find(|c| matches!(c, Change::Props(n, _) if *n == pname)) {
                    Some(Change::Props(_, list)) => list.extend(vals),
                    _ => changes.push(Change::Props(pname, vals)),
                }
            }
        }
    }
    for c in changes {
        match c {
            Change::Default(name) => {
                let all = list_profiles(live, vec![], &["Name", "Guid"])?;
                let guid = match all.iter().find(|p| p.name == name) {
                    Some(p) => p.guid.clone(),
                    None => {
                        let parent = default_profile(live)?.name;
                        make_profile(live, &name, &parent, &Map::new())?
                    }
                };
                set_default(live, &guid)?;
            }
            Change::New(name) => {
                if list_profiles(live, vec![], &["Name", "Guid"])?.iter().any(|p| p.name == name) {
                    return Err(invalid(format!("a profile '{name}' exists (iterm profile list)")));
                }
                let parent = default_profile(live)?.name;
                make_profile(live, &name, &parent, &Map::new())?;
            }
            Change::Delete(name) => {
                let all = list_profiles(live, vec![], &["Name", "Guid"])?;
                let p = all.iter().find(|p| p.name == name).ok_or_else(|| invalid(format!("no profile '{name}' (iterm profile list)")))?;
                if !p.guid.starts_with(DYN_PREFIX) {
                    return Err(invalid(format!("'{name}' was not made by this CLI; iTerm2's API deletes no other profile (Settings > Profiles > −)")));
                }
                if default_guid(live)? == p.guid {
                    if let Some(d) = all.iter().find(|x| x.name == "Default").or_else(|| all.iter().find(|x| !x.guid.starts_with(DYN_PREFIX))) {
                        set_default(live, &d.guid)?;
                    }
                }
                // to the Trash, never gone
                let f = dyn_file(&p.guid);
                let trash = home().join(".Trash").join(f.file_name().unwrap_or_default());
                std::fs::rename(&f, &trash).map_err(|e| internal(format!("{}: {e}", f.display())))?;
                // iTerm2 drops it on the file-system event: done when it is no longer listed
                let end = Instant::now() + Duration::from_secs(5);
                while Instant::now() < end && list_profiles(live, vec![], &["Guid"])?.iter().any(|x| x.guid == p.guid) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Change::Import(file) => {
                let name = file.file_stem().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).ok_or_else(|| invalid("profile_import: a file name"))?;
                let out = std::process::Command::new("/usr/bin/plutil")
                    .args(["-convert", "json", "-o", "-", "--"])
                    .arg(&file)
                    .output()
                    .map_err(|e| internal(format!("plutil: {e}")))?;
                let colors: Map<String, Value> = serde_json::from_slice(&out.stdout)
                    .ok()
                    .and_then(|v: Value| v.as_object().cloned())
                    .filter(|o| out.status.success() && !o.is_empty())
                    .ok_or_else(|| invalid(format!("{}: not an .itermcolors file", file.display())))?;
                let colors: Map<String, Value> = colors.into_iter().filter(|(k, v)| k.ends_with("Color") && v.is_object()).collect();
                let parent = default_profile(live)?.name;
                let guid = make_profile(live, &name, &parent, &colors)?;
                set_default(live, &guid)?;
                shown.insert("profile".into(), json!(name));
            }
            Change::Pref(k, v) => set_prefs(live, vec![(k, v)])?,
            Change::Bind(a, mac) => {
                let x = menu::action(&a).ok_or_else(|| invalid(format!("no action '{a}'")))?;
                let title = title_of(menu::action_item(x).path).to_string();
                let mut cur = get_prefs(live, &["NSUserKeyEquivalents"])?.into_iter().next().and_then(|v| v.as_object().cloned()).unwrap_or_default();
                match mac {
                    Some(m) => cur.insert(title, json!(m)),
                    None => cur.remove(&title),
                };
                set_prefs(live, vec![("NSUserKeyEquivalents".into(), Value::Object(cur))])?;
            }
            Change::Props(pname, vals) => {
                let guid = cache[&pname.clone().unwrap_or_default()].guid.clone();
                set_profile_props(live, &guid, &vals)?;
            }
        }
    }
    shown.insert("_live".into(), json!(live_now));
    line(Value::Object(shown))
}

fn reset(live: &mut Live, keys: Vec<String>) -> Result<Done, Fail> {
    if keys.is_empty() {
        return Err(invalid("iTerm2 has no reset-everything; name the keys (pref.KEY, keys.ACTION)"));
    }
    let mut prefs = Vec::new();
    let mut binds = Vec::new();
    for k in &keys {
        match parse_key(k) {
            Key::Pref(n) => prefs.push((n.to_string(), Value::Null)),
            Key::Bind(a) => binds.push(a.to_string()),
            _ => return Err(invalid(format!("{k}: iTerm2 keeps no default to reset a profile key to; set it (profile set --set {k}=VALUE)"))),
        }
    }
    let n = prefs.len() + binds.len();
    if !prefs.is_empty() {
        set_prefs(live, prefs)?;
    }
    let mut live_now = true;
    if !binds.is_empty() {
        live_now = false;
        let mut cur = get_prefs(live, &["NSUserKeyEquivalents"])?.into_iter().next().and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for a in binds {
            if let Some(x) = menu::action(&a) {
                cur.remove(title_of(menu::action_item(x).path));
            }
        }
        set_prefs(live, vec![("NSUserKeyEquivalents".into(), Value::Object(cur))])?;
    }
    line(json!({"reset": n, "_live": live_now}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_fonts_and_values_translate() {
        let c = hex_color("#2c2c2c").unwrap();
        assert_eq!(color_hex(&c).unwrap(), "#2c2c2c");
        assert_eq!(show(&c), json!("#2c2c2c"));
        assert!(hex_color("2c2c2c").is_none() && hex_color("#2c2c").is_none());
        assert_eq!(color_name("Ansi 12 Color"), "ansi.12");
        assert_eq!(color_name("Cursor Text Color"), "cursor_text");
        assert_eq!(color_name("Background Color"), "background");
        assert_eq!(split_font("MesloLGS NF 14"), ("MesloLGS NF", "14"));
        assert_eq!(split_font("Menlo-Regular"), ("Menlo-Regular", ""));
        assert_eq!(parse_as("x", "on", Some(&json!(false)), None).unwrap(), json!(true));
        assert_eq!(parse_as("x", "12", Some(&json!(3)), None).unwrap(), json!(12));
        assert!(parse_as("x", "1.5", Some(&json!(3)), None).is_err());
        assert_eq!(parse_as("x", "#ffffff", Some(&c), None).unwrap()["Red Component"], json!(1.0));
        assert_eq!(parse_as("x", "hi", Some(&json!("a")), None).unwrap(), json!("hi"));
        assert_eq!(parse_as("x", "0.5", None, Some("float")).unwrap(), json!(0.5));
        assert_eq!(parse_as("x", "[1]", None, None).unwrap(), json!([1]));
        assert_eq!(parse_as("x", "plain", None, None).unwrap(), json!("plain"));
    }

    #[test]
    fn keys_resolve_with_aliases_and_near_misses() {
        let mut known = Map::new();
        known.insert("Normal Font".into(), json!("Menlo 12"));
        known.insert("Background Color".into(), hex_color("#000000").unwrap());
        assert_eq!(resolve_key("font", &known).unwrap(), "Normal Font");
        assert_eq!(resolve_key("normal font", &known).unwrap(), "Normal Font");
        assert_eq!(resolve_key("Background Color", &known).unwrap(), "Background Color");
        let e = resolve_key("Backgrund Color", &known).unwrap_err();
        assert!(e.contains("closest: Background Color"), "{e}");
        assert!(resolve_key("zzzzzzzzzz", &known).unwrap_err().starts_with("no setting"));
        assert!(pref_keys().count() > 80);
        assert!(pref_keys().any(|(k, t, _)| k == "PromptOnQuit" && t == "bool"));
        assert!(matches!(parse_key("Normal Font@Work"), Key::Prop(Some("Normal Font"), Some("Work"))));
        assert!(matches!(parse_key("@Work"), Key::Prop(None, Some("Work"))));
        assert!(matches!(parse_key("pref.PromptOnQuit"), Key::Pref("PromptOnQuit")));
        assert!(matches!(parse_key("keys.zoom"), Key::Bind("zoom")));
        assert!(matches!(parse_key("preset:Solarized Dark"), Key::Preset("Solarized Dark")));
    }

    #[test]
    fn shortcuts_round_trip_through_the_mac_form() {
        assert_eq!(combo_to_mac("cmd+shift+d").unwrap(), "$@d");
        assert_eq!(mac_to_combo("$@d"), "shift+cmd+d");
        assert_eq!(mac_to_combo(&combo_to_mac("ctrl+opt+up").unwrap()), "ctrl+opt+up");
        assert_eq!(mac_to_combo(&combo_to_mac("cmd+f5").unwrap()), "cmd+f5");
        assert_eq!(mac_to_combo("@@"), "cmd+@");
        assert!(combo_to_mac("hyper+x").is_none());
        let v = dynamic_profile("Throwaway", "iterm-cli-1", "Default", &Map::new());
        assert_eq!(v["Profiles"][0]["Dynamic Profile Parent Name"], "Default");
    }
}
