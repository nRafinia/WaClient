//! Hand-over between the process that asked for a restart (to move the data)
//! and its successor: the successor must not touch the data or the instance
//! guard until the predecessor has fully exited and closed its databases.

use std::ffi::OsString;
use std::time::Duration;

pub const WAIT_PID_FLAG: &str = "--wait-pid";
/// How long the successor waits for the predecessor to exit.
pub const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Grace for acquiring the instance guard once the predecessor is gone.
const GUARD_GRACE: Duration = Duration::from_secs(3);

/// `--wait-pid <pid>` or `--wait-pid=<pid>`; a missing, malformed or zero pid is ignored.
pub fn parse_wait_pid(args: &[OsString]) -> Option<u32> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value = if arg == WAIT_PID_FLAG {
            it.next()?.to_str()?
        } else if let Some(v) = arg.to_str().and_then(|s| s.strip_prefix("--wait-pid=")) {
            v
        } else {
            continue;
        };
        return value.parse::<u32>().ok().filter(|&p| p != 0);
    }
    None
}

pub const OPEN_PROFILE_FLAG: &str = "--open-profile";

/// `--open-profile <id>` or `--open-profile=<id>`; an empty id is ignored.
pub fn parse_open_profile(args: &[OsString]) -> Option<String> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value = if arg == OPEN_PROFILE_FLAG {
            it.next()?.to_str()?
        } else if let Some(v) = arg.to_str().and_then(|s| s.strip_prefix("--open-profile=")) {
            v
        } else {
            continue;
        };
        return Some(value.to_string()).filter(|v| !v.is_empty());
    }
    None
}

/// The profile to activate at startup: the one named by `--open-profile` when
/// it still exists, otherwise the one used last.
pub fn startup_focus(
    open: Option<String>,
    last_active: Option<String>,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    open.filter(|id| exists(id)).or(last_active)
}

/// The arguments for the successor: ours without the autostart flag (so its
/// window is shown) and without stale `--wait-pid` / `--open-profile`, plus the
/// pid to wait for and, when given, the profile to reopen.
pub fn successor_args(
    args: &[OsString],
    minimized_flag: &str,
    own_pid: u32,
    open_profile: Option<&str>,
) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == minimized_flag {
            continue;
        }
        if arg == WAIT_PID_FLAG || arg == OPEN_PROFILE_FLAG {
            it.next();
            continue;
        }
        if arg.to_str().is_some_and(|s| s.starts_with("--wait-pid=") || s.starts_with("--open-profile=")) {
            continue;
        }
        out.push(arg.clone());
    }
    out.push(WAIT_PID_FLAG.into());
    out.push(own_pid.to_string().into());
    if let Some(id) = open_profile {
        out.push(OPEN_PROFILE_FLAG.into());
        out.push(id.into());
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// A normal launch.
    NotRequested,
    /// The predecessor is gone; the move may proceed.
    PredecessorExited,
    /// The predecessor is still running: the move must not start.
    TimedOut,
}

pub fn handoff_outcome(wait_pid: Option<u32>, exited: impl FnOnce(u32) -> bool) -> Handoff {
    match wait_pid {
        None => Handoff::NotRequested,
        Some(pid) if exited(pid) => Handoff::PredecessorExited,
        Some(_) => Handoff::TimedOut,
    }
}

impl Handoff {
    /// How long to keep trying for the instance guard.
    pub fn guard_wait(self) -> Duration {
        match self {
            Handoff::PredecessorExited => GUARD_GRACE,
            Handoff::NotRequested | Handoff::TimedOut => Duration::ZERO,
        }
    }

    pub fn move_allowed(self) -> bool {
        self != Handoff::TimedOut
    }
}

/// Waits until the process `pid` has exited. True when it has (or does not
/// exist any more), false on timeout or when it cannot be observed.
#[cfg(windows)]
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;
    const ERROR_INVALID_PARAMETER: u32 = 87;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
    }

    // SAFETY: plain Win32 calls; the handle is checked for null, used only for
    // the wait and closed exactly once.
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle == 0 {
            // No such process: it is already gone.
            return GetLastError() == ERROR_INVALID_PARAMETER;
        }
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
        let result = WaitForSingleObject(handle, millis);
        CloseHandle(handle);
        result == WAIT_OBJECT_0
    }
}

#[cfg(not(windows))]
pub fn wait_for_exit(_pid: u32, _timeout: Duration) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn wait_pid_is_parsed_in_both_forms_and_bad_values_are_ignored() {
        assert_eq!(parse_wait_pid(&os(&["--wait-pid", "1234"])), Some(1234));
        assert_eq!(parse_wait_pid(&os(&["--minimized", "--wait-pid=77"])), Some(77));
        assert_eq!(parse_wait_pid(&os(&[])), None);
        assert_eq!(parse_wait_pid(&os(&["--wait-pid"])), None);
        assert_eq!(parse_wait_pid(&os(&["--wait-pid", "abc"])), None);
        assert_eq!(parse_wait_pid(&os(&["--wait-pid", "0"])), None);
        assert_eq!(parse_wait_pid(&os(&["--wait-pid", "-5"])), None);
    }

    #[test]
    fn successor_args_drop_autostart_and_stale_pids_and_add_ours() {
        let args = os(&["--minimized", "--other", "--wait-pid", "1", "--wait-pid=2", "x"]);
        let out = successor_args(&args, "--minimized", 99, None);
        assert_eq!(out, os(&["--other", "x", "--wait-pid", "99"]));
        assert_eq!(parse_wait_pid(&out), Some(99));
    }

    #[test]
    fn open_profile_is_parsed_replaced_and_validated() {
        assert_eq!(parse_open_profile(&os(&["--open-profile", "p2"])), Some("p2".to_string()));
        assert_eq!(parse_open_profile(&os(&["--open-profile=p3"])), Some("p3".to_string()));
        assert_eq!(parse_open_profile(&os(&["--open-profile"])), None);
        assert_eq!(parse_open_profile(&os(&["--open-profile="])), None);
        assert_eq!(parse_open_profile(&os(&[])), None);

        let args = os(&["--open-profile", "old", "--open-profile=older", "x"]);
        let out = successor_args(&args, "--minimized", 7, Some("new"));
        assert_eq!(out, os(&["x", "--wait-pid", "7", "--open-profile", "new"]));
        assert_eq!(parse_open_profile(&out), Some("new".to_string()));
        assert_eq!(parse_open_profile(&successor_args(&args, "--minimized", 7, None)), None);
    }

    #[test]
    fn startup_focus_falls_back_to_last_active_when_the_profile_is_gone() {
        let exists = |id: &str| id == "p2";
        assert_eq!(startup_focus(Some("p2".into()), Some("p1".into()), exists), Some("p2".to_string()));
        assert_eq!(startup_focus(Some("gone".into()), Some("p1".into()), exists), Some("p1".to_string()));
        assert_eq!(startup_focus(None, Some("p1".into()), exists), Some("p1".to_string()));
        assert_eq!(startup_focus(Some("gone".into()), None, exists), None);
    }

    #[test]
    fn outcome_decides_whether_the_move_may_run() {
        assert_eq!(handoff_outcome(None, |_| unreachable!()), Handoff::NotRequested);
        assert_eq!(handoff_outcome(Some(5), |p| p == 5), Handoff::PredecessorExited);
        assert_eq!(handoff_outcome(Some(5), |_| false), Handoff::TimedOut);
        assert!(Handoff::NotRequested.move_allowed());
        assert!(Handoff::PredecessorExited.move_allowed());
        assert!(!Handoff::TimedOut.move_allowed());
        assert_eq!(Handoff::TimedOut.guard_wait(), Duration::ZERO);
        assert!(Handoff::PredecessorExited.guard_wait() > Duration::ZERO);
    }

    #[cfg(windows)]
    #[test]
    fn waiting_sees_a_process_exit_and_times_out_on_a_running_one() {
        let mut quick = std::process::Command::new("cmd").args(["/C", "exit", "0"]).spawn().expect("spawn");
        assert!(wait_for_exit(quick.id(), Duration::from_secs(10)));
        let _ = quick.wait();

        let mut slow = std::process::Command::new("ping")
            .args(["-n", "8", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        assert!(!wait_for_exit(slow.id(), Duration::from_millis(200)));
        let _ = slow.kill();
        let _ = slow.wait();

        assert!(wait_for_exit(0xFFFF_FFF1, Duration::from_millis(200)), "a pid that does not exist counts as gone");
    }
}
