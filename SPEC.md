# Portman app — SPEC

Status: **draft** · 2026-10-06

Portman today is a CLI (a Python script at `portman/portman` in the dotfiles
repo, `juliarvalenti/dotfiles`) that hands each worktree a non-colliding block
of dev-server ports. This spec turns it into **a CLI + a
macOS menu-bar app** that also answers the question the CLI can't:

> **What dev servers are running right now, which worktree owns each one, how
> much memory is it really using, and how long has it been up?**

## Why

On 2026-10-05 the laptop sat at 27 GB of swap with 0 GB free RAM. The cause
was not agents or builds. It was **two forgotten `next-server` processes**, one
up 8 days and one up 6 days, each holding ~7 GB. Nothing surfaced them:

- `ps`/RSS showed them at **0.2 GB**. Their memory had been compressed and
  swapped out, so resident size hid it. Their true footprint (what `top`'s MEM
  column and Activity Monitor show) was **6.9 GB and 6.7 GB**.
- Finding them took `top -o mem`, then `ps -o etime`, then `lsof -d cwd` to map
  each PID back to `polaris/apps/web` and `mycelium/mycelium-frontend`.
- Swap lives on disk, so the leak also ate disk, on a machine that has
  hard-locked several times from disk hitting zero.

With a worktree-heavy workflow (~78 worktrees across 8 repos), dev servers get
started in worktrees and forgotten. Portman already knows which worktrees exist
and which ports they lease, so it is the natural place to show what's running.

## Goals

1. **See every dev server**, grouped by project → worktree, with ports, **true
   memory footprint**, CPU, and uptime.
2. **Flag stale servers** (up for days, idle, large) so they get noticed before
   they cause trouble.
3. **Stop them in one click** (app) or one command (CLI).
4. **Show the three machine vitals that predicted every crash**: memory
   pressure, swap used, disk free.
5. **Keep the existing lease model and file formats unchanged.** Every current
   `.ports.toml` / `.ports.lock` keeps working.

## Non-goals

- A general-purpose Activity Monitor. Non-dev processes appear only as a short
  "heavy hitters" list for context.
- Linux/Windows. macOS only (uses Darwin APIs directly).
- **Automatic killing.** Portman never stops a process without an explicit
  user action unless the user turns on auto-reap in config. It's off by default.
- Remote machines.

## Architecture

One Rust core, two front ends.

```
portman/
  crates/
    portman-core/   # leases, crawl, process/port/vitals sampling, stop
    portman-cli/    # bin `portman` — drop-in replacement for the Python script
  app/              # Tauri 2 app; src-tauri depends on portman-core
    src/            # React 19 + Vite + Tailwind 4 (same stack as wow-forever-buddy)
```

- **Port the Python logic to `portman-core`** so the app and CLI share one
  implementation. The app links the core directly, with no shelling out to the
  CLI.
- The Python script stays the installed `portman` until the Rust CLI passes
  parity (see Milestones), then the symlink in the skill's install step moves.

### Data model (`portman-core`)

```rust
struct Snapshot {
    taken_at: SystemTime,
    vitals: Vitals,
    projects: Vec<Project>,        // grouped by manifest/repo
    unattributed: Vec<DevProcess>, // listening, but outside every crawl root
    containers: Vec<Container>,
    heavy_hitters: Vec<AppUsage>,  // top non-dev apps by footprint, for context
}

struct Project   { name: String, repo_root: PathBuf, worktrees: Vec<Worktree> }

struct Worktree {
    path: PathBuf,
    branch: Option<String>,
    lease: Option<Lease>,          // from .ports.lock (slot, port map)
    processes: Vec<DevProcess>,
    exists: bool,                  // false = process running in a deleted worktree
}

struct DevProcess {
    pid: i32,
    name: String,                  // "next-server", "vite", "node", "cargo", ...
    cmdline: String,
    cwd: PathBuf,
    listening: Vec<u16>,
    footprint_bytes: u64,          // phys_footprint — NOT RSS
    cpu_pct: f32,
    started_at: SystemTime,
    stale: Option<StaleReason>,    // TooOld | Idle | Both
    lease_match: LeaseMatch,       // OnLease | OffLease(port) | NoLease
}

struct Vitals {
    pressure: PressureLevel,       // normal / warn / critical
    ram_total: u64, ram_free: u64, compressed: u64,
    swap_used: u64, swap_total: u64,
    disk_free: u64,                // Data volume available space
}

struct Container { name: String, image: String, ports: Vec<(u16, u16)> }
struct AppUsage  { app: String, footprint_bytes: u64, process_count: u32 }
```

### What counts as a dev process

A process owned by the current user is included if **either**:

1. it has a TCP socket in `LISTEN` on a port ≥ 1024, **or**
2. its cwd is inside a crawl root (`~/Documents/GitHub` by default, or
   `PORTMAN_ROOTS`). This catches long-lived non-listening work like
   `cargo watch` and `tsc --watch`.

Attribution: walk the cwd upward to the nearest `.ports.toml` (project +
worktree) or, failing that, the git worktree root. Listening processes with a
cwd outside every root go to **Unattributed**. Both of the 2026-10-05 strays
landed there: the Mycelium.app bundled UI and an orphaned Claude scratchpad
server under `/private/tmp`.

### Measuring (macOS specifics — get these right)

| Signal | Source | Note |
|---|---|---|
| Memory per process | `proc_pid_rusage(RUSAGE_INFO_V4).ri_phys_footprint` | **Do not use RSS** (`sysinfo::Process::memory()`). RSS drops toward zero once pages are compressed or swapped, which is exactly the leak case. |
| CPU per process | `proc_pid_rusage` user+system time, delta between samples | |
| Start time | `proc_pidinfo(PROC_PIDTBSDINFO).pbi_start_tvsec` | |
| cwd | `proc_pidinfo(PROC_PIDVNODEPATHINFO)` | |
| Listening ports | `proc_pidinfo(PROC_PIDLISTFDS)` → `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` | Fall back to `lsof -nP -iTCP -sTCP:LISTEN` if the libproc path fails. `lsof` is ~100s of ms, too slow for every refresh. |
| Containers | `docker ps --format` (existing logic) | Skip silently if Docker isn't running. |
| Docker VM memory | footprint of `com.apple.Virtualization.VirtualMachine` | Shown under Docker in heavy hitters. |
| Heavy hitters | footprint summed per owning `.app` bundle (walk ppid) | So Firefox's ~10 `plugin-container`s read as one "Firefox 19 GB" row. |
| Swap | `sysctl vm.swapusage` | |
| Pressure / compressor | `kern.memorystatus_vm_pressure_level`, `host_statistics64` | |
| Disk free | `statfs("/System/Volumes/Data").f_bavail` | The `/` system volume is sealed and misleading. |

### Stale rule

A process is **stale** if `uptime > stale.after` (default **48 h**) **or** it
has been idle (CPU below `stale.idle_cpu_pct`, default **1%**) for
`stale.idle_for` (default **6 h**) while holding at least `stale.min_footprint`
(default **1 GB**). The app tracks idle time from its rolling samples. The CLI
approximates it from lifetime CPU time ÷ uptime plus a 3 s live sample.

## CLI

Existing commands keep their **exact output** so scripts and the skill don't
break:

```sh
portman claim   # unchanged
portman env     # unchanged
portman show    # unchanged
portman ls      # unchanged columns + new trailing MEM / UP columns
```

New:

```sh
portman ps [--all] [--json]          # dev processes grouped by project → worktree:
                                     # PID, NAME, PORTS, MEM (footprint), CPU, UP, STALE
portman stop <target> [--force]      # target: pid | :port | worktree path | "." (this worktree)
                                     # SIGTERM, wait 5 s, then SIGKILL only with --force or a y/N prompt
portman reap [--older-than 48h] [--idle] [--yes] [--json]
                                     # lists stale processes + total memory they'd free, asks y/N,
                                     # then stops them. Dry-run by default without --yes.
portman vitals [--json]              # pressure, RAM/compressed, swap used/total, disk free
```

`--json` emits the same `Snapshot` shape the app uses, so agents and scripts
can read it.

## App

### Menu bar (always on)

- Icon and title: **`4 · 14.2 GB`** = running dev servers · their total
  footprint. The icon turns amber when anything is stale or a vital is in warn,
  and red when critical.
- Dropdown:
  - one line per dev server: `● polaris/web  :3000  6.7 GB  8d` with a submenu
    **Open in browser / Stop / Reveal worktree / Copy env**
  - **Reap stale (2 · 13.6 GB)…**
  - vitals line: `Swap 26/28 GB · Disk 17 GB free`
  - Open Portman · Quit

### Window

```
┌──────────────────────────────────────────────────────────────────────┐
│ Pressure ● warn   RAM 0.0/26 GB (18.7 compressed)   Swap 26.1/28 GB   │
│ Disk 17 GB free                                    [Reap stale (2)]  │
├──────────────────────────────────────────────────────────────────────┤
│ ▾ polaris                                                             │
│   ▾ polaris  (main · slot 0)                                          │
│      ● next-server  :3000  6.7 GB  0%  8d  STALE      [Open] [Stop]   │
│   ▸ polaris-chris  (feat/x · slot 1)            idle — no processes   │
│ ▾ mycelium                                                            │
│   ▾ mycelium-frontend  (main · slot 0)                                │
│      ● next-server  :3001  6.9 GB  0%  6d  STALE      [Open] [Stop]   │
│   ▸ wt/1147  (slot 3)                    ● next-server :3031 383 MB 2d │
│ ▸ Unattributed (2)                                                    │
│ ▸ Containers (7)                                                      │
│ ▸ Heavy hitters   Firefox 19 GB · Docker VM 9 GB · Steam 0.9 GB      │
└──────────────────────────────────────────────────────────────────────┘
```

- Default sort: footprint, descending. Stale rows get a badge, and the project
  header shows its total footprint.
- Ports are links (`http://localhost:<port>`). A port bound **off-lease** (the
  dagster-hardcoded-port case from the skill's Gotchas) is highlighted.
- A worktree whose directory is gone but still has a running process shows
  **"deleted worktree"**. That's always a leak.
- **Stop** confirms inline with `Stop next-server (6.7 GB)?` and does the same
  SIGTERM → SIGKILL as the CLI.
- Refresh every `refresh_secs` (default 2 s) via a Tauri event pushing a fresh
  `Snapshot`, plus a 30 s footprint history per process for a sparkline.

### Tauri commands

```
get_snapshot() -> Snapshot
stop_process(pid, force: bool) -> StopResult
reap(dry_run: bool) -> Vec<DevProcess>
open_url(port) / reveal_path(path) / copy_env(worktree)
```

### Notifications (opt-out per kind)

- **Stale server:** "polaris/web next-server has been up 3 days, using 6.7 GB",
  with a **Stop** action. At most once per process per day.
- **Disk free < `alerts.disk_free_gb`** (default 10 GB).
- **Swap > `alerts.swap_gb`** (default 20 GB). The message names the top 3
  footprints so the cause is right there.

## Config

`~/.config/portman/config.toml` (all optional):

```toml
crawl_roots   = ["~/Documents/GitHub"]   # PORTMAN_ROOTS still overrides
refresh_secs  = 2
extra_dev_patterns = ["dagster", "uvicorn"]

[stale]
after         = "48h"
idle_for      = "6h"
idle_cpu_pct  = 1.0
min_footprint = "1GB"

[alerts]
disk_free_gb  = 10
swap_gb       = 20

[reap]
auto = false   # if true, the app stops stale processes itself and notifies. Off by default.
```

## Safety

- Signals only processes owned by the current user. It never touches
  root/system processes, the Docker VM, or `.app` processes. Those appear for
  context only and have no Stop button.
- Every stop is explicit unless `reap.auto = true`.
- Stop and reap show exactly what will be stopped and how much memory it frees
  before acting.

## Milestones

1. **Core + CLI parity.** `portman-core` with the lease logic ported from
   Python. Rust `claim/env/show/ls` produce byte-identical output on existing
   worktrees and pass a golden test against the Python script. Swap the
   install symlink.
2. **Process view.** Footprint/CPU/uptime/cwd/ports sampling, attribution,
   stale rule, plus `ps`, `stop`, `reap`, `vitals`.
3. **Menu-bar app.** Tray with count + total, dropdown with Stop/Open, plus
   vitals.
4. **Window + notifications.** Full grouped view, sparklines, stale/disk/swap
   alerts.
5. **Later.** Per-worktree **disk** usage (`target/`, `node_modules`, `.next`)
   with a Clean action, since Rust `target/` dirs reached 15–22 GB per
   worktree. Optionally replace the ad-hoc `memflight.sh` recorder with a
   built-in 1 s fsync'd vitals log.

### Acceptance check for M2–M3

Recreate 2026-10-05: start `next dev` in two worktrees, let them sit, and
artificially age them via config (`stale.after = "1m"`). Portman must:

- list both under the right project/worktree with footprint matching `top`'s
  MEM within ±5%,
- mark both stale and offer **Reap stale (2 · N GB)**,
- stop both, after which swap and footprint totals drop on the next refresh.

## Open questions

- **Rust port vs. wrap Python.** This spec ports to Rust. Wrapping the Python
  script from Tauri would be faster to M3 but leaves two languages and a
  process spawn on every refresh.
- **Docker per-container memory.** `docker stats` is slow (~1–2 s). Is the VM
  total enough, or is per-container worth a slower refresh?
