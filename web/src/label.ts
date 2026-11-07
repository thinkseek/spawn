// Semantic labels. The server derives one for every agent, session, task
// and attention item (crates/spawn-cli/src/serve/labels.rs): one line that
// says what it is doing, e.g. `Supply PA owner work · task 156 · Write the
// slice 12 owner contract · cancelled`. A row shows it first and keeps the
// raw id beside it, smaller: ids are the keys of the CLI and of SQL.

export interface Labels {
  /** One line, at most about 90 characters; null from an older server. */
  label: string | null;
  /** The task's title (an objective's backlog title, or its brief's first
   * sentence), when it has a task. */
  task_title: string | null;
  /** The mission's name, when it belongs to one. */
  mission_name: string | null;
}

function line(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const one = value.replace(/\s+/g, " ").trim();
  return one === "" ? null : one;
}

/** The label fields of a server object; nulls for any it lacks. */
export function labels(value: unknown): Labels {
  const raw =
    typeof value === "object" && value !== null && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : {};
  return {
    label: line(raw.label),
    task_title: line(raw.task_title),
    mission_name: line(raw.mission_name),
  };
}
