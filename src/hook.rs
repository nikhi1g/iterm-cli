//! `iterm hook pretooluse`: a Claude Code PreToolUse hook that keeps agents on this CLI. A Bash call that runs
//! iTerm2's bundled `it2` is blocked (exit 2) with the matching `iterm` command; anything else passes untouched.
//! `ITERM_ALLOW_IT2=1` in the command lets a deliberate it2 call through. Any parse failure lets the call through:
//! a hook must never wedge the agent. Ported from v2 (`iterm-v2-final:src/hook.rs`, `src/shell.rs`).

use std::io::Read;
use std::path::Path;

use serde_json::Value;

const PUNCT: &str = "();<>|&";
const WHITESPACE: &str = " \t\r\n";
const QUOTES: &str = "'\"";

/// Shell words with operators split off (`claude;`, `(claude)`, `claude&&x`), or None when the
/// command does not parse (an unclosed quote or a trailing escape).
pub fn shell_words(command: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();
    let mut pos = 0usize;
    let mut pushback: Vec<char> = Vec::new();
    let mut out = Vec::new();
    loop {
        // one get_token()
        let mut token = String::new();
        let mut quoted = false;
        let mut state: char = ' '; // ' ' whitespace, 'a' word, 'c' punctuation, a quote, '\\' escape, '\0' eof
        let mut escaped_state = 'a';
        let mut eof = false;
        loop {
            let next: Option<char> = if let Some(c) = pushback.pop() {
                Some(c)
            } else if pos < chars.len() {
                pos += 1;
                Some(chars[pos - 1])
            } else {
                None
            };
            match state {
                ' ' => match next {
                    None => {
                        eof = true;
                        break;
                    }
                    Some(c) if WHITESPACE.contains(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('#') => {
                        while pos < chars.len() && chars[pos] != '\n' {
                            pos += 1;
                        }
                        if pos < chars.len() {
                            pos += 1;
                        }
                    }
                    Some('\\') => {
                        escaped_state = 'a';
                        state = '\\';
                    }
                    Some(c) if PUNCT.contains(c) => {
                        token.push(c);
                        state = 'c';
                    }
                    Some(c) if QUOTES.contains(c) => state = c,
                    Some(c) => {
                        token.push(c);
                        state = 'a';
                    }
                },
                'a' | 'c' => match next {
                    None => {
                        eof = true;
                        break;
                    }
                    Some(c) if WHITESPACE.contains(c) => {
                        state = ' ';
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('#') => {
                        while pos < chars.len() && chars[pos] != '\n' {
                            pos += 1;
                        }
                        if pos < chars.len() {
                            pos += 1;
                        }
                        state = ' ';
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some(c) if state == 'c' => {
                        if PUNCT.contains(c) {
                            token.push(c);
                        } else {
                            if !WHITESPACE.contains(c) {
                                pushback.push(c);
                            }
                            break;
                        }
                    }
                    Some(c) if QUOTES.contains(c) => state = c,
                    Some('\\') => {
                        escaped_state = 'a';
                        state = '\\';
                    }
                    Some(c) if !PUNCT.contains(c) => token.push(c),
                    Some(c) => {
                        pushback.push(c);
                        state = ' ';
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                },
                '\\' => {
                    let c = next?; // no escaped character
                    if QUOTES.contains(escaped_state) && c != state && c != escaped_state {
                        token.push(state);
                    }
                    token.push(c);
                    state = escaped_state;
                }
                q => match next {
                    // inside a quote
                    None => return None, // No closing quotation
                    Some(c) if c == q => {
                        quoted = true;
                        state = 'a';
                    }
                    Some('\\') if q == '"' => {
                        quoted = true;
                        escaped_state = q;
                        state = '\\';
                    }
                    Some(c) => {
                        quoted = true;
                        token.push(c);
                    }
                },
            }
        }
        if token.is_empty() && !quoted {
            if eof {
                return Some(out);
            }
            continue;
        }
        out.push(token);
        if eof {
            return Some(out);
        }
    }
}


pub const AGENT_COMMANDS: [&str; 3] = ["claude", "claude-code", "omp"];

/// A word's program name: its basename without a package version (`@anthropic-ai/claude-code@2.1.0`).
pub fn program_name(word: &str) -> String {
    let trimmed = word.trim_end_matches('/');
    let name = Path::new(trimmed)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // re.sub(r'(?<=.)@[^@/]*$', '', name): drop a trailing @version that is not the whole name
    if let Some(at) = name.rfind('@') {
        if at > 0 && !name[at + 1..].contains('/') {
            return name[..at].to_string();
        }
    }
    name
}


const OPERATORS: [&str; 8] = [";", "&&", "||", "|", "&", "(", ")", "|&"];
const PREFIXES: [&str; 6] = ["env", "command", "exec", "builtin", "nohup", "time"];

/// Whether a shell command runs `it2` as a program (not merely mentions it).
pub fn runs_it2(command: &str) -> bool {
    let Some(words) = shell_words(command) else {
        return command
            .split_whitespace()
            .next()
            .map(|w| program_name(w) == "it2")
            .unwrap_or(false);
    };
    let mut at_command = true;
    for word in &words {
        if OPERATORS.contains(&word.as_str()) {
            at_command = true;
            continue;
        }
        if !at_command {
            continue;
        }
        let is_assignment = word
            .split_once('=')
            .map(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(false);
        if is_assignment || PREFIXES.contains(&word.as_str()) {
            continue; // still at the command position
        }
        if program_name(word) == "it2" {
            return true;
        }
        at_command = false;
    }
    false
}

/// The `iterm` equivalent of an it2 command, for the refusal message.
pub fn suggestion(command: &str) -> &'static str {
    let words = shell_words(command).unwrap_or_default();
    let i = words.iter().position(|w| program_name(w) == "it2").map_or(words.len(), |i| i + 1);
    let rest: Vec<&str> = words[i..].iter().map(String::as_str).collect();
    match rest.as_slice() {
        ["ls", ..] | ["session", "list", ..] => "iterm tree",
        ["split", ..] | ["session", "split", ..] => "iterm pane new --at P --side right|down [--cmd C|--agent]",
        ["send", ..] | ["session", "send", ..] => "iterm pane send P --text T",
        ["run", ..] | ["session", "run", ..] => "iterm pane send P --text T --enter",
        ["newtab", ..] | ["tab", "new", ..] => "iterm tab new [--cwd DIR] [--agent]",
        ["new", ..] | ["window", "new", ..] => "iterm window new [--cwd DIR]",
        ["session", "read", ..] | ["session", "capture", ..] => "iterm pane read P --tail N",
        ["session", "close", ..] => "iterm pane close P",
        ["session", "focus", ..] => "iterm pane select P",
        ["session", "set-name", ..] => "iterm pane set P --name N",
        ["session", "get-var", ..] | ["session", "set-var", ..] => "iterm pane get P --json / iterm pane set P --set k=v",
        ["tab", ..] => "iterm tab list|new|set|move|close",
        ["monitor", ..] => "iterm pane wait P --until idle|exit|screen~RE",
        _ => "iterm guide",
    }
}

/// The hook: read Claude Code's JSON from stdin; exit 2 with a reason on stderr to block.
pub fn pretooluse() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let Ok(event) = serde_json::from_str::<Value>(&input) else {
        return 0;
    };
    if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return 0;
    }
    let Some(command) = event
        .get("tool_input")
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
    else {
        return 0;
    };
    if command.contains("ITERM_ALLOW_IT2=1") || !runs_it2(command) {
        return 0;
    }
    eprintln!(
        "Blocked: this setup drives iTerm2 with `iterm`, not it2. Use: {}. `iterm help` lists every command; \
         `iterm guide` is the agent cheat sheet. If the user explicitly asked for it2, rerun with ITERM_ALLOW_IT2=1 in the command.",
        suggestion(command)
    );
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it2_is_seen_at_command_positions_only() {
        for c in [
            "it2 ls",
            "/Applications/iTerm.app/Contents/Resources/utilities/it2 split",
            "cd x && it2 session send hi",
            "FOO=1 it2 newtab",
            "env it2 ls",
            "(it2 tab new)",
            "echo a; it2 send x",
        ] {
            assert!(runs_it2(c), "{c}");
        }
        for c in [
            "echo it2",
            "grep it2 file",
            "iterm panes",
            "cat it2.txt",
            "git log --grep=it2",
        ] {
            assert!(!runs_it2(c), "{c}");
        }
        assert_eq!(
            suggestion("it2 session split --vertical"),
            "iterm pane new --at P --side right|down [--cmd C|--agent]"
        );
        assert_eq!(suggestion("it2 ls"), "iterm tree");
    }
}
