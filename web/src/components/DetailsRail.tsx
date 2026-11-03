import { GitBranch, Terminal, Wrench } from "lucide-react";
import type { ReactNode } from "react";
import type { RsiSummary, SessionDetail } from "../api";
import { formatBytes, formatDuration } from "../format";

interface DetailsRailProps {
  rsi: RsiSummary | null;
  detail: SessionDetail | null;
}

export function DetailsRail({ rsi, detail }: DetailsRailProps) {
  const execution = detail
    ? ([...detail.executions].sort(
        (left, right) => right.started_at_ms - left.started_at_ms,
      )[0] ?? null)
    : null;
  const tools = detail?.open_tools ?? [];

  return (
    <aside className="flex w-80 shrink-0 flex-col gap-3 overflow-y-auto border-l border-surface-border bg-app-shell p-3">
      <Card title="Execution" icon={<Terminal size={12} />}>
        {execution ? (
          <>
            <Field label="Engine" value={execution.engine || "—"} />
            <Field
              label="Duration"
              value={formatDuration(execution.duration_ms)}
            />
            <Field label="Cause" value={execution.cause ?? "—"} />
            <Field
              label="Exit code"
              value={
                execution.exit_code === null ? "—" : String(execution.exit_code)
              }
            />
            <Field
              label="Exit reported"
              value={execution.exit_reported ? "yes" : "no"}
            />
          </>
        ) : (
          <Empty text="No executions yet." />
        )}
      </Card>

      <Card
        title={tools.length > 0 ? `Open tools · ${tools.length}` : "Open tools"}
        icon={<Wrench size={12} />}
      >
        {tools.length === 0 ? (
          <Empty text="No tools waiting." />
        ) : (
          <div className="space-y-1.5">
            {tools.map((tool) => (
              <div
                key={`${tool.tool_use_id}-${tool.session_seq}`}
                className="rounded-md border border-surface-border bg-page-canvas px-2 py-1.5"
              >
                <div className="text-label text-foreground">
                  {tool.tool_name}
                </div>
                <div
                  className="truncate font-mono text-micro text-muted-foreground"
                  title={tool.tool_use_id}
                >
                  {tool.tool_use_id}
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>

      <Card title="RSI" icon={<GitBranch size={12} />}>
        {rsi ? (
          <>
            <Field
              label="Baseline"
              value={
                <span className="font-mono">{rsi.baseline.slice(0, 8)}</span>
              }
            />
            <Field label="Iteration" value={String(rsi.iteration)} />
            <Field
              label="Phase"
              value={rsi.open_iteration?.phase ?? "—"}
            />
            <Field
              label="Consecutive rejections"
              value={String(rsi.consecutive_rejected)}
            />
            <div className="flex items-baseline justify-between gap-3">
              <span className="shrink-0 text-caption text-muted-foreground">
                Solved
              </span>
              {rsi.solved_tasks.length === 0 ? (
                <span className="text-label text-muted-foreground">—</span>
              ) : (
                <span className="flex flex-wrap justify-end gap-1">
                  {rsi.solved_tasks.map((task) => (
                    <span
                      key={task}
                      className="rounded-xs bg-muted px-1.5 py-0.5 font-mono text-micro text-muted-foreground"
                    >
                      {task}
                    </span>
                  ))}
                </span>
              )}
            </div>

            <div className="mt-1 border-t border-surface-border pt-2">
              <div className="mb-1.5 text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
                Open iteration
              </div>
              {rsi.open_iteration ? (
                <>
                  <Field
                    label="Task"
                    value={`#${rsi.open_iteration.task_id}`}
                  />
                  <Field label="Phase" value={rsi.open_iteration.phase} />
                  <Field
                    label="Patch"
                    value={formatBytes(rsi.open_iteration.patch_bytes)}
                  />
                </>
              ) : (
                <p className="text-caption text-faint-foreground">
                  No open iteration.
                </p>
              )}
            </div>
          </>
        ) : (
          <Empty text="Loop not seeded." />
        )}
      </Card>
    </aside>
  );
}

function Card({
  title,
  icon,
  children,
}: {
  title: string;
  icon: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="rounded-xl border border-surface-border bg-surface p-3.5">
      <header className="mb-2.5 flex items-center gap-1.5 text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
        {icon}
        {title}
      </header>
      <div className="space-y-1.5">{children}</div>
    </section>
  );
}

function Field({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="shrink-0 text-caption text-muted-foreground">
        {label}
      </span>
      <span className="min-w-0 truncate text-right text-label text-foreground">
        {value}
      </span>
    </div>
  );
}

function Empty({ text }: { text: string }) {
  return (
    <p className="py-2 text-center text-caption text-faint-foreground">
      {text}
    </p>
  );
}
