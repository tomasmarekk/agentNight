//! Finds agent sessions and decides whether each one is still working.

mod claude;
mod codex;
mod generic;

use std::collections::HashMap;

use crate::procs::Procs;

/// A session counts as working while its log keeps changing this recently.
pub const ACTIVE_MS: i64 = 90_000;
/// A turn that is still open (agent running a long tool call) counts as working while
/// its log was written within this window.
pub const OPEN_TURN_MS: i64 = 3 * 3_600_000;
/// Process-tree CPU (percent of one core) above which an agent counts as busy.
const BUSY_CPU: f32 = 15.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Working,
    /// An agent goal (Codex `/goal`) is still active and will continue on its own.
    Goal,
    /// Stopped by a usage/rate limit at `at`; `until` is the announced reset.
    Limit {
        at: i64,
        until: Option<i64>,
    },
    /// Stopped by a connection or transient API failure at `at`.
    Interrupted {
        at: i64,
    },
    /// Waiting for the user (permission prompt, question).
    Input,
    Idle,
}

impl Status {
    /// Ordering used when merging subagents into their parent row.
    pub fn rank(&self) -> u8 {
        match self {
            Status::Working => 5,
            Status::Goal => 4,
            Status::Limit { .. } => 3,
            Status::Interrupted { .. } => 2,
            Status::Input => 1,
            Status::Idle => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub key: String,
    /// Icon id used by the UI.
    pub icon: &'static str,
    /// Harness name, e.g. "Claude Code" or "Codex App".
    pub label: String,
    /// Session or project name.
    pub title: String,
    pub pid: Option<u32>,
    pub status: Status,
    /// When the current status began, if the harness records it (epoch ms).
    pub since: Option<i64>,
    /// Last observed activity (epoch ms).
    pub last_active: i64,
    /// Number of subagents folded into this row.
    pub agents: u32,
}

/// Which processes belong to a harness.
#[derive(Clone, Copy, Debug)]
pub struct ProcMatch {
    /// Exact executable names (lowercase, without `.exe`).
    pub names: &'static [&'static str],
    /// Command line fragments for agents that run inside an interpreter.
    pub cmds: &'static [&'static str],
}

const INTERPRETERS: &[&str] = &[
    "node", "nodejs", "bun", "deno", "python", "python3", "pythonw", "py", "uv", "uvx",
];

/// Running processes per matcher; only the top-most matching process of each tree is
/// kept so CPU is not counted twice.
pub fn match_processes(procs: &Procs, matchers: &[ProcMatch]) -> Vec<Vec<u32>> {
    let mut hits: Vec<Vec<u32>> = vec![Vec::new(); matchers.len()];
    let mut parent_of: HashMap<u32, Option<u32>> = HashMap::new();
    for p in procs.iter() {
        let interpreter = INTERPRETERS.contains(&p.name.as_str());
        let cmd = if interpreter { Some(p.cmd()) } else { None };
        for (i, m) in matchers.iter().enumerate() {
            let by_name = m.names.contains(&p.name.as_str());
            let by_cmd = cmd
                .as_deref()
                .is_some_and(|c| m.cmds.iter().any(|frag| c.contains(frag)));
            if by_name || by_cmd {
                hits[i].push(p.pid);
                parent_of.insert(p.pid, p.parent());
            }
        }
    }
    for pids in &mut hits {
        let set: Vec<u32> = pids.clone();
        pids.retain(|pid| {
            !parent_of
                .get(pid)
                .copied()
                .flatten()
                .is_some_and(|parent| set.contains(&parent))
        });
        pids.sort_unstable();
    }
    hits
}

/// Tracks CPU load of the process trees detectors ask about. Only those trees are
/// sampled (sampling every process is the expensive part of a scan), and only load
/// sustained over two scans counts.
#[derive(Default)]
pub struct CpuTracker {
    prev: HashMap<u32, f32>,
    next: HashMap<u32, f32>,
}

impl CpuTracker {
    pub fn busy(&mut self, procs: &Procs, pid: u32) -> bool {
        let now = procs.tree_cpu(pid);
        let before = self.prev.get(&pid).copied().unwrap_or(0.0);
        self.next.insert(pid, now);
        now > BUSY_CPU && before > BUSY_CPU
    }

    /// Processes asked about in the previous round, to be sampled in this one.
    fn wanted(&self) -> Vec<u32> {
        self.prev.keys().copied().collect()
    }

    fn end_round(&mut self) {
        self.prev = std::mem::take(&mut self.next);
    }
}

pub struct Detector {
    claude: claude::Claude,
    codex: codex::Codex,
    generic: Vec<generic::Generic>,
    cpu: CpuTracker,
}

impl Detector {
    pub fn new() -> Self {
        Detector {
            claude: claude::Claude::new(),
            codex: codex::Codex::new(),
            generic: generic::DEFS.iter().map(generic::Generic::new).collect(),
            cpu: CpuTracker::default(),
        }
    }

    pub fn scan(&mut self, procs: &mut Procs, now: i64) -> Vec<Session> {
        procs.refresh();
        procs.refresh_cpu(&self.cpu.wanted());
        let procs = &*procs;
        let mut matchers = vec![claude::PROCS, codex::PROCS];
        matchers.extend(generic::DEFS.iter().map(|d| d.procs));
        let running = match_processes(procs, &matchers);

        let mut out = Vec::new();
        self.claude
            .scan(procs, &running[0], &mut self.cpu, now, &mut out);
        self.codex.scan(&running[1], now, &mut out);
        for (g, pids) in self.generic.iter_mut().zip(&running[2..]) {
            g.scan(procs, pids, &mut self.cpu, now, &mut out);
        }
        self.cpu.end_round();
        out
    }
}

/// Parses an RFC 3339 timestamp into epoch milliseconds.
pub fn parse_ts(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp_millis())
}
