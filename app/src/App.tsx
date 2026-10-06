import { type ReactNode, useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { ChevronDown, ChevronRight, ClipboardCopy, FolderOpen } from "lucide-react";
import { api, basename, bytes, gb, sum } from "./api";
import { ProcessRow, SmallButton } from "./ProcessRow";
import type { DevProcess, PressureLevel, Project, Snapshot, Worktree } from "./types";

const DOT: Record<PressureLevel, string> = {
  normal: "bg-emerald-500",
  warn: "bg-amber-500",
  critical: "bg-red-500",
};

const wtFootprint = (w: Worktree) => sum(w.processes.map((p) => p.footprint_bytes));
const projFootprint = (p: Project) => sum(p.worktrees.map(wtFootprint));

export default function App() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set(["containers", "unattributed"]));
  const [confirmPid, setConfirmPid] = useState<number | null>(null);
  const [reapOpen, setReapOpen] = useState(false);

  useEffect(() => {
    api.getSnapshot().then((s) => s && setSnap(s));
    const unlisten = [
      listen<Snapshot>("snapshot", (e) => setSnap(e.payload)),
      listen<number>("confirm-stop", (e) => {
        setConfirmPid(e.payload);
        // Make sure the row is visible.
        setCollapsed(new Set());
      }),
      listen("confirm-reap", () => setReapOpen(true)),
    ];
    return () => unlisten.forEach((p) => p.then((f) => f()));
  }, []);

  const toggle = (key: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const clearConfirm = useCallback(() => setConfirmPid(null), []);
  const row = (p: DevProcess) => (
    <ProcessRow
      key={p.pid}
      proc={p}
      confirmRequested={confirmPid === p.pid || p.pids.includes(confirmPid ?? -1)}
      onConfirmHandled={clearConfirm}
    />
  );

  if (!snap) {
    return <div className="flex h-full items-center justify-center text-zinc-500">Sampling…</div>;
  }

  return (
    <div className="flex h-full flex-col">
      <VitalsHeader snap={snap} onReap={() => setReapOpen(true)} />
      {reapOpen && <ReapConfirm onClose={() => setReapOpen(false)} />}
      <main className="flex-1 overflow-y-auto px-3 py-2">
        {snap.projects.length === 0 && snap.unattributed.length === 0 && (
          <p className="px-2 py-6 text-center text-zinc-500">No dev servers running.</p>
        )}
        {snap.projects.map((p) => (
          <Section
            key={p.repo_root}
            open={!collapsed.has(p.repo_root)}
            onToggle={() => toggle(p.repo_root)}
            title={<span className="font-semibold">{p.name}</span>}
            right={projFootprint(p) > 0 && <span className="tabular-nums text-zinc-500">{bytes(projFootprint(p))}</span>}
          >
            {p.worktrees.map((w) => (
              <WorktreeBlock
                key={w.path}
                w={w}
                open={!collapsed.has(w.path)}
                onToggle={() => toggle(w.path)}
                row={row}
              />
            ))}
          </Section>
        ))}

        {snap.unattributed.length > 0 && (
          <Section
            open={!collapsed.has("unattributed")}
            onToggle={() => toggle("unattributed")}
            title={<span className="font-semibold">Unattributed ({snap.unattributed.length})</span>}
            right={<span className="text-xs text-zinc-500">listening outside every crawl root</span>}
          >
            <div className="pl-4">{snap.unattributed.map(row)}</div>
          </Section>
        )}

        <Section
          open={!collapsed.has("containers")}
          onToggle={() => toggle("containers")}
          title={<span className="font-semibold">Containers ({snap.containers.length})</span>}
        >
          <div className="pl-6">
            {snap.containers.length === 0 && <p className="py-1 text-zinc-500">Docker isn't running, or nothing is up.</p>}
            {snap.containers.map((c) => (
              <div key={c.name} className="flex gap-3 py-0.5">
                <span className="w-56 truncate font-medium">{c.name}</span>
                <span className="w-56 truncate text-zinc-500">{c.image}</span>
                <span className="flex gap-1.5 font-mono text-xs">
                  {c.ports.map(([host, inner]) => (
                    <button
                      key={`${host}-${inner}`}
                      onClick={() => api.openUrl(host)}
                      className="text-sky-600 hover:underline dark:text-sky-400"
                    >
                      :{host}
                      {host !== inner && <span className="text-zinc-400">→{inner}</span>}
                    </button>
                  ))}
                </span>
              </div>
            ))}
          </div>
        </Section>

        <div className="flex items-center gap-2 px-1 py-1.5">
          <span className="w-4" />
          <span className="font-semibold">Heavy hitters</span>
          <span className="truncate text-zinc-500">
            {snap.heavy_hitters
              .map((h) => `${h.app} ${bytes(h.footprint_bytes)}`)
              .join(" · ")}
          </span>
        </div>
      </main>
    </div>
  );
}

function VitalsHeader({ snap, onReap }: { snap: Snapshot; onReap: () => void }) {
  const v = snap.vitals;
  const s = snap.summary;
  return (
    <header className="flex flex-wrap items-center gap-x-5 gap-y-1 border-b border-zinc-200 px-4 py-2.5 dark:border-zinc-700">
      <span className="flex items-center gap-1.5">
        Pressure <span className={`inline-block h-2 w-2 rounded-full ${DOT[v.pressure]}`} />
        <span className="text-zinc-500">{v.pressure}</span>
      </span>
      <span className="tabular-nums">
        RAM {gb(v.ram_free)}/{gb(v.ram_total)} GB free{" "}
        <span className="text-zinc-500">({gb(v.compressed)} compressed)</span>
      </span>
      <span className="tabular-nums">
        Swap {gb(v.swap_used)}/{gb(v.swap_total)} GB
      </span>
      <span className="tabular-nums">Disk {gb(v.disk_free)} GB free</span>
      <span className="ml-auto flex items-center gap-3">
        <span className="text-zinc-500 tabular-nums">
          {s.dev_count} servers · {bytes(s.dev_footprint)}
        </span>
        <SmallButton onClick={onReap} disabled={s.stale_count === 0} danger={s.stale_count > 0}>
          Reap stale ({s.stale_count})
        </SmallButton>
      </span>
    </header>
  );
}

function ReapConfirm({ onClose }: { onClose: () => void }) {
  const [targets, setTargets] = useState<DevProcess[] | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.reap(true).then(setTargets);
  }, []);

  if (!targets) return null;
  const total = sum(targets.map((t) => t.footprint_bytes));
  return (
    <div className="border-b border-amber-300 bg-amber-50 px-4 py-2 dark:border-amber-800 dark:bg-amber-950/40">
      {targets.length === 0 ? (
        <div className="flex items-center gap-3">
          <span>Nothing is stale.</span>
          <SmallButton onClick={onClose}>OK</SmallButton>
        </div>
      ) : (
        <>
          <p className="mb-1">
            Stop {targets.length} stale server{targets.length === 1 ? "" : "s"}, freeing {bytes(total)}? Each gets
            SIGTERM, then SIGKILL after 5 s.
          </p>
          <ul className="mb-2 ml-4 list-disc text-xs">
            {targets.map((t) => (
              <li key={t.pid} className="selectable">
                {t.name} <span className="text-zinc-500">({bytes(t.footprint_bytes)}, pid {t.pid}, {t.cwd})</span>
              </li>
            ))}
          </ul>
          <div className="flex gap-2">
            <SmallButton
              danger
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                await api.reap(false, targets.map((t) => t.pid)).catch(() => undefined);
                onClose();
              }}
            >
              {busy ? "Reaping…" : `Reap ${targets.length} · ${bytes(total)}`}
            </SmallButton>
            <SmallButton onClick={onClose} disabled={busy}>
              Cancel
            </SmallButton>
          </div>
        </>
      )}
    </div>
  );
}

function Section({
  open,
  onToggle,
  title,
  right,
  children,
}: {
  open: boolean;
  onToggle: () => void;
  title: ReactNode;
  right?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="mb-1">
      <button onClick={onToggle} className="flex w-full items-center gap-2 rounded px-1 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800">
        {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        {title}
        <span className="ml-auto">{right}</span>
      </button>
      {open && children}
    </section>
  );
}

function WorktreeBlock({
  w,
  open,
  onToggle,
  row,
}: {
  w: Worktree;
  open: boolean;
  onToggle: () => void;
  row: (p: DevProcess) => ReactNode;
}) {
  const meta = [w.branch, w.lease ? `slot ${w.lease.slot}` : null].filter(Boolean).join(" · ");
  const idle = w.processes.length === 0;
  const total = wtFootprint(w);
  return (
    <div className="ml-4">
      <div className="group flex items-center gap-2 rounded px-1 py-1 hover:bg-zinc-100 dark:hover:bg-zinc-800">
        <button onClick={onToggle} className="flex items-center gap-2" disabled={idle}>
          {idle ? <span className="w-[14px]" /> : open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          <span className="selectable" title={w.path}>
            {basename(w.path)}
          </span>
          {meta && <span className="text-zinc-500">({meta})</span>}
        </button>
        {!w.exists && (
          <span className="rounded bg-red-100 px-1.5 py-0.5 text-[10px] font-semibold text-red-700 dark:bg-red-900/50 dark:text-red-300">
            deleted worktree
          </span>
        )}
        <span className="ml-auto flex items-center gap-2">
          {idle ? (
            <span className="text-xs text-zinc-500">idle — no processes</span>
          ) : (
            !open && <span className="tabular-nums text-zinc-500">{bytes(total)}</span>
          )}
          <span className="flex gap-1 opacity-0 group-hover:opacity-100">
            {w.lease && (
              <IconButton title="Copy env (export lines)" onClick={() => api.copyEnv(w.path)}>
                <ClipboardCopy size={13} />
              </IconButton>
            )}
            {w.exists && (
              <IconButton title="Reveal in Finder" onClick={() => api.reveal(w.path)}>
                <FolderOpen size={13} />
              </IconButton>
            )}
          </span>
        </span>
      </div>
      {open && !idle && <div className="ml-4">{w.processes.map(row)}</div>}
    </div>
  );
}

function IconButton({ title, onClick, children }: { title: string; onClick: () => void; children: ReactNode }) {
  return (
    <button title={title} onClick={onClick} className="rounded p-1 text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700">
      {children}
    </button>
  );
}
