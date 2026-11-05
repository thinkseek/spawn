import type { SpanKind, TraceMarker } from "../../trace";

/** Draw order and legend order: executions, turns and tools take the
 * first three categorical slots in slot order; commands are neutral. */
export const KIND_ORDER: SpanKind[] = ["execution", "turn", "tool", "command"];

export const KIND_COLOR: Record<SpanKind, string> = {
  execution: "var(--trace-execution)",
  turn: "var(--trace-turn)",
  tool: "var(--trace-tool)",
  command: "var(--trace-command)",
};

export const KIND_LABEL: Record<SpanKind, string> = {
  execution: "Execution",
  turn: "Turn",
  tool: "Tool call",
  command: "Command",
};

/** Point events wear a status color only when they report a state;
 * everything else is a quiet neutral dot. */
export function markerColor(marker: TraceMarker): string {
  const name = marker.event.event_name;
  if (marker.failed) return "var(--destructive)";
  if (name === "artifact.committed") return "var(--success)";
  if (name.startsWith("notification.")) return "var(--warning)";
  return "var(--muted-foreground)";
}
