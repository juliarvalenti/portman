import { invoke } from "@tauri-apps/api/core";
import type { DevProcess, Snapshot, StopResult } from "./types";

export const api = {
  getSnapshot: () => invoke<Snapshot | null>("get_snapshot"),
  stop: (pid: number, force = false) => invoke<StopResult>("stop_process", { pid, force }),
  kill: (pid: number) => invoke<StopResult>("kill_process", { pid }),
  // `pids`: the servers the user confirmed; nothing outside it is stopped.
  reap: (dryRun: boolean, pids?: number[]) => invoke<DevProcess[]>("reap", { dryRun, pids }),
  openUrl: (port: number) => invoke<void>("open_url", { port }),
  reveal: (path: string) => invoke<void>("reveal_path", { path }),
  copyEnv: (worktree: string) => invoke<string>("copy_env", { worktree }),
};

// Same rules as portman_core::fmt so the window agrees with the tray and CLI.
const K = 1024;

export function bytes(b: number): string {
  if (b >= K ** 3) return `${(b / K ** 3).toFixed(1)} GB`;
  if (b >= K ** 2) return `${(b / K ** 2).toFixed(0)} MB`;
  if (b >= K) return `${(b / K).toFixed(0)} KB`;
  return `${b} B`;
}

export function gb(b: number): string {
  return (b / K ** 3).toFixed(1);
}

export function duration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

export function basename(path: string): string {
  return path.replace(/\/+$/, "").split("/").pop() ?? path;
}

export const sum = (xs: number[]) => xs.reduce((a, b) => a + b, 0);
