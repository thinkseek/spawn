import type { SessionState } from "../api";

const DOT: Record<SessionState, string> = {
  working: "bg-success animate-spawn-pulse",
  waiting: "bg-info",
  blocked: "bg-warning",
  completed: "bg-muted-foreground",
  unknown: "bg-faint-foreground",
};

const LABEL: Record<SessionState, string> = {
  working: "text-success",
  waiting: "text-info",
  blocked: "text-warning",
  completed: "text-muted-foreground",
  unknown: "text-faint-foreground",
};

export function StateDot({
  state,
  size = "md",
}: {
  state: SessionState;
  size?: "sm" | "md";
}) {
  return (
    <span
      aria-hidden="true"
      className={`inline-block shrink-0 rounded-full ${
        size === "sm" ? "size-1.5" : "size-2"
      } ${DOT[state]}`}
    />
  );
}

export function StateBadge({ state }: { state: SessionState }) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-sm border border-surface-border bg-surface px-1.5 py-0.5 text-micro font-medium ${LABEL[state]}`}
    >
      <StateDot state={state} size="sm" />
      {state}
    </span>
  );
}
