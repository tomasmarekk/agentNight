//! Powering off and keeping the machine awake while agents work.

use std::process::{Command, Stdio};

/// Shuts the computer down. Returns a short error description on failure.
pub fn shutdown() -> Result<(), String> {
    #[cfg(windows)]
    {
        run("shutdown", &["/s", "/f", "/t", "0"])
    }
    #[cfg(target_os = "macos")]
    {
        run(
            "osascript",
            &["-e", "tell application \"System Events\" to shut down"],
        )
        .or_else(|_| {
            run(
                "osascript",
                &["-e", "tell application \"loginwindow\" to «event aevtshut»"],
            )
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        run("systemctl", &["poweroff"])
            .or_else(|_| run("loginctl", &["poweroff"]))
            .or_else(|_| run("shutdown", &["-h", "now"]))
    }
}

/// Asks macOS for permission to send the shutdown event while the user is still around,
/// instead of failing silently at 3 a.m.
pub fn request_permission() {
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        let _ = run(
            "osascript",
            &["-e", "tell application \"System Events\" to get name"],
        );
    });
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = command(program, args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("{program}: {}", err.trim()))
    }
}

/// Prevents system sleep (not display sleep) while enabled, so agents keep running.
/// Must live on one long-lived thread: on Windows the request is per thread.
#[derive(Default)]
pub struct KeepAwake {
    on: bool,
    #[cfg(not(windows))]
    child: Option<std::process::Child>,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

impl KeepAwake {
    pub fn set(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        #[cfg(windows)]
        {
            const ES_CONTINUOUS: u32 = 0x8000_0000;
            const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
            let flags = if on {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            // SAFETY: plain Win32 call with valid flags.
            unsafe {
                SetThreadExecutionState(flags);
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if on {
                self.child = spawn_inhibitor();
            }
        }
    }
}

#[cfg(not(windows))]
fn spawn_inhibitor() -> Option<std::process::Child> {
    let pid = std::process::id().to_string();
    #[cfg(target_os = "macos")]
    let mut cmd = command("caffeinate", &["-i", "-w", &pid]);
    #[cfg(not(target_os = "macos"))]
    let mut cmd = command(
        "systemd-inhibit",
        &[
            "--what=sleep:idle",
            "--who=agentNight",
            "--why=Waiting for AI agents to finish",
            "--mode=block",
            "tail",
            &format!("--pid={pid}"),
            "-f",
            "/dev/null",
        ],
    );
    cmd.stderr(Stdio::null()).spawn().ok()
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.set(false);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Runs the real shutdown sequence against stand-in system commands that only
    /// record how they were called (and fail or succeed as configured).
    #[test]
    #[ignore = "replaces system commands through PATH; CI runs it"]
    fn shutdown_runs_the_system_command() {
        let dir = crate::logs::tests::tmp("power-stubs");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let stubs: &[(&str, i32)] = if cfg!(target_os = "macos") {
            &[("osascript", 0)]
        } else {
            &[("systemctl", 1), ("loginctl", 0), ("shutdown", 0)]
        };
        for (name, code) in stubs {
            let path = dir.join(name);
            let script = format!(
                "#!/bin/sh\necho \"{name} $*\" >> '{}'\nexit {code}\n",
                log.display()
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let original = std::env::var_os("PATH").unwrap_or_default();
        std::env::set_var("PATH", &dir);

        // Make sure the stubs are what gets resolved before calling the real function.
        let probe = Command::new(stubs[0].0).arg("probe").status();
        let probed = std::fs::read_to_string(&log).unwrap_or_default();
        let result = if probed.contains("probe") {
            std::fs::remove_file(&log).unwrap();
            Some(shutdown())
        } else {
            None
        };
        std::env::set_var("PATH", original);
        assert!(
            probe.is_ok() && result.is_some(),
            "stubs were not used: {probed:?}"
        );

        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(result.unwrap(), Ok(()));
        if cfg!(target_os = "macos") {
            assert_eq!(
                calls,
                "osascript -e tell application \"System Events\" to shut down\n"
            );
        } else {
            assert_eq!(calls, "systemctl poweroff\nloginctl poweroff\n");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
