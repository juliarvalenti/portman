//! Port leases — a line-for-line port of the original Python `portman`.
//!
//! The filesystem is the lease table. A worktree claims a slot by writing a
//! `.ports.lock` next to its `.ports.toml` manifest; it frees the slot by
//! ceasing to exist. There is no central registry and no `release` command.
//!
//! Everything here must stay byte-compatible with the Python script: lock
//! files are read and written by both during the migration, and `claim` /
//! `env` / `show` / `ls` output is golden-tested against it.

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::live;
use crate::Error;

pub const MANIFEST: &str = ".ports.toml";
pub const LOCK: &str = ".ports.lock";

/// Lock data exactly as stored: an ordered JSON object.
pub type LockData = Map<String, Value>;

fn claim_lockfile() -> PathBuf {
    crate::home().join(".cache").join("portman").join("claim.lock")
}

// ── discovery ────────────────────────────────────────────────────────────────

/// Walk up from `start` to the nearest dir containing a `.ports.toml`.
///
/// With `stop`, never look above it — so a nested worktree can't sail past
/// itself into a parent repo's manifest.
pub fn find_manifest(start: &Path, stop: Option<&Path>) -> Option<PathBuf> {
    for d in start.ancestors() {
        if d.join(MANIFEST).is_file() {
            return Some(d.join(MANIFEST));
        }
        if Some(d) == stop {
            break;
        }
    }
    None
}

/// `(worktree_top, main_top)` for a git checkout, else `None`.
///
/// For a linked worktree these differ (the manifest is gitignored and lives
/// only in main_top); for the primary clone they're equal.
pub fn git_worktree_dirs(start: &Path) -> Option<(PathBuf, PathBuf)> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(start).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let worktree_top = PathBuf::from(top);
    let common_dir = PathBuf::from(common);
    let main_top = if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| worktree_top.clone())
    } else {
        worktree_top.clone()
    };
    Some((worktree_top, main_top))
}

/// Locate the `(manifest, lease_dir)` pair for a directory.
///
/// The lease_dir owns the `.ports.lock` and is the current git worktree top,
/// so the lease dies when the worktree is removed. In a linked worktree the
/// gitignored manifest may live only in the main worktree — borrow it there.
/// Outside git, fall back to a plain walk-up.
pub fn resolve_lease(start: &Path) -> Result<(PathBuf, PathBuf), Error> {
    let Some((worktree_top, main_top)) = git_worktree_dirs(start) else {
        let mp = find_manifest(start, None)
            .ok_or_else(|| Error::new(format!("no {MANIFEST} found walking up from {}", start.display())))?;
        let dir = mp.parent().unwrap().to_path_buf();
        return Ok((mp, dir));
    };
    let mut mp = find_manifest(start, Some(&worktree_top));
    if mp.is_none() && main_top.join(MANIFEST).is_file() {
        mp = Some(main_top.join(MANIFEST));
    }
    let mp = mp.ok_or_else(|| {
        Error::new(format!(
            "no {MANIFEST} in worktree {} or main worktree {}",
            worktree_top.display(),
            main_top.display()
        ))
    })?;
    Ok((mp, worktree_top))
}

/// Crawl roots: `PORTMAN_ROOTS` (colon-separated), else the config file's
/// `crawl_roots`, else `~/Documents/GitHub`.
pub fn crawl_roots() -> Vec<PathBuf> {
    crawl_roots_with(&crate::config::Config::load())
}

pub fn crawl_roots_with(config: &crate::config::Config) -> Vec<PathBuf> {
    if let Ok(raw) = std::env::var("PORTMAN_ROOTS") {
        if !raw.is_empty() {
            return raw.split(':').filter(|p| !p.is_empty()).map(crate::expand_user).collect();
        }
    }
    if let Some(roots) = &config.crawl_roots {
        return roots.iter().map(|r| crate::expand_user(r)).collect();
    }
    vec![crate::home().join("Documents").join("GitHub")]
}

const SKIP_DIRS: &[&str] = &["node_modules", ".git", ".venv", "dist", ".next", "__pycache__"];

/// Every `.ports.lock` under the crawl roots. Missing dirs => already free.
pub fn find_locks() -> Vec<PathBuf> {
    find_locks_in(&crawl_roots())
}

pub fn find_locks_in(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut locks = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        // Bounded-depth walk (dirs at depth >= 4 are listed but not descended);
        // skip heavy/irrelevant trees.
        let walker = walkdir::WalkDir::new(root)
            .max_depth(5)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0
                    || !e.file_type().is_dir()
                    || !SKIP_DIRS.iter().any(|s| e.file_name() == *s)
            });
        for entry in walker.flatten() {
            if entry.depth() > 0 && entry.file_name() == LOCK && !entry.file_type().is_dir() {
                locks.push(entry.path().to_path_buf());
            }
        }
    }
    locks
}

pub fn read_lock(path: &Path) -> Option<LockData> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        // Python's `if existing:` treats an empty object as "no lock".
        Value::Object(m) if !m.is_empty() => Some(m),
        _ => None,
    }
}

/// port -> 'project@slot' from every lock except the one we're recomputing.
pub fn reserved_ports(exclude_dir: Option<&Path>) -> indexmap::IndexMap<i64, String> {
    let mut out = indexmap::IndexMap::new();
    for lp in find_locks() {
        if exclude_dir.is_some() && lp.parent() == exclude_dir {
            continue;
        }
        let Some(data) = read_lock(&lp) else { continue };
        let tag = format!("{}@{}", py_get(&data, "project"), py_get(&data, "slot"));
        if let Some(Value::Object(services)) = data.get("services") {
            for svc in services.values() {
                if let Some(Value::Number(n)) = svc.get("port") {
                    if let Some(p) = n.as_i64() {
                        out.entry(p).or_insert_with(|| tag.clone());
                    }
                }
            }
        }
    }
    out
}

// ── manifest / allocation ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Manifest {
    pub project: String,
    pub stride: i64,
    pub min_slot: i64,
    pub max_slot: i64,
    pub services: Vec<ServiceSpec>,
}

#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub name: String,
    pub env: String,
    pub default: i64,
    pub url_var: Option<String>,
    pub url_template: Option<String>,
}

pub fn load_manifest(path: &Path) -> Result<Manifest, Error> {
    let text = fs::read_to_string(path).map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| Error::new(format!("{}: {}", path.display(), e.message())))?;
    let dir_name = path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let project = doc
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .unwrap_or(dir_name);
    let alloc = doc.get("allocation");
    let int = |key: &str, default: i64| {
        alloc.and_then(|a| a.get(key)).and_then(|v| v.as_integer()).unwrap_or(default)
    };
    let mut services = Vec::new();
    if let Some(toml::Value::Table(svcs)) = doc.get("services") {
        for (name, spec) in svcs {
            let s = |k: &str| spec.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let env = s("env").ok_or_else(|| Error::new(format!("{}: services.{name} has no env", path.display())))?;
            let default = spec
                .get("default")
                .and_then(|v| v.as_integer())
                .ok_or_else(|| Error::new(format!("{}: services.{name} has no default", path.display())))?;
            services.push(ServiceSpec {
                name: name.clone(),
                env,
                default,
                url_var: s("url_var"),
                url_template: s("url_template"),
            });
        }
    }
    if services.is_empty() {
        return Err(Error::new(format!("{}: no [services] declared", path.display())));
    }
    Ok(Manifest {
        project,
        stride: int("stride", 10),
        min_slot: int("min_slot", 1),
        max_slot: int("max_slot", 20),
        services,
    })
}

/// Resolve every service's port for a slot: default + slot*stride.
pub fn ports_for_slot(manifest: &Manifest, slot: i64) -> Map<String, Value> {
    let mut services = Map::new();
    for spec in &manifest.services {
        let port = spec.default + slot * manifest.stride;
        let mut entry = Map::new();
        entry.insert("env".into(), Value::String(spec.env.clone()));
        entry.insert("port".into(), Value::from(port));
        if let Some(url_var) = &spec.url_var {
            let tmpl = spec.url_template.as_deref().unwrap_or("http://localhost:{port}");
            entry.insert("url_var".into(), Value::String(url_var.clone()));
            entry.insert("url".into(), Value::String(format_port(tmpl, port)));
        }
        services.insert(spec.name.clone(), Value::Object(entry));
    }
    services
}

/// `str.format(port=port)` for the subset templates use: `{port}`, `{{`, `}}`.
fn format_port(tmpl: &str, port: i64) -> String {
    tmpl.replace("{{", "\u{0}").replace("}}", "\u{1}").replace("{port}", &port.to_string())
        .replace('\u{0}', "{").replace('\u{1}', "}")
}

pub fn allocate(
    manifest: &Manifest,
    taken: &indexmap::IndexMap<i64, String>,
) -> Result<(i64, Map<String, Value>), Error> {
    for slot in manifest.min_slot..=manifest.max_slot {
        let services = ports_for_slot(manifest, slot);
        let clash = services
            .values()
            .any(|s| s.get("port").and_then(Value::as_i64).is_some_and(|p| taken.contains_key(&p)));
        if !clash {
            return Ok((slot, services));
        }
    }
    Err(Error::new(format!(
        "no free slot in [{}..{}] — run `portman ls`",
        manifest.min_slot, manifest.max_slot
    )))
}

// ── claim (get-or-create) ────────────────────────────────────────────────────

/// Idempotent claim for the worktree containing `cwd`. Returns the lock path,
/// its data, and whether this call created it.
pub fn get_or_claim(cwd: &Path) -> Result<(PathBuf, LockData, bool), Error> {
    let (manifest_path, root) = resolve_lease(cwd)?;
    let lock_path = root.join(LOCK);

    if let Some(existing) = read_lock(&lock_path) {
        return Ok((lock_path, existing, false));
    }

    let manifest = load_manifest(&manifest_path)?;

    let guard_path = claim_lockfile();
    if let Some(parent) = guard_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let guard = fs::File::create(&guard_path).ok();
    if let Some(g) = &guard {
        // Advisory only; a loud EADDRINUSE is the backstop. Shares the lock
        // file with the Python script, so the two can't double-claim.
        unsafe { libc::flock(g.as_raw_fd(), libc::LOCK_EX) };
    }
    // Re-check under the lock: another process may have just written ours.
    if let Some(existing) = read_lock(&lock_path) {
        return Ok((lock_path, existing, false));
    }
    let mut taken = reserved_ports(Some(&root));
    taken.extend(live::lsof_ports());
    taken.extend(live::docker_ports());
    let (slot, services) = allocate(&manifest, &taken)?;
    let mut data = Map::new();
    data.insert("project".into(), Value::String(manifest.project.clone()));
    data.insert("slot".into(), Value::from(slot));
    data.insert("path".into(), Value::String(root.to_string_lossy().into_owned()));
    data.insert("created".into(), Value::String(utc_now_iso()));
    data.insert("services".into(), Value::Object(services));

    let tmp = root.join(format!("{LOCK}.tmp"));
    let mut fh = fs::File::create(&tmp).map_err(|e| Error::new(format!("{}: {e}", tmp.display())))?;
    fh.write_all(python_json_dumps(&Value::Object(data.clone())).as_bytes())
        .and_then(|_| fh.write_all(b"\n"))
        .map_err(|e| Error::new(format!("{}: {e}", tmp.display())))?;
    drop(fh);
    fs::rename(&tmp, &lock_path).map_err(|e| Error::new(format!("{}: {e}", lock_path.display())))?;
    drop(guard);
    Ok((lock_path, data, true))
}

/// `datetime.now(timezone.utc).isoformat(timespec="seconds")`
fn utc_now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// `json.dumps(v, indent=2)`: two-space indent, `": "` separators, and
/// `ensure_ascii` — every non-ASCII char becomes a `\uXXXX` escape.
pub fn python_json_dumps(v: &Value) -> String {
    let pretty = serde_json::to_string_pretty(v).expect("json values always serialize");
    if pretty.is_ascii() {
        return pretty;
    }
    let mut out = String::with_capacity(pretty.len() + 16);
    for c in pretty.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

// ── env export lines / assignment ────────────────────────────────────────────

pub fn export_lines(data: &LockData) -> Vec<String> {
    let mut lines = Vec::new();
    for svc in services(data).values() {
        lines.push(format!("export {}={}", py_get(svc_obj(svc), "env"), py_get(svc_obj(svc), "port")));
        if svc.get("url_var").is_some() {
            lines.push(format!("export {}={}", py_get(svc_obj(svc), "url_var"), py_get(svc_obj(svc), "url")));
        }
    }
    lines
}

pub fn format_assignment(data: &LockData) -> String {
    let mut out = format!(
        "{}  slot {}  {}\n",
        py_get(data, "project"),
        py_get(data, "slot"),
        py_get(data, "path")
    );
    for (name, svc) in services(data) {
        let s = svc_obj(svc);
        let extra = if s.contains_key("url_var") {
            format!("  {}={}", py_get(s, "url_var"), py_get(s, "url"))
        } else {
            String::new()
        };
        out.push_str(&format!(
            "    {} {}={}{}\n",
            pad(name, 12),
            py_get(s, "env"),
            py_get(s, "port"),
            extra
        ));
    }
    out
}

static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);

pub fn services(data: &LockData) -> &Map<String, Value> {
    match data.get("services") {
        Some(Value::Object(m)) => m,
        _ => &EMPTY,
    }
}

fn svc_obj(v: &Value) -> &Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => &EMPTY,
    }
}

/// `str(d.get(key, '?'))` with Python's rendering of JSON scalars.
pub fn py_get(d: &Map<String, Value>, key: &str) -> String {
    d.get(key).map(py_str).unwrap_or_else(|| "?".into())
}

pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python's `f"{s:<width}"`: left-align, pad by characters.
pub fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

// ── typed view for the snapshot ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Lease {
    pub project: String,
    pub slot: i64,
    pub path: PathBuf,
    /// `None` for a primary clone's implicit slot 0 (no `.ports.lock`).
    pub lock_path: Option<PathBuf>,
    pub services: Vec<LeaseService>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LeaseService {
    pub name: String,
    pub env: String,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_var: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Lease {
    pub fn from_lock(lock_path: &Path, data: &LockData) -> Lease {
        let services = services(data)
            .iter()
            .filter_map(|(name, svc)| {
                let s = svc_obj(svc);
                Some(LeaseService {
                    name: name.clone(),
                    env: s.get("env")?.as_str()?.to_string(),
                    port: u16::try_from(s.get("port")?.as_i64()?).ok()?,
                    url_var: s.get("url_var").and_then(Value::as_str).map(str::to_string),
                    url: s.get("url").and_then(Value::as_str).map(str::to_string),
                })
            })
            .collect();
        Lease {
            project: data.get("project").and_then(Value::as_str).unwrap_or("?").to_string(),
            slot: data.get("slot").and_then(Value::as_i64).unwrap_or(-1),
            path: data
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| lock_path.parent().unwrap_or(lock_path).to_path_buf()),
            lock_path: Some(lock_path.to_path_buf()),
            services,
        }
    }

    /// Slot 0 = the manifest defaults, implicitly held by the primary clone.
    pub fn primary(manifest: &Manifest, dir: &Path) -> Lease {
        let services = ports_for_slot(manifest, 0);
        let mut data = Map::new();
        data.insert("project".into(), Value::String(manifest.project.clone()));
        data.insert("slot".into(), Value::from(0));
        data.insert("path".into(), Value::String(dir.to_string_lossy().into_owned()));
        data.insert("services".into(), Value::Object(services));
        let mut lease = Lease::from_lock(&dir.join(LOCK), &data);
        lease.lock_path = None;
        lease
    }

    pub fn ports(&self) -> impl Iterator<Item = u16> + '_ {
        self.services.iter().map(|s| s.port)
    }

    pub fn export_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for s in &self.services {
            lines.push(format!("export {}={}", s.env, s.port));
            if let (Some(var), Some(url)) = (&s.url_var, &s.url) {
                lines.push(format!("export {var}={url}"));
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_matches_python_formatting() {
        let v: Value = serde_json::from_str(r#"{"a": 1, "b": {"c": "é"}, "d": {}}"#).unwrap();
        assert_eq!(
            python_json_dumps(&v),
            "{\n  \"a\": 1,\n  \"b\": {\n    \"c\": \"\\u00e9\"\n  },\n  \"d\": {}\n}"
        );
    }

    #[test]
    fn url_template() {
        assert_eq!(format_port("ws://localhost:{port}", 3011), "ws://localhost:3011");
        assert_eq!(format_port("{{x}}:{port}", 1), "{x}:1");
    }

    #[test]
    fn pad_counts_chars() {
        assert_eq!(pad("é", 3), "é  ");
        assert_eq!(pad("toolongname!!", 12), "toolongname!!");
    }
}
