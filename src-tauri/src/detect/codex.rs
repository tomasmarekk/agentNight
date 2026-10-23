//! Codex CLI and the Codex desktop app. Both write rollouts to
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` with explicit turn and goal events.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::{parse_ts, ProcMatch, Session, Status, ACTIVE_MS, OPEN_TURN_MS};
use crate::classify::{classify, Stop};
use crate::logs::{base_name, read_first_line, FileSet, Root, Tail};

pub const PROCS: ProcMatch = ProcMatch {
    names: &["codex"],
    cmds: &["@openai/codex"],
};

/// An active goal keeps the session blocking this long after its last write; Codex
/// continues active goals on its own, so a longer silence means it is stuck.
const GOAL_IDLE_MS: i64 = 30 * 60_000;
const INITIAL_TAIL: u64 = 8 << 20;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    started_at: Option<i64>,
    goal: Option<Goal>,
    message: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Goal {
    status: Option<String>,
}

#[derive(Default)]
struct Rollout {
    tail: Tail,
    meta: Option<Meta>,
    open_turn: bool,
    turn_started: Option<i64>,
    goal: Option<String>,
    stop: Option<(Stop, i64)>,
}

#[derive(Clone)]
struct Meta {
    id: String,
    parent: Option<String>,
    cwd: String,
    app: bool,
}

impl Rollout {
    fn update(&mut self, path: &Path, len: u64) {
        if self.meta.is_none() {
            self.meta = read_meta(path);
        }
        let Rollout {
            tail,
            open_turn,
            turn_started,
            goal,
            stop,
            ..
        } = self;
        tail.read(path, len, INITIAL_TAIL, |raw| {
            let head = &raw.as_bytes()[..raw.len().min(200)];
            if !contains(head, b"\"event_msg\"") {
                return;
            }
            let Ok(line) = serde_json::from_str::<Line>(raw) else {
                return;
            };
            if line.kind != "event_msg" {
                return;
            }
            let Some(p) = line.payload else { return };
            let at = line
                .timestamp
                .as_deref()
                .and_then(parse_ts)
                .unwrap_or_else(crate::logs::now_ms);
            match p.kind.as_deref() {
                Some("task_started") => {
                    *open_turn = true;
                    *turn_started = Some(p.started_at.map(|s| s * 1000).unwrap_or(at));
                    *stop = None;
                }
                Some("task_complete" | "turn_aborted") => *open_turn = false,
                Some("thread_goal_updated") => {
                    *goal = p.goal.and_then(|g| g.status);
                }
                Some("error") => {
                    let text = p
                        .message
                        .as_ref()
                        .map(|m| m.as_str().map(str::to_string).unwrap_or(m.to_string()))
                        .unwrap_or_default();
                    *stop = classify(&text, at).map(|s| (s, at));
                }
                _ => {}
            }
        });
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn read_meta(path: &Path) -> Option<Meta> {
    let first = read_first_line(path, 4 << 20)?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    let p = v.get("payload")?;
    let s = |k: &str| p.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let parent = s("parent_thread_id").or_else(|| {
        p.pointer("/source/subagent/thread_spawn/parent_thread_id")
            .and_then(|x| x.as_str())
            .map(str::to_string)
    });
    Some(Meta {
        id: s("id")?,
        parent,
        cwd: s("cwd").unwrap_or_default(),
        app: s("originator").is_some_and(|o| o.to_lowercase().contains("desktop")),
    })
}

pub struct Codex {
    roots: Vec<Root>,
    files: FileSet,
    rollouts: HashMap<PathBuf, Rollout>,
}

impl Codex {
    pub fn new() -> Self {
        let home = std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| crate::logs::home_dir().map(|h| h.join(".codex")));
        let roots = home
            .map(|h| {
                vec![Root {
                    dir: h.join("sessions"),
                    depth: 3,
                    suffixes: &[".jsonl"],
                    parent: None,
                }]
            })
            .unwrap_or_default();
        Codex {
            roots,
            files: FileSet::new(Duration::from_secs(15), Duration::from_secs(24 * 3600)),
            rollouts: HashMap::new(),
        }
    }

    pub fn scan(&mut self, running: &[u32], now: i64, out: &mut Vec<Session>) {
        if running.is_empty() {
            self.rollouts.clear();
            return;
        }
        self.files.refresh(&self.roots);
        let mut rows: Vec<(Option<String>, Option<String>, Session)> = Vec::new();
        for (path, m) in &self.files.files {
            let r = self.rollouts.entry(path.clone()).or_default();
            r.update(path, m.len);
            let Some(meta) = r.meta.clone() else { continue };
            let age = now - m.mtime;
            let status = if age < ACTIVE_MS || (r.open_turn && age < OPEN_TURN_MS) {
                Status::Working
            } else if r.goal.as_deref() == Some("usage_limited") {
                Status::Limit {
                    at: m.mtime,
                    until: None,
                }
            } else if let Some((stop, at)) = r.stop {
                match stop {
                    Stop::Limit { until } => Status::Limit { at, until },
                    Stop::Network => Status::Interrupted { at },
                }
            } else if r.goal.as_deref() == Some("active") && age < GOAL_IDLE_MS {
                Status::Goal
            } else {
                Status::Idle
            };
            let since = match status {
                Status::Working => r.turn_started.filter(|_| r.open_turn),
                _ => None,
            };
            let session = Session {
                key: format!("codex:{}", meta.id),
                icon: "codex",
                label: if meta.app { "Codex App" } else { "Codex CLI" }.into(),
                title: base_name(&meta.cwd),
                pid: running.first().copied(),
                status,
                since,
                last_active: m.mtime,
                agents: 0,
            };
            rows.push((Some(meta.id), meta.parent, session));
        }
        let live: HashSet<&PathBuf> = self.files.files.keys().collect();
        self.rollouts.retain(|p, _| live.contains(p));
        out.extend(fold_subagents(rows));
    }
}

/// Folds subagent threads (at any depth) into their top-level thread's row so one task
/// shows as one session.
fn fold_subagents(rows: Vec<(Option<String>, Option<String>, Session)>) -> Vec<Session> {
    let ids: HashSet<String> = rows.iter().filter_map(|(id, _, _)| id.clone()).collect();
    let parent_of: HashMap<String, String> = rows
        .iter()
        .filter_map(|(id, parent, _)| Some((id.clone()?, parent.clone()?)))
        .collect();
    let top = |id: &str| {
        let mut cur = id.to_string();
        for _ in 0..16 {
            match parent_of.get(&cur) {
                Some(p) if ids.contains(p) => cur = p.clone(),
                _ => break,
            }
        }
        cur
    };
    let mut tops: HashMap<String, Session> = HashMap::new();
    let mut children = Vec::new();
    let mut standalone = Vec::new();
    for (id, _, s) in rows {
        match id {
            Some(id) => {
                let t = top(&id);
                if t == id {
                    tops.insert(id, s);
                } else {
                    children.push((t, s));
                }
            }
            None => standalone.push(s),
        }
    }
    for (t, child) in children {
        let Some(p) = tops.get_mut(&t) else {
            standalone.push(child);
            continue;
        };
        p.agents += 1;
        p.last_active = p.last_active.max(child.last_active);
        if child.status.rank() > p.status.rank() {
            p.status = child.status;
            p.since = child.since;
        }
    }
    standalone.extend(tops.into_values());
    standalone
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::meta;

    fn rollout(lines: &[&str]) -> Rollout {
        let path = crate::logs::tests::tmp("codex.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut r = Rollout::default();
        r.update(&path, meta(&path).unwrap().len);
        std::fs::remove_file(&path).ok();
        r
    }

    const META: &str = r#"{"timestamp":"2026-10-23T07:35:59.402Z","type":"session_meta","payload":{"id":"t1","cwd":"C:\\work\\rootlight","originator":"Codex Desktop"}}"#;

    #[test]
    fn open_turn_and_meta() {
        let r = rollout(&[
            META,
            r#"{"timestamp":"2026-10-23T07:36:00.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"a","started_at":1792751760}}"#,
            r#"{"timestamp":"2026-10-23T07:36:01.000Z","type":"response_item","payload":{"type":"reasoning"}}"#,
        ]);
        assert!(r.open_turn);
        assert_eq!(r.turn_started, Some(1_792_751_760_000));
        let meta = r.meta.unwrap();
        assert_eq!(meta.id, "t1");
        assert!(meta.app);
        assert_eq!(base_name(&meta.cwd), "rootlight");
    }

    #[test]
    fn completed_turn_with_active_goal() {
        let r = rollout(&[
            META,
            r#"{"timestamp":"2026-10-23T07:36:00.000Z","type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"active"}}}"#,
            r#"{"timestamp":"2026-10-23T07:36:00.000Z","type":"event_msg","payload":{"type":"task_started","started_at":1}}"#,
            r#"{"timestamp":"2026-10-23T07:40:00.000Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}"#,
        ]);
        assert!(!r.open_turn);
        assert_eq!(r.goal.as_deref(), Some("active"));
    }

    #[test]
    fn usage_limit_error() {
        let r = rollout(&[
            META,
            r#"{"timestamp":"2026-10-23T07:36:00.000Z","type":"event_msg","payload":{"type":"error","message":"You've hit your usage limit. Try again in 2 hours."}}"#,
        ]);
        assert!(matches!(r.stop, Some((Stop::Limit { until: Some(_) }, _))));
    }

    fn row(key: &str, status: Status) -> Session {
        Session {
            key: key.into(),
            icon: "codex",
            label: "Codex App".into(),
            title: "p".into(),
            pid: None,
            status,
            since: None,
            last_active: 0,
            agents: 0,
        }
    }

    #[test]
    fn subagents_fold_into_parent() {
        let rows = vec![
            (Some("p".into()), None, row("p", Status::Idle)),
            (
                Some("c1".into()),
                Some("p".into()),
                row("c1", Status::Working),
            ),
            (Some("c2".into()), Some("p".into()), row("c2", Status::Idle)),
            (Some("g".into()), Some("c2".into()), row("g", Status::Idle)),
            (
                Some("x".into()),
                Some("missing".into()),
                row("x", Status::Idle),
            ),
        ];
        let mut out = fold_subagents(rows);
        out.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].key, "p");
        assert_eq!(out[0].agents, 3);
        assert_eq!(out[0].status, Status::Working);
        assert_eq!(out[1].key, "x");
    }
}
