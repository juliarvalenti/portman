//! The `Snapshot` shape shared by the CLI's `--json` and the app.
//!
//! Times serialize as Unix milliseconds so the front end can use them directly.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Serialize;

use crate::lease::Lease;
pub use crate::live::Container;

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    #[serde(with = "unix_ms")]
    pub taken_at: SystemTime,
    pub vitals: Vitals,
    /// Grouped by repo (main worktree); sorted by total footprint, descending.
    pub projects: Vec<Project>,
    /// Listening, but outside every crawl root.
    pub unattributed: Vec<DevProcess>,
    pub containers: Vec<Container>,
    /// Top non-dev apps by footprint, for context.
    pub heavy_hitters: Vec<AppUsage>,
    pub summary: Summary,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub repo_root: PathBuf,
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// From `.ports.lock`; a primary clone with a manifest gets its implicit slot 0.
    pub lease: Option<Lease>,
    pub processes: Vec<DevProcess>,
    /// false = process running in a deleted worktree. Always a leak.
    pub exists: bool,
}

/// One dev server. A server is often a small process tree (`pnpm` → `next
/// dev` → `next-server`); its members are folded into one row so a server
/// reads — and stops — as a unit. `pid` is the tree's root.
#[derive(Debug, Clone, Serialize)]
pub struct DevProcess {
    pub pid: i32,
    /// Every member of the tree, root first. Stop signals all of them.
    pub pids: Vec<i32>,
    /// Name of the heaviest member: "next-server", "vite", "node", "cargo", ...
    pub name: String,
    pub cmdline: String,
    pub cwd: PathBuf,
    pub listening: Vec<u16>,
    /// `phys_footprint` summed over members — NOT RSS.
    pub footprint_bytes: u64,
    pub cpu_pct: f32,
    #[serde(with = "unix_ms")]
    pub started_at: SystemTime,
    pub uptime_secs: u64,
    /// Seconds the server has been continuously under `stale.idle_cpu_pct`.
    pub idle_secs: Option<u64>,
    pub stale: Option<StaleReason>,
    pub lease_match: LeaseMatch,
    /// Owned by us and not part of a `.app` bundle. Others are context only.
    pub stoppable: bool,
    /// Eligible for reap: stoppable, and either attributed to a worktree or
    /// running from a temp dir (an orphaned agent scratchpad server).
    pub reapable: bool,
    /// Owning `.app` bundle, if any (e.g. "Mycelium").
    pub app_bundle: Option<String>,
    /// Footprint samples over the last ~30 s, oldest first (app only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StaleReason {
    TooOld,
    Idle,
    Both,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "port")]
pub enum LeaseMatch {
    OnLease,
    /// Bound to a port outside the worktree's lease (e.g. a hardcoded port).
    OffLease(u16),
    NoLease,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PressureLevel {
    Normal,
    Warn,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct Vitals {
    pub pressure: PressureLevel,
    pub ram_total: u64,
    pub ram_free: u64,
    pub compressed: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    /// Data volume available space.
    pub disk_free: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppUsage {
    pub app: String,
    pub footprint_bytes: u64,
    pub process_count: u32,
}

/// Rollups the tray and notifications need, so every front end agrees.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Stoppable dev servers (projects + unattributed).
    pub dev_count: u32,
    pub dev_footprint: u64,
    pub stale_count: u32,
    pub stale_footprint: u64,
    /// Worst of: stale servers (warn), vitals vs. alert thresholds.
    pub level: PressureLevel,
}

impl Snapshot {
    /// Every dev server, attributed or not.
    pub fn dev_processes(&self) -> impl Iterator<Item = &DevProcess> {
        self.projects
            .iter()
            .flat_map(|p| p.worktrees.iter())
            .flat_map(|w| w.processes.iter())
            .chain(self.unattributed.iter())
    }

    /// Dev servers with their project/worktree label (`polaris/web`-style).
    pub fn labeled(&self) -> Vec<(String, &Worktree, &DevProcess)> {
        let mut out = Vec::new();
        for p in &self.projects {
            for w in &p.worktrees {
                for d in &w.processes {
                    out.push((crate::label(p, w, d), w, d));
                }
            }
        }
        out
    }

    pub fn find(&self, pid: i32) -> Option<&DevProcess> {
        self.dev_processes().find(|d| d.pid == pid || d.pids.contains(&pid))
    }

    /// Reap candidates.
    pub fn stale(&self) -> Vec<&DevProcess> {
        self.dev_processes().filter(|d| d.stale.is_some() && d.reapable).collect()
    }
}

pub mod unix_ms {
    use serde::Serializer;
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let ms = t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        s.serialize_u64(ms)
    }
}
