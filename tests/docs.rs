//! What agents read must be what the binary takes. `guide.rs`'s own tests check that each noun and verb the guide
//! and `AGENT_USAGE.md` name exists; these parse each example command whole, flags and values included, and each
//! flag `help NOUN` lists for a verb, through the real grammar (no app needed).

use itermkit::grammar::{self, Noun, Verb, MATRIX};

/// Words that start a command in the docs but are not nouns of the grammar (top-level commands).
const TOP: &[&str] = &["tree", "children", "help", "guide", "batch", "plan", "version", "coverage", "wait"];

/// Placeholders the docs write for a value, and a value of the right shape.
const VALUES: &[(&str, &str, &str)] = &[("--side", "S", "right"), ("--frame", "x,y,w,h", "0,25,800,600")];

/// One doc example as argv: brackets dropped (optional parts included), the first of `a|b` alternatives, `…`,
/// `<placeholders>` and the placeholder values of [`VALUES`] made concrete.
fn argv(cmd: &str) -> Vec<String> {
    let cmd = cmd.replace(['[', ']'], " ").replace('…', " ").replace("<id>", "1234");
    let words = itermkit::cli::shell_words(&cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"));
    let mut out: Vec<String> = words.into_iter().map(|w| if w.contains(' ') { w } else { w.split('|').next().unwrap_or("").to_string() }).filter(|w| !w.is_empty()).collect();
    for i in 1..out.len() {
        if let Some((_, _, v)) = VALUES.iter().find(|(f, p, _)| out[i - 1] == *f && out[i] == *p) {
            out[i] = v.to_string();
        }
    }
    out
}

/// A command with alternative forms (`noun verb X --a A | --b B`) as one command per form, each with the prefix
/// before the first flag.
fn forms(cmd: &str) -> Vec<String> {
    let mut parts = cmd.split(" | ");
    let first = parts.next().unwrap_or("").to_string();
    let prefix = first.split(" --").next().unwrap_or("").to_string();
    std::iter::once(first).chain(parts.map(|p| format!("{prefix} {p}"))).collect()
}

/// The commands in a line of prose: from each `iterm `, and each ` · ` or `; ` continuation that starts with a
/// noun; a command ends at two spaces, ` (`, ` #`, `: ` or a backtick.
fn commands(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (k, seg) in line.split(" · ").flat_map(|s| s.split("; ")).enumerate() {
        let seg = match seg.find("iterm ") {
            Some(i) => &seg[i + "iterm ".len()..],
            None if k > 0 => seg.trim_start(),
            None => continue,
        };
        let end = ["  ", " (", " #", ": ", "`"].iter().filter_map(|t| seg.find(t)).min().unwrap_or(seg.len());
        let cmd = seg[..end].trim();
        let first = cmd.split_whitespace().next().unwrap_or("");
        if first.split('|').all(|n| Noun::parse(n).is_some()) && !TOP.contains(&first) {
            out.extend(forms(cmd));
        }
    }
    out
}

/// Parse `cmd` as the CLI would (every noun of `a|b|c`); the failures.
fn parse_all(cmd: &str) -> Vec<String> {
    let words = argv(cmd);
    let (nouns, rest) = (cmd.split_whitespace().next().unwrap(), &words[1..]);
    nouns
        .split('|')
        .filter_map(|n| grammar::parse(Noun::parse(n).unwrap(), rest).err().map(|e| format!("`iterm {n} {}`: {e}", rest.join(" "))))
        .collect()
}

#[test]
fn every_guide_example_parses_with_its_flags() {
    let mut seen = 0;
    let mut bad = Vec::new();
    for line in itermkit::guide::GUIDE.lines() {
        for cmd in commands(&line.replace('`', "  ")) {
            seen += 1;
            bad.extend(parse_all(&cmd));
        }
    }
    assert!(seen > 30, "the guide's commands were found: {seen}");
    assert!(bad.is_empty(), "the guide shows commands the grammar refuses:\n{}", bad.join("\n"));
}

#[test]
fn every_agent_usage_example_parses_with_its_flags() {
    let page = include_str!("../AGENT_USAGE.md");
    let mut seen = 0;
    let mut bad = Vec::new();
    for line in page.lines().filter(|l| l.starts_with("iterm ") || l.contains("`iterm ")) {
        for cmd in commands(&line.replace('`', "  ")) {
            seen += 1;
            bad.extend(parse_all(&cmd));
        }
    }
    assert!(seen >= 12, "the page's commands were found: {seen}");
    assert!(bad.is_empty(), "AGENT_USAGE.md shows commands the grammar refuses:\n{}", bad.join("\n"));
}

/// The `--flags` of a help line.
fn flags(h: &str) -> Vec<String> {
    h.split(|c: char| c.is_whitespace() || "[]()|,:;".contains(c)).filter(|w| w.starts_with("--") && w.len() > 2).map(|w| w.split('=').next().unwrap().to_string()).collect()
}

/// A flag the grammar takes for `noun verb`, alone or with a value: some attempt parses, or fails only on the value
/// itself (`--side: left|right|up|down`). Unknown flags and wrong arity fail in `args` (`arguments: …`), a flag out
/// of its verb's slot as "… do not apply to …".
fn takes(noun: Noun, verb: Verb, flag: &str) -> Result<(), String> {
    let attempt = |words: &[&str]| grammar::parse(noun, &words.iter().map(|w| w.to_string()).collect::<Vec<_>>()).err().map(|e| e.to_string());
    let v = verb.name();
    let errors = [attempt(&[v, flag]), attempt(&[v, flag, "1"])];
    let refused = |e: &Option<String>| e.as_ref().is_some_and(|e| e.starts_with("arguments:") || e.contains("do not apply"));
    if errors.iter().all(refused) {
        return Err(errors.into_iter().flatten().collect::<Vec<_>>().join(" / "));
    }
    Ok(())
}

#[test]
fn every_flag_help_lists_is_taken_by_its_verb() {
    let mut bad = Vec::new();
    let mut seen = 0;
    for (noun, verbs) in MATRIX {
        for (verb, h) in *verbs {
            for f in flags(h) {
                seen += 1;
                if let Err(e) = takes(*noun, *verb, &f) {
                    bad.push(format!("help {noun}: {} lists {f}: {e}", verb.name()));
                }
            }
        }
    }
    assert!(seen > 80, "help's flags were found: {seen}");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn every_flag_of_the_top_level_help_exists() {
    // the global flags every command takes (`cli.rs`)
    let mut opts: Vec<String> = ["--plan", "--apply", "--json"].map(String::from).to_vec();
    opts.extend(grammar::options().iter().map(|o| o.name.to_string()));
    let missing: Vec<String> = grammar::HELP.lines().flat_map(flags).filter(|f| !opts.contains(f)).collect();
    assert!(missing.is_empty(), "iterm help names flags no command takes: {missing:?}");
}

#[test]
fn the_extraction_finds_commands_and_their_refusals() {
    // not vacuous: the reader finds each form of a line, and a flag the grammar lacks fails
    assert_eq!(commands("iterm pane read X --tail 40         a pane's last lines; pane find X --match RE"), ["pane read X --tail 40", "pane find X --match RE"]);
    assert_eq!(commands("iterm tab new [--in VTAB] · vtab new NAME · window new"), ["tab new [--in VTAB]", "vtab new NAME", "window new"]);
    assert_eq!(forms("pane move X --at Y --side S | --to new-tab"), ["pane move X --at Y --side S", "pane move X --to new-tab"]);
    assert_eq!(argv("window set W --frame x,y,w,h --side left|right"), ["window", "set", "W", "--frame", "0,25,800,600", "--side", "left"]);
    assert!(parse_all("pane new --no-such-flag").len() == 1 && parse_all("pane|tab new --side sideways").len() == 2);
    assert!(takes(Noun::Pane, Verb::New, "--ratio").is_ok() && takes(Noun::Pane, Verb::New, "--no-such-flag").is_err());
    assert!(takes(Noun::Pane, Verb::List, "--weights").is_err(), "a geometry flag on a read verb is out of its slot");
}
