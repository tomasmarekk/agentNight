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
}

impl Monitor {
    pub fn new(cfg: Config) -> Self {
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
        if let Err(e) = power::shutdown() {
            eprintln!("agentNight: shutdown failed: {e}");
            self.notify(&format!("Shutdown failed: {e}"));
        }
    }
}
