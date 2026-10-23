//! Table-driven detection for the remaining harnesses. A session counts as working while
//! its log keeps changing (or, for CLI agents, while its process tree is busy running
//! tools); the tail of the log tells whether it stopped on a limit or a network error.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::{CpuTracker, ProcMatch, Session, Status, ACTIVE_MS, OPEN_TURN_MS};
use crate::classify::{classify, Stop};
use crate::logs::{base_name, read_first_line, read_tail, Base, FileSet, LogSpec, Meta, Root};
use crate::procs::Procs;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format {
    /// One session per file (`.jsonl` or `.json` transcripts).
    Files,
    /// Pi's JSONL sessions, which record explicit stop reasons.
    Pi,
    /// Databases or binary state: one row per harness, judged by write activity.
    Opaque,
}

pub struct Def {
    pub id: &'static str,
    pub icon: &'static str,
    pub name: &'static str,
    pub procs: ProcMatch,
    pub logs: &'static [LogSpec],
    pub format: Format,
    /// Desktop app: its process tree is the whole editor, so CPU load says nothing about
    /// the agent.
    pub app: bool,
}

const fn spec(
    base: Base,
    rel: &'static str,
    depth: u8,
    suffixes: &'static [&'static str],
) -> LogSpec {
    LogSpec {
        base,
        rel,
        depth,
        suffixes,
        parent: None,
    }
}

/// Like [`spec`], but only files directly inside a `chats` directory.
const fn chats(base: Base, rel: &'static str, suffixes: &'static [&'static str]) -> LogSpec {
    LogSpec {
        base,
        rel,
        depth: 2,
        suffixes,
        parent: Some("chats"),
    }
}

const fn procs(names: &'static [&'static str], cmds: &'static [&'static str]) -> ProcMatch {
    ProcMatch { names, cmds }
}

use Base::{Config, Data, Home};
use Format::{Files, Opaque, Pi};

pub static DEFS: &[Def] = &[
    Def {
        id: "gemini",
        icon: "gemini",
        name: "Gemini CLI",
        procs: procs(&["gemini"], &["@google/gemini-cli", "/gemini-cli/"]),
        logs: &[
            chats(Home, ".gemini/tmp", &[".json", ".jsonl"]),
            chats(Home, ".gemini/projects", &[".jsonl"]),
        ],
        format: Files,
        app: false,
    },
    Def {
        id: "opencode",
        icon: "opencode",
        name: "opencode",
        procs: procs(&["opencode"], &["opencode-ai", "/opencode/bin/"]),
        logs: &[
            spec(Home, ".local/share/opencode", 0, &[".db", ".db-wal"]),
            spec(Home, ".local/share/opencode/storage/session", 2, &[".json"]),
        ],
        format: Opaque,
        app: false,
    },
    Def {
        id: "cursor",
        icon: "cursor",
        name: "Cursor",
        procs: procs(&["cursor"], &[]),
        logs: &[spec(
            Config,
            "Cursor/User/globalStorage",
            0,
            &[".vscdb", ".vscdb-wal"],
        )],
        format: Opaque,
        app: true,
    },
    Def {
        id: "cursor-cli",
        icon: "cursor",
        name: "Cursor CLI",
        procs: procs(&["cursor-agent"], &["cursor-agent"]),
        logs: &[spec(Home, ".cursor/chats", 3, &[".db", ".db-wal"])],
        format: Opaque,
        app: false,
    },
    Def {
        id: "pi",
        icon: "pi",
        name: "Pi",
        procs: procs(&["pi"], &["pi-coding-agent", "@mariozechner/pi"]),
        logs: &[spec(Home, ".pi/agent/sessions", 2, &[".jsonl"])],
        format: Pi,
        app: false,
    },
    Def {
        id: "droid",
        icon: "droid",
        name: "Factory Droid",
        procs: procs(&["droid"], &["@factory/cli"]),
        logs: &[spec(Home, ".factory/sessions", 2, &[".jsonl"])],
        format: Files,
        app: false,
    },
    Def {
        id: "antigravity",
        icon: "antigravity",
        name: "Antigravity",
        procs: procs(&["antigravity"], &[]),
        logs: &[
            spec(Home, ".gemini/antigravity/conversations", 0, &[".pb"]),
            spec(Home, ".gemini/antigravity/brain", 2, &[]),
        ],
        format: Opaque,
        app: true,
    },
    Def {
        id: "qwen",
        icon: "qwen",
        name: "Qwen Code",
        procs: procs(&["qwen"], &["@qwen-code/qwen-code", "/qwen-code/"]),
        logs: &[chats(Home, ".qwen/projects", &[".jsonl"])],
        format: Files,
        app: false,
    },
    Def {
        id: "copilot",
        icon: "copilot",
        name: "Copilot CLI",
        procs: procs(&["copilot"], &["@github/copilot"]),
        logs: &[spec(Home, ".copilot/session-state", 2, &[".jsonl"])],
        format: Files,
        app: false,
    },
    Def {
        id: "windsurf",
        icon: "windsurf",
        name: "Windsurf",
        procs: procs(&["windsurf"], &[]),
        logs: &[spec(Home, ".codeium/windsurf/cascade", 0, &[".pb"])],
        format: Opaque,
        app: true,
    },
    Def {
        id: "kiro",
        icon: "kiro",
        name: "Kiro",
        procs: procs(&["kiro", "kiro-cli"], &[]),
        logs: &[spec(
            Config,
            "Kiro/User/globalStorage/kiro.kiroagent",
            3,
            &[],
        )],
        format: Opaque,
        app: true,
    },
    Def {
        id: "goose",
        icon: "goose",
        name: "Goose",
        procs: procs(&["goose", "goosed"], &[]),
        logs: &[
            spec(Home, ".local/share/goose/sessions", 1, &[]),
            spec(Data, "Block/goose/data/sessions", 1, &[]),
        ],
        format: Opaque,
        app: false,
    },
    Def {
        id: "amp",
        icon: "amp",
        name: "Amp",
        procs: procs(&["amp"], &["@sourcegraph/amp"]),
        logs: &[spec(Home, ".local/share/amp", 2, &[])],
        format: Opaque,
        app: false,
    },
    Def {
        id: "kimi",
        icon: "kimi",
        name: "Kimi CLI",
        procs: procs(&["kimi"], &["kimi-cli", "kimi_cli"]),
        logs: &[spec(Home, ".kimi/sessions", 3, &[".jsonl"])],
        format: Files,
        app: false,
    },
    Def {
        id: "aider",
        icon: "aider",
        name: "Aider",
        procs: procs(
            &["aider"],
            &["aider-chat", "/bin/aider", "/scripts/aider", "-m aider"],
        ),
        logs: &[],
        format: Opaque,
        app: false,
    },
    Def {
        id: "crush",
        icon: "crush",
        name: "Crush",
        procs: procs(&["crush"], &[]),
        logs: &[],
        format: Opaque,
        app: false,
    },
    Def {
        id: "cline",
        icon: "cline",
        name: "Cline",
        procs: procs(&["code"], &[]),
        logs: &[spec(
            Config,
            "Code/User/globalStorage/saoudrizwan.claude-dev/tasks",
            1,
            &[".json"],
        )],
        format: Opaque,
        app: true,
    },
    Def {
        id: "roo",
        icon: "roo",
        name: "Roo Code",
        procs: procs(&["code"], &[]),
        logs: &[spec(
            Config,
            "Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks",
            1,
            &[".json"],
        )],
        format: Opaque,
        app: true,
    },
];

/// What the tail of one session file says.
#[derive(Default)]
struct FileState {
    seen: Option<Meta>,
    stop: Option<(Stop, i64)>,
    open_turn: bool,
    title: Option<String>,
}

pub struct Generic {
    def: &'static Def,
    roots: Vec<Root>,
    files: FileSet,
    state: HashMap<PathBuf, FileState>,
}

impl Generic {
    pub fn new(def: &'static Def) -> Self {
        Generic {
            def,
            roots: def.logs.iter().filter_map(LogSpec::resolve).collect(),
            files: FileSet::new(Duration::from_secs(20), Duration::from_secs(6 * 3600)),
            state: HashMap::new(),
        }
    }

    pub fn scan(
        &mut self,
        procs: &Procs,
        pids: &[u32],
        cpu: &mut CpuTracker,
        now: i64,
        out: &mut Vec<Session>,
    ) {
        if pids.is_empty() {
            self.state.clear();
            return;
        }
        let def = self.def;
        self.files.refresh(&self.roots);
        let mut busy = false;
        if !def.app {
            for &pid in pids {
                busy |= cpu.busy(procs, pid);
            }
        }
        let pid = pids.first().copied();
        let harness_row = |status: Status, last_active: i64| Session {
            key: def.id.to_string(),
            icon: def.icon,
            label: def.name.to_string(),
            title: def.name.to_string(),
            pid,
            status,
            since: None,
            last_active,
            agents: 0,
        };

        if def.format == Opaque {
            let newest = self.files.files.values().map(|m| m.mtime).max();
            let active = newest.is_some_and(|t| now - t < ACTIVE_MS);
            let status = if active || busy {
                Status::Working
            } else {
                Status::Idle
            };
            out.push(harness_row(status, newest.unwrap_or(0)));
            return;
        }

        let mut rows = Vec::new();
        for (path, m) in &self.files.files {
            let st = self.state.entry(path.clone()).or_default();
            if st.seen != Some(*m) {
                st.seen = Some(*m);
                refresh(st, path, m, def.format);
            }
            let age = now - m.mtime;
            let status = match st.stop {
                Some((Stop::Limit { until }, at)) => Status::Limit { at, until },
                Some((Stop::Network, at)) => Status::Interrupted { at },
                None if age < ACTIVE_MS || (st.open_turn && age < OPEN_TURN_MS) => Status::Working,
                None => Status::Idle,
            };
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            rows.push(Session {
                key: format!("{}:{stem}", def.id),
                icon: def.icon,
                label: def.name.to_string(),
                title: st.title.clone().unwrap_or_else(|| def.name.to_string()),
                pid,
                status,
                since: None,
                last_active: m.mtime,
                agents: 0,
            });
        }
        if busy && !rows.iter().any(|r| r.status == Status::Working) {
            match rows.iter_mut().max_by_key(|r| r.last_active) {
                Some(newest) if matches!(newest.status, Status::Idle) => {
                    newest.status = Status::Working
                }
                Some(_) => {}
                None => rows.push(harness_row(Status::Working, now)),
            }
        }
        let live: HashSet<&PathBuf> = self.files.files.keys().collect();
        self.state.retain(|p, _| live.contains(p));
        out.extend(rows);
    }
}

fn refresh(st: &mut FileState, path: &Path, m: &Meta, format: Format) {
    if st.title.is_none() {
        st.title = read_first_line(path, 64 << 10)
            .as_deref()
            .and_then(project_dir)
            .map(|cwd| base_name(&cwd));
    }
    let Some(tail) = read_tail(path, m.len, 64 << 10) else {
        return;
    };
    let is_jsonl = path.extension().is_some_and(|e| e == "jsonl");
    (st.stop, st.open_turn) = match format {
        Format::Pi => pi_state(&tail, m.mtime),
        _ if is_jsonl => (jsonl_stop(&tail, m.mtime), false),
        _ => (json_stop(&tail, m.mtime), false),
    };
}

/// Working directory recorded in a session header (`cwd`, `work_dir`, `projectPath`).
fn project_dir(line: &str) -> Option<String> {
    for key in ["\"cwd\"", "\"work_dir\"", "\"workDir\"", "\"projectPath\""] {
        let Some(i) = line.find(key) else { continue };
        let rest = line[i + key.len()..].trim_start().strip_prefix(':')?;
        let rest = rest.trim_start().strip_prefix('"')?;
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return (!out.is_empty()).then_some(out),
                '\\' => {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                }
                c => out.push(c),
            }
        }
    }
    None
}

const ERROR_MARKERS: &[&str] = &[
    "\"error\"",
    "\"type\":\"error\"",
    "\"iserror\":true",
    "\"is_error\":true",
    "\"level\":\"error\"",
    "api_error",
    "\"errormessage\"",
    "\"status\":\"error\"",
];

/// Stop reason if the last record of a JSONL transcript is an error.
fn jsonl_stop(tail: &str, at: i64) -> Option<(Stop, i64)> {
    let last = tail.lines().rev().find(|l| !l.trim().is_empty())?;
    let lower = last
        .to_lowercase()
        .replace("\"error\":null", "")
        .replace("\"error\": null", "");
    if !ERROR_MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    classify(&lower, at).map(|s| (s, at))
}

/// Stop reason if the last message of a JSON chat file (Gemini CLI) is an error notice.
fn json_stop(tail: &str, at: i64) -> Option<(Stop, i64)> {
    let lower = tail.to_lowercase();
    let i = ["\"type\": \"", "\"type\":\""]
        .iter()
        .filter_map(|p| lower.rfind(p).map(|i| i + p.len()))
        .max()?;
    let kind: String = lower[i..].chars().take_while(|c| *c != '"').collect();
    if !matches!(kind.as_str(), "error" | "warning" | "info") {
        return None;
    }
    classify(&lower[i..], at).map(|s| (s, at))
}

#[derive(Deserialize)]
struct PiLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<PiMessage>,
}

#[derive(Deserialize)]
struct PiMessage {
    role: Option<String>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
}

/// Pi records `stopReason` on every assistant message.
fn pi_state(tail: &str, at: i64) -> (Option<(Stop, i64)>, bool) {
    for line in tail.lines().rev() {
        let Ok(l) = serde_json::from_str::<PiLine>(line) else {
            continue;
        };
        if l.kind.as_deref() != Some("message") {
            continue;
        }
        let Some(m) = l.message else { continue };
        return match (m.role.as_deref(), m.stop_reason.as_deref()) {
            (Some("assistant"), Some("toolUse")) => (None, true),
            (Some("assistant"), Some("error")) => {
                let text = m.error_message.unwrap_or_default();
                (classify(&text, at).map(|s| (s, at)), false)
            }
            (Some("assistant"), _) => (None, false),
            _ => (None, true),
        };
    }
    (None, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_from_header() {
        let line = r#"{"type":"session","id":"x","cwd":"C:\\Users\\me\\proj","timestamp":1}"#;
        assert_eq!(project_dir(line).as_deref(), Some("C:\\Users\\me\\proj"));
        assert_eq!(
            project_dir(r#"{"cwd": "/home/me/app"}"#).as_deref(),
            Some("/home/me/app")
        );
        assert_eq!(project_dir(r#"{"x":1}"#), None);
    }

    #[test]
    fn jsonl_error_tail() {
        let tail = "{\"type\":\"user\"}\n{\"type\":\"system\",\"subtype\":\"ui_telemetry\",\"event.name\":\"qwen-code.api_error\",\"error\":\"429 Too Many Requests\"}\n";
        assert!(matches!(jsonl_stop(tail, 0), Some((Stop::Limit { .. }, 0))));
        let ok = "{\"type\":\"assistant\",\"error\":null,\"text\":\"fixed the connection pool\"}\n";
        assert_eq!(jsonl_stop(ok, 0), None);
    }

    #[test]
    fn gemini_json_tail() {
        let tail = r#"{"type": "gemini", "content": "ok"}, {"id": "9", "type": "error", "content": "Quota exceeded for quota metric 'Gemini 2.5 Pro Requests'"}]}"#;
        assert!(matches!(json_stop(tail, 0), Some((Stop::Limit { .. }, 0))));
        let fine =
            r#"{"type": "error", "content": "x"}, {"type": "gemini", "content": "network code"}]}"#;
        assert_eq!(json_stop(fine, 0), None);
    }

    #[test]
    fn pi_stop_reasons() {
        let tool = r#"{"type":"message","message":{"role":"assistant","stopReason":"toolUse"}}"#;
        assert_eq!(pi_state(tool, 0), (None, true));
        let done = r#"{"type":"message","message":{"role":"assistant","stopReason":"stop"}}"#;
        assert_eq!(pi_state(done, 0), (None, false));
        let err = r#"{"type":"message","message":{"role":"assistant","stopReason":"error","errorMessage":"fetch failed"}}"#;
        assert_eq!(pi_state(err, 7), (Some((Stop::Network, 7)), false));
        let result = r#"{"type":"message","message":{"role":"toolResult"}}"#;
        assert_eq!(pi_state(result, 0), (None, true));
    }
}
