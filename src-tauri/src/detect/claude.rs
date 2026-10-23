//! Claude Code (CLI, desktop app, IDE extensions, Agent SDK).
//!
//! Claude Code keeps a registry of live sessions in `~/.claude/sessions/<pid>.json` with a
//! `busy` / `idle` / `waiting` status. The transcript in `~/.claude/projects` tells us why a
//! session stopped (rate limit, connection error). Older versions without the registry
//! fall back to transcript activity.

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

const INITIAL_TAIL: u64 = 2 << 20;

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

#[derive(Default)]
struct Transcript {
    tail: Tail,
    /// Why the last turn stopped, if it ended in an error we care about.
    stop: Option<(Stop, i64)>,
    /// The last turn has not finished yet.
    open_turn: bool,
}

impl Transcript {
    fn update(&mut self, path: &Path, len: u64) {
        let Transcript {
            tail,
            stop,
            open_turn,
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
                if let Some(path) = &path {
                    if let Some(m) = meta(path) {
                        let t = self.transcripts.entry(path.clone()).or_default();
                        t.update(path, m.len);
                        stop = t.stop;
                        live_paths.insert(path.clone());
                    }
                }
                let status = match reg.status.as_deref() {
                    Some("busy") => Status::Working,
                    Some("waiting") => Status::Input,
                    _ => match stop {
                        Some((Stop::Limit { until }, at)) => Status::Limit { at, until },
                        Some((Stop::Network, at)) => Status::Interrupted { at },
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
                    since: reg.status_updated_at,
                    last_active: reg.status_updated_at.unwrap_or(now),
                    agents: 0,
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

    #[test]
    fn slug_matches_claude_code() {
        assert_eq!(
            project_slug("C:\\Users\\Palo\\Desktop\\projects\\git\\agentNight"),
            "C--Users-Palo-Desktop-projects-git-agentNight"
        );
    }
}
