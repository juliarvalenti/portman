//! `portman` — drop-in replacement for the Python script, plus `ps`, `stop`,
//! `reap`, and `vitals`.
//!
//! `claim`, `env`, `show` and `ls` keep their exact output (golden-tested
//! against the Python script); `ls` only gains trailing MEM / UP columns.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use indexmap::IndexMap;
use portman_core::config::parse_duration;
use portman_core::lease::{self, pad, LOCK};
use portman_core::stop::{self, StopResult, GRACE};
use portman_core::{darwin, fmt, live, Config, DevProcess, LeaseMatch, Sampler, Snapshot, StaleReason};

const HELP: &str = "portman — per-worktree dev-server port leases, reconciled against reality.

The filesystem is the lease table. A worktree claims a slot by writing a
`.ports.lock` next to its `.ports.toml` manifest; it frees the slot by ceasing
to exist (delete the worktree, the lock goes with it). There is no central
registry and no `release` command — a deleted directory has no lock and no live
process, so its ports vanish from the inventory for free.

Allocation reconciles three live sources instead of trusting stored state:
  - lsof   : what is actually bound right now
  - docker : running container host ports (with names)
  - crawl  : every .ports.lock on the laptop = reservations, running or not

Commands:
  portman claim     Idempotent. Reuse this worktree's lock, else allocate the
                    lowest free slot, write .ports.lock, print the assignment.
  portman env       Emit `export VAR=port` lines (claims first if needed).
                    Use as: eval \"$(portman env)\" && pnpm dev
  portman ls        Whole-laptop inventory: every lock + what's actually live.
  portman show      This worktree's current assignment.

  portman ps [--all] [--json]
                    Running dev servers by project → worktree: PID, NAME,
                    PORTS, MEM (true footprint), CPU, UP, STALE.
  portman stop <target> [--force]
                    target: pid | :port | worktree path | \".\" (this worktree).
                    SIGTERM, wait 5 s, then SIGKILL only with --force or a y/N.
  portman reap [--older-than 48h] [--idle] [--yes] [--force] [--json]
                    List stale servers and the memory they'd free, then stop
                    them (asks y/N; a dry run when not a terminal and no --yes).
  portman vitals [--json]
                    Memory pressure, RAM/compressed, swap, disk free.

Config:
  ~/.config/portman/config.toml (stale thresholds, crawl roots, alerts).
  PORTMAN_ROOTS     Colon-separated dirs to crawl for locks; overrides config.
                    Default: ~/Documents/GitHub
";

const COMMANDS: &[&str] = &["claim", "env", "ls", "show", "ps", "stop", "reap", "vitals"];

/// `ps` samples CPU over this window; `reap` uses the spec's 3 s.
const PS_WINDOW: Duration = Duration::from_millis(1000);
const REAP_WINDOW: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    };
    let rest = &args[1..];
    let result = match cmd.as_str() {
        "-h" | "--help" => {
            println!("{HELP}");
            Ok(())
        }
        "claim" => cmd_claim(),
        "env" => cmd_env(),
        "show" => cmd_show(),
        "ls" => cmd_ls(),
        "ps" => cmd_ps(rest),
        "stop" => cmd_stop(rest),
        "reap" => cmd_reap(rest),
        "vitals" => cmd_vitals(rest),
        other => Err(format!("unknown command '{other}' — one of: {}", COMMANDS.join(", "))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("portman: {msg}");
            }
            ExitCode::FAILURE
        }
    }
}

type CmdResult = Result<(), String>;

fn cwd() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))
}

// ── lease commands (Python parity) ───────────────────────────────────────────

fn cmd_claim() -> CmdResult {
    let (lock_path, data, created) = lease::get_or_claim(&cwd()?).map_err(|e| e.0)?;
    if created {
        let root = lock_path.parent().unwrap_or(&lock_path);
        eprintln!("portman: claimed slot {} for {} → {}", lease::py_get(&data, "slot"), lease::py_get(&data, "project"), root.display());
    }
    print!("{}", lease::format_assignment(&data));
    Ok(())
}

fn cmd_env() -> CmdResult {
    let (_, data, _) = lease::get_or_claim(&cwd()?).map_err(|e| e.0)?;
    println!("{}", lease::export_lines(&data).join("\n"));
    Ok(())
}

fn cmd_show() -> CmdResult {
    let (_, lease_dir) = lease::resolve_lease(&cwd()?).map_err(|e| e.0)?;
    let Some(data) = lease::read_lock(&lease_dir.join(LOCK)) else {
        eprintln!("portman: no .ports.lock here yet — run `portman claim`");
        return Err(String::new());
    };
    print!("{}", lease::format_assignment(&data));
    Ok(())
}

fn cmd_ls() -> CmdResult {
    let listeners = live::lsof_listeners();
    let mut live_ports = indexmap_from(&listeners);
    let docker = live::docker_ports();
    for (port, who) in &docker {
        live_ports.insert(*port, who.clone());
    }
    let pid_of = |port: i64| -> Option<i32> {
        if docker.contains_key(&port) {
            return None;
        }
        listeners.iter().find(|l| l.port == port).map(|l| l.pid)
    };

    let mut locks = lease::find_locks();
    locks.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    if locks.is_empty() {
        let roots: Vec<String> = lease::crawl_roots().iter().map(|r| r.display().to_string()).collect();
        eprintln!("portman: no .ports.lock files under {}", roots.join(":"));
    }
    let mut seen: Vec<serde_json::Value> = Vec::new();
    let mut out = String::new();
    for lp in &locks {
        let Some(data) = lease::read_lock(lp) else { continue };
        let path = data.get("path").map(lease::py_str).unwrap_or_else(|| lp.parent().unwrap().display().to_string());
        out.push_str(&format!("\n{}  slot {}  {}\n", lease::py_get(&data, "project"), lease::py_get(&data, "slot"), path));
        for (name, svc) in lease::services(&data) {
            let port = svc.get("port").cloned().unwrap_or(serde_json::Value::Null);
            seen.push(port.clone());
            let port_i = port.as_i64();
            let owner = port_i.and_then(|p| live_ports.get(&p));
            let state = match owner {
                Some(o) => format!("LIVE  {o}"),
                None => "idle".to_string(),
            };
            let line = format!("    {} {} {}", pad(name, 12), pad(&lease::py_str(&port), 7), state);
            out.push_str(&with_usage(line, port_i.and_then(pid_of)));
            out.push('\n');
        }
    }
    let mut orphans: Vec<(&i64, &String)> =
        live_ports.iter().filter(|(p, _)| !seen.iter().any(|s| s.as_i64() == Some(**p))).collect();
    orphans.sort_by_key(|(p, _)| **p);
    if !orphans.is_empty() {
        out.push_str("\nlive ports with no lease:\n");
        for (p, who) in orphans {
            let line = format!("    {} {} {}", pad("?", 12), pad(&p.to_string(), 7), who);
            out.push_str(&with_usage(line, pid_of(*p)));
            out.push('\n');
        }
    }
    print!("{out}");
    Ok(())
}

/// port -> `command(pid)`, first listener wins (Python's `setdefault`).
fn indexmap_from(listeners: &[live::LsofListener]) -> IndexMap<i64, String> {
    let mut m = IndexMap::new();
    for l in listeners {
        m.entry(l.port).or_insert_with(|| format!("{}({})", l.command, l.pid));
    }
    m
}

/// The Python line, then trailing MEM / UP for live, non-docker owners.
fn with_usage(line: String, pid: Option<i32>) -> String {
    let Some(pid) = pid else { return line };
    let (Some(info), Some(ru)) = (darwin::bsd_info(pid), darwin::rusage(pid)) else { return line };
    let up = std::time::SystemTime::now().duration_since(info.started_at).unwrap_or_default().as_secs();
    format!("{}  {:>8}  {:>4}", pad(&line, 52), fmt::bytes(ru.footprint), fmt::duration(up))
}

// ── ps ───────────────────────────────────────────────────────────────────────

struct Flags {
    all: bool,
    json: bool,
    force: bool,
    yes: bool,
    idle: bool,
    older_than: Option<Duration>,
    positional: Vec<String>,
}

fn parse_flags(args: &[String], allowed: &[&str]) -> Result<Flags, String> {
    let mut f = Flags { all: false, json: false, force: false, yes: false, idle: false, older_than: None, positional: Vec::new() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        if name.starts_with('-') && name.len() > 1 && !allowed.contains(&name) {
            return Err(format!("unknown option {name}"));
        }
        match name {
            "--all" | "-a" => f.all = true,
            "--json" => f.json = true,
            "--force" | "-f" => f.force = true,
            "--yes" | "-y" => f.yes = true,
            "--idle" => f.idle = true,
            "--older-than" => {
                let v = inline.or_else(|| it.next().cloned()).ok_or("--older-than needs a duration like 48h")?;
                f.older_than = Some(parse_duration(&v).ok_or(format!("bad duration {v:?}"))?);
            }
            _ => f.positional.push(a.clone()),
        }
    }
    Ok(f)
}

fn cmd_ps(args: &[String]) -> CmdResult {
    let f = parse_flags(args, &["--all", "-a", "--json"])?;
    let snap = Sampler::sample_over(Config::load(), PS_WINDOW);
    if f.json {
        return print_json(&snap);
    }
    print!("{}", render_ps(&snap, f.all));
    Ok(())
}

fn print_json<T: serde::Serialize>(v: &T) -> CmdResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    println!("{s}");
    Ok(())
}

const PS_HEADER: &str = "      PID  NAME            PORTS            MEM   CPU    UP  STALE";

fn ps_row(d: &DevProcess, indent: &str) -> String {
    let ports = d
        .listening
        .iter()
        .map(|p| match d.lease_match {
            // `!` marks a port bound outside the worktree's lease.
            LeaseMatch::OffLease(off) if off == *p => format!(":{p}!"),
            _ => format!(":{p}"),
        })
        .collect::<Vec<_>>()
        .join(",");
    let stale = match d.stale {
        Some(StaleReason::TooOld) => "STALE (old)",
        Some(StaleReason::Idle) => "STALE (idle)",
        Some(StaleReason::Both) => "STALE (old, idle)",
        None => "",
    };
    let app = match (&d.app_bundle, d.stoppable) {
        (Some(app), _) => format!("  [{app}.app]"),
        _ => String::new(),
    };
    format!(
        "{indent}{:>7}  {} {} {:>8} {:>4.0}% {:>5}  {stale}{app}",
        d.pid,
        pad(&truncate(&d.name, 15), 15),
        pad(&truncate(if ports.is_empty() { "-" } else { &ports }, 15), 15),
        fmt::bytes(d.footprint_bytes),
        d.cpu_pct,
        fmt::duration(d.uptime_secs),
    )
    .trim_end()
    .to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

fn render_ps(snap: &Snapshot, all: bool) -> String {
    let mut out = String::new();
    let mut any = false;
    out.push_str(PS_HEADER);
    out.push('\n');
    for p in &snap.projects {
        let worktrees: Vec<_> = p.worktrees.iter().filter(|w| all || !w.processes.is_empty()).collect();
        if worktrees.is_empty() {
            continue;
        }
        let total: u64 = p.worktrees.iter().flat_map(|w| &w.processes).map(|d| d.footprint_bytes).sum();
        out.push_str(&format!("\n{}  {}\n", p.name, fmt::bytes(total)));
        for w in worktrees {
            let name = w.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut meta = Vec::new();
            if let Some(b) = &w.branch {
                meta.push(b.clone());
            }
            if let Some(l) = &w.lease {
                meta.push(format!("slot {}", l.slot));
            }
            let meta = if meta.is_empty() { String::new() } else { format!("  ({})", meta.join(" · ")) };
            let flag = if !w.exists { "  DELETED WORKTREE" } else if w.processes.is_empty() { "  idle — no processes" } else { "" };
            out.push_str(&format!("  {name}{meta}{flag}\n"));
            for d in &w.processes {
                out.push_str(&ps_row(d, "  "));
                out.push('\n');
                any = true;
            }
        }
    }
    let unattributed: Vec<_> = snap.unattributed.iter().filter(|d| all || d.stoppable).collect();
    if !unattributed.is_empty() {
        out.push_str(&format!("\nunattributed  ({} outside the crawl roots)\n", unattributed.len()));
        for d in unattributed {
            out.push_str(&ps_row(d, "  "));
            out.push('\n');
            out.push_str(&format!("               cwd {}\n", d.cwd.display()));
            any = true;
        }
    }
    if all {
        if !snap.containers.is_empty() {
            out.push_str("\ncontainers\n");
            for c in &snap.containers {
                let ports: Vec<String> = c.ports.iter().map(|(h, ct)| format!(":{h}->{ct}")).collect();
                out.push_str(&format!("  {} {} {}\n", pad(&c.name, 28), pad(&c.image, 32), ports.join(",")));
            }
        }
        if !snap.heavy_hitters.is_empty() {
            let hh: Vec<String> = snap.heavy_hitters.iter().map(|a| format!("{} {}", a.app, fmt::bytes(a.footprint_bytes))).collect();
            out.push_str(&format!("\nheavy hitters  {}\n", hh.join(" · ")));
        }
    }
    if !any {
        out.push_str("\nno dev servers running\n");
    }
    let s = &snap.summary;
    out.push_str(&format!("\n{} dev server{} · {}", s.dev_count, if s.dev_count == 1 { "" } else { "s" }, fmt::bytes(s.dev_footprint)));
    if s.stale_count > 0 {
        out.push_str(&format!(" · {} stale ({}) — `portman reap`", s.stale_count, fmt::bytes(s.stale_footprint)));
    }
    out.push_str(&format!("\n{}\n", vitals_line(snap)));
    out
}

fn vitals_line(snap: &Snapshot) -> String {
    let v = &snap.vitals;
    format!(
        "Pressure {:?} · Swap {}/{} GB · Disk {} GB free",
        v.pressure,
        fmt::gb(v.swap_used),
        fmt::gb(v.swap_total),
        fmt::gb(v.disk_free)
    )
    .replace("Normal", "normal")
    .replace("Warn", "warn")
    .replace("Critical", "critical")
}

// ── stop ─────────────────────────────────────────────────────────────────────

fn cmd_stop(args: &[String]) -> CmdResult {
    let f = parse_flags(args, &["--force", "-f"])?;
    let [target] = f.positional.as_slice() else {
        return Err("usage: portman stop <pid | :port | worktree path | .> [--force]".into());
    };
    let snap = Sampler::new(Config::load()).sample();
    let targets = resolve_targets(&snap, target)?;
    let mut failed = false;
    for (label, pids, name, footprint) in targets {
        eprintln!("stopping {name} (pid {}, {}) {label}", pids[0], fmt::bytes(footprint));
        let res = stop_interactive(&pids, f.force);
        failed |= !report_stop(&name, footprint, &res);
    }
    if failed {
        Err(String::new())
    } else {
        Ok(())
    }
}

/// (label, pids, name, footprint) for each server a target names.
fn resolve_targets(snap: &Snapshot, target: &str) -> Result<Vec<(String, Vec<i32>, String, u64)>, String> {
    let row = |d: &DevProcess, label: String| (label, d.pids.clone(), d.name.clone(), d.footprint_bytes);
    let labeled: Vec<(String, &DevProcess)> = snap
        .labeled()
        .into_iter()
        .map(|(l, _, d)| (format!("in {l}"), d))
        .chain(snap.unattributed.iter().map(|d| (format!("in {}", d.cwd.display()), d)))
        .collect();

    if let Ok(pid) = target.parse::<i32>() {
        if let Some((l, d)) = labeled.iter().find(|(_, d)| d.pid == pid) {
            return Ok(vec![row(d, l.clone())]);
        }
        if let Some((l, d)) = labeled.iter().find(|(_, d)| d.pids.contains(&pid)) {
            // A member of a server tree: stop just that process.
            let name = darwin::bsd_info(pid).map(|i| i.comm).unwrap_or_default();
            let fp = darwin::rusage(pid).map(|r| r.footprint).unwrap_or(0);
            return Ok(vec![(format!("({} member) {l}", d.name), vec![pid], name, fp)]);
        }
        stop::check_stoppable(pid)?;
        let name = darwin::bsd_info(pid).map(|i| i.comm).unwrap_or_default();
        let fp = darwin::rusage(pid).map(|r| r.footprint).unwrap_or(0);
        return Ok(vec![(String::new(), vec![pid], name, fp)]);
    }
    if let Some(port) = target.strip_prefix(':') {
        let port: u16 = port.parse().map_err(|_| format!("bad port {target:?}"))?;
        let hits: Vec<_> = labeled.iter().filter(|(_, d)| d.listening.contains(&port)).map(|(l, d)| row(d, l.clone())).collect();
        if hits.is_empty() {
            if let Some(c) = snap.containers.iter().find(|c| c.ports.iter().any(|(h, _)| *h == port)) {
                return Err(format!(":{port} is container {} — use `docker stop {}`", c.name, c.name));
            }
            return Err(format!("nothing of yours is listening on :{port}"));
        }
        return Ok(hits);
    }
    let dir = if target == "." {
        let here = cwd()?;
        lease::git_worktree_dirs(&here).map(|(top, _)| top).unwrap_or(here)
    } else {
        Path::new(target).canonicalize().map_err(|e| format!("{target}: {e}"))?
    };
    let mut hits = Vec::new();
    for p in &snap.projects {
        for w in &p.worktrees {
            for d in &w.processes {
                if w.path == dir || w.path.starts_with(&dir) || d.cwd.starts_with(&dir) {
                    hits.push(row(d, format!("in {}", portman_core::label(p, w, d))));
                }
            }
        }
    }
    for d in &snap.unattributed {
        if d.cwd.starts_with(&dir) {
            hits.push(row(d, format!("in {}", d.cwd.display())));
        }
    }
    if hits.is_empty() {
        return Err(format!("no dev servers running in {}", dir.display()));
    }
    Ok(hits)
}

fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// SIGTERM; if it survives the grace period, SIGKILL with --force or on a y.
fn stop_interactive(pids: &[i32], force: bool) -> StopResult {
    let res = stop::stop_pids(pids, force, GRACE);
    if res.error.is_some() || res.still_running.is_empty() || force {
        return res;
    }
    if is_tty() && confirm(&format!("still running after {}s — send SIGKILL?", GRACE.as_secs())) {
        return stop::kill_pids(&res.still_running);
    }
    res
}

/// Print the outcome; false on failure.
fn report_stop(name: &str, footprint: u64, res: &StopResult) -> bool {
    if let Some(e) = &res.error {
        eprintln!("portman: {e}");
        return false;
    }
    if res.stopped {
        let how = if res.killed { "killed" } else { "stopped" };
        println!("{how} {name} — freed ~{}", fmt::bytes(footprint));
        return true;
    }
    let pids: Vec<String> = res.still_running.iter().map(|p| p.to_string()).collect();
    eprintln!("portman: {name} still running (pid {}) — retry with --force", pids.join(", "));
    false
}

// ── reap ─────────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct ReapReport<'a> {
    dry_run: bool,
    candidates: Vec<&'a DevProcess>,
    total_footprint: u64,
    results: Vec<StopResult>,
}

fn cmd_reap(args: &[String]) -> CmdResult {
    let f = parse_flags(args, &["--older-than", "--idle", "--yes", "-y", "--force", "-f", "--json"])?;
    let mut config = Config::load();
    if let Some(d) = f.older_than {
        config.stale.after = d;
    }
    let snap = Sampler::sample_over(config, REAP_WINDOW);
    let labels: Vec<(String, &DevProcess)> = snap
        .labeled()
        .into_iter()
        .map(|(l, _, d)| (l, d))
        .chain(snap.unattributed.iter().map(|d| (d.cwd.display().to_string(), d)))
        .filter(|(_, d)| d.reapable && d.stale.is_some())
        .filter(|(_, d)| !f.idle || matches!(d.stale, Some(StaleReason::Idle | StaleReason::Both)))
        .collect();
    let total: u64 = labels.iter().map(|(_, d)| d.footprint_bytes).sum();

    if !f.json {
        if labels.is_empty() {
            println!("nothing stale");
            return Ok(());
        }
        println!("{PS_HEADER}");
        for (label, d) in &labels {
            println!("{}   {label}", ps_row(d, "  "));
        }
        println!("\n{} stale · {} would be freed", labels.len(), fmt::bytes(total));
    }

    let go = if f.yes {
        true
    } else if !f.json && is_tty() && !labels.is_empty() {
        confirm(&format!("stop {} server{}?", labels.len(), if labels.len() == 1 { "" } else { "s" }))
    } else {
        if !f.json && !labels.is_empty() {
            println!("dry run — pass --yes to stop them");
        }
        false
    };

    let results: Vec<StopResult> = if go && !labels.is_empty() {
        // Stop in parallel so N servers don't cost N × the grace period.
        std::thread::scope(|s| {
            let handles: Vec<_> =
                labels.iter().map(|(_, d)| s.spawn(|| stop::stop_pids(&d.pids, f.force, GRACE))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        })
    } else {
        Vec::new()
    };

    if f.json {
        return print_json(&ReapReport {
            dry_run: !go,
            candidates: labels.iter().map(|(_, d)| *d).collect(),
            total_footprint: total,
            results,
        });
    }
    let mut ok = true;
    for ((_, d), res) in labels.iter().zip(&results) {
        ok &= report_stop(&d.name, d.footprint_bytes, res);
    }
    if ok {
        Ok(())
    } else {
        Err(String::new())
    }
}

// ── vitals ───────────────────────────────────────────────────────────────────

fn cmd_vitals(args: &[String]) -> CmdResult {
    let f = parse_flags(args, &["--json"])?;
    let v = darwin::vitals();
    if f.json {
        return print_json(&v);
    }
    let pressure = format!("{:?}", v.pressure).to_lowercase();
    println!("Pressure  {pressure}");
    println!("RAM       {} / {} GB free  ({} GB compressed)", fmt::gb(v.ram_free), fmt::gb(v.ram_total), fmt::gb(v.compressed));
    println!("Swap      {} / {} GB used", fmt::gb(v.swap_used), fmt::gb(v.swap_total));
    println!("Disk      {} GB free", fmt::gb(v.disk_free));
    Ok(())
}
