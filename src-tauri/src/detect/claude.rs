//! Claude Code (CLI, desktop app, IDE extensions, Agent SDK).
//!
//! Claude Code keeps a registry of live sessions in `~/.claude/sessions/<pid>.json` with a
//! `busy` / `idle` / `waiting` status. The transcript in `~/.claude/projects` tells us why a
//! session stopped (rate limit, connection error) and whether an `idle` session still has
//! work in flight: background commands and subagents wake it up again when they finish,
//! and an unmet `/goal` keeps it going. Older versions without the registry fall back to
//! transcript activity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::{parse_ts, CpuTracker, ProcMatch, Session, Status, ACTIVE_MS, OPEN_TURN_MS};
use crate::classify::{classify, Stop};
use crate::logs::{base_name, meta, FileSet, Root, Tail};
use crate::procs::Procs;

pub const PROCS: ProcMatch = ProcMatch {
    names: &["claude"],
    cmds: &["@anthropic-ai/claude-code", "/claude-code/cli"],
};

const INITIAL_TAIL: u64 = 16 << 20;
/// An unmet `/goal` keeps an idle session counted as working this long.
const GOAL_IDLE_MS: i64 = 30 * 60_000;
/// Background tasks older than this are assumed lost rather than still running.
const TASK_MAX_MS: i64 = 12 * 3_600_000;
/// Final statuses in a `<task-notification>`.
const TASK_DONE: &[&str] = &[
    "completed",
    "failed",
    "killed",
    "stopped",
    "cancelled",
    "error",
];

#[derive(Deserialize)]
struct Registry {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    name: Option<String>,
    status: Option<String>,
    #[serde(rename = "statusUpdatedAt")]
    status_updated_at: Option<i64>,
    #[serde(rename = "startedAt")]
    started_at: Option<i64>,
    entrypoint: Option<String>,
    #[serde(rename = "logPath")]
    log_path: Option<String>,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isApiErrorMessage", default)]
    api_error: bool,
    error: Option<String>,
    timestamp: Option<String>,
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    stop_reason: Option<String>,
}

/// A background command or subagent that has not reported back yet.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Task {
    started: i64,
    agent: bool,
}

#[derive(Default)]
struct Transcript {
    tail: Tail,
    /// Why the last turn stopped, if it ended in an error we care about.
    stop: Option<(Stop, i64)>,
    /// The last turn has not finished yet.
    open_turn: bool,
    /// Background commands and subagents still running, by task id.
    tasks: HashMap<String, Task>,
    /// A `/goal` is set and not met yet.
    goal: bool,
}

impl Transcript {
    fn update(&mut self, path: &Path, len: u64) {
        let Transcript {
            tail,
            stop,
            open_turn,
            tasks,
            goal,
        } = self;
        tail.read(path, len, INITIAL_TAIL, |raw| {
            let Ok(line) = serde_json::from_str::<Line>(raw) else {
                return;
            };
            let at = line
                .timestamp
                .as_deref()
                .and_then(parse_ts)
                .unwrap_or_else(crate::logs::now_ms);
            track_tasks(raw, line.kind.as_deref(), at, tasks);
            if raw.contains("\"type\":\"goal_status\",\"met\":true") {
                *goal = false;
            } else if raw.contains("\"type\":\"goal_status\",\"met\":false") {
                *goal = true;
            }
            match line.kind.as_deref() {
                Some("assistant") if line.api_error => {
                    let text = error_text(raw, line.error.as_deref());
                    *stop = classify(&text, at).map(|s| (s, at));
                    *open_turn = false;
                }
                Some("assistant") => {
                    *stop = None;
                    let reason = line.message.and_then(|m| m.stop_reason);
                    *open_turn = !matches!(
                        reason.as_deref(),
                        Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal")
                    );
                }
                Some("user") => {
                    *stop = None;
                    *open_turn = !raw.contains("[Request interrupted by user");
                }
                _ => {}
            }
        });
    }
}

/// Work an idle session still has in flight.
#[derive(Default)]
struct Work {
    /// Start of the oldest background task still running.
    started: Option<i64>,
    /// Background subagents still running.
    agents: u32,
    goal: bool,
}

impl Transcript {
    /// Running background tasks started after `since`.
    fn work(&self, since: i64, now: i64) -> Work {
        let live = self
            .tasks
            .values()
            .filter(|t| t.started >= since && now - t.started < TASK_MAX_MS);
        let mut work = Work {
            goal: self.goal,
            ..Work::default()
        };
        for t in live {
            work.started = Some(work.started.map_or(t.started, |s| s.min(t.started)));
            work.agents += t.agent as u32;
        }
        work
    }
}

/// Background work started and finished in this transcript line. Keys are matched
/// unescaped, so text quoted inside tool input or output does not count.
fn track_tasks(raw: &str, kind: Option<&str>, at: i64, tasks: &mut HashMap<String, Task>) {
    if let Some(id) = string_after(raw, "\"backgroundTaskId\":\"") {
        tasks.insert(
            id,
            Task {
                started: at,
                agent: false,
            },
        );
    }
    if raw.contains("\"status\":\"async_launched\"") {
        if let Some(id) = string_after(raw, "\"agentId\":\"") {
            tasks.insert(
                id,
                Task {
                    started: at,
                    agent: true,
                },
            );
        }
    }
    if raw.contains("\"name\":\"TaskStop\"") || raw.contains("Successfully stopped task") {
        if let Some(id) = string_after(raw, "\"task_id\":\"") {
            tasks.remove(&id);
        }
    }
    // Notifications count only where Claude Code delivers them: the message queue, a
    // queued command, or a user message that is the notification itself.
    let delivered = match kind {
        Some("queue-operation") => raw.contains("\"content\":\"<task-notification>"),
        Some("attachment") => {
            raw.contains("\"type\":\"queued_command\",\"prompt\":\"<task-notification>")
        }
        Some("user") => raw.contains("\"role\":\"user\",\"content\":\"<task-notification>"),
        _ => false,
    };
    if !delivered {
        return;
    }
    let mut rest = raw;
    while let Some(i) = rest.find("<task-notification>") {
        rest = &rest[i + "<task-notification>".len()..];
        let block = &rest[..rest.find("</task-notification>").unwrap_or(rest.len())];
        if let (Some(id), Some(status)) = (tag(block, "task-id"), tag(block, "status")) {
            if TASK_DONE.contains(&status) {
                tasks.remove(id);
            }
        }
    }
}

/// The JSON string value right after `key` (which ends with the opening quote).
fn string_after(raw: &str, key: &str) -> Option<String> {
    let start = raw.find(key)? + key.len();
    let value = &raw[start..start + raw[start..].find('"')?];
    (!value.is_empty() && value.len() <= 128).then(|| value.to_string())
}

fn tag<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let end = start + block[start..].find("</")?;
    Some(block[start..end].trim())
}

/// Text of an API error record plus its machine readable error code.
fn error_text(raw: &str, code: Option<&str>) -> String {
    let mut text = String::from(code.unwrap_or_default());
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(api) = v.get("apiError").and_then(|a| a.as_str()) {
            text.push(' ');
            text.push_str(api);
        }
        if let Some(parts) = v.pointer("/message/content").and_then(|c| c.as_array()) {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    text.push(' ');
                    text.push_str(t);
                }
            }
        }
    }
    text
}

pub struct Claude {
    roots: Vec<PathBuf>,
    fallback_roots: Vec<Root>,
    transcripts: HashMap<PathBuf, Transcript>,
    paths: HashMap<String, PathBuf>,
    fallback: FileSet,
}

fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        roots.push(PathBuf::from(dir));
    }
    if let Some(home) = crate::logs::home_dir() {
        let dir = home.join(".claude");
        if !roots.contains(&dir) {
            roots.push(dir);
        }
    }
    roots
}

/// Claude Code's project directory name for a working directory.
fn project_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn label(entrypoint: Option<&str>) -> &'static str {
    match entrypoint {
        Some("claude-desktop") => "Claude App",
        Some("claude-vscode") => "Claude Code · VS Code",
        Some(e) if e.starts_with("sdk") => "Claude Agent SDK",
        _ => "Claude Code",
    }
}

impl Claude {
    pub fn new() -> Self {
        let roots = roots();
        let fallback_roots = roots
            .iter()
            .map(|r| Root {
                dir: r.join("projects"),
                depth: 1,
                suffixes: &[".jsonl"],
                parent: None,
            })
            .collect();
        Claude {
            roots,
            fallback_roots,
            transcripts: HashMap::new(),
            paths: HashMap::new(),
            fallback: FileSet::new(Duration::from_secs(20), Duration::from_secs(3 * 3600)),
        }
    }

    pub fn scan(
        &mut self,
        procs: &Procs,
        running: &[u32],
        cpu: &mut CpuTracker,
        now: i64,
        out: &mut Vec<Session>,
    ) {
        let mut registered = HashSet::new();
        let mut live_paths = HashSet::new();
        for root in self.roots.clone() {
            for reg in read_registry(&root.join("sessions")) {
                if !is_claude_process(procs, &reg) {
                    continue;
                }
                let session_id = reg.session_id.clone().unwrap_or_default();
                registered.insert(session_id.clone());
                let path = self.transcript_path(&root, &reg);
                let mut stop = None;
                let mut work = Work::default();
                if let Some(path) = &path {
                    if let Some(m) = meta(path) {
                        let t = self.transcripts.entry(path.clone()).or_default();
                        t.update(path, m.len);
                        stop = t.stop;
                        // Tasks from before this process started died with the old one.
                        work = t.work(reg.started_at.unwrap_or(0) - 60_000, now);
                        live_paths.insert(path.clone());
                    }
                }
                let idle_for = now - reg.status_updated_at.unwrap_or(now);
                let mut since = reg.status_updated_at;
                let status = match reg.status.as_deref() {
                    Some("busy") => Status::Working,
                    Some("waiting") => Status::Input,
                    _ => match stop {
                        Some((Stop::Limit { until }, at)) => Status::Limit { at, until },
                        Some((Stop::Network, at)) => Status::Interrupted { at },
                        // The turn ended but background commands or subagents are still
                        // running; the session wakes up when they finish.
                        None if work.started.is_some() => {
                            since = work.started;
                            Status::Working
                        }
                        None if work.goal && idle_for < GOAL_IDLE_MS => Status::Goal,
                        None if cpu.busy(procs, reg.pid) => Status::Working,
                        None => Status::Idle,
                    },
                };
                let title = reg
                    .name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| reg.cwd.as_deref().map(base_name))
                    .unwrap_or_else(|| "Claude Code".into());
                out.push(Session {
                    key: format!("claude:{session_id}:{}", reg.pid),
                    icon: "claude",
                    label: label(reg.entrypoint.as_deref()).into(),
                    title,
                    pid: Some(reg.pid),
                    status,
                    since,
                    last_active: reg.status_updated_at.unwrap_or(now),
                    agents: if work.agents > 0 { work.agents + 1 } else { 0 },
                });
            }
        }

        if !running.is_empty() {
            self.scan_fallback(&registered, &mut live_paths, now, out);
        }
        self.transcripts.retain(|p, _| live_paths.contains(p));
    }

    fn transcript_path(&mut self, root: &Path, reg: &Registry) -> Option<PathBuf> {
        if let Some(p) = &reg.log_path {
            return Some(PathBuf::from(p));
        }
        let id = reg.session_id.as_deref()?;
        if let Some(p) = self.paths.get(id) {
            if p.exists() {
                return Some(p.clone());
            }
        }
        let projects = root.join("projects");
        let file = format!("{id}.jsonl");
        let mut found = None;
        if let Some(cwd) = &reg.cwd {
            let p = projects.join(project_slug(cwd)).join(&file);
            if p.exists() {
                found = Some(p);
            }
        }
        if found.is_none() {
            found = std::fs::read_dir(&projects)
                .ok()?
                .flatten()
                .map(|e| e.path().join(&file))
                .find(|p| p.exists());
        }
        if let Some(p) = &found {
            self.paths.insert(id.to_string(), p.clone());
        }
        found
    }

    /// Sessions of Claude Code versions without the registry: judged by transcript writes.
    fn scan_fallback(
        &mut self,
        registered: &HashSet<String>,
        live_paths: &mut HashSet<PathBuf>,
        now: i64,
        out: &mut Vec<Session>,
    ) {
        self.fallback.refresh(&self.fallback_roots);
        for (path, m) in &self.fallback.files {
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if registered.contains(&id) {
                continue;
            }
            let t = self.transcripts.entry(path.clone()).or_default();
            t.update(path, m.len);
            live_paths.insert(path.clone());
            let age = now - m.mtime;
            let status = match t.stop {
                Some((Stop::Limit { until }, at)) => Status::Limit { at, until },
                Some((Stop::Network, at)) => Status::Interrupted { at },
                None if age < ACTIVE_MS || (t.open_turn && age < OPEN_TURN_MS) => Status::Working,
                None if t.work(0, now).started.is_some() => Status::Working,
                None if t.goal && age < GOAL_IDLE_MS => Status::Goal,
                None => Status::Idle,
            };
            let project = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(Session {
                key: format!("claude:{id}"),
                icon: "claude",
                label: "Claude Code".into(),
                title: project.rsplit('-').next().unwrap_or(&project).to_string(),
                pid: None,
                status,
                since: None,
                last_active: m.mtime,
                agents: 0,
            });
        }
    }
}

fn read_registry(dir: &Path) -> Vec<Registry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.strip_suffix(".json")
                .is_some_and(|stem| stem.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Registry>(&bytes).ok())
        .collect()
}

/// Guards against stale registry files whose PID now belongs to another program.
fn is_claude_process(procs: &Procs, reg: &Registry) -> bool {
    let Some(p) = procs.get(reg.pid) else {
        return false;
    };
    let plausible = p.name.contains("claude") || matches!(p.name.as_str(), "node" | "bun");
    let started_ok = match (procs.start_time(reg.pid), reg.started_at) {
        (Some(proc_start), Some(reg_start)) => proc_start <= (reg_start / 1000) as u64 + 120,
        _ => true,
    };
    plausible && started_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(lines: &[&str]) -> Transcript {
        let path = crate::logs::tests::tmp("claude.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut t = Transcript::default();
        t.update(&path, meta(&path).unwrap().len);
        std::fs::remove_file(&path).ok();
        t
    }

    #[test]
    fn rate_limit_record_is_detected() {
        let t = transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"go"},"timestamp":"2026-10-21T16:20:00.000Z"}"#,
            r#"{"type":"assistant","error":"rate_limit","apiError":"usage_limit_reached","isApiErrorMessage":true,"timestamp":"2026-10-21T16:21:48.270Z","message":{"role":"assistant","content":[{"type":"text","text":"You've hit your session limit · resets 10:30pm (Europe/Prague)"}]}}"#,
        ]);
        assert!(matches!(t.stop, Some((Stop::Limit { until: Some(_) }, _))));
        assert!(!t.open_turn);
    }

    #[test]
    fn connection_error_is_detected() {
        let t = transcript(&[
            r#"{"type":"assistant","error":"unknown","isApiErrorMessage":true,"timestamp":"2026-10-21T16:03:24.699Z","message":{"content":[{"type":"text","text":"API Error: Connection error."}]}}"#,
        ]);
        assert!(matches!(t.stop, Some((Stop::Network, _))));
    }

    #[test]
    fn later_activity_clears_errors() {
        let t = transcript(&[
            r#"{"type":"assistant","error":"unknown","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"API Error: Connection error."}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"continue"}}"#,
        ]);
        assert!(t.stop.is_none());
        assert!(t.open_turn);
    }

    #[test]
    fn finished_turn_is_closed() {
        let t = transcript(&[
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[]}}"#,
            r#"{"type":"system","subtype":"stop_hook_summary"}"#,
        ]);
        assert!(!t.open_turn);
        assert!(t.stop.is_none());
    }

    const BG_START: &str = r#"{"type":"user","timestamp":"2026-10-23T14:06:00.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"Command running in background with ID: b0x0fik1x."}]},"toolUseResult":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"b0x0fik1x"}}"#;
    const BG_DONE: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-23T14:22:00.000Z","content":"<task-notification>\n<task-id>b0x0fik1x</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<output-file>C:\tmp\b0x0fik1x.output</output-file>\n<status>completed</status>\n<summary>Background command finished</summary>\n</task-notification>"}"#;
    const END_TURN: &str = r#"{"type":"assistant","timestamp":"2026-10-23T14:06:05.000Z","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Waiting for CI."}]}}"#;

    fn started(t: &Transcript) -> Option<i64> {
        t.work(0, parse_ts("2026-10-23T15:00:00Z").unwrap()).started
    }

    /// The turn ends while a background command runs: the session is not done.
    #[test]
    fn background_command_keeps_the_session_working() {
        let t = transcript(&[BG_START, END_TURN]);
        assert!(!t.open_turn);
        assert_eq!(started(&t), parse_ts("2026-10-23T14:06:00.000Z"));
        let t = transcript(&[BG_START, END_TURN, BG_DONE]);
        assert_eq!(started(&t), None);
    }

    #[test]
    fn stopped_and_failed_tasks_are_done() {
        let stop = r#"{"type":"assistant","timestamp":"2026-10-23T14:07:00.000Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_2","name":"TaskStop","input":{"task_id":"b0x0fik1x"}}]}}"#;
        assert_eq!(started(&transcript(&[BG_START, stop])), None);
        let failed = BG_DONE.replace("completed", "failed");
        assert_eq!(started(&transcript(&[BG_START, &failed])), None);
    }

    #[test]
    fn tasks_from_before_the_process_started_are_ignored() {
        let t = transcript(&[BG_START, END_TURN]);
        let after = parse_ts("2026-10-23T14:10:00Z").unwrap();
        assert_eq!(t.work(after, after + 60_000).started, None);
        // Lost tasks expire.
        assert_eq!(t.work(0, after + TASK_MAX_MS).started, None);
    }

    #[test]
    fn background_subagents_are_counted() {
        let launch = |id: &str| {
            format!(
                r#"{{"type":"user","timestamp":"2026-10-23T14:06:00.000Z","message":{{"role":"user","content":[]}},"toolUseResult":{{"isAsync":true,"status":"async_launched","agentId":"{id}","description":"Research"}}}}"#
            )
        };
        let done = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-23T14:20:00.000Z","content":"<task-notification>\n<task-id>a2</task-id>\n<status>completed</status>\n<summary>Agent finished</summary>\n<result>ok</result>\n</task-notification>"}"#;
        let (a1, a2) = (launch("a1"), launch("a2"));
        let t = transcript(&[&a1, &a2, END_TURN]);
        assert_eq!(
            t.work(0, parse_ts("2026-10-23T14:30:00Z").unwrap()).agents,
            2
        );
        let t = transcript(&[&a1, &a2, END_TURN, done]);
        assert_eq!(
            t.work(0, parse_ts("2026-10-23T14:30:00Z").unwrap()).agents,
            1
        );
    }

    /// Tool input and output that merely quote these records are ignored.
    #[test]
    fn quoted_records_do_not_count() {
        let quoted = r#"{"type":"user","timestamp":"2026-10-23T14:06:00.000Z","message":{"role":"user","content":[{"type":"tool_result","content":"{\"backgroundTaskId\":\"bfake\",\"status\":\"async_launched\",\"agentId\":\"afake\"}"}]}}"#;
        assert_eq!(started(&transcript(&[quoted, END_TURN])), None);

        // A notification typed into a tool call (or echoed in its output) is not real.
        let typed = r#"{"type":"assistant","timestamp":"2026-10-23T14:07:00.000Z","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"command":"echo '<task-notification>\n<task-id>b0x0fik1x</task-id>\n<status>completed</status>\n</task-notification>'"}}]}}"#;
        let echoed = r#"{"type":"user","timestamp":"2026-10-23T14:07:01.000Z","message":{"role":"user","content":[{"tool_use_id":"toolu_3","type":"tool_result","content":"<task-notification>\n<task-id>b0x0fik1x</task-id>\n<status>completed</status>\n</task-notification>"}]}}"#;
        let t = transcript(&[BG_START, typed, echoed, END_TURN]);
        assert!(started(&t).is_some());
        // Delivered as a user message, it is real.
        let delivered = r#"{"type":"user","timestamp":"2026-10-23T14:20:00.000Z","message":{"role":"user","content":"<task-notification>\n<task-id>b0x0fik1x</task-id>\n<status>completed</status>\n</task-notification>"}}"#;
        assert_eq!(started(&transcript(&[BG_START, END_TURN, delivered])), None);
    }

    #[test]
    fn unmet_goal_is_tracked_until_met() {
        let goal = |met: bool| {
            format!(
                r#"{{"type":"attachment","timestamp":"2026-10-23T14:06:00.000Z","attachment":{{"type":"goal_status","met":{met},"condition":"ship it"}}}}"#
            )
        };
        assert!(transcript(&[&goal(false), END_TURN]).goal);
        assert!(!transcript(&[&goal(false), END_TURN, &goal(true)]).goal);
    }

    #[test]
    fn slug_matches_claude_code() {
        assert_eq!(
            project_slug("C:\\Users\\Palo\\Desktop\\projects\\git\\agentNight"),
            "C--Users-Palo-Desktop-projects-git-agentNight"
        );
    }
}
