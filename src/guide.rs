//! `iterm guide [TOPIC]`: what an agent reads first. What iTerm2 is, the commands it needs, and the
//! orchestration model: do it yourself, or delegate to a subagent in a split pane of your own vertical tab.

pub const GUIDE: &str = "\
## iTerm2
The terminal (windows > tabs > split panes; a tab group is a vertical tab). `iterm` controls all of it, for
humans and agents: everything you could do by hand, and more. You run in a pane (`iterm pane get @me`). The CLI
is open: any agent may read, make, move, resize, rename, color or close anything; who spawned whom is recorded
(tree, children) but is not a permission. One line per item, key=value; --json for JSON.
Claude Code, Codex and OMP agents run side by side (--provider claude|codex|omp; claude is the default).

## see
iterm tree                          everything: windows, vertical tabs, tabs, panes, agents (state, name, cwd)
iterm pane get @me                  yourself; @parent is who spawned you, @focused is the human's pane
iterm pane list --all               every pane (selectors: IDs, names, globs, @me, …: iterm help)
iterm pane read X --tail 40         a pane's last lines; pane find X --match RE
iterm agent list                    agents: provider, model, state (starting ready busy idle prompt done exited)
iterm pane wait X --until idle|exit|text~RE [--timeout S]

## shape
iterm pane new --at @me --side right|down [--cmd C]      split (a shell, or a command)
iterm tab new [--in VTAB] · vtab new NAME · window new
iterm pane|tab|vtab set X --name N --color red|orange|yellow|green|teal|blue|purple|pink|#rrggbb|none
  renaming an agent's pane renames its session too (/rename), unless --no-session; rename yourself: pane set @me --name N
iterm pane move X --at Y --side S | --to new-tab|new-window · tab move X --to new-window|VTAB | --at PANE --side S
iterm tab|vtab order … · window set W --state zoomed|normal|minimized|fullscreen | --frame x,y,w,h | --tile grid
iterm content find X --match TEXT --show (the find bar, ⌘F) · action list|run NAME (every key binding) · app menu
iterm pane|tab|vtab|window select X (takes the human's focus: only when asked) · close X [--force]
iterm pane resize X --ratio 0.6 · tab set X --equalize · pane set X --bury
Every write runs at once; add --plan to see what it would do first.

## orchestrate
Decide per request:
- Do it yourself: one task, small or tightly coupled (a few files, minutes of work), or anything where explaining
  it would cost more than doing it.
- Delegate to a cheaper agent (--model sonnet|haiku, --effort low|medium): mechanical, well-specified work
  (renames across files, searches, test runs, boilerplate, docs).
- Delegate to an equal agent (your model, same effort): hard independent parts, or a review you want unanchored
  by your own reading (give it the code, not your conclusion).
- Several tasks: fan out, one agent per task, each in a split pane of YOUR vertical tab, run in parallel.
- An agent with no --at/--side goes to your dock: below the pane to your right (e.g. st -l), each next one to the
  right of the last, equal columns; you and that pane keep your sizes. --plan shows where.
Spawn in your own tab, beside you:
  iterm agent run 'task one' 'task two' --worktree --model sonnet      spawn one each, wait, one result line each
  iterm agent new --task-file T.md --model opus --effort high --worktree     (to your dock)
  iterm agent new --task '…' --read-only --at w1 --side down                  (or anywhere you name)
- Writers each get their own git worktree (--worktree); readers pass --read-only. Two writers never share a folder.
- A task is the child's whole context: goal, files to read, what is ruled out, acceptance checks, how to hand off.
- Spawned agents are named (--name; a worktree agent for its feature, the task's first line, on wt/<name>) and
  colored automatically; --provider codex|omp for other models.
- Wait, read: iterm children --wait (branch= ahead= landed=) · agent wait X · agent msg X --text T · agent inbox [--wait]
- Features go home by landing, never by hand: iterm agent run … --worktree → review the diffs →
  iterm agent land --all --check 'cargo test' --close (one merge commit each) · agent landed · agent unland X
- Bring back a Claude Code sub-agent (the Agent tool's) with its name, color and task: iterm agent list --history
  then iterm agent resume agent-<id> [--worktree|--read-only] (it reads its old transcript and continues).
- Subagents have every command too (recursion: a child may orchestrate its own children the same way).

## rules
- Not sandboxed. Before destroying work you didn't create (closing others' panes, tabs or windows, quitting
  iTerm2, stopping agents you didn't spawn, removing worktrees, merging into someone else's branch), ask whoever
  made it or the human: iterm agent msg X --text 'may I close tab t3? reason …'. What you made is yours.
- Never answer a permission or folder-trust prompt for the human; input to a pane showing one is held (queued).
- Closing a pane whose agent is live needs --force. Never move the human's focus unless asked (select, --focus).
- Never forge identity: you are the pane in $ITERM_SESSION_ID.
- Messages: iterm agent msg TO --text T; TO is a name or an id. Every message lands in the receiver's inbox.
  Claude to Claude (route=sendmessage): nothing pushes it for you; send it too, with SendMessage to the row's
  sendmessage_to (the receiver's Claude session name; pushed mid-turn, cheapest). The app also types a one-line
  notice in: notice_typed=yes only once the app confirmed it (notice=unconfirmed: not known to have landed).
  Other agents get the text typed in whole.
- Reading: agent inbox --mark-read (marks it read for the sender) · agent inbox --wait [S] blocks, no polling ·
  agent inbox --sent shows which of your messages were read · agent inbox --all-panes: every message, any pane.

## help
iterm help · iterm help NOUN (window vtab tab pane content agent task …) · iterm guide TOPIC
(topics: iterm2, see, shape, orchestrate, rules)";

/// The whole guide, or the sections whose heading starts with `topic`.
pub fn guide(topic: Option<&str>) -> Result<String, String> {
    let Some(t) = topic.map(|t| t.trim().to_ascii_lowercase()) else { return Ok(GUIDE.to_string()) };
    let found: Vec<&str> = GUIDE.split("## ").filter(|s| !s.is_empty() && s.to_ascii_lowercase().starts_with(t.as_str())).collect();
    if found.is_empty() {
        return Err(format!("no guide topic '{t}': iterm2, see, shape, orchestrate, rules, help"));
    }
    Ok(found.iter().map(|s| format!("## {}", s.trim_end())).collect::<Vec<_>>().join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics() {
        assert!(guide(None).unwrap().starts_with("## iTerm2"));
        assert!(guide(Some("iterm2")).unwrap().starts_with("## iTerm2"));
        let o = guide(Some("Orchestrate")).unwrap();
        assert!(o.starts_with("## orchestrate") && o.contains("--worktree") && !o.contains("## rules"));
        assert!(o.contains("agent land --all --check") && o.contains("agent unland"), "the worktree cycle is named");
        assert!(guide(Some("nope")).is_err());
    }

    /// Every command the guide names is in the grammar (a guide that lies costs agents more than none).
    #[test]
    fn commands_exist() {
        for line in GUIDE.lines() {
            let mut words = line.split_whitespace().skip_while(|w| *w != "iterm").skip(1);
            let (Some(noun), verb) = (words.next(), words.next()) else { continue };
            if ["tree", "children", "help", "guide", "batch", "plan", "version", "coverage", "wait"].contains(&noun) {
                continue;
            }
            for noun in noun.split('|') {
                let n = crate::grammar::Noun::parse(noun).unwrap_or_else(|| panic!("guide names noun '{noun}': {line}"));
                let v = verb.unwrap_or("");
                let v = v.split('|').next().unwrap_or(v);
                let v = crate::grammar::Verb::parse(v).unwrap_or_else(|| panic!("guide names verb '{v}': {line}"));
                assert!(crate::grammar::has(n, v), "guide names `{noun} {v:?}`, which the grammar lacks: {line}");
            }
        }
    }

    /// The same for `AGENT_USAGE.md`, the page agents read when they find the repository (it once promised v2's
    /// commands and a dry run by default, and an agent that trusted it opened ten real tabs).
    #[test]
    fn agent_usage_names_only_real_commands_and_the_real_gate() {
        let page = include_str!("../AGENT_USAGE.md");
        for line in page.lines().filter(|l| l.starts_with("iterm ")) {
            let mut words = line.split_whitespace().skip(1);
            let noun = words.next().unwrap_or("");
            if ["tree", "children", "help", "guide"].contains(&noun) {
                continue;
            }
            let n = crate::grammar::Noun::parse(noun).unwrap_or_else(|| panic!("names noun '{noun}': {line}"));
            let v = crate::grammar::Verb::parse(words.next().unwrap_or("")).unwrap_or_else(|| panic!("no verb: {line}"));
            assert!(crate::grammar::has(n, v), "names `{noun} {v:?}`: {line}");
        }
        assert!(page.contains("**Writes run at once.**") && page.contains("`--plan` is the dry run"));
    }
}
