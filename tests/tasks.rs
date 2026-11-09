//! `--task` and `--task-file`: how a brief becomes agents. A task file is one agent's whole task; with `--split`
//! it is cut by `agent::split_tasks` (`## ` sections, else top-level list items); a `{i}` path reads one file per
//! index.

mod common;

use common::*;

/// Spawn read-only agents from `args` with the files `files` in the caller's folder: each new agent's whole task.
fn tasks_of(files: &[(&str, &str)], args: &str) -> (i32, Vec<String>) {
    let w = world("tasks");
    let lead = w.window();
    for (name, text) in files {
        std::fs::write(w.dir.join(name), text).unwrap();
    }
    let (code, rows) = run(w.env(Some(lead)), &format!("agent new {args} --read-only"));
    let tasks = rows
        .iter()
        .filter(|r| s(r, "code") == "created")
        .map(|r| {
            // the task file the agent's command reads: the whole task, as the agent receives it
            let cmd = w.node(pid(r)).unwrap().cmd.unwrap();
            let f = cmd.split("$(cat ").nth(1).unwrap().split(" &&").next().unwrap().trim_matches('\'').to_string();
            std::fs::read_to_string(f).unwrap()
        })
        .collect();
    (code, tasks)
}

#[test]
fn split_makes_one_agent_per_section() {
    let (code, tasks) = tasks_of(&[("T.md", "## parser\nfix it\n\n## docs\nwrite them\n")], "--task-file T.md --split");
    assert_eq!(code, 0);
    assert_eq!(tasks, ["parser\nfix it", "docs\nwrite them"]);
}

#[test]
fn split_without_sections_makes_one_agent_per_top_level_item() {
    // `- `, `* ` and `1. ` at the start of a line; nested items stay with theirs
    let (_, tasks) = tasks_of(&[("T.md", "- one\n  - detail\n* two\n3. three\n")], "--task-file T.md --split");
    assert_eq!(tasks, ["one\n  - detail", "two", "three"]);
}

#[test]
fn split_of_a_file_without_sections_or_items_is_one_agent_with_all_of_it() {
    let brief = "# Fix the parser\n\nRead src/parse.rs first.\n- only one item\n";
    let (_, tasks) = tasks_of(&[("T.md", brief)], "--task-file T.md --split");
    assert_eq!(tasks, [brief.trim()]);
}

#[test]
fn several_files_are_one_agent_each_and_never_split() {
    let (_, tasks) = tasks_of(&[("a.md", "- x\n- y\n"), ("b.md", "## p\n1\n## q\n2\n")], "--task-file a.md --task-file b.md");
    assert_eq!(tasks, ["- x\n- y", "## p\n1\n## q\n2"]);
}

#[test]
fn a_numbered_path_reads_one_file_per_index() {
    let (_, tasks) = tasks_of(&[("t1.md", "first\n"), ("t2.md", "- a\n- b\n")], "--task-file t{i}.md --count 2");
    assert_eq!(tasks, ["first", "- a\n- b"]);
    let (code, _) = tasks_of(&[], "--task-file t{i}.md");
    assert_eq!(code, 1, "{{i}} needs --count");
}

#[test]
fn tasks_and_a_file_together_are_tasks_first() {
    let (_, tasks) = tasks_of(&[("T.md", "from the file\n")], "--task inline --task-file T.md");
    assert_eq!(tasks, ["inline", "from the file"]);
}

#[test]
fn no_line_of_a_split_brief_is_lost() {
    let (_, tasks) = tasks_of(&[("T.md", "# Fix the parser\nRead src/parse.rs first.\n\n- keep the API\n- add tests\n")], "--task-file T.md --split");
    let all = tasks.join("\n");
    assert!(all.contains("Fix the parser") && all.contains("Read src/parse.rs first."), "{tasks:?}");
}

#[test]
fn a_brief_with_sections_and_a_bullet_list_is_one_agent() {
    // (it was one agent per bullet, the brief's first lines dropped: fixed in c10d373)
    let brief = "Make the parser strict.\n\n## Acceptance\n- cargo test green\n- no new warnings\n\n## Hand-off\nmessage the lead\n";
    let (_, tasks) = tasks_of(&[("T.md", brief)], "--task-file T.md");
    assert_eq!(tasks, [brief.trim()]);
}
