// Mirrors crates/portman-core/src/model.rs (+ lease.rs `Lease`, stop.rs
// `StopResult`). Times are Unix milliseconds; paths are strings.

export type PressureLevel = "normal" | "warn" | "critical";
export type StaleReason = "too_old" | "idle" | "both";
export type LeaseMatch =
  | { kind: "on_lease" }
  | { kind: "no_lease" }
  | { kind: "off_lease"; port: number };

export interface LeaseService {
  name: string;
  env: string;
  port: number;
  url_var?: string;
  url?: string;
}

export interface Lease {
  project: string;
  slot: number;
  path: string;
  /** null for a primary clone's implicit slot 0. */
  lock_path: string | null;
  services: LeaseService[];
}

export interface DevProcess {
  pid: number;
  pids: number[];
  name: string;
  cmdline: string;
  cwd: string;
  listening: number[];
  footprint_bytes: number;
  cpu_pct: number;
  started_at: number;
  uptime_secs: number;
  idle_secs: number | null;
  stale: StaleReason | null;
  lease_match: LeaseMatch;
  stoppable: boolean;
  reapable: boolean;
  app_bundle: string | null;
  /** Footprint over the last ~30 s, oldest first. */
  history?: number[];
}

export interface Worktree {
  path: string;
  branch: string | null;
  lease: Lease | null;
  processes: DevProcess[];
  exists: boolean;
}

export interface Project {
  name: string;
  repo_root: string;
  worktrees: Worktree[];
}

export interface Vitals {
  pressure: PressureLevel;
  ram_total: number;
  ram_free: number;
  compressed: number;
  swap_used: number;
  swap_total: number;
  disk_free: number;
}

export interface Container {
  name: string;
  image: string;
  /** [host, container] */
  ports: [number, number][];
}

export interface AppUsage {
  app: string;
  footprint_bytes: number;
  process_count: number;
}

export interface Summary {
  dev_count: number;
  dev_footprint: number;
  stale_count: number;
  stale_footprint: number;
  level: PressureLevel;
}

export interface Snapshot {
  taken_at: number;
  vitals: Vitals;
  projects: Project[];
  unattributed: DevProcess[];
  containers: Container[];
  heavy_hitters: AppUsage[];
  summary: Summary;
}

export interface StopResult {
  pids: number[];
  stopped: boolean;
  killed: boolean;
  still_running: number[];
  error: string | null;
}
