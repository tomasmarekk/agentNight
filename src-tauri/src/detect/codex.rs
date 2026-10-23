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
        let mut threads = Vec::new();
        for (path, m) in &self.files.files {
            let r = self.rollouts.entry(path.clone()).or_default();
            r.update(path, m.len);
            let Some(meta) = r.meta.clone() else { continue };
            threads.push(Thread {
                meta,
                mtime: m.mtime,
                open_turn: r.open_turn,
                turn_started: r.turn_started,
                goal: r.goal.clone(),
                stop: r.stop,
            });
        }
        let live: HashSet<&PathBuf> = self.files.files.keys().collect();
        self.rollouts.retain(|p, _| live.contains(p));
        for tree in group(threads) {
            out.push(tree_session(&tree, running.first().copied(), now));
        }
    }
}

/// What one rollout (one thread) says as of this scan.
#[derive(Clone)]
struct Thread {
    meta: Meta,
    mtime: i64,
    open_turn: bool,
    turn_started: Option<i64>,
    goal: Option<String>,
    stop: Option<(Stop, i64)>,
}

/// Groups subagent threads (at any depth) under their top-level thread, which comes
/// first in each group.
fn group(threads: Vec<Thread>) -> Vec<Vec<Thread>> {
    let ids: HashSet<String> = threads.iter().map(|t| t.meta.id.clone()).collect();
    let parent_of: HashMap<String, String> = threads
        .iter()
        .filter_map(|t| Some((t.meta.id.clone(), t.meta.parent.clone()?)))
        .collect();
    let top = |id: &str| {
        let mut cur = id.to_string();
        for _ in 0..16 {
            match parent_of.get(&cur) {
                Some(p) if ids.contains(p) && *p != cur => cur = p.clone(),
                _ => break,
            }
        }
        cur
    };
    let mut groups: HashMap<String, Vec<Thread>> = HashMap::new();
    for t in threads {
        let root = top(&t.meta.id);
        let group = groups.entry(root.clone()).or_default();
        if t.meta.id == root {
            group.insert(0, t);
        } else {
            group.push(t);
        }
    }
    groups.into_values().collect()
}

/// One row per task: the top-level thread plus its subagents, judged together. A main
/// agent waiting for its subagents writes nothing for hours, so an open turn counts as
/// long as anything in the tree was active recently.
fn tree_session(tree: &[Thread], pid: Option<u32>, now: i64) -> Session {
    let root = &tree[0];
    let last_active = tree.iter().map(|t| t.mtime).max().unwrap_or(root.mtime);
    let quiet = now - last_active;
    let active = |t: &Thread, is_root: bool| {
        let open_for = if is_root { quiet } else { now - t.mtime };
        now - t.mtime < ACTIVE_MS || (t.open_turn && open_for < OPEN_TURN_MS)
    };
    let agents = tree
        .iter()
        .enumerate()
        .filter(|(i, t)| active(t, *i == 0))
        .count() as u32;
    let status = if agents > 0 {
        Status::Working
    } else if root.goal.as_deref() == Some("usage_limited") {
        Status::Limit {
            at: last_active,
            until: None,
        }
    } else if let Some((stop, at)) = root.stop {
        match stop {
            Stop::Limit { until } => Status::Limit { at, until },
            Stop::Network => Status::Interrupted { at },
        }
    } else if root.goal.as_deref() == Some("active") && quiet < GOAL_IDLE_MS {
        Status::Goal
    } else {
        Status::Idle
    };
    Session {
        key: format!("codex:{}", root.meta.id),
        icon: "codex",
        label: if root.meta.app {
            "Codex App"
        } else {
            "Codex CLI"
        }
        .into(),
        title: base_name(&root.meta.cwd),
        pid,
        since: root.turn_started.filter(|_| root.open_turn && agents > 0),
        status,
        last_active,
        agents: if agents > 1 { agents } else { 0 },
    }
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

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_000 * 60 * MIN;

    /// A thread whose log was last written `ago` ms before `NOW`.
    fn thread(id: &str, parent: Option<&str>, ago: i64, open_turn: bool) -> Thread {
        Thread {
            meta: Meta {
                id: id.into(),
                parent: parent.map(Into::into),
                cwd: "C:\\work\\rootlight".into(),
                app: true,
            },
            mtime: NOW - ago,
            open_turn,
            turn_started: Some(NOW - 8 * 60 * MIN),
            goal: None,
            stop: None,
        }
    }

    fn sessions(threads: Vec<Thread>) -> Vec<Session> {
        let mut out: Vec<Session> = group(threads)
            .iter()
            .map(|tree| tree_session(tree, None, NOW))
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// The real case: a main agent waiting on its subagents for hours (its own log is
    /// old), three subagents working, many finished earlier.
    #[test]
    fn counts_only_agents_working_now() {
        let mut threads = vec![
            thread("main", None, 472 * MIN, true),
            thread("w1", Some("main"), MIN / 5, true),
            thread("w2", Some("main"), MIN / 2, true),
            thread("w3", Some("main"), 2 * MIN, true),
            thread("done-recently", Some("main"), 2 * MIN, false),
        ];
        for i in 0..14 {
            threads.push(thread(
                &format!("old{i}"),
                Some("main"),
                (60 + i * 30) * MIN,
                false,
            ));
        }
        let out = sessions(threads);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "codex:main");
        assert_eq!(out[0].status, Status::Working);
        assert_eq!(out[0].agents, 4);
        assert_eq!(out[0].title, "rootlight");
    }

    #[test]
    fn main_agent_waiting_on_a_long_subagent_command_still_works() {
        let out = sessions(vec![
            thread("main", None, 300 * MIN, true),
            thread("w1", Some("main"), 40 * MIN, true),
        ]);
        assert_eq!(out[0].status, Status::Working);
        assert_eq!(out[0].agents, 2);
    }

    #[test]
    fn abandoned_open_turns_do_not_count() {
        let out = sessions(vec![
            thread("main", None, 5 * 60 * MIN, true),
            thread("w1", Some("main"), 4 * 60 * MIN, true),
        ]);
        assert_eq!(out[0].status, Status::Idle);
        assert_eq!(out[0].agents, 0);
    }

    #[test]
    fn single_agent_shows_no_count_and_nested_subagents_fold() {
        let out = sessions(vec![
            thread("main", None, MIN / 2, true),
            thread("child", Some("main"), 30 * MIN, false),
            thread("grandchild", Some("child"), 3 * MIN, false),
            thread("orphan", Some("missing"), 10 * MIN, false),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].key, "codex:main");
        assert_eq!(out[0].status, Status::Working);
        assert_eq!(out[0].agents, 0);
        assert_eq!(out[1].key, "codex:orphan");
        assert_eq!(out[1].status, Status::Idle);
    }

    #[test]
    fn active_goal_holds_after_the_turn_ends() {
        let mut main = thread("main", None, 10 * MIN, false);
        main.goal = Some("active".into());
        assert_eq!(sessions(vec![main.clone()])[0].status, Status::Goal);
        main.mtime = NOW - 45 * MIN;
        assert_eq!(sessions(vec![main])[0].status, Status::Idle);
    }
}
