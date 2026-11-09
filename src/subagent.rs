//! Claude Code's own sub-agents (the Agent tool's), from its records: each leaves its transcript at
//! `<claude home>/projects/<project>/<parent session>/subagents/agent-<id>.jsonl`, with `agent-<id>.meta.json`
//! beside it (`description`, the name it was given; `agentType`; `toolUseId`, the parent's Agent call). The task is
//! the transcript's first message, the model the parent's Agent call's `model`. `claude --resume` takes no agent
//! id, so `agent resume agent-<id>` starts a new Claude agent told its name, task and transcript. Read when asked.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// One sub-agent as Claude Code recorded it.
#[derive(Debug, Clone, Default)]
pub struct Sub {
    pub id: String,
    pub transcript: PathBuf,
    pub parent_session: String,
    pub name: String,
    pub agent_type: String,
    pub task: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub modified: Option<SystemTime>,
}

fn projects() -> PathBuf {
    crate::claude::home().join("projects")
}

/// `agent-a29d…` or `a29d…` as the bare id, if it looks like one (hex, as the Agent tool makes them).
pub fn id_of(word: &str) -> Option<&str> {
    let id = word.strip_prefix("agent-").unwrap_or(word);
    (id.len() >= 8 && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// Every sub-agent transcript under `root` (`<project>/<session>/subagents/agent-*.jsonl`) with its time.
fn transcripts(root: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    let dirs = |p: &Path| std::fs::read_dir(p).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>()).unwrap_or_default();
    for project in dirs(root) {
        for session in dirs(&project) {
            let Ok(d) = std::fs::read_dir(session.join("subagents")) else { continue };
            for e in d.flatten() {
                let p = e.path();
                let is = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agent-") && n.ends_with(".jsonl"));
                if is {
                    let t = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                    out.push((p, t));
                }
            }
        }
    }
    out
}

/// Sub-agent `id` (bare), found in any project.
pub fn find(id: &str) -> Option<Sub> {
    find_in(&projects(), id)
}

fn find_in(root: &Path, id: &str) -> Option<Sub> {
    let file = format!("agent-{id}.jsonl");
    transcripts(root).into_iter().find(|(p, _)| p.file_name().is_some_and(|n| n == file.as_str())).map(|(p, t)| load(&p, Some(t), true))
}

/// The newest `n` sub-agents that ran under folder `under` (any, with None), newest first; each one's name, task and
/// folder read, not its model (that reads the parent session).
pub fn history(n: usize, under: Option<&Path>) -> Vec<Sub> {
    history_in(&projects(), n, under)
}

fn history_in(root: &Path, n: usize, under: Option<&Path>) -> Vec<Sub> {
    let mut all = transcripts(root);
    all.sort_by(|a, b| b.1.cmp(&a.1));
    all.into_iter()
        .map(|(p, t)| load(&p, Some(t), false))
        .filter(|s| under.is_none_or(|u| s.cwd.as_deref().is_some_and(|c| Path::new(c).starts_with(u))))
        .take(n)
        .collect()
}

/// A message's text: a string, or its text blocks joined.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn load(transcript: &Path, modified: Option<SystemTime>, model: bool) -> Sub {
    let id = transcript.file_stem().and_then(|s| s.to_str()).unwrap_or("").trim_start_matches("agent-").to_string();
    let session_dir = transcript.parent().and_then(Path::parent);
    let parent_session = session_dir.and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_string();
    let meta: Value = std::fs::read(transcript.with_extension("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let s = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut sub = Sub { id, transcript: transcript.to_path_buf(), parent_session, name: s("description"), agent_type: s("agentType"), modified, ..Sub::default() };
    // the first line is the task as given; any line carries the folder it ran in
    if let Ok(f) = std::fs::File::open(transcript) {
        if let Some(Ok(line)) = BufReader::new(f).lines().next() {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                sub.task = v.get("message").and_then(|m| m.get("content")).map(text_of).unwrap_or_default();
                sub.cwd = v.get("cwd").and_then(Value::as_str).map(str::to_string);
            }
        }
    }
    // the model, from the parent's Agent call (only lines naming that call are parsed)
    let call = s("toolUseId");
    if let (false, true, Some(dir)) = (call.is_empty(), model, session_dir) {
        let parent = dir.with_extension("jsonl");
        if let Ok(f) = std::fs::File::open(parent) {
            for line in BufReader::new(f).lines().map_while(Result::ok).filter(|l| l.contains(&call) && l.contains("\"tool_use\"")) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let blocks = v.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
                if let Some(b) = blocks.iter().find(|b| b.get("id").and_then(Value::as_str) == Some(call.as_str())) {
                    sub.model = b.pointer("/input/model").and_then(Value::as_str).map(str::to_string);
                    if sub.name.is_empty() {
                        sub.name = b.pointer("/input/description").and_then(Value::as_str).unwrap_or("").to_string();
                    }
                    break;
                }
            }
        }
    }
    if sub.name.is_empty() {
        sub.name = format!("agent-{}", sub.id);
    }
    sub
}

/// A name as an agent name (1–64 letters, digits, `-`): "My files explorer revamp" -> "my-files-explorer-revamp".
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s: String = s.trim_end_matches('-').chars().take(40).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "agent".into() } else { s }
}

/// What the new agent is told: who it was, its task, and where its transcript is.
pub fn resume_task(sub: &Sub) -> String {
    format!(
        "You are resuming a Claude Code sub-agent named \"{}\"{}. Its original task was:\n\n{}\n\nIts transcript (JSONL) is at {}. \
         Read it to recover what was done, decided and found, then continue the task from where it stopped. Don't redo \
         finished steps.",
        sub.name,
        if sub.agent_type.is_empty() { String::new() } else { format!(" ({})", sub.agent_type) },
        sub.task.trim(),
        sub.transcript.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_task_model_and_history() {
        let root = std::env::temp_dir().join(format!("iterm-subagent-test-{}", std::process::id()));
        let sess = root.join("proj").join("sess1");
        std::fs::create_dir_all(sess.join("subagents")).unwrap();
        std::fs::write(
            sess.join("subagents/agent-a29d8f1ac1f220aa4.jsonl"),
            "{\"agentId\":\"a29d8f1ac1f220aa4\",\"cwd\":\"/tmp\",\"message\":{\"role\":\"user\",\"content\":\"Review core.mjs\"}}\n{}\n",
        )
        .unwrap();
        std::fs::write(sess.join("subagents/agent-a29d8f1ac1f220aa4.meta.json"), r#"{"agentType":"general-purpose","description":"My files explorer revamp","toolUseId":"toolu_1"}"#).unwrap();
        std::fs::write(
            root.join("proj/sess1.jsonl"),
            "{\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Agent\",\"input\":{\"description\":\"x\",\"model\":\"sonnet\"}}]}}\n",
        )
        .unwrap();
        let s = find_in(&root, "a29d8f1ac1f220aa4").unwrap();
        assert_eq!((s.name.as_str(), s.agent_type.as_str(), s.task.as_str(), s.model.as_deref(), s.cwd.as_deref()), ("My files explorer revamp", "general-purpose", "Review core.mjs", Some("sonnet"), Some("/tmp")));
        assert_eq!(s.parent_session, "sess1");
        assert!(resume_task(&s).contains("agent-a29d8f1ac1f220aa4.jsonl"));
        assert_eq!(history_in(&root, 5, None).len(), 1);
        assert_eq!(history_in(&root, 5, Some(Path::new("/tmp"))).len(), 1);
        assert_eq!(history_in(&root, 5, Some(Path::new("/elsewhere"))).len(), 0);
        assert!(find_in(&root, "deadbeefdeadbeef").is_none());
        let _ = std::process::Command::new("/usr/bin/trash").arg(&root).output();
    }

    #[test]
    fn ids_and_slugs() {
        assert_eq!(id_of("agent-a29d8f1ac1f220aa4"), Some("a29d8f1ac1f220aa4"));
        assert_eq!(id_of("w3"), None);
        assert_eq!(slug("My files explorer revamp"), "my-files-explorer-revamp");
        assert_eq!(slug("Prototype mode, Help, examples"), "prototype-mode-help-examples");
        assert_eq!(slug("!!"), "agent");
    }
}
