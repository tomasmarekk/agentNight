use std::collections::HashMap;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Lightweight view of the process table, refreshed once per scan.
pub struct Procs {
    sys: System,
    children: HashMap<u32, Vec<u32>>,
}

/// What a harness matcher needs to know about one process.
pub struct ProcRef<'a> {
    pub pid: u32,
    /// Lowercase executable name without `.exe`.
    pub name: String,
    proc_: &'a sysinfo::Process,
}

impl ProcRef<'_> {
    /// Lowercase command line with forward slashes. Only computed on demand because it
    /// is only needed for interpreter processes (node, bun, python...).
    pub fn cmd(&self) -> String {
        let mut out = String::new();
        for part in self.proc_.cmd() {
            if out.len() > 2048 {
                break;
            }
            out.push_str(&part.to_string_lossy());
            out.push(' ');
        }
        out.to_lowercase().replace('\\', "/")
    }

    pub fn parent(&self) -> Option<u32> {
        self.proc_.parent().map(|p| p.as_u32())
    }
}

impl Procs {
    pub fn new() -> Self {
        Procs {
            sys: System::new(),
            children: HashMap::new(),
        }
    }

    /// Refreshes the process list. CPU usage is not sampled here because it is the
    /// expensive part; see [`Procs::refresh_cpu`].
    pub fn refresh(&mut self) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet),
        );
        self.children.clear();
        for (pid, p) in self.sys.processes() {
            if let Some(parent) = p.parent() {
                self.children
                    .entry(parent.as_u32())
                    .or_default()
                    .push(pid.as_u32());
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = ProcRef<'_>> {
        self.sys.processes().iter().map(|(pid, p)| ProcRef {
            pid: pid.as_u32(),
            name: normalize_name(&p.name().to_string_lossy()),
            proc_: p,
        })
    }

    pub fn get(&self, pid: u32) -> Option<ProcRef<'_>> {
        self.sys.process(Pid::from_u32(pid)).map(|p| ProcRef {
            pid,
            name: normalize_name(&p.name().to_string_lossy()),
            proc_: p,
        })
    }

    /// Process start time in seconds since the Unix epoch.
    pub fn start_time(&self, pid: u32) -> Option<u64> {
        self.sys.process(Pid::from_u32(pid)).map(|p| p.start_time())
    }

    /// Samples CPU usage of the given processes and all their descendants.
    pub fn refresh_cpu(&mut self, roots: &[u32]) {
        if roots.is_empty() {
            return;
        }
        let pids: Vec<Pid> = roots
            .iter()
            .flat_map(|&r| self.tree(r))
            .map(Pid::from_u32)
            .collect();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            ProcessRefreshKind::nothing().with_cpu(),
        );
    }

    /// A process and its descendants.
    fn tree(&self, pid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            if out.len() >= 512 || out.contains(&p) {
                continue;
            }
            out.push(p);
            if let Some(kids) = self.children.get(&p) {
                stack.extend(kids.iter().copied());
            }
        }
        out
    }

    /// CPU usage (percent of one core) of a process and all its descendants, as of the
    /// last [`Procs::refresh_cpu`] that covered them.
    pub fn tree_cpu(&self, pid: u32) -> f32 {
        self.tree(pid)
            .into_iter()
            .filter_map(|p| self.sys.process(Pid::from_u32(p)))
            .map(|p| p.cpu_usage())
            .sum()
    }
}

pub fn normalize_name(name: &str) -> String {
    let lower = name.to_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_normalized() {
        assert_eq!(normalize_name("Claude.exe"), "claude");
        assert_eq!(normalize_name("codex"), "codex");
    }

    #[test]
    fn sees_own_process() {
        let mut procs = Procs::new();
        procs.refresh();
        assert!(procs.get(std::process::id()).is_some());
    }
}
