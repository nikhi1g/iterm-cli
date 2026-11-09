//! `setting`, `profile` and `key` over the app's settings (one `SettingOp` per command; the app owns the schema
//! and validation). Profiles: `profile list`; `profile get|set [NAME]` act on the default profile, or on NAME
//! (keys sent as `KEY@NAME`); `profile new NAME` makes one (a copy of the default), and `setting set --set
//! profile=NAME` makes it the default; app preferences are `pref.KEY`, key bindings `keys.ACTION`.

use crate::app::wire::SettingOp;
use serde_json::{json, Map, Value};

use crate::exec::Exec;
use crate::grammar::{Noun, Request, Verb};
use crate::out::{ok, planned, Error, Obj, Result};


fn parse_obj(line: Option<&String>) -> Result<Map<String, Value>> {
    match line.map(|l| serde_json::from_str::<Value>(l)) {
        Some(Ok(Value::Object(o))) => Ok(o),
        _ => Err(Error::failed("the app's settings reply is not an object")),
    }
}

impl Exec<'_> {
    fn schema(&mut self) -> Result<Vec<Map<String, Value>>> {
        let d = self.conn.setting(SettingOp::Keys)?;
        Ok(d.lines.iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| v.as_object().cloned()).collect())
    }

    fn values(&mut self, keys: Vec<String>) -> Result<Map<String, Value>> {
        let d = self.conn.setting(SettingOp::Get { keys })?;
        parse_obj(d.lines.first())
    }

    fn set_rows(&mut self, r: &Request, pairs: Vec<(String, String)>) -> Result<Vec<Obj>> {
        if pairs.is_empty() {
            return Err(Error::invalid(format!("{} set needs --set key=value… (iterm setting keys)", r.noun)));
        }
        if !r.apply {
            return Ok(pairs.iter().map(|(k, v)| planned("set", crate::obj! {"key" => k, "value" => v})).collect());
        }
        let mut done = parse_obj(self.conn.setting(SettingOp::Set { pairs })?.lines.first())?;
        let live = done.remove("_live").and_then(|v| v.as_bool()).unwrap_or(false);
        Ok(done
            .into_iter()
            .map(|(k, v)| {
                let mut o = crate::obj! {"key" => k, "value" => v};
                if !live {
                    o.insert("reason".into(), json!("saved; applies at the next launch"));
                }
                ok("set", o)
            })
            .collect())
    }

    fn reset_rows(&mut self, r: &Request, keys: Vec<String>) -> Result<Vec<Obj>> {
        if keys.is_empty() && !r.all {
            return Err(Error::invalid(format!("{} reset KEY… | --all", r.noun)));
        }
        if !r.apply {
            return Ok(vec![planned("reset", crate::obj! {"keys" => if keys.is_empty() { vec!["all".to_string()] } else { keys }})]);
        }
        let d = parse_obj(self.conn.setting(SettingOp::Reset { keys })?.lines.first())?;
        Ok(vec![ok("reset", crate::obj! {"count" => d.get("reset").cloned().unwrap_or(json!(0))})])
    }

    /// The profile a `profile get|set [NAME]` acts on: None for the one in use (the default), else NAME.
    fn named_profile(&mut self, r: &Request) -> Result<Option<String>> {
        let v = self.values(vec!["profile".into()])?;
        let active = v.get("profile").and_then(Value::as_str).unwrap_or("Default").to_owned();
        Ok(r.words.first().filter(|w| **w != active).cloned())
    }

    pub fn settings(&mut self, r: &Request) -> Result<Vec<Obj>> {
        let grep = r.p.one("--grep").map(str::to_ascii_lowercase);
        let hit = |k: &str| grep.as_deref().is_none_or(|g| k.to_ascii_lowercase().contains(g));
        match (r.noun, r.verb) {
            (Noun::Setting | Noun::Profile, Verb::Keys) => {
                Ok(self.schema()?.into_iter().filter(|o| hit(o.get("key").and_then(Value::as_str).unwrap_or(""))).map(|o| ok("key", o)).collect())
            }
            (Noun::Profile, Verb::List) => {
                let v = self.values(vec!["profile".into(), "profiles".into()])?;
                let active = v.get("profile").and_then(Value::as_str).unwrap_or("Default");
                let names = v.get("profiles").and_then(Value::as_array).cloned().unwrap_or_default();
                Ok(names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|n| ok("profile", crate::obj! {"name" => n, "active" => n == active}))
                    .collect())
            }
            (Noun::Profile, Verb::Get) | (Noun::Setting, Verb::List | Verb::Get) => {
                // another profile's keys are `KEY@NAME` (`@NAME`: all of them)
                let keys = match r.noun {
                    Noun::Profile => self.named_profile(r)?.map(|n| vec![format!("@{n}")]).unwrap_or_default(),
                    _ => r.words.clone(),
                };
                Ok(self.values(keys)?.into_iter().filter(|(k, _)| !k.starts_with("keys.") && hit(k)).map(|(k, v)| ok("setting", crate::obj! {"key" => k, "value" => v})).collect())
            }
            (Noun::Profile, Verb::Set) => {
                let pairs = match self.named_profile(r)? {
                    Some(n) => r.payload.iter().map(|(k, v)| (format!("{k}@{n}"), v.clone())).collect(),
                    None => r.payload.clone(),
                };
                self.set_rows(r, pairs)
            }
            (Noun::Setting, Verb::Set) => self.set_rows(r, r.payload.clone()),
            // a theme file becomes a new profile named after it (the app reads the file), switched to
            (Noun::Profile, Verb::Import) => {
                let file = r.words.first().ok_or_else(|| Error::invalid("profile import FILE --apply"))?;
                let abs = std::fs::canonicalize(file).map_err(|e| Error::invalid(format!("{file}: {e}")))?;
                self.set_rows(r, vec![("profile_import".into(), abs.to_string_lossy().into_owned())])
            }
            // a new profile starts as a copy of the one in use; `setting set --set profile=NAME` switches to it
            (Noun::Profile, Verb::New | Verb::Copy) => {
                let name = r.words.first().ok_or_else(|| Error::invalid(format!("profile {} NAME", r.verb)))?;
                self.set_rows(r, vec![("profile_new".into(), name.clone())])
            }
            (Noun::Profile, Verb::Delete) => {
                let name = r.words.first().ok_or_else(|| Error::invalid("profile delete NAME"))?;
                self.set_rows(r, vec![("profile_delete".into(), name.clone())])
            }
            (Noun::Setting, Verb::Reset) => self.reset_rows(r, r.words.clone()),
            (Noun::Key, Verb::Keys) => Ok(self
                .schema()?
                .into_iter()
                .filter_map(|o| o.get("key").and_then(Value::as_str).and_then(|k| k.strip_prefix("keys.")).map(String::from))
                .filter(|a| hit(a))
                .map(|a| ok("action", crate::obj! {"action" => a}))
                .collect()),
            (Noun::Key, Verb::List | Verb::Get) => {
                let want: Vec<String> = r.words.iter().map(|a| format!("keys.{a}")).collect();
                let all = want.is_empty();
                let vals = self.values(if all { vec![] } else { want })?;
                Ok(vals
                    .into_iter()
                    .filter_map(|(k, v)| k.strip_prefix("keys.").map(|a| (a.to_string(), v)))
                    .filter(|(a, _)| hit(a))
                    .map(|(a, v)| ok("key", crate::obj! {"action" => a, "combo" => v.as_str().unwrap_or("default")}))
                    .collect())
            }
            (Noun::Key, Verb::Set) => {
                let action = r.words.first().ok_or_else(|| Error::invalid("key set ACTION --key COMBO"))?;
                let combo = r
                    .p
                    .one("--key")
                    .map(str::to_string)
                    .or_else(|| r.words.get(1).cloned())
                    .or_else(|| r.payload.iter().find(|(k, _)| k == "combo" || k == "key").map(|(_, v)| v.clone()))
                    .ok_or_else(|| Error::invalid("key set ACTION --key COMBO (opt+t, cmd+shift+d)"))?;
                self.set_rows(r, vec![(format!("keys.{action}"), combo)])
            }
            (Noun::Key, Verb::Reset | Verb::Delete) => {
                // --all is every key override, never the other settings
                let keys: Vec<String> = if r.words.is_empty() && r.all {
                    self.schema()?.into_iter().filter_map(|o| o.get("key").and_then(Value::as_str).filter(|k| k.starts_with("keys.")).map(String::from)).collect()
                } else {
                    r.words.iter().map(|a| format!("keys.{a}")).collect()
                };
                self.reset_rows(r, keys)
            }
            (n, v) => Err(Error::invalid(format!("{n} {v} is n/a"))),
        }
    }
}
