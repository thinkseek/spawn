import type { ReactNode } from "react";

import type { DiskPlace, FleetHealth, LadderStep } from "../../fleet";
import { absoluteTitle, clockTime, formatDuration } from "../../format";
import type { CoreMark } from "../../coreLines";
import type { Backups, DiskBucketShares, GateRoom, Memory, Storage, WorkspaceBackendView } from "../../resources";
import {
  backupAge,
  backupView,
  memoryBar,
  sharedBuildCacheView,
  storageView,
  waitingGates,
  workspaceBackendView,
  workspaceBreakdownView,
} from "../../resources";
import { cx } from "../ui";

export const LADDER: Array<{
  step: LadderStep | "liveness";
  label: string;
  bar: string;
  hint: string;
}> = [
  {
    step: "rules",
    label: "Rules",
    bar: "bg-muted-foreground",
    hint: "Settled by deterministic rules",
  },
  {
    step: "model",
    label: "Jev",
    bar: "bg-jev",
    hint: "Ambiguous cases settled by the model judge",
  },
  {
    step: "dispatcher",
    label: "Dispatcher",
    bar: "bg-info",
    hint: "Routine cases the dispatcher handled",
  },
  {
    step: "supervisor",
    label: "Supervisor",
    bar: "bg-chart-line",
    hint: "Decisions escalated to the supervisor agent",
  },
  {
    step: "human",
    label: "Human",
    bar: "bg-warning",
    hint: "Decisions that needed a person",
  },
];

export const LIVENESS = {
  label: "Liveness",
  bar: "bg-faint-foreground",
  hint: "Engines restarted automatically by the liveness check",
};

export function Cell({
  label,
  trailing,
  children,
  className,
}: {
  label: ReactNode;
  trailing?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cx("min-w-0 rounded-card bg-surface px-4 py-3 shadow-card", className)}>
      <div className="mb-1.5 flex items-center gap-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        <span className="truncate">{label}</span>
        {trailing && <span className="ml-auto shrink-0 normal-case tracking-normal">{trailing}</span>}
      </div>
      {children}
    </div>
  );
}

export function Stat({
  label,
  value,
  tone,
  title,
}: {
  label: string;
  value: ReactNode;
  tone?: string;
  title?: string;
}) {
  return (
    <span className="inline-flex items-baseline gap-1" title={title}>
      <span className={cx("tnum font-medium text-foreground", tone)}>
        {value}
      </span>
      <span className="text-faint-foreground">{label}</span>
    </span>
  );
}

function formatGib(bytes: number | null): string {
  return bytes == null ? "—" : `${(bytes / 1024 ** 3).toFixed(1)} GiB`;
}

/** Free disk at one place, as a bar against its warning and its floor:
 * the scale runs to twice the warning (or a little past the free space,
 * whichever is more), so the two marks always show. */
function DiskBar({ place }: { place: DiskPlace }) {
  const free = place.free_bytes;
  const scale = Math.max(place.warn_bytes * 2, (free ?? 0) * 1.15, 1);
  const at = (bytes: number) => `${Math.min(100, (bytes / scale) * 100)}%`;
  const tone =
    free == null
      ? { bar: "bg-faint-foreground", text: "text-faint-foreground", word: "unread" }
      : free < place.floor_bytes
        ? { bar: "bg-destructive", text: "text-destructive", word: "under the floor: no new executions" }
        : free < place.warn_bytes
          ? { bar: "bg-warning", text: "text-warning", word: "under the warning" }
          : { bar: "bg-success", text: "text-foreground", word: "fine" };
  return (
    <div
      className="min-w-0"
      title={`${place.path}: ${formatGib(free)} free (${tone.word}); a warning under ${formatGib(place.warn_bytes)}, no new executions under ${formatGib(place.floor_bytes)}`}
    >
      <div className="flex items-baseline gap-2 text-caption">
        <span className="text-muted-foreground">Disk · {place.label}</span>
        <span className={cx("tnum ml-auto font-medium", tone.text)}>{formatGib(free)}</span>
        <span className="text-faint-foreground">free</span>
      </div>
      <div className="relative mt-1 h-1.5 rounded-full bg-muted">
        {free != null && (
          <div className={cx("h-full rounded-full", tone.bar)} style={{ width: at(free) }} />
        )}
        {[
          ["floor", place.floor_bytes],
          ["warn", place.warn_bytes],
        ].map(([name, bytes]) => (
          <span
            key={name}
            aria-hidden="true"
            className="absolute -top-0.5 h-2.5 w-px bg-foreground/70"
            style={{ left: at(bytes as number) }}
          />
        ))}
      </div>
      <div className="tnum mt-0.5 flex justify-between text-micro text-faint-foreground">
        <span>floor {formatGib(place.floor_bytes)}</span>
        <span>warn {formatGib(place.warn_bytes)}</span>
      </div>
    </div>
  );
}

/** Memory available now (MemAvailable), as a bar of the total against the
 * warning line (4 GiB). A value the server could not read shows as unread,
 * and an unknown total scales the bar to the warning instead. */
function MemoryBar({ memory }: { memory: Memory | null }) {
  if (!memory) {
    return <p className="text-caption text-faint-foreground">This server reports no memory.</p>;
  }
  const available = memory.available_bytes;
  const { tone, scale } = memoryBar(memory);
  const at = (bytes: number) => `${Math.min(100, (bytes / scale) * 100)}%`;
  const style =
    tone === "unread"
      ? { bar: "bg-faint-foreground", text: "text-faint-foreground", word: "unread" }
      : tone === "short"
        ? { bar: "bg-warning", text: "text-warning", word: "under the warning" }
        : { bar: "bg-success", text: "text-foreground", word: "fine" };
  return (
    <div
      className="min-w-0"
      title={`Memory available (MemAvailable, read now): ${formatGib(available)} of ${
        memory.total_bytes == null ? "an unread total" : formatGib(memory.total_bytes)
      } (${style.word}); a warning under ${formatGib(memory.warn_bytes)}`}
    >
      <div className="flex items-baseline gap-2 text-caption">
        <span className="text-muted-foreground">Memory</span>
        <span className={cx("tnum ml-auto font-medium", style.text)}>{formatGib(available)}</span>
        <span className="text-faint-foreground">available</span>
      </div>
      <div className="relative mt-1 h-1.5 rounded-full bg-muted">
        {available != null && (
          <div className={cx("h-full rounded-full", style.bar)} style={{ width: at(available) }} />
        )}
        <span
          aria-hidden="true"
          className="absolute -top-0.5 h-2.5 w-px bg-foreground/70"
          style={{ left: at(memory.warn_bytes) }}
        />
      </div>
      <div className="tnum mt-0.5 flex justify-between text-micro text-faint-foreground">
        <span>warn {formatGib(memory.warn_bytes)}</span>
        <span>{memory.total_bytes == null ? "total unread" : `of ${formatGib(memory.total_bytes)}`}</span>
      </div>
    </div>
  );
}

/** The newest off-host backups (`health.backups`): the database copy and
 * the transcripts and sources, each with how old its newest upload is. */
function BackupRows({ backups }: { backups: Backups | null }) {
  if (!backups) {
    return <p className="mt-1.5 text-faint-foreground">This server reports no backups.</p>;
  }
  const now = Date.now();
  return (
    <ul className="mt-1.5 space-y-0.5 border-t border-surface-border pt-1.5">
      {backupView(backups, now).map((row) => (
        <li key={row.name} className="flex min-w-0 items-baseline gap-2">
          <span className="shrink-0 text-muted-foreground">{row.name}</span>
          <span className={cx("tnum shrink-0", row.age_ms === null ? "text-warning" : "font-medium text-foreground")}>
            {backupAge(row.age_ms)}
          </span>
          <span className="ml-auto min-w-0 truncate text-right text-faint-foreground" title={row.key ?? ""}>
            {row.key ?? ""}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** What uses the disk (`health.storage`): the archive, Claude Code's own
 * transcripts, the workspaces and the database, one row each with its size
 * and note, then how long the free space lasts at the growth of what
 * persists (the archive, Claude Code's transcripts and the database);
 * then how old the newest off-host backups are (`health.backups`). */
export function StorageCard({
  storage,
  backups = null,
  diagnostics = [],
}: {
  storage: Storage | null;
  backups?: Backups | null;
  /** The Core lines answer's diagnostics in words (objective 176). */
  diagnostics?: CoreMark[];
}) {
  if (!storage) {
    return (
      <div className="min-w-0 text-caption">
        <p className="text-faint-foreground">This server reports no storage.</p>
        <BackupRows backups={backups} />
        <DiagnosticRows diagnostics={diagnostics} />
      </div>
    );
  }
  const { rows, lasts, lasts_note, measured_from, lasts_from } = storageView(storage);
  const fell = (storage.projects?.before ?? 0) > (storage.projects?.files ?? Infinity);
  return (
    <div className="min-w-0 text-caption">
      <ul className="space-y-0.5">
        {rows.map((row, index) => (
          <li key={row.name} className="flex min-w-0 items-baseline gap-2">
            <span className="shrink-0 text-muted-foreground">{row.name}</span>
            <span className="tnum shrink-0 font-medium text-foreground">{row.size}</span>
            <span
              className={cx("ml-auto min-w-0 truncate text-right", index === 1 && fell ? "text-warning" : "text-faint-foreground")}
              title={row.note}
            >
              {row.note}
            </span>
            {row.measured_from !== null && row.note !== `measured from ${row.measured_from}` && (
              <span className="tnum shrink-0 text-faint-foreground" title="when this line's own growth is first measured">
                measured from {row.measured_from}
              </span>
            )}
          </li>
        ))}
      </ul>
      <p
        className="tnum mt-1.5 text-faint-foreground"
        title={`Free on the workspace root: ${formatGib(storage.free_bytes)}; sizes read ${clockTime(storage.read_at_ms)}`}
      >
        {lasts ??
          `How long the disk lasts: ${lasts_from !== null ? `measured from ${lasts_from}` : measured_from === null ? "not measured yet" : `measured from ${measured_from}`}`}
      </p>
      {lasts_note !== null && <p className="text-micro text-faint-foreground">{lasts_note}</p>}
      <WorkspaceBreakdownSection storage={storage} />
      <SharedBuildCacheRow storage={storage} />
      <BackupRows backups={backups} />
      <DiagnosticRows diagnostics={diagnostics} />
    </div>
  );
}

/** Stacked 3-bucket bar for workspace composition: `repo`, `.git`, `rebuildable`. */
function CompositionBar({ shares }: { shares: DiskBucketShares }) {
  return (
    <div
      className="flex h-1.5 w-full overflow-hidden rounded-full bg-muted"
      aria-label={`repo ${shares.repo_pct}%, .git ${shares.git_pct}%, rebuildable ${shares.rebuildable_pct}%`}
    >
      {shares.repo_pct > 0 && (
        <div className="h-full bg-foreground/70" style={{ width: `${shares.repo_pct}%` }} />
      )}
      {shares.git_pct > 0 && (
        <div className="h-full bg-info" style={{ width: `${shares.git_pct}%` }} />
      )}
      {shares.rebuildable_pct > 0 && (
        <div className="h-full bg-warning" style={{ width: `${shares.rebuildable_pct}%` }} />
      )}
    </div>
  );
}

function CompositionLegend({
  repo,
  git,
  rebuildable,
  shares,
}: {
  repo: string;
  git: string;
  rebuildable: string;
  shares: DiskBucketShares;
}) {
  return (
    <div className="tnum mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-micro text-faint-foreground">
      <span className="inline-flex items-center gap-1">
        <span aria-hidden="true" className="size-1.5 rounded-full bg-foreground/70" />
        repo {repo} ({shares.repo_pct}%)
      </span>
      <span className="inline-flex items-center gap-1">
        <span aria-hidden="true" className="size-1.5 rounded-full bg-info" />
        .git {git} ({shares.git_pct}%)
      </span>
      <span className="inline-flex items-center gap-1">
        <span aria-hidden="true" className="size-1.5 rounded-full bg-warning" />
        rebuildable {rebuildable} ({shares.rebuildable_pct}%)
      </span>
    </div>
  );
}

/** Per-workspace disk classification (`repo` / `.git` / rebuildable caches). */
function WorkspaceBreakdownSection({ storage }: { storage: Storage }) {
  const breakdown = workspaceBreakdownView(storage);
  if (!breakdown) return null;
  return (
    <div className="mt-1.5 border-t border-surface-border pt-1.5">
      <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
        <span className="text-muted-foreground">Workspace breakdown</span>
        <span className="tnum font-medium text-foreground">{breakdown.total}</span>
        <span className="tnum text-faint-foreground">
          {breakdown.workspace_count} {breakdown.workspace_count === 1 ? "workspace" : "workspaces"}
        </span>
        {breakdown.truncated && (
          <span className="rounded bg-wait-tint px-1 py-px text-micro text-warning">truncated</span>
        )}
      </div>
      {breakdown.workspace_count === 0 ? (
        <p className="mt-0.5 text-micro text-faint-foreground">No workspaces on disk.</p>
      ) : (
        <>
          <div className="mt-1">
            <CompositionBar shares={breakdown.shares} />
          </div>
          <CompositionLegend
            repo={breakdown.repo}
            git={breakdown.git}
            rebuildable={breakdown.rebuildable}
            shares={breakdown.shares}
          />
          {breakdown.workspaces.length > 0 && (
            <details className="mt-1">
              <summary className="cursor-pointer select-none text-micro text-faint-foreground hover:text-foreground">
                By workspace ({breakdown.shown_count}
                {breakdown.hidden_count > 0 ? ` of ${breakdown.workspace_count}` : ""})
              </summary>
              <ul className="mt-1 max-h-48 space-y-1.5 overflow-y-auto pr-1">
                {breakdown.workspaces.map((ws) => (
                  <li key={ws.name} className="space-y-0.5">
                    <div className="flex min-w-0 items-baseline gap-2">
                      <span className="min-w-0 truncate font-mono text-micro text-foreground" title={ws.name}>
                        {ws.name}
                      </span>
                      {ws.truncated && (
                        <span className="shrink-0 text-micro text-warning">truncated</span>
                      )}
                      <span className="tnum ml-auto shrink-0 font-medium text-foreground">
                        {ws.total}
                      </span>
                    </div>
                    <CompositionBar shares={ws.shares} />
                    <div className="tnum flex flex-wrap justify-between gap-x-2 text-micro text-faint-foreground">
                      <span>{ws.summary}</span>
                      <span>
                        {ws.files.toLocaleString("en-US")} {ws.files === 1 ? "file" : "files"}
                      </span>
                    </div>
                  </li>
                ))}
                {breakdown.hidden_count > 0 && (
                  <li className="tnum text-micro text-faint-foreground">
                    +{breakdown.hidden_count} more workspaces not shown
                  </li>
                )}
              </ul>
            </details>
          )}
        </>
      )}
    </div>
  );
}

/** Shared host build cache status, size, and path (`health.storage.shared_build_cache`). */
function SharedBuildCacheRow({ storage }: { storage: Storage }) {
  const cache = sharedBuildCacheView(storage);
  const badgeClass =
    cache.status === "enabled"
      ? "bg-muted text-success"
      : cache.status === "disabled"
        ? "bg-wait-tint text-warning"
        : "bg-muted text-faint-foreground";
  return (
    <div className="mt-1.5 border-t border-surface-border pt-1.5">
      <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
        <span className="shrink-0 text-muted-foreground">Shared build cache</span>
        <span className={cx("rounded px-1.5 py-px text-micro font-medium", badgeClass)}>
          {cache.badge}
        </span>
        {cache.size !== null && (
          <span className="tnum shrink-0 font-medium text-foreground">{cache.size}</span>
        )}
        <span className="tnum text-faint-foreground">{cache.note}</span>
        {cache.path && (
          <span
            className="ml-auto min-w-0 truncate font-mono text-micro text-faint-foreground"
            title={cache.path}
          >
            {cache.path}
          </span>
        )}
      </div>
    </div>
  );
}

/** The storage diagnostics (objective 176): each figure's name, value and
 * range, the out-of-range ones in the warning tone. Nothing without any.
 *
 * A name wraps rather than truncates (objective 227): what it says can sit
 * at its end (225's basis, "Free disk lost a day (the last day's fall)"),
 * and on a phone's one column (the viewport less 56 px) a cut name loses
 * it. The value stays whole on the name's first line (`shrink-0`, the row
 * `items-baseline`). Not a title: a phone has no hover, and the row's title
 * is already its range. */
function DiagnosticRows({ diagnostics }: { diagnostics: CoreMark[] }) {
  if (diagnostics.length === 0) return null;
  return (
    <ul className="mt-1.5 space-y-0.5" aria-label="Storage diagnostics">
      <li className="text-micro font-medium text-faint-foreground">Diagnostics</li>
      {diagnostics.map((mark) => (
        <li key={mark.name} className="flex min-w-0 items-baseline gap-2" title={mark.range !== "" ? mark.range : undefined}>
          <span className="min-w-0 text-pretty text-muted-foreground">{mark.name}</span>
          <span
            className={cx(
              "tnum ml-auto shrink-0",
              mark.tone === "warning" ? "text-warning" : mark.tone === "muted" ? "text-faint-foreground" : "text-foreground",
            )}
          >
            {mark.value}
          </span>
        </li>
      ))}
    </ul>
  );
}

function ago(ms: number | null): string {
  return ms == null ? "—" : formatDuration(Math.max(0, Date.now() - ms));
}

/** Each gated mission's waits for gate room: the gates waiting now (the
 * task, how many `no_room` since when, escalated or not), then the gated
 * missions whose gate has room. */
function GateRoomList({ rows }: { rows: GateRoom[] | null }) {
  if (rows === null) {
    return <p className="text-caption text-faint-foreground">This server reports no gate room.</p>;
  }
  const waiting = waitingGates(rows);
  const free = rows.filter((row) => row.task === null);
  return (
    <div className="min-w-0 text-caption">
      <div className="flex items-baseline gap-2">
        <span className="text-muted-foreground">Gate room</span>
        <span
          className={cx(
            "tnum ml-auto font-medium",
            waiting.length > 0 ? "text-warning" : rows.length === 0 ? "text-faint-foreground" : "text-foreground",
          )}
        >
          {rows.length === 0
            ? "no gated mission"
            : waiting.length === 0
              ? "no gate waits"
              : `${waiting.length} gate${waiting.length === 1 ? "" : "s"} waiting`}
        </span>
      </div>
      {waiting.map((row) => (
        <div
          key={row.mission}
          className="mt-0.5 flex min-w-0 items-baseline gap-1.5"
          title={`Mission #${row.mission} ${row.name}: task #${row.task}'s gate found no room in TMPDIR ${row.waits} time${
            row.waits === 1 ? "" : "s"
          } in a row, since ${clockTime(row.since_ms)}, latest ${clockTime(row.last_ms)}; ${
            row.escalated ? "escalated to the supervisor" : "not escalated yet (after two hours)"
          }`}
        >
          <span className="min-w-0 truncate text-muted-foreground">
            #{row.mission} {row.name}
          </span>
          <span className="shrink-0 font-mono text-micro text-faint-foreground">T{row.task}</span>
          <span className="tnum ml-auto shrink-0 text-warning">
            {row.waits}× for {ago(row.since_ms)}
          </span>
          {row.escalated && <span className="shrink-0 text-micro text-warning">escalated</span>}
        </div>
      ))}
      {free.length > 0 && (
        <div
          className="mt-0.5 truncate text-micro text-faint-foreground"
          title={free.map((row) => `#${row.mission} ${row.name}`).join("\n")}
        >
          room for {free.map((row) => `#${row.mission}`).join(", ")}
        </div>
      )}
    </div>
  );
}

export function formatRate(value: number | null): string {
  if (value == null) return "—";
  if (value === 0) return "0";
  if (value < 0.01) return value.toFixed(3);
  if (value < 10) return value.toFixed(2);
  return value.toFixed(1);
}

/** Liveness of the always-on agents, as the strip's first cell reads it. */
export interface Liveness {
  /** Always-on agents (the supervisor and the residents) and how many run. */
  total: number;
  up: number;
  /** The live always-on agent whose execution started last, and how long ago. */
  shortest: { name: string; ms: number; since: number } | null;
}

export const HUMAN_RATE_HINT =
  "Human per agent-hour: each time a person stepped in (a command, a pause, a resume, a task they assigned, a cancel and reassign) in the last 24 h, divided by the hours agents ran in that time; the Home card's figure. Lower is better; 0 means the fleet ran without anyone.";

/** Compact badge for `health.workspace_backend` when reported by the server. */
function WorkspaceBackendBadge({ view }: { view: WorkspaceBackendView }) {
  const toneClass =
    view.tone === "accent"
      ? "bg-muted text-info"
      : view.tone === "warning"
        ? "bg-wait-tint text-warning"
        : "bg-muted text-muted-foreground";
  return (
    <span
      className={cx("inline-flex items-center gap-1 rounded px-1.5 py-px text-micro font-medium", toneClass)}
      title={view.title}
    >
      <span className="text-faint-foreground">workspace</span>
      <span className="font-mono">{view.label}</span>
    </span>
  );
}

export function HealthStrip({
  health,
  liveness,
  diagnostics = [],
}: {
  health: FleetHealth;
  liveness: Liveness;
  /** The Core lines answer's diagnostics in words (objective 176). */
  diagnostics?: CoreMark[];
}) {
  const restarts = health.ladder_24h.liveness;
  const down = liveness.total - liveness.up;
  const workspaceBackend = workspaceBackendView(health.workspace_backend);
  const workspaceBackendBadge = workspaceBackend ? (
    <WorkspaceBackendBadge view={workspaceBackend} />
  ) : null;

  return (
    <section
      aria-label="Fleet health"
      className="grid shrink-0 grid-cols-1 gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.25fr)]"
    >
      <Cell label="Liveness">
        <div className="flex items-baseline gap-2">
          <span
            className={cx(
              "tnum text-display-sm leading-none font-semibold tracking-tight",
              liveness.total === 0
                ? "text-faint-foreground"
                : down > 0
                  ? "text-warning"
                  : "text-success",
            )}
          >
            {liveness.up}/{liveness.total}
          </span>
          <span className="text-caption text-muted-foreground">always-on agents up</span>
        </div>
        <div className="mt-2 space-y-0.5 text-caption text-muted-foreground">
          <div
            className="flex min-w-0 items-baseline gap-1"
            title={
              liveness.shortest
                ? `The ${liveness.shortest.name}'s current execution started ${absoluteTitle(liveness.shortest.since)}`
                : "No always-on agent is running"
            }
          >
            <span className="shrink-0">Shortest uptime</span>
            {liveness.shortest && (
              <span className="min-w-0 truncate text-faint-foreground">· {liveness.shortest.name}</span>
            )}
            <span className="tnum ml-auto shrink-0 font-medium text-foreground">
              {liveness.shortest ? formatDuration(liveness.shortest.ms) : "—"}
            </span>
          </div>
          <div className="flex items-baseline gap-1" title={LIVENESS.hint}>
            <span>Liveness restarts · 24h</span>
            <span
              className={cx(
                "tnum ml-auto font-medium",
                restarts > 0 ? "text-warning" : "text-foreground",
              )}
            >
              {restarts}
            </span>
          </div>
          <div className="flex items-baseline gap-1">
            <span>Agents live</span>
            <span className="tnum ml-auto font-medium text-foreground">
              {health.live}
              <span className="font-normal text-faint-foreground"> of {health.agents_total}</span>
            </span>
          </div>
        </div>
      </Cell>

      <Cell label="Resources">
        <div className="space-y-2.5">
          {health.disk.length === 0 ? (
            <p className="text-caption text-faint-foreground">This server reports no disk places.</p>
          ) : (
            health.disk.map((place) => (
              <DiskBar key={`${place.label}:${place.path}`} place={place} />
            ))
          )}
          <MemoryBar memory={health.memory} />
          <GateRoomList rows={health.gate_room} />
        </div>
      </Cell>

      <Cell label="Storage" className="sm:col-span-2" trailing={workspaceBackendBadge}>
        <StorageCard storage={health.storage} backups={health.backups} diagnostics={diagnostics} />
      </Cell>

    </section>
  );
}
