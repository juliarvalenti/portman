# portman

Per-worktree dev-server port leases, plus a live view of the dev servers
using them: which worktree owns each one, how much memory it **really** uses
(`phys_footprint`, not RSS), and how long it has been up. See
[SPEC.md](SPEC.md) for the why.

```
crates/portman-core/   leases, crawl, process/port/vitals sampling, stop
crates/portman-cli/    bin `portman` — drop-in replacement for the Python script
app/                   Tauri 2 menu-bar app (see app/README.md)
```

macOS only.

## CLI

```sh
cargo build --release          # builds target/release/portman
```

```sh
portman claim     # unchanged: idempotent claim of this worktree's port block
portman env       # unchanged: eval "$(portman env)" && pnpm dev
portman show      # unchanged
portman ls        # unchanged columns + trailing MEM / UP for live ports

portman ps [--all] [--json]              # dev servers by project → worktree
portman stop <pid | :port | path | .> [--force]
portman reap [--older-than 48h] [--idle] [--yes] [--force] [--json]
portman vitals [--json]                  # pressure, RAM/compressed, swap, disk
```

- `ps` folds a server's process tree (`pnpm` → `next dev` → `next-server`)
  into one row; `stop` signals the whole tree. `!` after a port means it's
  bound outside the worktree's lease. `--all` adds idle leases, `.app`
  listeners, containers, and heavy hitters.
- `stop` sends SIGTERM, waits 5 s, then SIGKILLs only with `--force` or a `y`.
- `reap` lists stale servers and the memory they'd free. It asks y/N on a
  terminal, and is a dry run otherwise unless `--yes`. Candidates are
  servers attributed to a worktree, plus orphans running from temp dirs
  (agent scratchpads). It never touches `.app` processes, other users'
  processes, or servers elsewhere outside the crawl roots (e.g. Homebrew
  services).
- `--json` emits the same `Snapshot` the app uses (times in Unix ms).

### What counts as a dev server

A process you own that listens on TCP ≥ 1024, or that runs from inside a
crawl root and looks like a dev tool (node, vite, cargo, python, …, plus
`extra_dev_patterns`). Stdio MCP servers and editor extension hosts are
skipped. It's attributed to the git worktree containing its cwd; listeners
outside every crawl root go to **unattributed**.

### Stale

`uptime > stale.after` (48 h), **or** idle (CPU < `stale.idle_cpu_pct`, 1%)
for `stale.idle_for` (6 h) while holding ≥ `stale.min_footprint` (1 GB). The
CLI approximates idle from lifetime CPU ÷ uptime plus a live sample (1 s for
`ps`, 3 s for `reap`).

## Config

`~/.config/portman/config.toml`, every key optional (`PORTMAN_CONFIG`
overrides the path, `PORTMAN_ROOTS` overrides `crawl_roots`):

```toml
crawl_roots   = ["~/Documents/GitHub"]
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

[notifications]   # opt-out per kind (app)
stale = true
disk  = true
swap  = true

[reap]
auto = false      # app stops stale servers itself. Off by default.
```

## Tests

```sh
cargo test
```

`crates/portman-cli/tests/golden.rs` runs the Python script
(`PORTMAN_PY`, default `~/Documents/GitHub/ahk-bindings/portman/portman`)
and the Rust binary through the same claim/env/show/ls scenarios. It
compares stdout, stderr, exit codes and the lock files written, byte for
byte (`ls` may only append MEM / UP). It skips if the script is absent.

## Install

Once you're happy with parity, point the existing symlink at the Rust build:

```sh
cargo build --release
ln -sf "$PWD/target/release/portman" ~/.local/bin/portman
```
