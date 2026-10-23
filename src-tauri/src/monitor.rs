//! Background loop: scans sessions, runs the shutdown policy and publishes snapshots.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;
use crate::detect::{Detector, Session, Status};
use crate::logs::now_ms;
use crate::net::NetMonitor;
use crate::policy::{Phase, Policy};
use crate::power::{self, KeepAwake};
use crate::procs::Procs;

/// Finished sessions stay listed this long.
const RECENT_MS: i64 = 15 * 60_000;
/// A limit with an announced reset holds until the reset plus this slack, giving the
/// harness time to resume on its own.
const LIMIT_SLACK_MS: i64 = 20 * 60_000;
/// Hold for limits without an announced reset (a 5-hour window plus slack).
const LIMIT_DEFAULT_MS: i64 = 5 * 3_600_000 + LIMIT_SLACK_MS;
/// Never hold longer than this for a limit (weekly limits will not lift overnight).
const LIMIT_MAX_MS: i64 = 6 * 3_600_000;
/// Connection errors from shortly before arming still count.
const ARM_SLACK_MS: i64 = 5 * 60_000;

/// `agentnight --scan`: prints the sessions agentNight sees right now, for
/// troubleshooting detection.
pub fn print_scan() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        // SAFETY: attaches to the parent console so release builds can print.
        unsafe {
            AttachConsole(u32::MAX);
        }
    }
    let mut m = Monitor::new(Config::from_env());
    // CPU load only counts when sustained over a few samples.
    for _ in 0..4 {
        m.scan();
        std::thread::sleep(Duration::from_millis(1500));
    }
    m.scan();
    println!(
        "{}",
        serde_json::to_string_pretty(&m.rows).unwrap_or_default()
    );
}

pub enum Cmd {
    Arm(bool),
    Visible(bool),
}

#[derive(Serialize, Clone, PartialEq, Default, Debug)]
pub struct Snapshot {
    pub armed: bool,
    pub can_arm: bool,
    pub phase: Phase,
    /// Epoch ms when the settling period or countdown ends.
    pub deadline: Option<i64>,
    /// Sessions keeping the computer on.
    pub active: usize,
    pub sessions: Vec<Row>,
    pub dry_run: bool,
    pub notice: Option<String>,
}

#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct Row {
    pub key: String,
    pub icon: &'static str,
    pub label: String,
    pub title: String,
    pub pid: Option<u32>,
    /// working | goal | limit | offline | input | error | done
    pub state: &'static str,
    pub blocking: bool,
    /// Epoch ms when the current state began.
    pub since: i64,
    /// Epoch ms when a limit is expected to lift.
    pub until: Option<i64>,
    pub agents: u32,
}

pub struct Monitor {
    cfg: Config,
    procs: Procs,
    detector: Detector,
    policy: Policy,
    net: NetMonitor,
    awake: KeepAwake,
    seen: HashMap<String, (&'static str, i64)>,
    armed_at: Option<i64>,
    visible: bool,
    rows: Vec<Row>,
    notice: Option<(String, Instant)>,
    deadline: Option<(Phase, i64)>,
    shutdown: fn() -> Result<(), String>,
}

impl Monitor {
    pub fn new(cfg: Config) -> Self {
        Self::with_shutdown(cfg, power::shutdown)
    }

    fn with_shutdown(cfg: Config, shutdown: fn() -> Result<(), String>) -> Self {
        Monitor {
            policy: Policy::new(&cfg),
            cfg,
            procs: Procs::new(),
            detector: Detector::new(),
            net: NetMonitor::start(),
            awake: KeepAwake::default(),
            seen: HashMap::new(),
            armed_at: None,
            visible: false,
            rows: Vec::new(),
            notice: None,
            deadline: None,
            shutdown,
        }
    }

    /// Runs until the command channel closes. `publish` receives every changed snapshot.
    pub fn run(mut self, rx: Receiver<Cmd>, mut publish: impl FnMut(&Snapshot)) {
        let mut last = Snapshot::default();
        let mut scanned: Option<Instant> = None;
        loop {
            let armed = self.policy.armed();
            let scan_every = if armed || self.visible {
                Duration::from_secs(3)
            } else {
                Duration::from_secs(10)
            };
            let wait = if armed {
                Duration::from_secs(1)
            } else {
                scan_every
            };
            let mut force = false;
            match rx.recv_timeout(wait) {
                Ok(Cmd::Arm(on)) => {
                    self.set_armed(on);
                    force = true;
                }
                Ok(Cmd::Visible(v)) => {
                    self.visible = v;
                    force = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if force || scanned.is_none_or(|t| t.elapsed() >= scan_every) {
                scanned = Some(Instant::now());
                self.scan();
            }
            let snapshot = self.step();
            if snapshot != last {
                publish(&snapshot);
                last = snapshot;
            }
        }
        self.awake.set(false);
    }

    fn set_armed(&mut self, on: bool) {
        if on && !self.policy.armed() && !self.rows.iter().any(|r| r.blocking) {
            self.notify("Nothing is running yet");
            return;
        }
        self.policy.set_armed(on);
        self.armed_at = on.then(now_ms);
        self.net.set_active(on);
        self.awake.set(on);
        if on {
            power::request_permission();
        }
        self.notice = None;
    }

    fn notify(&mut self, text: &str) {
        self.notice = Some((text.to_string(), Instant::now()));
    }

    fn scan(&mut self) {
        let now = now_ms();
        let sessions = self.detector.scan(&mut self.procs, now);
        self.rows = self.rows(sessions, now);
    }

    fn rows(&mut self, sessions: Vec<Session>, now: i64) -> Vec<Row> {
        let mut seen = HashMap::new();
        let mut rows = Vec::new();
        for s in sessions {
            let (state, blocking, until, at) = match s.status {
                Status::Working => ("working", true, None, s.since),
                Status::Goal => ("goal", true, None, None),
                Status::Limit { at, until } => {
                    let hold = until
                        .map(|u| u + LIMIT_SLACK_MS)
                        .unwrap_or(at + LIMIT_DEFAULT_MS)
                        .min(at + LIMIT_MAX_MS);
                    if now < hold {
                        ("limit", true, until, Some(at))
                    } else {
                        ("done", false, None, Some(at))
                    }
                }
                Status::Interrupted { at } => {
                    let after_arming = self.armed_at.is_some_and(|a| at >= a - ARM_SLACK_MS);
                    if after_arming {
                        ("offline", true, None, Some(at))
                    } else {
                        ("error", false, None, Some(at))
                    }
                }
                Status::Input => ("input", false, None, s.since),
                Status::Idle => ("done", false, None, Some(s.last_active)),
            };
            let since = match self.seen.get(&s.key) {
                Some((prev, since)) if *prev == state => *since,
                _ => at.map_or(now, |t| t.min(now)),
            };
            seen.insert(s.key.clone(), (state, since));
            let visible = blocking || state == "input" || now - since < RECENT_MS;
            if !visible {
                continue;
            }
            rows.push(Row {
                key: s.key,
                icon: s.icon,
                label: s.label,
                title: s.title,
                pid: s.pid,
                state,
                blocking,
                since,
                until,
                agents: s.agents,
            });
        }
        self.seen = seen;
        rows.sort_by_key(|r| {
            let group = match r.state {
                "working" | "goal" => 0,
                "limit" | "offline" => 1,
                "input" => 2,
                _ => 3,
            };
            (group, if group == 3 { -r.since } else { r.since })
        });
        rows
    }

    fn step(&mut self) -> Snapshot {
        let busy = self.rows.iter().any(|r| r.blocking);
        let tick = self.policy.tick(Instant::now(), busy, self.net.status());
        if tick.fire {
            self.fire();
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(8))
        {
            self.notice = None;
        }
        let armed = self.policy.armed();
        let phase = if armed { tick.phase } else { Phase::Off };
        // Keep the deadline stable between ticks so unchanged snapshots are not re-sent.
        self.deadline = match tick.remaining.filter(|_| armed) {
            Some(r) => {
                let fresh = now_ms() + r.as_millis() as i64;
                match self.deadline {
                    Some((p, d)) if p == phase && (d - fresh).abs() < 1500 => Some((p, d)),
                    _ => Some((phase, fresh)),
                }
            }
            None => None,
        };
        Snapshot {
            armed,
            can_arm: armed || busy,
            phase,
            deadline: self.deadline.map(|(_, d)| d),
            active: self.rows.iter().filter(|r| r.blocking).count(),
            sessions: self.rows.clone(),
            dry_run: self.cfg.dry_run,
            notice: self.notice.as_ref().map(|(t, _)| t.clone()),
        }
    }

    fn fire(&mut self) {
        self.set_armed(false);
        if self.cfg.dry_run {
            eprintln!("agentNight: dry run, not shutting down");
            self.notify("Dry run: the computer would shut down now");
            return;
        }
        if let Err(e) = (self.shutdown)() {
            eprintln!("agentNight: shutdown failed: {e}");
            self.notify(&format!("Shutdown failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};

    use serde_json::{json, Value};

    static SHUTDOWNS: AtomicUsize = AtomicUsize::new(0);

    fn record_shutdown() -> Result<(), String> {
        SHUTDOWNS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Body of the stand-in agent process started by `fake_agent`.
    #[test]
    #[ignore = "helper process for end_to_end_night"]
    fn fake_agent_body() {
        if std::env::var_os("AGENTNIGHT_FAKE_AGENT").is_some() {
            std::thread::sleep(Duration::from_secs(300));
        }
    }

    /// Runs this test binary again as a process named `claude`, so detection, PID and
    /// start-time checks see what they would see for a real Claude Code process.
    fn fake_agent(dir: &Path) -> Child {
        let exe = dir.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        Command::new(&exe)
            .args(["--ignored", "--exact", "monitor::tests::fake_agent_body"])
            .env("AGENTNIGHT_FAKE_AGENT", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The session files Claude Code would write for the stand-in agent.
    struct Session {
        home: PathBuf,
        pid: u32,
        started: i64,
    }

    impl Session {
        fn status(&self, status: &str) {
            let dir = self.home.join(".claude").join("sessions");
            std::fs::create_dir_all(&dir).unwrap();
            let registry = json!({
                "pid": self.pid, "sessionId": "e2e", "cwd": self.home.join("demo"),
                "startedAt": self.started, "status": status, "statusUpdatedAt": now_ms(),
                "name": "Overnight refactor", "entrypoint": "cli",
            });
            std::fs::write(dir.join(format!("{}.json", self.pid)), registry.to_string()).unwrap();
        }

        fn record(&self, mut record: Value) {
            record["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
            let dir = self.home.join(".claude").join("projects").join("demo");
            std::fs::create_dir_all(&dir).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("e2e.jsonl"))
                .unwrap();
            writeln!(file, "{record}").unwrap();
        }

        fn api_error(&self, code: &str, text: &str) {
            self.record(json!({
                "type": "assistant", "isApiErrorMessage": true, "error": code,
                "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            }));
        }

        fn resume(&self) {
            self.record(
                json!({"type": "user", "message": {"role": "user", "content": "continue"}}),
            );
            self.status("busy");
        }
    }

    fn state(s: &Snapshot) -> Option<&'static str> {
        s.sessions
            .iter()
            .find(|r| r.key.starts_with("claude:e2e:"))
            .map(|r| r.state)
    }

    fn wait_for(out: &Receiver<Snapshot>, what: &str, secs: u64, ok: impl Fn(&Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut last = None;
        while Instant::now() < deadline {
            if let Ok(s) = out.recv_timeout(Duration::from_millis(200)) {
                if ok(&s) {
                    return;
                }
                last = Some(s);
            }
        }
        panic!("timed out waiting for {what}; last snapshot: {last:#?}");
    }

    /// Every snapshot for `secs` seconds keeps the shutdown armed and waiting.
    fn holds(out: &Receiver<Snapshot>, what: &str, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(s) = out.recv_timeout(Duration::from_millis(200)) {
                assert!(s.armed && s.phase == Phase::Watching, "{what}: {s:#?}");
            }
        }
        assert_eq!(SHUTDOWNS.load(Ordering::SeqCst), 0, "{what}: shut down");
    }

    /// A whole night: the agent works, hits a usage limit, resumes, loses its
    /// connection, resumes again and finishes. Only the last finish may power off.
    /// Needs internet access (shutdown waits for a working connection).
    #[test]
    #[ignore = "end-to-end, takes about a minute and needs internet; CI runs it"]
    fn end_to_end_night() {
        let home = crate::logs::tests::tmp("e2e-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("AGENTNIGHT_HOME", &home);
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CODEX_HOME");

        let mut agent = fake_agent(&home);
        let session = Session {
            home: home.clone(),
            pid: agent.id(),
            started: now_ms(),
        };
        session.status("busy");

        let cfg = Config {
            grace: Duration::from_secs(2),
            online_grace: Duration::from_secs(1),
            countdown: Duration::from_secs(2),
            dry_run: false,
        };
        let (tx, rx) = mpsc::channel();
        let (out_tx, out) = mpsc::channel();
        let monitor = std::thread::spawn(move || {
            Monitor::with_shutdown(cfg, record_shutdown).run(rx, |s| {
                let _ = out_tx.send(s.clone());
            })
        });
        tx.send(Cmd::Visible(true)).unwrap();

        wait_for(&out, "the working session", 20, |s| {
            s.can_arm && state(s) == Some("working")
        });
        tx.send(Cmd::Arm(true)).unwrap();
        wait_for(&out, "arming", 10, |s| {
            s.armed && s.phase == Phase::Watching
        });

        // Usage limit: the agent stops, the computer must stay on.
        session.api_error(
            "rate_limit",
            "You've hit your session limit · resets 11:59pm",
        );
        session.status("idle");
        wait_for(&out, "the limit hold", 15, |s| state(s) == Some("limit"));
        holds(&out, "usage limit", 8);

        // Resumed, then the connection drops: still no shutdown.
        session.resume();
        wait_for(&out, "work to resume", 15, |s| state(s) == Some("working"));
        session.api_error("unknown", "API Error: Connection error.");
        session.status("idle");
        wait_for(&out, "the connection hold", 15, |s| {
            state(s) == Some("offline")
        });
        holds(&out, "connection lost", 8);

        // Resumed and finished normally: settle, count down, power off exactly once.
        session.resume();
        wait_for(&out, "work to resume", 15, |s| state(s) == Some("working"));
        session.record(json!({
            "type": "assistant",
            "message": {"role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]},
        }));
        session.status("idle");
        wait_for(&out, "the countdown", 30, |s| s.phase == Phase::Countdown);
        assert_eq!(SHUTDOWNS.load(Ordering::SeqCst), 0);
        wait_for(&out, "the shutdown", 15, |s| !s.armed);
        assert_eq!(SHUTDOWNS.load(Ordering::SeqCst), 1);

        drop(tx);
        monitor.join().unwrap();
        let _ = agent.kill();
        let _ = agent.wait();
        let _ = std::fs::remove_dir_all(&home);
    }
}
