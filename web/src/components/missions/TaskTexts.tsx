import { useEffect, useState } from "react";

import { fetchTaskTexts } from "../../missions";
import type { MissionTask, TaskTexts } from "../../missions";
import { formatBytes, isCut } from "../../validated";
import { cx } from "../ui";
import { Markdown } from "./Markdown";

/** Whether the board sent any of `task`'s texts cut (its first lines). */
export function hasCutText(task: MissionTask): boolean {
  return (
    isCut(task.brief, task.brief_bytes ?? null) ||
    isCut(task.report, task.report_bytes ?? null) ||
    isCut(task.review_note, task.review_note_bytes ?? null)
  );
}

export interface TaskTextsRead {
  /** The texts in full; null until read (or when none was cut). */
  texts: TaskTexts | null;
  error: string | null;
  loading: boolean;
}

/** `task`'s texts in full, read once `wanted` (the task opened, or a text
 * asked for) and again when the task changes; nothing is read when the
 * board sent every text whole. One GET for the three. */
export function useTaskTexts(task: MissionTask, wanted: boolean): TaskTextsRead {
  const [texts, setTexts] = useState<TaskTexts | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const id = task.id;
  const cut = hasCutText(task);
  useEffect(() => {
    if (!wanted || id === null || !cut) return;
    const controller = new AbortController();
    setLoading(true);
    fetchTaskTexts(id, controller.signal)
      .then((read) => {
        setTexts(read);
        setError(null);
      })
      .catch((cause: unknown) => {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        setError(cause instanceof Error ? cause.message : "request failed");
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
    // A change to the task (a report, a revision) reads its texts again.
  }, [id, wanted, cut, task.updated_at_ms]);
  return { texts: texts?.id === id ? texts : null, error, loading };
}

/** One of a task's texts: the whole once read, else the board's first
 * lines; a text sent cut offers the rest, a text not sent (a folded
 * task's report) offers itself. */
export function TaskText({
  what,
  excerpt,
  bytes,
  full,
  read,
  onWant,
  className,
}: {
  /** What the text is, for its button: "report", "brief", "note". */
  what: string;
  excerpt: string | null;
  bytes: number | null | undefined;
  /** The whole text, once read. */
  full: string | null | undefined;
  read: TaskTextsRead;
  onWant: () => void;
  className?: string;
}) {
  const whole = read.texts !== null ? (full ?? null) : null;
  const shown = whole ?? excerpt;
  const more = whole === null && isCut(excerpt, bytes ?? null);
  return (
    <>
      {shown && <Markdown source={shown} className={className} />}
      {more && (
        <p className={cx("text-caption", shown && "mt-1")}>
          {read.error ? (
            <span className="text-destructive">
              Could not read the {what}: {read.error}{" "}
            </span>
          ) : null}
          {read.loading ? (
            <span className="text-faint-foreground">Reading the {what}…</span>
          ) : (
            <button
              type="button"
              onClick={onWant}
              className="rounded text-info hover:underline"
            >
              {shown ? `Show the whole ${what}` : `Show the ${what}`} ({formatBytes(bytes ?? 0)})
            </button>
          )}
        </p>
      )}
    </>
  );
}
