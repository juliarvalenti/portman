//! Tauri commands. Stops go through `portman_core::stop`, which refuses
//! anything not owned by the user or inside a `.app` bundle.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use portman_core::stop::{self, StopResult};
use portman_core::{DevProcess, Snapshot, Worktree};
use tauri::State;

use crate::AppState;

/// The worktree (if attributed) and server row a pid belongs to.
pub fn locate(snap: &Snapshot, pid: i32) -> Option<(Option<&Worktree>, &DevProcess)> {
    let owns = |d: &DevProcess| d.pid == pid || d.pids.contains(&pid);
    for p in &snap.projects {
        for w in &p.worktrees {
            if let Some(d) = w.processes.iter().find(|d| owns(d)) {
                return Some((Some(w), d));
            }
        }
    }
    snap.unattributed.iter().find(|d| owns(d)).map(|d| (None, d))
}

/// All member pids of the server `pid` belongs to (just `pid` if unknown).
fn group_pids(state: &AppState, pid: i32) -> Vec<i32> {
    state
        .latest()
        .and_then(|s| locate(&s, pid).map(|(_, d)| d.pids.clone()))
        .unwrap_or_else(|| vec![pid])
}

#[tauri::command]
pub fn get_snapshot(state: State<'_, AppState>) -> Option<Snapshot> {
    state.latest()
}

#[tauri::command]
pub async fn stop_process(state: State<'_, AppState>, pid: i32, force: bool) -> Result<StopResult, String> {
    let pids = group_pids(&state, pid);
    let res = tauri::async_runtime::spawn_blocking(move || stop::stop_pids(&pids, force, stop::GRACE))
        .await
        .map_err(|e| e.to_string())?;
    state.refresh_now();
    Ok(res)
}

/// "Force kill" after a stop left survivors.
#[tauri::command]
pub async fn kill_process(state: State<'_, AppState>, pid: i32) -> Result<StopResult, String> {
    let pids = group_pids(&state, pid);
    let res = tauri::async_runtime::spawn_blocking(move || stop::kill_pids(&pids))
        .await
        .map_err(|e| e.to_string())?;
    state.refresh_now();
    Ok(res)
}

/// Stale servers. With `dry_run = false`, stops them (SIGTERM, then SIGKILL
/// after the grace period — the user confirmed the reap). `pids` is the list
/// the user was shown: only servers still stale *and* in it are stopped, so
/// nothing that turned stale after the confirm opened is touched.
#[tauri::command]
pub async fn reap(state: State<'_, AppState>, dry_run: bool, pids: Option<Vec<i32>>) -> Result<Vec<DevProcess>, String> {
    let stale: Vec<DevProcess> = state
        .latest()
        .map(|s| s.stale().into_iter().cloned().collect())
        .unwrap_or_default();
    let stale: Vec<DevProcess> = match (&pids, dry_run) {
        (Some(confirmed), false) => stale.into_iter().filter(|d| confirmed.contains(&d.pid)).collect(),
        (None, false) => return Err("reap needs the confirmed pids".into()),
        _ => stale,
    };
    if dry_run || stale.is_empty() {
        return Ok(stale);
    }
    let groups: Vec<Vec<i32>> = stale.iter().map(|d| d.pids.clone()).collect();
    tauri::async_runtime::spawn_blocking(move || reap_all(groups))
        .await
        .map_err(|e| e.to_string())?;
    state.refresh_now();
    Ok(stale)
}

/// Stop several servers concurrently so the grace periods overlap.
pub fn reap_all(groups: Vec<Vec<i32>>) -> Vec<StopResult> {
    let handles: Vec<_> = groups
        .into_iter()
        .map(|pids| std::thread::spawn(move || stop::stop_pids(&pids, true, stop::GRACE)))
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

#[tauri::command]
pub fn open_url(port: u16) -> Result<(), String> {
    Command::new("open")
        .arg(format!("http://localhost:{port}"))
        .status()
        .map_err(|e| e.to_string())
        .map(|_| ())
}

#[tauri::command]
pub fn reveal_path(path: PathBuf) -> Result<(), String> {
    // A deleted worktree can't be revealed; show its surviving parent.
    let target = path.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
    let mut cmd = Command::new("open");
    if target == path {
        cmd.arg("-R");
    }
    cmd.arg(target).status().map_err(|e| e.to_string()).map(|_| ())
}

/// Copy the worktree's `export VAR=port` lines to the clipboard; returns them.
#[tauri::command]
pub fn copy_env(state: State<'_, AppState>, worktree: PathBuf) -> Result<String, String> {
    let snap = state.latest().ok_or("no snapshot yet")?;
    let lease = snap
        .projects
        .iter()
        .flat_map(|p| p.worktrees.iter())
        .find(|w| w.path == worktree)
        .and_then(|w| w.lease.clone())
        .ok_or_else(|| format!("{} has no port lease", worktree.display()))?;
    let text = lease.export_lines().join("\n") + "\n";
    pbcopy(&text)?;
    Ok(text)
}

pub fn pbcopy(text: &str) -> Result<(), String> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("pbcopy: no stdin")?
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}
