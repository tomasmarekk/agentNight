//! When is it safe to power off? Only after every session has been finished for a while,
//! the network has been up for a while, and a visible countdown ran out.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Switch is off.
    #[default]
    Off,
    /// Armed, agents still working (or waiting on a limit / connection).
    Watching,
    /// Everything finished; making sure it stays that way.
    Settling,
    /// Everything finished but the network is down or unverified.
    Offline,
    /// Final countdown before powering off.
    Countdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Net {
    Unknown,
    Online,
    Offline,
}

#[derive(Debug, PartialEq)]
pub struct Tick {
    pub phase: Phase,
    /// Time left until the current phase ends (settling or countdown).
    pub remaining: Option<Duration>,
    /// Power off now.
    pub fire: bool,
}

pub struct Policy {
    grace: Duration,
    online_grace: Duration,
    countdown: Duration,
    armed: bool,
    quiet_since: Option<Instant>,
    online_since: Option<Instant>,
    countdown_until: Option<Instant>,
}

impl Policy {
    pub fn new(cfg: &Config) -> Self {
        Policy {
            grace: cfg.grace,
            online_grace: cfg.online_grace,
            countdown: cfg.countdown,
            armed: false,
            quiet_since: None,
            online_since: None,
            countdown_until: None,
        }
    }

    pub fn armed(&self) -> bool {
        self.armed
    }

    pub fn set_armed(&mut self, on: bool) {
        self.armed = on;
        self.quiet_since = None;
        self.online_since = None;
        self.countdown_until = None;
    }

    /// `busy`: at least one session is working or waiting to resume.
    pub fn tick(&mut self, now: Instant, busy: bool, net: Net) -> Tick {
        let idle = |phase| Tick {
            phase,
            remaining: None,
            fire: false,
        };
        if !self.armed {
            return idle(Phase::Off);
        }
        if net == Net::Online {
            self.online_since.get_or_insert(now);
        } else {
            self.online_since = None;
        }
        if busy {
            self.quiet_since = None;
            self.countdown_until = None;
            return idle(Phase::Watching);
        }
        let quiet_since = *self.quiet_since.get_or_insert(now);

        if let Some(until) = self.countdown_until {
            if net == Net::Offline {
                self.countdown_until = None;
                return idle(Phase::Offline);
            }
            if now >= until {
                self.set_armed(false);
                return Tick {
                    phase: Phase::Countdown,
                    remaining: Some(Duration::ZERO),
                    fire: true,
                };
            }
            return Tick {
                phase: Phase::Countdown,
                remaining: Some(until - now),
                fire: false,
            };
        }

        let Some(online_since) = self.online_since else {
            return idle(Phase::Offline);
        };
        let ready_at = (quiet_since + self.grace).max(online_since + self.online_grace);
        if now >= ready_at {
            self.countdown_until = Some(now + self.countdown);
            return Tick {
                phase: Phase::Countdown,
                remaining: Some(self.countdown),
                fire: false,
            };
        }
        Tick {
            phase: Phase::Settling,
            remaining: Some(ready_at - now),
            fire: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn policy() -> Policy {
        Policy::new(&Config {
            grace: 300 * S,
            online_grace: 180 * S,
            countdown: 60 * S,
            dry_run: true,
        })
    }

    #[test]
    fn disarmed_never_fires() {
        let mut p = policy();
        let t0 = Instant::now();
        for i in 0..1000 {
            assert_eq!(
                p.tick(t0 + i * 10 * S, false, Net::Online).phase,
                Phase::Off
            );
        }
    }

    #[test]
    fn full_sequence_after_work_finishes() {
        let mut p = policy();
        let t0 = Instant::now();
        p.set_armed(true);
        assert_eq!(p.tick(t0, true, Net::Online).phase, Phase::Watching);
        assert_eq!(
            p.tick(t0 + 600 * S, true, Net::Online).phase,
            Phase::Watching
        );
        // Work done at t=601.
        let t = p.tick(t0 + 601 * S, false, Net::Online);
        assert_eq!(t.phase, Phase::Settling);
        assert_eq!(t.remaining, Some(300 * S));
        assert_eq!(
            p.tick(t0 + 900 * S, false, Net::Online).phase,
            Phase::Settling
        );
        let t = p.tick(t0 + 901 * S, false, Net::Online);
        assert_eq!(t.phase, Phase::Countdown);
        assert!(!t.fire);
        assert!(!p.tick(t0 + 960 * S, false, Net::Online).fire);
        assert!(p.tick(t0 + 961 * S, false, Net::Online).fire);
        assert!(!p.armed());
    }

    #[test]
    fn brief_pause_between_goal_turns_does_not_fire() {
        let mut p = policy();
        let t0 = Instant::now();
        p.set_armed(true);
        p.tick(t0, true, Net::Online);
        // Agent idles for 4 minutes between turns, then continues.
        assert_eq!(
            p.tick(t0 + 10 * S, false, Net::Online).phase,
            Phase::Settling
        );
        assert_eq!(
            p.tick(t0 + 250 * S, false, Net::Online).phase,
            Phase::Settling
        );
        assert_eq!(
            p.tick(t0 + 251 * S, true, Net::Online).phase,
            Phase::Watching
        );
        // Grace starts over.
        let t = p.tick(t0 + 260 * S, false, Net::Online);
        assert_eq!(t.remaining, Some(300 * S));
    }

    #[test]
    fn new_work_cancels_countdown() {
        let mut p = policy();
        let t0 = Instant::now();
        p.set_armed(true);
        p.tick(t0, false, Net::Online);
        assert_eq!(
            p.tick(t0 + 300 * S, false, Net::Online).phase,
            Phase::Countdown
        );
        assert_eq!(
            p.tick(t0 + 330 * S, true, Net::Online).phase,
            Phase::Watching
        );
        assert!(!p.tick(t0 + 400 * S, false, Net::Online).fire);
        assert!(p.armed());
    }

    #[test]
    fn offline_holds_and_requires_stable_network() {
        let mut p = policy();
        let t0 = Instant::now();
        p.set_armed(true);
        p.tick(t0, true, Net::Online);
        // Work finished but the network dropped.
        for i in 1..100 {
            let t = p.tick(t0 + i * 60 * S, false, Net::Offline);
            assert_eq!(t.phase, Phase::Offline);
            assert!(!t.fire);
        }
        // Network back at t=6000: quiet for long, but must stay online for 180 s.
        let back = t0 + 6000 * S;
        assert_eq!(p.tick(back, false, Net::Online).phase, Phase::Settling);
        assert_eq!(
            p.tick(back + 179 * S, false, Net::Online).phase,
            Phase::Settling
        );
        assert_eq!(
            p.tick(back + 180 * S, false, Net::Online).phase,
            Phase::Countdown
        );
    }

    #[test]
    fn network_loss_during_countdown_aborts_it() {
        let mut p = policy();
        let t0 = Instant::now();
        p.set_armed(true);
        p.tick(t0, false, Net::Online);
        assert_eq!(
            p.tick(t0 + 300 * S, false, Net::Online).phase,
            Phase::Countdown
        );
        assert_eq!(
            p.tick(t0 + 310 * S, false, Net::Offline).phase,
            Phase::Offline
        );
        assert!(!p.tick(t0 + 400 * S, false, Net::Offline).fire);
    }

    #[test]
    fn unknown_network_never_fires() {
        let mut p = policy();
        let t0 = Instant::now();
        p.set_armed(true);
        for i in 0..100 {
            assert!(!p.tick(t0 + i * 60 * S, false, Net::Unknown).fire);
        }
    }
}
