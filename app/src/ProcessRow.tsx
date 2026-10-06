import { type ReactNode, useEffect, useRef, useState } from "react";
import { api, bytes, duration } from "./api";
import type { DevProcess, LeaseMatch, StopResult } from "./types";

type StopState =
  | { kind: "idle" }
  | { kind: "confirm" }
  | { kind: "stopping"; force: boolean }
  | { kind: "survived"; result: StopResult }
  | { kind: "error"; message: string };

export function Sparkline({ values }: { values: number[] }) {
  if (values.length < 2) return <span className="inline-block w-16" />;
  const w = 64;
  const h = 16;
  const max = Math.max(...values);
  const min = Math.min(...values);
  const span = max - min || 1;
  const points = values
    .map((v, i) => {
      const x = (i / (values.length - 1)) * w;
      const y = h - 1 - ((v - min) / span) * (h - 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  const rising = values[values.length - 1] > values[0] * 1.02;
  return (
    <svg width={w} height={h} className="shrink-0" aria-label="footprint, last 30 s">
      <polyline
        points={points}
        fill="none"
        strokeWidth={1.25}
        className={rising ? "stroke-amber-500" : "stroke-zinc-400 dark:stroke-zinc-500"}
      />
    </svg>
  );
}

function offLeasePort(m: LeaseMatch): number | null {
  return m.kind === "off_lease" ? m.port : null;
}

export function ProcessRow({
  proc,
  confirmRequested,
  onConfirmHandled,
}: {
  proc: DevProcess;
  /** The tray asked to stop this one: open the inline confirm. */
  confirmRequested?: boolean;
  onConfirmHandled?: () => void;
}) {
  const [stop, setStop] = useState<StopState>({ kind: "idle" });
  const rowRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (confirmRequested) {
      setStop({ kind: "confirm" });
      rowRef.current?.scrollIntoView({ block: "center", behavior: "smooth" });
      onConfirmHandled?.();
    }
  }, [confirmRequested, onConfirmHandled]);

  const offLease = offLeasePort(proc.lease_match);
  const firstPort = proc.listening[0];

  async function doStop(force: boolean) {
    setStop({ kind: "stopping", force });
    try {
      const res = force ? await api.kill(proc.pid) : await api.stop(proc.pid, false);
      if (res.error) setStop({ kind: "error", message: res.error });
      else if (res.still_running.length) setStop({ kind: "survived", result: res });
      else setStop({ kind: "idle" });
    } catch (e) {
      setStop({ kind: "error", message: String(e) });
    }
  }

  const title = [
    proc.cmdline,
    proc.cwd,
    `pid ${proc.pids.join(", ")}`,
    proc.idle_secs != null ? `idle ${duration(proc.idle_secs)}` : null,
  ]
    .filter(Boolean)
    .join("\n");

  return (
    <div ref={rowRef}>
      <div
        className={`group flex items-center gap-3 rounded px-2 py-1 hover:bg-zinc-100 dark:hover:bg-zinc-800 ${
          stop.kind === "confirm" ? "bg-red-50 dark:bg-red-950/40" : ""
        }`}
        title={title}
      >
        <span className={`text-[10px] ${proc.stale ? "text-amber-500" : "text-emerald-500"}`}>●</span>
        <span className="w-36 truncate font-medium">
          {proc.name}
          {proc.pids.length > 1 && (
            <span
              className="ml-1 text-xs font-normal text-zinc-400"
              title={`Memory and CPU include ${proc.pids.length - 1} child process${proc.pids.length > 2 ? "es" : ""}`}
            >
              +{proc.pids.length - 1}
            </span>
          )}
        </span>
        <span className="flex w-36 gap-1.5 truncate font-mono text-xs">
          {proc.listening.length === 0 && <span className="text-zinc-400">—</span>}
          {proc.listening.map((p) => (
            <button
              key={p}
              onClick={() => api.openUrl(p)}
              className={`hover:underline ${
                p === offLease
                  ? "rounded bg-amber-100 px-1 text-amber-800 dark:bg-amber-900/50 dark:text-amber-300"
                  : "text-sky-600 dark:text-sky-400"
              }`}
              title={p === offLease ? `:${p} is outside this worktree's lease` : `http://localhost:${p}`}
            >
              :{p}
            </button>
          ))}
        </span>
        <span className="w-16 text-right tabular-nums">{bytes(proc.footprint_bytes)}</span>
        <Sparkline values={proc.history ?? []} />
        <span className="w-12 text-right tabular-nums text-zinc-500">{proc.cpu_pct.toFixed(0)}%</span>
        <span className="w-10 text-right tabular-nums text-zinc-500">{duration(proc.uptime_secs)}</span>
        <span className="flex w-36 gap-1">
          {proc.stale && (
            <span
              className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-semibold tracking-wide text-amber-800 dark:bg-amber-900/50 dark:text-amber-300"
              title={proc.stale === "idle" ? "idle and large" : proc.stale === "both" ? "old, idle and large" : "up a long time"}
            >
              STALE
            </span>
          )}
          {proc.app_bundle && (
            <span className="rounded bg-zinc-200 px-1.5 py-0.5 text-[10px] text-zinc-600 dark:bg-zinc-700 dark:text-zinc-300">
              {proc.app_bundle}.app
            </span>
          )}
        </span>
        <span className="ml-auto flex gap-1">
          {firstPort != null && (
            <SmallButton onClick={() => api.openUrl(firstPort)}>Open</SmallButton>
          )}
          {proc.stoppable && stop.kind === "idle" && (
            <SmallButton onClick={() => setStop({ kind: "confirm" })}>Stop</SmallButton>
          )}
        </span>
      </div>
      {stop.kind !== "idle" && (
        <div className="ml-7 flex items-center gap-2 px-2 pb-1 text-xs">
          {stop.kind === "confirm" && (
            <>
              <span>
                Stop {proc.name} ({bytes(proc.footprint_bytes)})?
              </span>
              <SmallButton danger onClick={() => doStop(false)}>
                Stop
              </SmallButton>
              <SmallButton onClick={() => setStop({ kind: "idle" })}>Cancel</SmallButton>
            </>
          )}
          {stop.kind === "stopping" && (
            <span className="text-zinc-500">{stop.force ? "Killing…" : "Sent SIGTERM, waiting up to 5 s…"}</span>
          )}
          {stop.kind === "survived" && (
            <>
              <span className="text-amber-600 dark:text-amber-400">
                Still running after 5 s (pid {stop.result.still_running.join(", ")}).
              </span>
              <SmallButton danger onClick={() => doStop(true)}>
                Force kill
              </SmallButton>
              <SmallButton onClick={() => setStop({ kind: "idle" })}>Leave it</SmallButton>
            </>
          )}
          {stop.kind === "error" && (
            <>
              <span className="text-red-600 dark:text-red-400">{stop.message}</span>
              <SmallButton onClick={() => setStop({ kind: "idle" })}>OK</SmallButton>
            </>
          )}
        </div>
      )}
    </div>
  );
}

export function SmallButton({
  children,
  onClick,
  danger,
  disabled,
  title,
}: {
  children: ReactNode;
  onClick: () => void;
  danger?: boolean;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={`rounded border px-2 py-0.5 text-xs disabled:opacity-40 ${
        danger
          ? "border-red-500 bg-red-500 text-white hover:bg-red-600"
          : "border-zinc-300 bg-white hover:bg-zinc-50 dark:border-zinc-600 dark:bg-zinc-800 dark:hover:bg-zinc-700"
      }`}
    >
      {children}
    </button>
  );
}
