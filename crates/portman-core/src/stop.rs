//! Stopping dev servers: SIGTERM, wait, then SIGKILL only when asked.
//!
//! Safety: only processes owned by the current user, never pid ≤ 1, never
//! ourselves, never anything inside a `.app` bundle (that includes the Docker
//! VM). Those appear for context only.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::darwin;
use crate::sampler::app_bundle_name;

pub const GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Default)]
pub struct StopResult {
    pub pids: Vec<i32>,
    /// Every pid is gone.
    pub stopped: bool,
    /// SIGKILL was sent.
    pub killed: bool,
    /// Still alive after the grace period (only when not forced).
    pub still_running: Vec<i32>,
    pub error: Option<String>,
}

/// Why `pid` may not be signalled, or `Ok` if it may.
pub fn check_stoppable(pid: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!("refusing to signal pid {pid}"));
    }
    if pid == std::process::id() as i32 {
        return Err("refusing to stop portman itself".into());
    }
    let info = darwin::bsd_info(pid).ok_or_else(|| format!("no such process {pid}"))?;
    if info.uid != darwin::current_uid() {
        return Err(format!("{pid} ({}) is not owned by you", info.comm));
    }
    if let Some(app) = darwin::exe_path(pid).as_deref().and_then(app_bundle_name) {
        return Err(format!("{pid} ({}) belongs to {app}.app — quit it from the app instead", info.comm));
    }
    Ok(())
}

fn signal(pid: i32, sig: libc::c_int) -> bool {
    unsafe { libc::kill(pid, sig) == 0 }
}

/// SIGTERM every pid, wait up to `grace`, then SIGKILL survivors if `force`.
pub fn stop_pids(pids: &[i32], force: bool, grace: Duration) -> StopResult {
    let mut res = StopResult { pids: pids.to_vec(), ..Default::default() };
    for &pid in pids {
        if let Err(e) = check_stoppable(pid) {
            // A member that already exited is fine; anything else aborts.
            if darwin::is_alive(pid) {
                res.error = Some(e);
                return res;
            }
        }
    }
    // Children first so wrappers like `pnpm` don't respawn or complain.
    for &pid in pids.iter().rev() {
        signal(pid, libc::SIGTERM);
    }
    let survivors = wait_gone(pids, grace);
    if survivors.is_empty() {
        res.stopped = true;
        return res;
    }
    if !force {
        res.still_running = survivors;
        return res;
    }
    for &pid in &survivors {
        signal(pid, libc::SIGKILL);
    }
    res.killed = true;
    res.still_running = wait_gone(&survivors, Duration::from_secs(2));
    res.stopped = res.still_running.is_empty();
    res
}

/// SIGKILL pids that survived an earlier SIGTERM.
pub fn kill_pids(pids: &[i32]) -> StopResult {
    let mut res = StopResult { pids: pids.to_vec(), killed: true, ..Default::default() };
    for &pid in pids {
        if darwin::is_alive(pid) {
            if let Err(e) = check_stoppable(pid) {
                res.error = Some(e);
                return res;
            }
            signal(pid, libc::SIGKILL);
        }
    }
    res.still_running = wait_gone(pids, Duration::from_secs(2));
    res.stopped = res.still_running.is_empty();
    res
}

fn wait_gone(pids: &[i32], timeout: Duration) -> Vec<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        let alive: Vec<i32> = pids.iter().copied().filter(|&p| darwin::is_alive(p)).collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn refuses_unsafe_targets() {
        assert!(check_stoppable(1).is_err());
        assert!(check_stoppable(std::process::id() as i32).is_err());
    }

    #[test]
    fn stops_a_child() {
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id() as i32;
        let res = stop_pids(&[pid], false, Duration::from_secs(5));
        let _ = child.wait();
        assert!(res.stopped, "{res:?}");
        assert!(!res.killed);
    }

    #[test]
    fn force_kills_a_term_ignorer() {
        let mut child = Command::new("sh").args(["-c", "trap '' TERM; sleep 60"]).spawn().unwrap();
        let pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(200));
        let res = stop_pids(&[pid], false, Duration::from_millis(500));
        assert_eq!(res.still_running, vec![pid]);
        let res = stop_pids(&[pid], true, Duration::from_millis(500));
        let _ = child.wait();
        assert!(res.killed && res.stopped, "{res:?}");
    }
}
