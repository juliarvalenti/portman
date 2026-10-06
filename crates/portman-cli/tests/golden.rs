//! Golden test: the Rust `portman` must match the Python script byte for byte
//! on `claim`, `env`, `show` and `ls` (ls may only append MEM / UP columns).
//!
//! Each scenario runs once with Python and once with Rust from the same
//! starting state, comparing stdout, stderr, exit code, and the lock files
//! written (with the `created` timestamp normalized).
//!
//! The Python script is found via `PORTMAN_PY`, else the dotfiles checkout.
//! If neither exists the test is skipped with a note.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const RUST: &str = env!("CARGO_BIN_EXE_portman");

fn python_script() -> Option<PathBuf> {
    let p = std::env::var_os("PORTMAN_PY").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap()).join("Documents/GitHub/ahk-bindings/portman/portman")
    });
    p.is_file().then_some(p)
}

#[derive(Debug, PartialEq)]
struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

struct Env {
    root: PathBuf,
    py: PathBuf,
}

impl Env {
    fn run(&self, rust: bool, cwd: &Path, args: &[&str]) -> Out {
        let mut cmd = if rust {
            Command::new(RUST)
        } else {
            let mut c = Command::new("python3");
            c.arg(&self.py);
            c
        };
        let out = cmd
            .args(args)
            .current_dir(cwd)
            .env("PORTMAN_ROOTS", &self.root)
            .env("PORTMAN_CONFIG", self.root.join("no-such-config.toml"))
            .output()
            .unwrap();
        Out {
            stdout: String::from_utf8(out.stdout).unwrap(),
            stderr: String::from_utf8(out.stderr).unwrap(),
            code: out.status.code().unwrap_or(-1),
        }
    }

    /// Run `steps` with Python, snapshot outputs + locks, wipe the locks,
    /// then run the same steps with Rust and compare everything.
    fn compare(&self, steps: &[(&Path, &[&str])]) {
        let py = self.play(false, steps);
        let rs = self.play(true, steps);
        for (i, (p, r)) in py.0.iter().zip(&rs.0).enumerate() {
            assert_eq!(p, r, "step {i} {:?} differs (left = python, right = rust)", steps[i].1);
        }
        assert_eq!(py.1, rs.1, "lock files differ (left = python, right = rust)");
    }

    fn play(&self, rust: bool, steps: &[(&Path, &[&str])]) -> (Vec<Out>, Vec<(PathBuf, String)>) {
        self.wipe_locks();
        let outs = steps.iter().map(|(cwd, args)| self.run(rust, cwd, args)).collect();
        let locks = self.locks();
        self.wipe_locks();
        (outs, locks)
    }

    fn locks(&self) -> Vec<(PathBuf, String)> {
        let mut v: Vec<_> = walk(&self.root)
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n == ".ports.lock"))
            .map(|p| {
                let text = fs::read_to_string(&p).unwrap();
                (p, normalize_created(&text))
            })
            .collect();
        v.sort();
        v
    }

    fn wipe_locks(&self) {
        for p in walk(&self.root) {
            if p.file_name().is_some_and(|n| n == ".ports.lock") {
                fs::remove_file(p).unwrap();
            }
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            if p.file_name().unwrap() != ".git" {
                out.extend(walk(&p));
            }
        } else {
            out.push(p);
        }
    }
    out
}

fn normalize_created(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_start().starts_with("\"created\":") {
                let ok = l.contains("+00:00\"") && l.len() == "  \"created\": \"2026-10-06T20:01:35+00:00\",".len();
                assert!(ok, "unexpected created format: {l}");
                "  \"created\": \"<ts>\",".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if text.ends_with('\n') { "\n" } else { "" }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

/// Ports in a quiet range so live listeners don't make the runs diverge.
const MANIFEST: &str = r#"[project]
name = "polaris"

[allocation]
stride = 10
min_slot = 1
max_slot = 20

[services.web]
env = "PORT"
default = 41000

[services.websocket]
env = "WEBSOCKET_PORT"
default = 41001
url_var = "NEXT_PUBLIC_WEBSOCKET_URL"
url_template = "ws://localhost:{port}"

[services.dagster]
env = "DAGSTER_PORT"
default = 41002
url_var = "DAGSTER_URL"
"#;

fn setup() -> Option<(tempfile::TempDir, Env)> {
    let Some(py) = python_script() else {
        eprintln!("SKIP golden test: Python portman not found (set PORTMAN_PY)");
        return None;
    };
    let tmp = tempfile::tempdir().unwrap();
    // Physical path, as git and getcwd report it.
    let root = tmp.path().canonicalize().unwrap();

    // Main clone with a gitignored manifest, plus a linked worktree.
    let main = root.join("polaris");
    fs::create_dir_all(main.join("apps/web")).unwrap();
    git(&main, &["init", "-q"]);
    fs::write(main.join(".gitignore"), ".ports.toml\n.ports.lock\n").unwrap();
    fs::write(main.join("apps/web/README"), "x\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "init"]);
    fs::write(main.join(".ports.toml"), MANIFEST).unwrap();
    git(&main, &["worktree", "add", "-q", "-b", "feat", root.join("polaris-chris").to_str().unwrap()]);

    // Non-git directory with its own manifest, non-ASCII project name, no
    // [project] table defaults, and a slot range that runs out.
    let loose = root.join("lööse");
    fs::create_dir_all(loose.join("sub")).unwrap();
    fs::write(
        loose.join(".ports.toml"),
        "[project]\nname = \"café\"\n[allocation]\nmin_slot = 1\nmax_slot = 1\n[services.api]\nenv = \"API_PORT\"\ndefault = 42000\n",
    )
    .unwrap();
    let loose2 = root.join("loose2");
    fs::create_dir_all(&loose2).unwrap();
    fs::write(
        loose2.join(".ports.toml"),
        "[allocation]\nmin_slot = 1\nmax_slot = 1\n[services.api]\nenv = \"API_PORT\"\ndefault = 42000\n",
    )
    .unwrap();

    // Manifest without services; a dir with no manifest at all.
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join(".ports.toml"), "[project]\nname = \"e\"\n").unwrap();
    fs::create_dir_all(root.join("nothing")).unwrap();

    Some((tmp, Env { root, py }))
}

#[test]
fn lease_commands_match_python() {
    let Some((_tmp, env)) = setup() else { return };
    let r = &env.root;
    let main = r.join("polaris");
    let web = main.join("apps/web");
    let wt = r.join("polaris-chris");
    let wt_web = wt.join("apps/web");
    let loose = r.join("lööse");
    let loose_sub = loose.join("sub");
    let loose2 = r.join("loose2");
    let empty = r.join("empty");
    let nothing = r.join("nothing");

    // Fresh claims, idempotent re-claims, env, show, from nested dirs.
    env.compare(&[
        (&web, &["show"]),
        (&web, &["claim"]),
        (&main, &["claim"]),
        (&web, &["env"]),
        (&main, &["show"]),
        (&wt_web, &["env"]),
        (&wt, &["claim"]),
        (&wt_web, &["show"]),
        (&loose_sub, &["claim"]),
        (&loose, &["env"]),
        // Only slot 1 exists and lööse holds it.
        (&loose2, &["claim"]),
        (&loose2, &["show"]),
        (&empty, &["claim"]),
        (&nothing, &["claim"]),
        (&nothing, &["show"]),
        (&nothing, &["env"]),
    ]);

    // env claims on first use (quietly).
    env.compare(&[(&wt_web, &["env"]), (&wt, &["show"])]);
}

/// `ls` with locks present: the lease sections must match exactly; the
/// "live ports with no lease" section reflects the whole machine, so it is
/// compared only when Python's output was stable across the Rust run.
#[test]
fn ls_matches_python() {
    let Some((_tmp, env)) = setup() else { return };
    let r = &env.root;
    // No locks yet: stderr notice.
    let py = env.run(false, r, &["ls"]);
    let rs = env.run(true, r, &["ls"]);
    assert_eq!(py.stderr, rs.stderr);
    assert_eq!(py.code, rs.code);

    env.run(false, &r.join("polaris"), &["claim"]);
    env.run(false, &r.join("polaris-chris"), &["claim"]);
    env.run(false, &r.join("lööse"), &["claim"]);
    // A lock with odd values the Python script still prints.
    fs::write(r.join("nothing/.ports.lock"), "{\"slot\": 3, \"services\": {\"x\": {\"env\": \"X\", \"port\": 42999}}}\n").unwrap();

    for _attempt in 0..3 {
        let py1 = env.run(false, r, &["ls"]);
        let rs = env.run(true, r, &["ls"]);
        let py2 = env.run(false, r, &["ls"]);
        assert_eq!(py1.stderr, rs.stderr);
        assert_eq!(py1.code, rs.code);
        let (py_leases, py_orphans) = split_ls(&py1.stdout);
        let (rs_leases, rs_orphans) = split_ls(&rs.stdout);
        assert_prefix_match(&py_leases, &rs_leases);
        if py1.stdout == py2.stdout {
            assert_prefix_match(&py_orphans, &rs_orphans);
            return;
        }
    }
    panic!("machine's listening ports kept changing; couldn't compare the orphan section");
}

fn split_ls(s: &str) -> (Vec<String>, Vec<String>) {
    let marker = "\nlive ports with no lease:\n";
    let (a, b) = s.split_once(marker).unwrap_or((s, ""));
    (a.lines().map(str::to_string).collect(), b.lines().map(str::to_string).collect())
}

/// Each Rust line is the Python line, optionally followed by `  MEM  UP`.
fn assert_prefix_match(py: &[String], rs: &[String]) {
    assert_eq!(py.len(), rs.len(), "line count differs:\npython:\n{}\nrust:\n{}", py.join("\n"), rs.join("\n"));
    for (p, r) in py.iter().zip(rs) {
        if p == r {
            continue;
        }
        let rest = r.strip_prefix(p.as_str()).unwrap_or_else(|| panic!("line differs:\npython: {p:?}\nrust:   {r:?}"));
        let cols: Vec<&str> = rest.split_whitespace().collect();
        assert!(
            cols.len() == 3 && ["B", "KB", "MB", "GB"].contains(&cols[1]),
            "unexpected trailing columns {rest:?} after {p:?}"
        );
    }
}
