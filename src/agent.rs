//! What an agent pane runs, and how its screen reads (ported from iterm_api's `launch.rs` and
//! `model.rs`): model and effort resolution, the `claude` command line with its task file, task
//! files split into items, and the observed state of a pane from its bottom lines.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::out::{Error, Result};

/// The default agent: `pane new --agent` without --cmd or --model.
pub const AGENT_COMMAND: &str = "claude";

/// Model aliases and the IDs they mean.
pub const MODELS: [(&str, &str); 4] =
    [("opus", "claude-opus-5-5"), ("sonnet", "claude-sonnet-5-5"), ("haiku", "claude-haiku-5-5"), ("fable", "claude-fable-5-1")];
/// Claude Code's --effort levels.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The one line every agent the CLI launches gets in its system prompt (Claude, OMP: --append-system-prompt;
/// Codex: developer_instructions).
pub const AGENT_LINE: &str = "You run in a iTerm2 pane. Panes, tabs, windows, sub-agents and messages: use the iterm CLI \
     (run `iterm guide` first: what iTerm2 is, the commands, and when to delegate), never it2 or AppleScript.";

/// The accents agent panes get as they spawn, by name (`pane set --color` takes these or #rrggbb).
pub const COLORS: [(&str, &str); 8] = [
    ("red", "#e5534b"),
    ("orange", "#e8913a"),
    ("yellow", "#d9b72b"),
    ("green", "#4caf50"),
    ("teal", "#26a69a"),
    ("blue", "#4a90e2"),
    ("purple", "#9b59b6"),
    ("pink", "#e05297"),
];

/// A color name from [`COLORS`] or `#rrggbb` as `#rrggbb`.
pub fn color_hex(c: &str) -> Result<String> {
    let c = c.trim().to_ascii_lowercase();
    if let Some((_, h)) = COLORS.iter().find(|(n, _)| *n == c) {
        return Ok(h.to_string());
    }
    let h = c.trim_start_matches('#');
    if h.len() == 6 && h.chars().all(|x| x.is_ascii_hexdigit()) {
        return Ok(format!("#{h}"));
    }
    Err(Error::invalid(format!("color '{c}': {} | #rrggbb | none", COLORS.map(|(n, _)| n).join("|"))))
}

/// Claude Code's own session colors (`/color NAME`: its prompt bar), with the CLI's hex for each (teal is cyan).
pub const CLAUDE_COLORS: [(&str, &str); 8] = [
    ("red", "#e5534b"),
    ("orange", "#e8913a"),
    ("yellow", "#d9b72b"),
    ("green", "#4caf50"),
    ("cyan", "#26a69a"),
    ("blue", "#4a90e2"),
    ("purple", "#9b59b6"),
    ("pink", "#e05297"),
];

/// The `/color` line for a Claude session and the hex it stands for: the nearest of Claude's eight to `hex`
/// (`#rrggbb`), or `default` for none.
pub fn claude_color_line(hex: Option<&str>) -> (String, Option<&'static str>) {
    let rgb = |h: &str| -> Option<(i32, i32, i32)> {
        let h = h.trim_start_matches('#');
        let p = |i: usize| i32::from_str_radix(h.get(i..i + 2)?, 16).ok();
        Some((p(0)?, p(2)?, p(4)?))
    };
    let Some((r, g, b)) = hex.and_then(rgb) else { return ("/color default".into(), None) };
    let (name, h) = CLAUDE_COLORS
        .iter()
        .min_by_key(|(_, h)| rgb(h).map_or(i32::MAX, |(r2, g2, b2)| (r - r2).pow(2) + (g - g2).pow(2) + (b - b2).pow(2)))
        .copied()
        .unwrap_or(CLAUDE_COLORS[0]);
    (format!("/color {name}"), Some(h))
}

/// The spawn accent for the `n`th agent (by creation index), so siblings differ.
pub fn color_for(n: usize) -> &'static str {
    COLORS[n % COLORS.len()].1
}

/// The colors agents get as they spawn: 16 hues apart enough to tell a tab of them apart (the named eight first).
pub const AUTO: [&str; 16] = [
    "#e5534b", "#e8913a", "#d9b72b", "#4caf50", "#26a69a", "#4a90e2", "#9b59b6", "#e05297",
    "#a0522d", "#8bc34a", "#00bcd4", "#3f51b5", "#c0ca33", "#ff7043", "#7e57c2", "#78909c",
];

/// An agent's color: the one seed `n` picks, else the next hue no live pane has (`taken`), so siblings differ;
/// the seed's own when every hue is taken.
pub fn auto_color(n: usize, taken: &[String]) -> &'static str {
    (0..AUTO.len()).map(|k| AUTO[(n + k) % AUTO.len()]).find(|c| !taken.iter().any(|t| t.eq_ignore_ascii_case(c))).unwrap_or(AUTO[n % AUTO.len()])
}

/// What renames an agent's own session from inside it, typed as a message: `/rename NAME` (Claude Code, Codex
/// and OMP all have it). None for anything else: only the pane is renamed.
pub fn rename_line(provider: &str, name: &str) -> Option<String> {
    matches!(provider, "claude" | "codex" | "omp").then(|| format!("/rename {name}"))
}

/// An alias (`opus`), an alias with a version (`opus-5.5`, `sonnet5.5`: `claude-<family>-5-5`), or a
/// full ID (`claude-…`, as given).
pub fn resolve_model(raw: &str) -> Result<String> {
    let s = raw.trim().to_ascii_lowercase();
    let bad = || Error::invalid(format!("model '{raw}': use opus|sonnet|haiku|fable, a version (opus-5.5) or a full ID (claude-…)"));
    let rest = s.strip_prefix("claude-").unwrap_or(&s);
    for (family, id) in MODELS {
        let Some(v) = rest.strip_prefix(family) else { continue };
        let v = v.trim_start_matches(['-', '_', ' ']);
        if v.is_empty() {
            return Ok(id.to_string());
        }
        let parts: Vec<&str> = v.split(['.', '-']).collect();
        if parts.len() <= 2 && parts.iter().all(|p| !p.is_empty() && p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit())) {
            return Ok(format!("claude-{family}-{}", parts.join("-")));
        }
        break;
    }
    let id_chars = s.chars().all(|c| c.is_ascii_alphanumeric() || "-._[]".contains(c));
    if s.starts_with("claude-") && s.len() > 7 && id_chars {
        return Ok(s);
    }
    Err(bad())
}

pub fn check_effort(effort: &str) -> Result<String> {
    let e = effort.trim().to_ascii_lowercase();
    if EFFORTS.contains(&e.as_str()) {
        Ok(e)
    } else {
        Err(Error::invalid(format!("effort '{effort}': use {}", EFFORTS.join("|"))))
    }
}

/// A shell word, quoted only when it needs it.
pub fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// A plain `<prog> …` command: its program is `prog` and it has no operators or `--`.
fn plain(cmd: &str, prog: &str) -> bool {
    let mut words = cmd.split_whitespace();
    words.next().map(|w| w.rsplit('/').next() == Some(prog)).unwrap_or(false)
        && !cmd.contains(" -- ")
        && !cmd.chars().any(|c| "();<>|&`$".contains(c))
}

fn plain_claude(cmd: &str) -> bool {
    plain(cmd, "claude")
}

pub fn plain_codex(cmd: &str) -> bool {
    plain(cmd, "codex")
}

/// The command an agent pane runs: a plain claude command gets the model, effort, the agent line,
/// its name and the task as its first prompt, read from a one-shot file the starting shell deletes,
/// so the task never passes through a terminal (`command rm`: the login shell may alias rm, e.g. to trash).
/// Anything else runs as given.
pub fn agent_command(cmd: &str, model: Option<&str>, effort: Option<&str>, name: Option<&str>, task_file: Option<&Path>) -> String {
    // Codex and OMP (plain, with or without flags of their own): the model, the effort, the agent line, and the
    // task as the first prompt
    let prog = cmd.split_whitespace().next().map(|w| w.rsplit('/').next().unwrap_or(w));
    if let Some(prog) = prog.filter(|p| matches!(*p, "codex" | "omp") && plain(cmd, p)) {
        let prog = Some(prog);
        let has = |flag: &str| cmd.split_whitespace().any(|w| w == flag || w.starts_with(&format!("{flag}=")));
        let mut out = cmd.to_string();
        if let Some(m) = model.filter(|_| !has("--model") && !has("-m")) {
            out.push_str(&format!(" --model {}", quote(m)));
        }
        if prog == Some("codex") {
            // Codex's levels: minimal|low|medium|high|xhigh (max → xhigh)
            if let Some(e) = effort {
                out.push_str(&format!(" -c model_reasoning_effort={}", if e == "max" { "xhigh" } else { e }));
            }
            out.push_str(&format!(" -c {}", quote(&format!("developer_instructions={}", toml_str(AGENT_LINE)))));
        } else {
            if let Some(e) = effort {
                out.push_str(&format!(" --thinking={e}"));
            }
            out.push_str(&format!(" --append-system-prompt {}", quote(AGENT_LINE)));
        }
        if let Some(f) = task_file {
            let f = quote(&f.to_string_lossy());
            out.push_str(&format!(" \"$(cat {f} && command rm -f {f})\""));
        }
        return out;
    }
    if !plain_claude(cmd) {
        return cmd.to_string();
    }
    let mut out = cmd.to_string();
    for (flag, v) in [("--model", model), ("--effort", effort)] {
        if let Some(v) = v {
            if !cmd.split_whitespace().any(|w| w == flag || w.starts_with(&format!("{flag}="))) {
                out.push_str(&format!(" {flag} {}", quote(v)));
            }
        }
    }
    if !cmd.contains("--append-system-prompt") {
        out.push_str(&format!(" --append-system-prompt {}", quote(AGENT_LINE)));
    }
    if let Some(n) = name {
        out.push_str(&format!(" --name {}", quote(n)));
    }
    if let Some(f) = task_file {
        let f = quote(&f.to_string_lossy());
        out.push_str(&format!(" -- \"$(cat {f} && command rm -f {f})\""));
    }
    out
}

/// A TOML basic string (Codex's `-c key=value` parses the value as TOML).
fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// One task file as items: `## ` sections, else top-level list items, else the whole text.
pub fn split_tasks(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let cut = |starts: &[usize], strip: &dyn Fn(&str) -> String| -> Vec<String> {
        starts
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let end = starts.get(k + 1).copied().unwrap_or(lines.len());
                let mut part = vec![strip(lines[*s])];
                part.extend(lines[s + 1..end].iter().map(|l| l.to_string()));
                part.join("\n").trim().to_string()
            })
            .filter(|t| !t.is_empty())
            .collect()
    };
    // what comes before the first item (a title, shared context) belongs to every item
    let with_preamble = |first: usize, items: Vec<String>| -> Vec<String> {
        let pre = lines[..first].join("\n").trim().to_string();
        if pre.is_empty() {
            return items;
        }
        items.into_iter().map(|t| format!("{pre}\n\n{t}")).collect()
    };
    let heads: Vec<usize> = (0..lines.len()).filter(|i| lines[*i].starts_with("## ")).collect();
    if heads.len() >= 2 {
        return with_preamble(heads[0], cut(&heads, &|l: &str| l.trim_start_matches('#').trim().to_string()));
    }
    let item = |l: &str| {
        l.starts_with("- ")
            || l.starts_with("* ")
            || l.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    let items: Vec<usize> = (0..lines.len()).filter(|i| item(lines[*i])).collect();
    if items.len() >= 2 {
        return with_preamble(
            items[0],
            cut(&items, &|l: &str| {
                let t = l.trim_start_matches("- ").trim_start_matches("* ");
                match t.split_once(". ") {
                    Some((n, rest)) if n.chars().all(|c| c.is_ascii_digit()) => rest.to_string(),
                    _ => t.to_string(),
                }
            }),
        );
    }
    let t = text.trim();
    if t.is_empty() { vec![] } else { vec![t.to_string()] }
}

// ---- observed state --------------------------------------------------------------------------

pub const READY: &str = "READY";
pub const BUSY: &str = "BUSY";
pub const MENU: &str = "MENU";
pub const TRUST: &str = "TRUST";
pub const EXITED: &str = "EXITED";

/// Claude's input box: a `❯` line directly under a horizontal rule, or its shortcuts hint.
const CLAUDE_READY: &str = r"(?m)^─{10,}.*\n❯|\? for shortcuts";
const CLAUDE_TRUST: &str = r"Accessing\s*workspace:|Yes,\s*I\s*trust\s*this\s*folder";
/// Permission and selection menus, and generic approval prompts: over-matching only withholds
/// keystrokes, the safe direction.
const CLAUDE_MENU: &str = concat!(
    r"(?m)^\s*❯\s*\d+\.|Do\s*you\s*want\s*to\s*(proceed|make\s*this\s*edit|create)|",
    r"Esc\s*to\s*cancel|\[[yY]/[nN]\]|\([yY]/[nN]\)|^\W*(Allow|Approve)\b[^\n]*\?\s*$"
);
/// Non-empty screen lines that decide the state.
const BOTTOM: usize = 15;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("built-in pattern"))
}

/// The state of a live pane from its screen lines. Trust screens and menus first: their `❯` lines
/// must never count as ready. A Claude pane without its input box is working; a shell is ready.
pub fn classify(lines: &[String], claude: bool) -> &'static str {
    static R: OnceLock<Regex> = OnceLock::new();
    static T: OnceLock<Regex> = OnceLock::new();
    static M: OnceLock<Regex> = OnceLock::new();
    let kept: Vec<&str> = lines.iter().map(String::as_str).filter(|l| !l.trim().is_empty()).collect();
    let text = kept[kept.len().saturating_sub(BOTTOM)..].join("\n");
    if re(&T, CLAUDE_TRUST).is_match(&text) {
        return TRUST;
    }
    if re(&M, CLAUDE_MENU).is_match(&text) {
        return MENU;
    }
    if !claude || re(&R, CLAUDE_READY).is_match(&text) { READY } else { BUSY }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_commands_tasks_states() {
        assert_eq!(resolve_model("opus").unwrap(), "claude-opus-5-5");
        assert_eq!(resolve_model("sonnet-4.5").unwrap(), "claude-sonnet-4-5");
        assert!(resolve_model("gpt").is_err());
        assert!(check_effort("xhigh").is_ok() && check_effort("ultra").is_err());
        let c = agent_command("claude", Some("claude-opus-5-5"), Some("high"), Some("w1"), Some(Path::new("/s/t 1")));
        assert!(c.starts_with("claude --model claude-opus-5-5 --effort high --append-system-prompt 'You run in a iTerm2 pane"), "{c}");
        let x = agent_command("codex", Some("gpt-5.5"), Some("max"), Some("w2"), None);
        assert!(x.starts_with("codex --model gpt-5.5 -c model_reasoning_effort=xhigh -c 'developer_instructions=\"You run"), "{x}");
        // a plain codex/omp with flags of its own still gets the rest (regression, live run 2026-10-29)
        let x = agent_command("codex -c a=1", Some("gpt-6-luna"), None, None, Some(Path::new("/s/t")));
        assert!(x.starts_with("codex -c a=1 --model gpt-6-luna -c 'developer_instructions=") && x.ends_with("command rm -f /s/t)\""), "{x}");
        assert_eq!(agent_command("codex --model m2", Some("m1"), None, None, None).matches("--model").count(), 1);
        assert_eq!(agent_command("codex | tee x", Some("m"), None, None, None), "codex | tee x");
        let o = agent_command("omp", None, Some("high"), None, None);
        assert!(o.starts_with("omp --thinking=high --append-system-prompt 'You run"), "{o}");
        assert_eq!(rename_line("omp", "w3").as_deref(), Some("/rename w3"));
        assert_eq!(rename_line("shell", "w3"), None);
        assert_eq!(color_hex("Blue").unwrap(), "#4a90e2");
        assert_eq!(color_hex("#A1B2C3").unwrap(), "#a1b2c3");
        assert!(color_hex("mauve").is_err());
        assert_ne!(color_for(1), color_for(2));
        assert!(c.ends_with(" --name w1 -- \"$(cat '/s/t 1' && command rm -f '/s/t 1')\""), "{c}");
        assert_eq!(agent_command("npm test | tee x", Some("m"), None, None, None), "npm test | tee x");
        assert_eq!(split_tasks("## a\nx\n## b\ny"), vec!["a\nx", "b\ny"]);
        assert_eq!(split_tasks("- one\n- two\n"), vec!["one", "two"]);
        assert_eq!(split_tasks("just this"), vec!["just this"]);
        let l = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
        assert_eq!(classify(&l("$ "), false), READY);
        assert_eq!(classify(&l("working…"), true), BUSY);
        assert_eq!(classify(&l("────────────────\n❯ "), true), READY);
        assert_eq!(classify(&l("Do you want to proceed?\n❯ 1. Yes"), true), MENU);
        assert_eq!(classify(&l("Yes, I trust this folder"), true), TRUST);
    }

    #[test]
    fn claude_colors_map_to_its_eight() {
        assert_eq!(claude_color_line(Some("#e5534b")), ("/color red".to_string(), Some("#e5534b")));
        assert_eq!(claude_color_line(Some("#26a69a")).0, "/color cyan");
        assert_eq!(claude_color_line(Some("#00bcd4")).0, "/color cyan");
        assert_eq!(claude_color_line(Some("#7e57c2")).0, "/color purple");
        assert_eq!(claude_color_line(None), ("/color default".to_string(), None));
    }
}
