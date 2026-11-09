//! Agents as the app sees them: the provider (from the pane's foreground process), what the agent says about itself
//! (Claude Code's title glyph), the screen's meaning (from its bottom lines) and the state machine they drive. Pure;
//! the app feeds it events (a title, a burst of output starting, output going quiet, the process exiting), never a
//! timer.

/// Who runs in the pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
    Omp,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Omp => "omp",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        match s {
            "claude" => Some(Provider::Claude),
            "codex" => Some(Provider::Codex),
            "omp" => Some(Provider::Omp),
            _ => None,
        }
    }
}

const NAMES: [(&[u8], Provider); 4] = [(b"claude", Provider::Claude), (b"claude-code", Provider::Claude), (b"codex", Provider::Codex), (b"omp", Provider::Omp)];
const LAUNCHERS: [&[u8]; 3] = [b"node", b"bun", b"deno"];

/// The last path component without its extension.
fn base(path: &[u8]) -> &[u8] {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    match name.iter().rposition(|&b| b == b'.') {
        Some(0) | None => name,
        Some(i) => &name[..i],
    }
}

fn named(n: &[u8]) -> Option<Provider> {
    NAMES.iter().find(|(k, _)| *k == n).map(|(_, p)| *p)
}

/// The provider of a process: its executable or argv[0] (Claude's binary is named after its version, its argv[0] is
/// `claude`), or for a launcher (node, bun, deno) the script it runs, the first argument that isn't a flag.
pub fn provider_of<'a>(exec: &[u8], mut argv: impl Iterator<Item = &'a [u8]>) -> Option<Provider> {
    let argv0 = argv.next().unwrap_or(b"");
    let names = [base(exec), base(argv0)];
    if let Some(p) = names.iter().find_map(|n| named(n)) {
        return Some(p);
    }
    if !names.iter().any(|n| LAUNCHERS.contains(n)) {
        return None;
    }
    let script = argv.find(|a| !a.starts_with(b"-"))?;
    script.split(|&b| b == b'/').map(base).find_map(named)
}

/// What an agent's title says, when it says anything: Claude Code prefixes its title with "◐ " / "◑ " (alternating
/// every 960 ms) while a turn runs and "✳ " otherwise (unless CLAUDE_CODE_DISABLE_TERMINAL_TITLE). `Some(true)`:
/// working. The agent's own word: cheaper and more accurate than its screen, and it arrives for panes out of sight.
pub fn title_working(provider: Provider, title: &str) -> Option<bool> {
    if provider != Provider::Claude {
        return None;
    }
    let mut c = title.chars();
    let glyph = c.next()?;
    if c.next() != Some(' ') {
        return None;
    }
    match glyph {
        '◐' | '◑' => Some(true),
        '✳' => Some(false),
        _ => None,
    }
}

/// What an agent's screen shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// A permission, selection or folder-trust prompt: only the human answers it.
    Prompt,
    /// Working ("esc to interrupt").
    Busy,
    /// Its input box, waiting for the next message.
    Ready,
    /// Its input box after a finished turn (Claude's "✻ Cooked for 2s"): a task done, even one that finished
    /// within the agent's startup output.
    Answered,
    Unknown,
}

const PROMPT: [&str; 12] = [
    "Do you want to proceed",
    "Do you want to make this edit",
    "Do you want to create",
    "Do you trust the files",
    "Yes, I trust this folder",
    "Accessing workspace:",
    "Allow command?",
    "Approve this",
    "Esc to cancel",
    "[y/N]",
    "(y/n)",
    "Press enter to confirm",
];
const BUSY: [&str; 3] = ["esc to interrupt", "Esc to interrupt", "ctrl+c to interrupt"];
const READY: [&str; 3] = ["? for shortcuts", "⏎ send", "Ctrl+J newline"];

/// How many non-blank bottom rows `classify` reads: enough for every provider's footer.
pub const BOTTOM: usize = 15;

/// The meaning of an agent's bottom lines (the last `BOTTOM` non-blank rows).
pub fn classify<'a>(lines: impl DoubleEndedIterator<Item = &'a str>) -> Screen {
    let bottom: Vec<&str> = lines.rev().filter(|l| !l.trim().is_empty()).take(BOTTOM).collect();
    let has = |set: &[&str]| bottom.iter().any(|l| set.iter().any(|p| l.contains(p)));
    // a numbered choice under the cursor (Claude's and Codex's menus)
    let menu = bottom.iter().any(|l| {
        let t = l.trim_start();
        (t.starts_with("❯ 1.") || t.starts_with("› 1.") || t.starts_with("> 1.")) && t.len() > 5
    });
    // Claude's spinner line, "✽ Mulling… (3m 9s · …)": work even where a narrow pane cuts the footer's hint
    let spinner = bottom.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with(['·', '✢', '✳', '✶', '✻', '✽']) && t.contains("… (")
    });
    if menu || has(&PROMPT) {
        Screen::Prompt
    } else if spinner || has(&BUSY) {
        Screen::Busy
    } else if bottom.iter().any(|l| {
        let t = l.trim_start().trim_start_matches('─').trim_start();
        // Claude's "✻ Cooked for 2s", Codex's "Worked for 12s • 3:09 AM"
        (t.starts_with('✻') && t.contains(" for ")) || t.starts_with("Worked for ")
    }) {
        Screen::Answered
    } else if has(&READY) || bottom.iter().any(|l| l.trim_start().starts_with('❯') || l.trim_start().starts_with('›')) {
        Screen::Ready
    } else {
        Screen::Unknown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Busy,
    Idle,
    /// Waiting on the human (permission or trust prompt).
    Prompt,
    Done,
    Failed,
    Exited,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Ready => "ready",
            State::Busy => "busy",
            State::Idle => "idle",
            State::Prompt => "prompt",
            State::Done => "done",
            State::Failed => "failed",
            State::Exited => "exited",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        [State::Starting, State::Ready, State::Busy, State::Idle, State::Prompt, State::Done, State::Failed, State::Exited]
            .into_iter()
            .find(|x| x.as_str() == s)
    }

    pub fn finished(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Exited)
    }
}

/// What happened in the pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A burst of output started.
    Output,
    /// Output went quiet; the screen shows this.
    Quiet(Screen),
    /// The agent's process ended with this status.
    Exit(i32),
    /// The agent's title says it works (true) or stopped (false), and the screen shows this (`Unknown` when it
    /// wasn't read: a working title read in any state but Prompt).
    Title(bool, Screen),
}

/// The state after `s`. Finished states stay finished; a prompt holds until the screen no longer shows one.
pub fn next(state: State, s: Signal) -> State {
    if state.finished() {
        return state;
    }
    match s {
        Signal::Exit(0) => State::Done,
        Signal::Exit(_) => State::Failed,
        // its startup output, and a prompt's own redraws, don't make it busy
        Signal::Output if matches!(state, State::Prompt | State::Starting) => state,
        Signal::Output => State::Busy,
        Signal::Quiet(Screen::Prompt) => State::Prompt,
        Signal::Quiet(Screen::Busy) => State::Busy,
        Signal::Quiet(Screen::Answered) => State::Idle,
        Signal::Quiet(Screen::Ready | Screen::Unknown) | Signal::Title(false, Screen::Busy | Screen::Ready | Screen::Unknown) => {
            match state {
                State::Starting => State::Ready,
                State::Ready => State::Ready,
                _ => State::Idle,
            }
        },
        // a prompt shown while the title still spins holds until the screen changes
        Signal::Title(_, Screen::Prompt) => State::Prompt,
        Signal::Title(true, _) => State::Busy,
        Signal::Title(false, Screen::Answered) => State::Idle,
    }
}

/// `next` for an agent whose title has said whether it works (`title_working`): the title decides busy and not
/// busy; output and quiet only find a prompt shown, or gone. Output alone (an echo, a resize, a redraw) is not work.
pub fn next_titled(state: State, s: Signal) -> State {
    match s {
        Signal::Output => state,
        Signal::Quiet(Screen::Prompt) if !state.finished() => State::Prompt,
        Signal::Quiet(_) if state == State::Prompt => State::Idle,
        Signal::Quiet(_) => state,
        _ => next(state, s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(exec: &str, argv: &[&str]) -> Option<Provider> {
        provider_of(exec.as_bytes(), argv.iter().map(|a| a.as_bytes()))
    }

    #[test]
    fn providers_by_executable_argv0_or_script() {
        assert_eq!(p("/Users/me/.local/share/claude/versions/2.1.310", &["claude", "--resume"]), Some(Provider::Claude));
        assert_eq!(p("/opt/homebrew/bin/codex", &["codex"]), Some(Provider::Codex));
        assert_eq!(p("/Users/me/.bun/bin/omp", &["omp"]), Some(Provider::Omp));
        assert_eq!(p("/opt/homebrew/bin/node", &["node", "--no-warnings", "/x/node_modules/@anthropic-ai/claude-code/cli.js"]), Some(Provider::Claude));
        assert_eq!(p("/opt/homebrew/bin/node", &["node", "/usr/lib/node_modules/@openai/codex/bin/codex.js"]), Some(Provider::Codex));
        assert_eq!(p("/bin/zsh", &["-zsh"]), None);
        assert_eq!(p("/opt/homebrew/bin/node", &["node", "server.js"]), None);
    }

    #[test]
    fn screens() {
        let c = |s: &str| classify(s.lines());
        assert_eq!(c("Bash(rm -rf build)\n Do you want to proceed?\n❯ 1. Yes\n  2. No"), Screen::Prompt);
        assert_eq!(c("Quick safety check\n❯ 1. Yes, I trust this folder\n  2. No, exit"), Screen::Prompt);
        assert_eq!(c("✻ Thinking… (12s · esc to interrupt)\n────\n❯ \n────"), Screen::Busy);
        assert_eq!(c("● Done.\n────\n❯ \n────\n  ? for shortcuts"), Screen::Ready);
        // regression (live run 2026-10-26): a turn finished within the startup output read as "ready"
        assert_eq!(c("❯ Reply with exactly: OK1\n⏺ OK1\n✻ Cooked for 2s · done 1:12 PM\n────\n❯ \n────"), Screen::Answered);
        assert_eq!(c("✻ Cooking… (3s · esc to interrupt)\n────\n❯ \n────"), Screen::Busy, "the spinner is work, not a finished turn");
        assert_eq!(c("$ ls\nfoo bar"), Screen::Unknown);
        // regression (live run 2026-10-29): Codex's finished turn read as "ready", so its task never went idle
        assert_eq!(c("• done\n\n  Worked for 12s • 3:09 AM\n\n› Ask Codex to do anything\n  ? for shortcuts"), Screen::Answered);
        assert_eq!(c("› Ask Codex to do anything\n  ? for shortcuts"), Screen::Ready);
        // Claude Code 2.1.316 (live 2026-10-31, 64 columns)
        let footer = "\n─────────────────────── w5 ─\n❯\n────────────────────────────\n  ⏵⏵ auto mode on";
        assert_eq!(c(&format!("✽ Mulling… (3m 9s · still thinking with xhigh effort)\n{footer} (shift+tab to cycle) · esc to interrupt · ←…")), Screen::Busy);
        assert_eq!(c(&format!("· Whirlpooling… (3m 8s · ↓ 13.9k tokens)\n{footer} (shift+tab to cyc…")), Screen::Busy, "the spinner line where the hint is cut");
        assert_eq!(c(&format!("✻ Crunched for 2m 53s · done 1:12 AM · 1 shell still running\n{footer} · 1 shell · ← 7 agents · ↓ to manage")), Screen::Answered);
    }

    #[test]
    fn state_machine() {
        use Signal::*;
        let run = |signals: &[Signal]| signals.iter().fold(State::Starting, |s, x| next(s, *x));
        assert_eq!(run(&[Output, Quiet(Screen::Ready)]), State::Ready);
        assert_eq!(run(&[Output, Quiet(Screen::Answered)]), State::Idle, "a task done in the startup burst");
        assert_eq!(run(&[Quiet(Screen::Ready), Output]), State::Busy);
        assert_eq!(run(&[Quiet(Screen::Ready), Output, Quiet(Screen::Ready)]), State::Idle);
        assert_eq!(run(&[Output, Quiet(Screen::Busy)]), State::Busy, "quiet but still working (a long tool call)");
        // a prompt holds through output (its own redraws) until the screen changes
        assert_eq!(run(&[Output, Quiet(Screen::Prompt), Output]), State::Prompt);
        assert_eq!(run(&[Output, Quiet(Screen::Prompt), Output, Quiet(Screen::Ready)]), State::Idle);
        assert_eq!(run(&[Output, Exit(0), Output]), State::Done, "finished stays finished");
        assert_eq!(run(&[Exit(1)]), State::Failed);
    }

    #[test]
    fn claude_titles() {
        // Claude Code 2.1.316: MZ=["\u25D0","\u25D1"] while animating, IZ="\u2733" otherwise
        let t = |s| title_working(Provider::Claude, s);
        assert_eq!(t("◐ w7"), Some(true));
        assert_eq!(t("◑ iTerm2 multi-project long-running sessions"), Some(true));
        assert_eq!(t("✳ w8"), Some(false));
        assert_eq!(t("✳ Claude Code"), Some(false));
        assert_eq!(t("zsh"), None);
        assert_eq!(t("◐"), None, "a glyph alone is not the agent's title");
        assert_eq!(t(""), None);
        assert_eq!(title_working(Provider::Codex, "◐ x"), None, "only Claude's glyphs are known");
    }

    #[test]
    fn titled_state_machine() {
        use Signal::*;
        let run = |signals: &[Signal]| signals.iter().fold(State::Starting, |s, x| next_titled(s, *x));
        // regression (live 2026-10-31): a turn that never goes quiet (the spinner redraws) stayed Starting
        assert_eq!(run(&[Output, Title(true, Screen::Unknown)]), State::Busy);
        assert_eq!(run(&[Title(false, Screen::Ready)]), State::Ready, "Claude's startup title");
        assert_eq!(run(&[Title(true, Screen::Unknown), Title(false, Screen::Answered)]), State::Idle);
        assert_eq!(run(&[Title(true, Screen::Unknown), Title(false, Screen::Busy)]), State::Idle, "the title is the word, not a stale spinner line");
        // quiet while the title spins: a long tool call, still busy; output while idle (an echo, a resize): idle
        assert_eq!(run(&[Title(true, Screen::Unknown), Quiet(Screen::Ready)]), State::Busy);
        assert_eq!(run(&[Title(true, Screen::Unknown), Title(false, Screen::Answered), Output]), State::Idle);
        // a permission prompt, whatever the title does, until the screen no longer shows it
        assert_eq!(run(&[Title(true, Screen::Unknown), Quiet(Screen::Prompt)]), State::Prompt);
        assert_eq!(run(&[Title(true, Screen::Unknown), Quiet(Screen::Prompt), Title(true, Screen::Prompt)]), State::Prompt);
        assert_eq!(run(&[Title(true, Screen::Unknown), Quiet(Screen::Prompt), Title(true, Screen::Busy)]), State::Busy);
        assert_eq!(run(&[Title(false, Screen::Prompt)]), State::Prompt);
        assert_eq!(run(&[Title(false, Screen::Prompt), Quiet(Screen::Answered)]), State::Idle);
        assert_eq!(run(&[Title(true, Screen::Unknown), Exit(0), Title(true, Screen::Unknown)]), State::Done);
    }
}
