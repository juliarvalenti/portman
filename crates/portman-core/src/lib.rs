//! portman-core: per-worktree port leases, plus a live view of the dev
//! servers using them — shared by the `portman` CLI and the menu-bar app.
//!
//! - [`lease`]   — the lease model (`.ports.toml` / `.ports.lock`), unchanged
//!   from the Python script and byte-compatible with it.
//! - [`sampler`] — process/port/vitals sampling into a [`Snapshot`].
//! - [`stop`]    — SIGTERM → SIGKILL with safety checks.
//!
//! macOS only: sampling uses Darwin APIs directly.

pub mod config;
pub mod darwin;
pub mod fmt;
pub mod lease;
pub mod live;
pub mod model;
pub mod sampler;
pub mod stop;

use std::path::PathBuf;

pub use config::Config;
pub use model::*;
pub use sampler::Sampler;
pub use stop::StopResult;

/// A user-facing failure; the CLI prints it as `portman: <msg>`.
#[derive(Debug, Clone)]
pub struct Error(pub String);

impl Error {
    pub fn new(msg: impl Into<String>) -> Error {
        Error(msg.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `Path(p).expanduser()` for the `~` and `~/...` forms.
pub fn expand_user(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Short `worktree/subdir` label for a server, e.g. `polaris/web` for a
/// next-server running in `polaris/apps/web`.
pub fn label(_project: &Project, w: &Worktree, d: &DevProcess) -> String {
    let wt = w.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match d.cwd.strip_prefix(&w.path) {
        Ok(rel) if rel.as_os_str().is_empty() => wt,
        Ok(_) => {
            let leaf = d.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{wt}/{leaf}")
        }
        Err(_) => wt,
    }
}
