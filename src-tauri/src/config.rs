use std::time::Duration;

/// Timings and safety switches. Defaults are tuned for unattended overnight runs; the
/// environment overrides exist for testing.
#[derive(Clone, Debug)]
pub struct Config {
    /// How long every session must stay finished before the countdown starts. Covers
    /// `/goal` loops, stop hooks and agents that pause briefly between turns.
    pub grace: Duration,
    /// How long the network must be back before shutting down, so reconnecting agents
    /// get a chance to resume.
    pub online_grace: Duration,
    /// Visible countdown before the actual shutdown.
    pub countdown: Duration,
    /// Log instead of shutting down.
    pub dry_run: bool,
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            grace: secs("AGENTNIGHT_GRACE_SECS", 300),
            online_grace: secs("AGENTNIGHT_ONLINE_GRACE_SECS", 180),
            countdown: secs("AGENTNIGHT_COUNTDOWN_SECS", 60),
            dry_run: dry_run(),
        }
    }
}

fn secs(key: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(default),
    )
}

fn flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Debug builds never power off unless explicitly allowed; release builds always do
/// unless `AGENTNIGHT_DRY_RUN=1`.
fn dry_run() -> bool {
    if cfg!(debug_assertions) {
        !flag("AGENTNIGHT_ALLOW_SHUTDOWN")
    } else {
        flag("AGENTNIGHT_DRY_RUN")
    }
}
