//! Builds `Snapshot`s: process sampling, dev-server detection, attribution to
//! project → worktree, the stale rule, and heavy hitters.
//!
//! A `Sampler` is stateful so CPU can be measured as a delta between samples
//! and idle time / footprint history can accumulate. The CLI primes it, waits
//! briefly, and samples once; the app keeps one alive and samples every
//! `refresh_secs`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Config;
use crate::darwin;
use crate::lease::{self, Lease, LockData};
use crate::live::{self, Container};
use crate::model::*;

/// Process names that are dev servers/watchers when run from a crawl root.
const DEV_NAMES: &[&str] = &[
    "node", "next-server", "vite", "bun", "deno", "cargo", "cargo-watch", "watchexec", "python",
    "python3", "uvicorn", "gunicorn", "ruby", "rails", "puma", "go", "air", "java", "php",
    "esbuild", "tsx", "turbo", "nodemon", "dotnet", "beam.smp", "mix", "wrangler", "workerd",
];

/// Command-line fragments that mark a dev process regardless of binary name.
const DEV_CMD_PATTERNS: &[&str] = &[
    "next dev", "vite", "webpack", "tsc --watch", "tsc -w", "cargo watch", "nodemon", "uvicorn",
    "dagster", "storybook", "wrangler dev", "astro dev", "remix dev", "nuxt dev",
];

/// Shells end the ppid walk for app attribution, so a CLI started from a
/// terminal isn't counted as part of the terminal app.
const SHELLS: &[&str] = &["zsh", "-zsh", "bash", "-bash", "fish", "sh", "login", "tmux", "screen"];

/// Executables under these prefixes are the OS's own and never dev servers.
const SYSTEM_PREFIXES: &[&str] = &["/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/"];

const LOCK_CRAWL_TTL: Duration = Duration::from_secs(15);
const DOCKER_TTL: Duration = Duration::from_secs(10);
const HISTORY_WINDOW: Duration = Duration::from_secs(30);
const HEAVY_HITTERS: usize = 5;

/// pid + start time: pids get reused, this doesn't.
type ProcKey = (i32, SystemTime);

#[derive(Debug, Clone)]
struct Proc {
    pid: i32,
    ppid: i32,
    uid: u32,
    comm: String,
    exe: String,
    started_at: SystemTime,
    footprint: u64,
    cpu_ns: Option<u64>,
}

#[derive(Debug, Clone)]
struct DevCandidate {
    proc: Proc,
    cwd: Option<PathBuf>,
    cmdline: String,
    listening: Vec<u16>,
}

/// Where a cwd belongs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Attribution {
    worktree: PathBuf,
    repo_root: PathBuf,
    exists: bool,
}

pub struct Sampler {
    pub config: Config,
    roots: Vec<PathBuf>,
    me: i32,
    uid: u32,
    prev_cpu: HashMap<ProcKey, (Instant, u64)>,
    idle_since: HashMap<ProcKey, SystemTime>,
    history: HashMap<ProcKey, VecDeque<u64>>,
    locks: Option<(Instant, Vec<(PathBuf, LockData)>)>,
    containers: Option<(Instant, Vec<Container>)>,
    /// Keep per-server footprint history (the app's sparklines).
    track_history: bool,
}

impl Sampler {
    pub fn new(config: Config) -> Sampler {
        // Kernel-reported cwds are physical paths; match them against
        // physical roots.
        let roots = crate::lease::crawl_roots_with(&config)
            .into_iter()
            .map(|r| r.canonicalize().unwrap_or(r))
            .collect();
        Sampler {
            config,
            roots,
            me: std::process::id() as i32,
            uid: darwin::current_uid(),
            prev_cpu: HashMap::new(),
            idle_since: HashMap::new(),
            history: HashMap::new(),
            locks: None,
            containers: None,
            track_history: false,
        }
    }

    /// The app keeps per-process footprint history for sparklines.
    pub fn with_history(mut self) -> Sampler {
        self.track_history = true;
        self
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Record a CPU baseline so the next `sample()` measures live CPU.
    pub fn prime(&mut self) {
        let now = Instant::now();
        for p in self.read_procs() {
            if let Some(cpu) = p.cpu_ns {
                self.prev_cpu.insert((p.pid, p.started_at), (now, cpu));
            }
        }
    }

    /// One-shot sample with a live CPU window, for the CLI.
    pub fn sample_over(config: Config, window: Duration) -> Snapshot {
        let mut s = Sampler::new(config);
        s.prime();
        std::thread::sleep(window);
        s.sample()
    }

    /// Drop cached lock-crawl and docker results (after a claim or stop).
    pub fn invalidate(&mut self) {
        self.locks = None;
        self.containers = None;
    }

    fn read_procs(&self) -> Vec<Proc> {
        darwin::all_pids()
            .into_iter()
            .filter_map(|pid| {
                let info = darwin::bsd_info(pid)?;
                let ru = darwin::rusage(pid);
                Some(Proc {
                    pid,
                    ppid: info.ppid,
                    uid: info.uid,
                    exe: darwin::exe_path(pid).unwrap_or_default(),
                    comm: info.comm,
                    started_at: info.started_at,
                    footprint: ru.map(|r| r.footprint).unwrap_or(0),
                    cpu_ns: ru.map(|r| r.cpu_ns),
                })
            })
            .collect()
    }

    pub fn sample(&mut self) -> Snapshot {
        let taken_at = SystemTime::now();
        let now = Instant::now();
        let procs = self.read_procs();
        let by_pid: HashMap<i32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();

        // ── CPU deltas ───────────────────────────────────────────────────────
        let mut cpu_pct: HashMap<i32, f32> = HashMap::new();
        let mut next_cpu = HashMap::new();
        for p in &procs {
            let Some(cpu) = p.cpu_ns else { continue };
            let key = (p.pid, p.started_at);
            let pct = match self.prev_cpu.get(&key) {
                Some(&(t, prev)) if now > t => {
                    cpu.saturating_sub(prev) as f64 / (now - t).as_nanos() as f64 * 100.0
                }
                // First sight: lifetime average.
                _ => lifetime_pct(cpu, p.started_at, taken_at),
            };
            cpu_pct.insert(p.pid, pct as f32);
            next_cpu.insert(key, (now, cpu));
        }
        self.prev_cpu = next_cpu;

        // ── dev candidates ───────────────────────────────────────────────────
        let mut lsof_fallback: Option<HashMap<i32, Vec<u16>>> = None;
        let mut devs: Vec<DevCandidate> = Vec::new();
        for p in &procs {
            if p.uid != self.uid || p.pid == self.me || is_system_exe(&p.exe) || is_ignored_app(&p.exe) {
                continue;
            }
            let listening = match darwin::listening_ports(p.pid, darwin::nfiles(p.pid)) {
                Some(ports) => ports,
                None => lsof_fallback
                    .get_or_insert_with(lsof_by_pid)
                    .get(&p.pid)
                    .cloned()
                    .unwrap_or_default(),
            };
            let listening: Vec<u16> = listening.into_iter().filter(|&port| port >= 1024).collect();
            let cwd = darwin::cwd(p.pid);
            let in_root = cwd.as_deref().is_some_and(|c| self.root_of(c).is_some());
            if listening.is_empty() && !in_root {
                continue;
            }
            let cmdline = darwin::cmdline(p.pid).unwrap_or_else(|| p.comm.clone());
            // Non-listening work only counts if it looks like a watcher the
            // user started — not an agent's MCP server or an editor's
            // extension host that happens to share the repo as cwd.
            if listening.is_empty()
                && (!self.is_dev_like(p, &cmdline) || is_mcp_like(&cmdline) || owning_app(p, &by_pid).is_some())
            {
                continue;
            }
            devs.push(DevCandidate { proc: p.clone(), cwd, cmdline, listening });
        }

        // ── attribution + grouping into server trees ─────────────────────────
        let attrs: Vec<Option<Attribution>> =
            devs.iter().map(|d| d.cwd.as_deref().and_then(|c| self.attribute(c))).collect();
        let dev_idx: HashMap<i32, usize> = devs.iter().enumerate().map(|(i, d)| (d.proc.pid, i)).collect();
        // Parent = nearest dev ancestor (skipping non-dev hops like `sh -c`)
        // with the same attribution.
        let parent_of = |i: usize| -> Option<usize> {
            let mut pid = devs[i].proc.ppid;
            for _ in 0..32 {
                if pid <= 1 {
                    return None;
                }
                if let Some(&j) = dev_idx.get(&pid) {
                    let same = match (&attrs[i], &attrs[j]) {
                        (Some(a), Some(b)) => a.worktree == b.worktree,
                        (None, None) => devs[i].cwd == devs[j].cwd,
                        _ => false,
                    };
                    return same.then_some(j);
                }
                pid = by_pid.get(&pid)?.ppid;
            }
            None
        };
        let mut root_of: Vec<usize> = (0..devs.len()).collect();
        for i in 0..devs.len() {
            let mut r = i;
            let mut hops = 0;
            while let Some(p) = parent_of(r) {
                r = p;
                hops += 1;
                if hops > 32 {
                    break;
                }
            }
            root_of[i] = r;
        }
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, &r) in root_of.iter().enumerate() {
            groups.entry(r).or_default().push(i);
        }

        // ── build DevProcess rows ────────────────────────────────────────────
        let mut seen_keys: HashSet<ProcKey> = HashSet::new();
        let mut dev_pids: HashSet<i32> = HashSet::new();
        let mut rows: Vec<(Option<Attribution>, DevProcess)> = Vec::new();
        for (&root, members) in &groups {
            let mut members = members.clone();
            members.sort_by_key(|&i| (i != root, devs[i].proc.started_at));
            let r = &devs[root];
            let heaviest = members.iter().copied().max_by_key(|&i| devs[i].proc.footprint).unwrap_or(root);
            let footprint: u64 = members.iter().map(|&i| devs[i].proc.footprint).sum();
            let cpu: f32 = members.iter().map(|&i| cpu_pct.get(&devs[i].proc.pid).copied().unwrap_or(0.0)).sum();
            let cpu_ns: u64 = members.iter().filter_map(|&i| devs[i].proc.cpu_ns).sum();
            let mut listening: Vec<u16> = members.iter().flat_map(|&i| devs[i].listening.clone()).collect();
            listening.sort_unstable();
            listening.dedup();
            let key = (r.proc.pid, r.proc.started_at);
            seen_keys.insert(key);
            dev_pids.extend(members.iter().map(|&i| devs[i].proc.pid));

            let uptime = taken_at.duration_since(r.proc.started_at).unwrap_or_default();
            let idle_secs = self.track_idle(key, cpu, cpu_ns, r.proc.started_at, taken_at);
            let history = if self.track_history {
                let cap = (HISTORY_WINDOW.as_secs() / self.config.refresh_secs.max(1)) as usize + 1;
                let h = self.history.entry(key).or_default();
                h.push_back(footprint);
                while h.len() > cap {
                    h.pop_front();
                }
                h.iter().copied().collect()
            } else {
                Vec::new()
            };
            let app_bundle = app_bundle_name(&r.proc.exe);
            let stoppable = app_bundle.is_none() && r.proc.pid > 1;
            let reapable = stoppable && (attrs[root].is_some() || r.cwd.as_deref().is_some_and(is_temp_dir));
            let dp = DevProcess {
                pid: r.proc.pid,
                pids: members.iter().map(|&i| devs[i].proc.pid).collect(),
                name: devs[heaviest].proc.comm.clone(),
                cmdline: r.cmdline.clone(),
                cwd: r.cwd.clone().unwrap_or_default(),
                listening,
                footprint_bytes: footprint,
                cpu_pct: cpu,
                started_at: r.proc.started_at,
                uptime_secs: uptime.as_secs(),
                idle_secs,
                // Only reap candidates can be stale: an `.app` or a long-lived
                // service (Homebrew postgres) being up for days is expected.
                stale: if reapable { self.stale_reason(uptime, idle_secs, footprint) } else { None },
                lease_match: LeaseMatch::NoLease,
                stoppable,
                reapable,
                app_bundle,
                history,
            };
            rows.push((attrs[root].clone(), dp));
        }
        self.idle_since.retain(|k, _| seen_keys.contains(k));
        self.history.retain(|k, _| seen_keys.contains(k));

        // ── projects / worktrees ─────────────────────────────────────────────
        let mut projects: HashMap<PathBuf, Project> = HashMap::new();
        let mut unattributed = Vec::new();
        let ensure = |projects: &mut HashMap<PathBuf, Project>, a: &Attribution, lease: Option<Lease>| {
            let proj = projects.entry(a.repo_root.clone()).or_insert_with(|| Project {
                name: project_name(&a.repo_root),
                repo_root: a.repo_root.clone(),
                worktrees: Vec::new(),
            });
            if let Some(w) = proj.worktrees.iter().position(|w| w.path == a.worktree) {
                return w;
            }
            proj.worktrees.push(Worktree {
                path: a.worktree.clone(),
                branch: if a.exists { git_branch(&a.worktree) } else { None },
                lease: lease.or_else(|| lease_for(a)),
                processes: Vec::new(),
                exists: a.exists,
            });
            proj.worktrees.len() - 1
        };
        for (attr, mut dp) in rows {
            match attr {
                Some(a) => {
                    let w = ensure(&mut projects, &a, None);
                    let wt = &mut projects.get_mut(&a.repo_root).unwrap().worktrees[w];
                    dp.lease_match = lease_match(wt.lease.as_ref(), &dp.listening);
                    wt.processes.push(dp);
                }
                None => unattributed.push(dp),
            }
        }
        // Idle leases: worktrees holding a slot with nothing running.
        for (lock_path, data) in self.crawled_locks().clone() {
            let dir = lock_path.parent().unwrap_or(&lock_path).to_path_buf();
            if let Some(a) = self.attribute(&dir) {
                ensure(&mut projects, &a, Some(Lease::from_lock(&lock_path, &data)));
            }
        }

        let mut projects: Vec<Project> = projects.into_values().collect();
        for p in &mut projects {
            for w in &mut p.worktrees {
                w.processes.sort_by(|a, b| b.footprint_bytes.cmp(&a.footprint_bytes));
            }
            p.worktrees.sort_by(|a, b| {
                wt_footprint(b).cmp(&wt_footprint(a)).then_with(|| a.path.cmp(&b.path))
            });
        }
        projects.sort_by(|a, b| {
            proj_footprint(b).cmp(&proj_footprint(a)).then_with(|| a.name.cmp(&b.name))
        });
        unattributed.sort_by(|a, b| b.footprint_bytes.cmp(&a.footprint_bytes));

        let vitals = darwin::vitals();
        let heavy_hitters = heavy_hitters(&procs, &by_pid, &dev_pids);
        let containers = self.cached_containers();
        let mut snap = Snapshot {
            taken_at,
            vitals,
            projects,
            unattributed,
            containers,
            heavy_hitters,
            summary: Summary { dev_count: 0, dev_footprint: 0, stale_count: 0, stale_footprint: 0, level: PressureLevel::Normal },
        };
        snap.summary = summarize(&snap, &self.config);
        snap
    }

    fn root_of(&self, path: &Path) -> Option<&PathBuf> {
        self.roots.iter().find(|r| path.starts_with(r))
    }

    fn is_dev_like(&self, p: &Proc, cmdline: &str) -> bool {
        let name = p.comm.to_ascii_lowercase();
        let base = Path::new(&p.exe).file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if DEV_NAMES.iter().any(|n| name == *n || base == *n || name.starts_with("python3.")) {
            return true;
        }
        DEV_CMD_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .chain(self.config.extra_dev_patterns.iter().cloned())
            .any(|pat| !pat.is_empty() && cmdline.contains(&pat))
    }

    /// Walk the cwd upward to the git worktree top (or nearest manifest).
    fn attribute(&self, cwd: &Path) -> Option<Attribution> {
        let root = self.root_of(cwd)?.clone();
        if cwd == root {
            return None;
        }
        if !cwd.exists() {
            // Deleted worktree: the topmost missing ancestor is the worktree;
            // its surviving parent tells us which project it belonged to.
            let mut gone = cwd.to_path_buf();
            while let Some(parent) = gone.parent() {
                if parent.exists() {
                    break;
                }
                gone = parent.to_path_buf();
            }
            let parent = gone.parent()?.to_path_buf();
            let repo_root = if parent == root {
                gone.clone()
            } else {
                self.attribute(&parent).map(|a| a.repo_root).unwrap_or_else(|| gone.clone())
            };
            return Some(Attribution { worktree: gone, repo_root, exists: false });
        }
        let mut manifest_dir: Option<PathBuf> = None;
        for d in cwd.ancestors() {
            if d == root {
                break;
            }
            let git = d.join(".git");
            if git.is_dir() {
                return Some(Attribution { worktree: d.into(), repo_root: d.into(), exists: true });
            }
            if git.is_file() {
                let repo_root = linked_worktree_main(&git).unwrap_or_else(|| d.to_path_buf());
                return Some(Attribution { worktree: d.into(), repo_root, exists: true });
            }
            if manifest_dir.is_none() && d.join(lease::MANIFEST).is_file() {
                manifest_dir = Some(d.into());
            }
        }
        // Not in git: the manifest's dir, else the top-level dir under the root.
        let dir = manifest_dir.unwrap_or_else(|| {
            let rel = cwd.strip_prefix(&root).unwrap();
            root.join(rel.components().next().unwrap())
        });
        Some(Attribution { worktree: dir.clone(), repo_root: dir, exists: true })
    }

    /// Idle tracking. Returns seconds continuously below `idle_cpu_pct`.
    fn track_idle(&mut self, key: ProcKey, cpu_now: f32, cpu_ns: u64, started: SystemTime, now: SystemTime) -> Option<u64> {
        let thresh = self.config.stale.idle_cpu_pct;
        if cpu_now >= thresh {
            self.idle_since.remove(&key);
            return Some(0);
        }
        let since = *self.idle_since.entry(key).or_insert_with(|| {
            // First sight: if the lifetime average is also idle, assume it
            // has been idle since it started (the CLI's approximation).
            if lifetime_pct(cpu_ns, started, now) < thresh as f64 {
                started
            } else {
                now
            }
        });
        Some(now.duration_since(since).unwrap_or_default().as_secs())
    }

    fn stale_reason(&self, uptime: Duration, idle_secs: Option<u64>, footprint: u64) -> Option<StaleReason> {
        let s = &self.config.stale;
        let too_old = uptime > s.after;
        let idle = idle_secs.is_some_and(|i| i >= s.idle_for.as_secs()) && footprint >= s.min_footprint;
        match (too_old, idle) {
            (true, true) => Some(StaleReason::Both),
            (true, false) => Some(StaleReason::TooOld),
            (false, true) => Some(StaleReason::Idle),
            (false, false) => None,
        }
    }

    fn crawled_locks(&mut self) -> &Vec<(PathBuf, LockData)> {
        let fresh = self.locks.as_ref().is_some_and(|(t, _)| t.elapsed() < LOCK_CRAWL_TTL);
        if !fresh {
            let locks = lease::find_locks_in(&self.roots)
                .into_iter()
                .filter_map(|p| lease::read_lock(&p).map(|d| (p, d)))
                .collect();
            self.locks = Some((Instant::now(), locks));
        }
        &self.locks.as_ref().unwrap().1
    }

    fn cached_containers(&mut self) -> Vec<Container> {
        let fresh = self.containers.as_ref().is_some_and(|(t, _)| t.elapsed() < DOCKER_TTL);
        if !fresh {
            self.containers = Some((Instant::now(), live::containers()));
        }
        self.containers.as_ref().unwrap().1.clone()
    }
}

fn lifetime_pct(cpu_ns: u64, started: SystemTime, now: SystemTime) -> f64 {
    let up = now.duration_since(started).unwrap_or_default().as_nanos().max(1) as f64;
    cpu_ns as f64 / up * 100.0
}

fn lsof_by_pid() -> HashMap<i32, Vec<u16>> {
    let mut out: HashMap<i32, Vec<u16>> = HashMap::new();
    for l in live::lsof_listeners() {
        if let Ok(port) = u16::try_from(l.port) {
            let v = out.entry(l.pid).or_default();
            if !v.contains(&port) {
                v.push(port);
            }
        }
    }
    out
}

fn is_system_exe(exe: &str) -> bool {
    // Simulator runtimes ship their own copy of the OS's daemons
    // (siriactionsd, ...), under /Library or ~/Library/Developer.
    SYSTEM_PREFIXES.iter().any(|p| exe.starts_with(p)) || exe.contains("/Developer/CoreSimulator/")
}

/// Docker's own processes publish container ports; containers are listed
/// separately. Portman's app shouldn't list itself either.
fn is_ignored_app(exe: &str) -> bool {
    exe.contains("/Docker.app/") || exe.contains("/Portman.app/") || exe.ends_with("/portman-app")
}

/// Outermost app bundle in an executable path: `Firefox` for
/// `/Applications/Firefox.app/Contents/MacOS/plugin-container.app/...`.
/// Bundles without the `.app` suffix (Steam's `Steam/Contents/MacOS/...`)
/// are recognized by their `Contents/MacOS` layout.
pub fn app_bundle_name(exe: &str) -> Option<String> {
    let parts: Vec<&str> = exe.split('/').collect();
    if let Some(c) = parts.iter().find(|c| c.ends_with(".app") && c.len() > 4) {
        return Some(c.trim_end_matches(".app").to_string());
    }
    parts
        .windows(3)
        .find(|w| w[1] == "Contents" && w[2] == "MacOS" && !w[0].is_empty())
        .map(|w| w[0].to_string())
}

/// Temp dirs: where orphaned ad-hoc servers (agent scratchpads) live. A
/// server elsewhere outside the crawl roots — a project in some other
/// folder, a Homebrew service — is shown but never reaped in bulk.
fn is_temp_dir(cwd: &Path) -> bool {
    ["/private/tmp", "/tmp", "/private/var/folders"].iter().any(|t| cwd.starts_with(t))
}

/// Stdio MCP servers (spawned by agents) aren't dev servers.
fn is_mcp_like(cmdline: &str) -> bool {
    let c = cmdline.to_ascii_lowercase();
    c.contains("-mcp") || c.contains("_mcp") || c.contains("mcp-") || c.contains("mcp_") || c.contains("/mcp")
        || c.split_whitespace().any(|w| w == "mcp")
}

fn heavy_hitters(procs: &[Proc], by_pid: &HashMap<i32, &Proc>, dev_pids: &HashSet<i32>) -> Vec<AppUsage> {
    let docker_running = procs.iter().any(|p| p.exe.contains("/Docker.app/"));
    let mut buckets: HashMap<String, (u64, u32)> = HashMap::new();
    for p in procs {
        if p.footprint == 0 || dev_pids.contains(&p.pid) {
            continue;
        }
        let bucket = if p.exe.contains("com.apple.Virtualization.VirtualMachine") {
            if docker_running { "Docker VM".to_string() } else { "Virtual machine".to_string() }
        } else if let Some(app) = app_bundle_name(&p.exe) {
            app
        } else {
            owning_app(p, by_pid).unwrap_or_else(|| tool_name(p))
        };
        let e = buckets.entry(bucket).or_default();
        e.0 += p.footprint;
        e.1 += 1;
    }
    let mut out: Vec<AppUsage> = buckets
        .into_iter()
        .map(|(app, (footprint_bytes, process_count))| AppUsage { app, footprint_bytes, process_count })
        .collect();
    out.sort_by(|a, b| b.footprint_bytes.cmp(&a.footprint_bytes).then_with(|| a.app.cmp(&b.app)));
    out.truncate(HEAVY_HITTERS);
    out
}

/// Display name for a non-app process. Self-updating CLIs install binaries
/// named after their version (`~/.local/share/claude/versions/2.1.280`), so
/// a version-like name falls back to the nearest meaningful path component.
fn tool_name(p: &Proc) -> String {
    let is_versionish = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-');
    if !is_versionish(&p.comm) {
        return p.comm.clone();
    }
    Path::new(&p.exe)
        .components()
        .rev()
        .skip(1)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .find(|c| !is_versionish(c) && !["versions", "bin", "libexec"].contains(&c.as_str()))
        .unwrap_or_else(|| p.comm.clone())
}

/// Walk ppid to the first ancestor inside a `.app`, stopping at shells so
/// terminal-launched tools aren't charged to the terminal.
fn owning_app(p: &Proc, by_pid: &HashMap<i32, &Proc>) -> Option<String> {
    let mut pid = p.ppid;
    for _ in 0..16 {
        if pid <= 1 {
            return None;
        }
        let a = by_pid.get(&pid)?;
        if SHELLS.contains(&a.comm.as_str()) {
            return None;
        }
        if let Some(app) = app_bundle_name(&a.exe) {
            return Some(app);
        }
        pid = a.ppid;
    }
    None
}

/// For a linked worktree's `.git` file (`gitdir: <main>/.git/worktrees/<x>`),
/// the main worktree. Submodules (`.git/modules/...`) are their own project.
fn linked_worktree_main(git_file: &Path) -> Option<PathBuf> {
    let gitdir = gitdir_of(git_file)?;
    let parent = gitdir.parent()?;
    if parent.file_name()? != "worktrees" {
        return None;
    }
    let dot_git = parent.parent()?;
    (dot_git.file_name()? == ".git").then(|| dot_git.parent().map(Path::to_path_buf))?
}

fn gitdir_of(git_file: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(git_file).ok()?;
    let raw = text.trim().strip_prefix("gitdir:")?.trim();
    let p = PathBuf::from(raw);
    Some(if p.is_absolute() { p } else { git_file.parent()?.join(p) })
}

pub fn git_branch(worktree: &Path) -> Option<String> {
    let git = worktree.join(".git");
    let head = if git.is_dir() { git.join("HEAD") } else { gitdir_of(&git)?.join("HEAD") };
    let text = fs::read_to_string(head).ok()?;
    let text = text.trim();
    match text.strip_prefix("ref: ") {
        Some(r) => Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string()),
        None => Some(text.chars().take(7).collect()), // detached: short sha
    }
}

fn project_name(repo_root: &Path) -> String {
    if let Ok(m) = lease::load_manifest(&repo_root.join(lease::MANIFEST)) {
        return m.project;
    }
    if let Some(d) = lease::read_lock(&repo_root.join(lease::LOCK)) {
        if let Some(p) = d.get("project").and_then(|v| v.as_str()) {
            return p.to_string();
        }
    }
    repo_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "?".into())
}

fn lease_for(a: &Attribution) -> Option<Lease> {
    if !a.exists {
        return None;
    }
    let lock = a.worktree.join(lease::LOCK);
    if let Some(data) = lease::read_lock(&lock) {
        return Some(Lease::from_lock(&lock, &data));
    }
    // A primary clone never claims; it implicitly holds slot 0.
    if a.worktree == a.repo_root {
        let m = lease::load_manifest(&a.worktree.join(lease::MANIFEST)).ok()?;
        return Some(Lease::primary(&m, &a.worktree));
    }
    None
}

fn lease_match(lease: Option<&Lease>, listening: &[u16]) -> LeaseMatch {
    let Some(lease) = lease else { return LeaseMatch::NoLease };
    match listening.iter().find(|p| !lease.ports().any(|lp| lp == **p)) {
        Some(&p) => LeaseMatch::OffLease(p),
        None => LeaseMatch::OnLease,
    }
}

fn wt_footprint(w: &Worktree) -> u64 {
    w.processes.iter().map(|p| p.footprint_bytes).sum()
}

fn proj_footprint(p: &Project) -> u64 {
    p.worktrees.iter().map(wt_footprint).sum()
}

fn summarize(s: &Snapshot, config: &Config) -> Summary {
    let devs: Vec<&DevProcess> = s.dev_processes().filter(|d| d.stoppable).collect();
    let stale = s.stale();
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let mut level = s.vitals.pressure;
    let warn = !stale.is_empty()
        || gb(s.vitals.swap_used) > config.alerts.swap_gb
        || gb(s.vitals.disk_free) < config.alerts.disk_free_gb;
    if warn {
        level = level.max(PressureLevel::Warn);
    }
    if gb(s.vitals.disk_free) < config.alerts.disk_free_gb / 2.0 {
        level = PressureLevel::Critical;
    }
    Summary {
        dev_count: devs.len() as u32,
        dev_footprint: devs.iter().map(|d| d.footprint_bytes).sum(),
        stale_count: stale.len() as u32,
        stale_footprint: stale.iter().map(|d| d.footprint_bytes).sum(),
        level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_names() {
        assert_eq!(
            app_bundle_name("/Applications/Firefox.app/Contents/MacOS/plugin-container.app/Contents/MacOS/plugin-container").as_deref(),
            Some("Firefox")
        );
        assert_eq!(app_bundle_name("/usr/local/bin/node"), None);
        assert_eq!(
            app_bundle_name("/Users/x/Library/Application Support/Steam/Steam.AppBundle/Steam/Contents/MacOS/steam_osx").as_deref(),
            Some("Steam")
        );
        assert!(is_mcp_like("/x/bin/python /x/bin/elevenlabs-mcp"));
        let p = Proc {
            pid: 2,
            ppid: 1,
            uid: 0,
            comm: "2.1.280".into(),
            exe: "/Users/x/.local/share/claude/versions/2.1.280".into(),
            started_at: SystemTime::now(),
            footprint: 1,
            cpu_ns: None,
        };
        assert_eq!(tool_name(&p), "claude");
        assert!(!is_mcp_like("node next dev"));
    }

    #[test]
    fn lease_matching() {
        let lease = Lease {
            project: "p".into(),
            slot: 1,
            path: "/x".into(),
            lock_path: None,
            services: vec![crate::lease::LeaseService { name: "web".into(), env: "PORT".into(), port: 3010, url_var: None, url: None }],
        };
        assert_eq!(lease_match(Some(&lease), &[3010]), LeaseMatch::OnLease);
        assert_eq!(lease_match(Some(&lease), &[3010, 3002]), LeaseMatch::OffLease(3002));
        assert_eq!(lease_match(None, &[3010]), LeaseMatch::NoLease);
    }

    #[test]
    fn attributes_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("polaris");
        fs::create_dir_all(main.join(".git/worktrees/chris")).unwrap();
        fs::create_dir_all(main.join("apps/web")).unwrap();
        fs::create_dir_all(main.join(".worktrees")).unwrap();
        let wt = root.join("polaris-chris");
        fs::create_dir_all(wt.join("apps/web")).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/chris").display())).unwrap();
        fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(main.join(".git/worktrees/chris/HEAD"), "ref: refs/heads/feat/x\n").unwrap();

        let mut cfg = Config::default();
        cfg.crawl_roots = Some(vec![root.to_string_lossy().into_owned()]);
        let s = Sampler::new(cfg);

        let a = s.attribute(&main.join("apps/web")).unwrap();
        assert_eq!((a.worktree.as_path(), a.repo_root.as_path()), (main.as_path(), main.as_path()));
        let b = s.attribute(&wt.join("apps/web")).unwrap();
        assert_eq!((b.worktree.as_path(), b.repo_root.as_path()), (wt.as_path(), main.as_path()));
        assert_eq!(git_branch(&wt).as_deref(), Some("feat/x"));
        assert_eq!(git_branch(&main).as_deref(), Some("main"));

        // Deleted linked worktree inside the main repo still maps to polaris.
        let gone = s.attribute(&main.join(".worktrees/old/apps/web")).unwrap();
        assert_eq!(gone.worktree, main.join(".worktrees/old"));
        assert_eq!(gone.repo_root, main);
        assert!(!gone.exists);

        // Deleted sibling worktree is its own project.
        let gone = s.attribute(&root.join("mycelium-wt/frontend")).unwrap();
        assert_eq!(gone.worktree, root.join("mycelium-wt"));
        assert!(s.attribute(Path::new("/private/tmp/x")).is_none());
        assert!(s.attribute(&root).is_none());
    }
}
