import { FileDiff, ListOrdered, ScrollText, X } from "lucide-react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { formatBytes, formatDuration } from "../../format";
import type { RsiIteration } from "../../rsi";
import { fetchIteration, isTerminalPhase } from "../../rsi";
import { PhaseBadge, useNow } from "./Phase";

export function iterationDuration(
  iteration: RsiIteration,
  now: number,
): number | null {
  if (iteration.started_at_ms <= 0) return null;
  const end = isTerminalPhase(iteration.phase) ? iteration.updated_at_ms : now;
  return Math.max(0, end - iteration.started_at_ms);
}

interface IterationListProps {
  iterations: RsiIteration[];
  selected: number | null;
  onSelect: (iteration: number) => void;
}

export function IterationList({
  iterations,
  selected,
  onSelect,
}: IterationListProps) {
  const now = useNow(1_000);
  return (
    <section className="rounded-xl border border-surface-border bg-surface">
      <header className="flex items-center gap-1.5 border-b border-surface-border px-3.5 py-2.5 text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
        <ListOrdered size={12} />
        Iterations
        <span className="ml-auto normal-case tracking-normal">
          {iterations.length > 0 ? `${iterations.length} shown` : ""}
        </span>
      </header>
      {iterations.length === 0 ? (
        <p className="px-3.5 py-6 text-center text-caption text-faint-foreground">
          No iterations yet.
        </p>
      ) : (
        <ul className="max-h-[28rem] divide-y divide-surface-border overflow-y-auto">
          {iterations.map((iteration) => {
            const active = iteration.iteration === selected;
            return (
              <li key={iteration.iteration}>
                <button
                  type="button"
                  onClick={() => onSelect(iteration.iteration)}
                  aria-current={active ? "true" : undefined}
                  className={`flex w-full items-start gap-3 px-3.5 py-2.5 text-left transition-colors ${
                    active ? "bg-surface-selected" : "hover:bg-surface-hover"
                  }`}
                >
                  <span className="w-10 shrink-0 pt-0.5 font-mono text-label text-muted-foreground">
                    #{iteration.iteration}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="flex items-center gap-2">
                      <PhaseBadge phase={iteration.phase} />
                      <span className="text-caption text-muted-foreground">
                        task {iteration.task_id}
                      </span>
                      <span className="ml-auto shrink-0 font-mono text-micro text-faint-foreground">
                        {formatDuration(iterationDuration(iteration, now))}
                      </span>
                    </span>
                    {iteration.note && (
                      <span
                        className="mt-1 block truncate text-caption text-muted-foreground"
                        title={iteration.note}
                      >
                        {iteration.note}
                      </span>
                    )}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

interface IterationDetailProps {
  iteration: number;
  /** Phase from the dashboard poll, so polling stops once it settles. */
  phase: string | null;
  onClose: () => void;
}

/** Side panel with the gate log and the patch of one iteration. */
export function IterationDetail({
  iteration,
  phase,
  onClose,
}: IterationDetailProps) {
  const settled = phase !== null && isTerminalPhase(phase);
  const detail = usePoll(
    `rsi-iteration-${iteration}`,
    (signal) => fetchIteration(iteration, signal),
    settled ? 600_000 : 3_000,
  );
  const data = detail.data;

  return (
    <aside className="fixed inset-y-0 right-0 z-30 flex w-full max-w-3xl flex-col border-l border-surface-border bg-app-shell shadow-2xl">
      <header className="flex items-center gap-3 border-b border-surface-border bg-page-canvas px-5 py-3">
        <h2 className="font-mono text-title-sm font-medium text-foreground">
          Iteration #{iteration}
        </h2>
        {data && <PhaseBadge phase={data.phase} />}
        {data && (
          <span className="text-caption text-muted-foreground">
            task {data.task_id}
          </span>
        )}
        <button
          type="button"
          onClick={onClose}
          aria-label="Close"
          className="ml-auto rounded-md p-1 text-muted-foreground hover:bg-surface-hover hover:text-foreground"
        >
          <X size={16} />
        </button>
      </header>

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {detail.error
              ? `Could not load this iteration: ${detail.error}`
              : "Loading…"}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 space-y-3 overflow-y-auto p-4">
          <div className="grid grid-cols-2 gap-x-6 gap-y-1.5 rounded-xl border border-surface-border bg-surface p-3.5">
            <Field
              label="Candidate"
              value={
                <span className="font-mono">
                  {data.candidate ? data.candidate.slice(0, 12) : "—"}
                </span>
              }
            />
            <Field
              label="Patch"
              value={formatBytes(new Blob([data.patch]).size)}
            />
            <Field label="Updates" value={settled ? "settled" : "every 3s"} />
            <Field label="Note" value={data.note ?? "—"} />
          </div>

          {data.prompt && (
            <Block title="Prompt" icon={<ScrollText size={12} />}>
              <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-words px-3.5 py-3 font-mono text-caption text-muted-foreground">
                {data.prompt}
              </pre>
            </Block>
          )}

          <Block title="Gate log" icon={<ScrollText size={12} />}>
            <pre className="max-h-96 overflow-auto whitespace-pre-wrap break-words px-3.5 py-3 font-mono text-caption text-foreground">
              {data.gate_log || "No gate output yet."}
            </pre>
          </Block>

          <Block title="Patch" icon={<FileDiff size={12} />}>
            {data.patch ? (
              <DiffView patch={data.patch} />
            ) : (
              <p className="px-3.5 py-4 text-caption text-faint-foreground">
                No patch captured yet.
              </p>
            )}
          </Block>
        </div>
      )}
    </aside>
  );
}

function lineClass(line: string): string {
  if (line.startsWith("diff ") || line.startsWith("index ")) {
    return "font-medium text-foreground";
  }
  if (line.startsWith("+++") || line.startsWith("---")) {
    return "text-muted-foreground";
  }
  if (line.startsWith("@@")) return "bg-info/10 text-info";
  if (line.startsWith("+")) return "bg-success/10 text-success";
  if (line.startsWith("-")) return "bg-destructive/10 text-destructive";
  return "text-muted-foreground";
}

function DiffView({ patch }: { patch: string }) {
  const lines = patch.replace(/\n$/, "").split("\n");
  return (
    <div className="max-h-[36rem] overflow-auto py-2 font-mono text-caption">
      {lines.map((line, index) => (
        <div
          // Lines repeat, so the index is part of the key.
          key={index}
          className={`whitespace-pre px-3.5 ${lineClass(line)}`}
        >
          {line || " "}
        </div>
      ))}
    </div>
  );
}

function Block({
  title,
  icon,
  children,
}: {
  title: string;
  icon: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="overflow-hidden rounded-xl border border-surface-border bg-surface">
      <header className="flex items-center gap-1.5 border-b border-surface-border px-3.5 py-2 text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
        {icon}
        {title}
      </header>
      <div className="bg-page-canvas">{children}</div>
    </section>
  );
}

function Field({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex min-w-0 items-baseline justify-between gap-3">
      <span className="shrink-0 text-caption text-muted-foreground">
        {label}
      </span>
      <span className="min-w-0 truncate text-right text-label text-foreground">
        {value}
      </span>
    </div>
  );
}
