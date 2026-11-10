// The agent page's trims (design-ui-information §2.5, §4.6): the judgement
// as one line unless it says something the rules did not, the judgement
// history as runs, the summary as what the agent does and for how long.
// Pure, with no runtime import, so node can test it (test/agentPage.test.ts).

/** What these rules read of a judgement (fleet.ts `Judgement`). */
export interface JudgementLike {
  judge: string;
  state: string;
  reason: string | null;
  rules_state: string | null;
  attention: boolean | null;
}

/** What these rules read of a history record (fleet.ts `JudgementRecord`). */
export interface RecordLike {
  session_seq: number | null;
  recorded_at_ms: number | null;
  observation: JudgementLike;
}

/** A run of consecutive judgements that say the same thing. */
export interface JudgementRun<R extends RecordLike> {
  /** Newest first, as the history is. */
  records: R[];
  newestAt: number | null;
  oldestAt: number | null;
}

/** Reasons said shorter on one line than in the full card. */
const SHORT_REASON: Record<string, string> = {
  JUDGE_UNAVAILABLE: "judge unavailable",
};

/** "Rules", "Jev", or the judge's own name. */
export function judgeName(judge: string): string {
  const key = judge.toLowerCase();
  if (key === "rules") return "Rules";
  if (key === "jev") return "Jev";
  return judge;
}

function stateWord(state: string): string {
  return state.toLowerCase();
}

/** Whether the judgement says what the rules did not: it disagrees with
 * their guess, or the judge asks for attention. Only then is the full card
 * worth its room. */
export function judgementSpeaks(judgement: JudgementLike): boolean {
  const disagrees = judgement.rules_state !== null && judgement.rules_state !== judgement.state;
  return disagrees || judgement.attention === true;
}

/** The judgement in one line: "Jev: waiting · judge unavailable".
 * `reasonWords` gives a reason's words (fleet.ts `reasonText`). */
export function judgementLine(
  judgement: JudgementLike,
  reasonWords: (reason: string | null) => string | null,
): string {
  const parts = [`${judgeName(judgement.judge)}: ${stateWord(judgement.state)}`];
  const reason = judgement.reason
    ? (SHORT_REASON[judgement.reason] ?? reasonWords(judgement.reason))
    : null;
  if (reason) parts.push(reason.charAt(0).toLowerCase() + reason.slice(1));
  return parts.join(" · ");
}

/** Whether two records say the same thing: the same judge, state and
 * reason, and neither disagrees with the rules (a disagreement is news, so
 * it stands alone). */
function same(a: JudgementLike, b: JudgementLike): boolean {
  return (
    a.judge.toLowerCase() === b.judge.toLowerCase() &&
    a.state === b.state &&
    a.reason === b.reason &&
    !judgementSpeaks(a) &&
    !judgementSpeaks(b)
  );
}

/** The history (newest first) as runs of consecutive records that say the
 * same thing. */
export function judgementRuns<R extends RecordLike>(records: readonly R[]): JudgementRun<R>[] {
  const runs: JudgementRun<R>[] = [];
  for (const record of records) {
    const last = runs.at(-1);
    if (last && same(last.records[0].observation, record.observation)) {
      last.records.push(record);
      last.oldestAt = record.recorded_at_ms ?? last.oldestAt;
    } else {
      runs.push({ records: [record], newestAt: record.recorded_at_ms, oldestAt: record.recorded_at_ms });
    }
  }
  return runs;
}

/** A run's line: "Rules · Working ×9, 16:37–16:45"; one record has no
 * count and one time. `clock` formats a time (format.ts `clockTime`). */
export function runLabel<R extends RecordLike>(run: JudgementRun<R>, clock: (ms: number | null) => string): string {
  const first = run.records[0].observation;
  const state = first.state.charAt(0) + first.state.slice(1).toLowerCase();
  const head = `${judgeName(first.judge)} · ${state}`;
  if (run.records.length === 1) return `${head}, ${clock(run.newestAt)}`;
  const span = run.oldestAt === run.newestAt ? clock(run.newestAt) : `${clock(run.oldestAt)}–${clock(run.newestAt)}`;
  return `${head} ×${run.records.length}, ${span}`;
}

/** How long the agent has been in its state, for the summary's headline:
 * "for 12 min", "for 3 h", "for 2 d"; null when unknown or in the future. */
export function forHowLong(since: number | null, now: number): string | null {
  if (since === null || !Number.isFinite(since) || since > now) return null;
  const minutes = Math.floor((now - since) / 60_000);
  if (minutes < 1) return "for under a minute";
  if (minutes < 60) return `for ${minutes} min`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `for ${hours} h`;
  return `for ${Math.floor(hours / 24)} d`;
}

/** What a held task reads as in the summary when the agent has no input of
 * a person's (`task`): "T247 · Cost you can see", its title alone when no
 * task id is known, null when there is none. `held` is the task the agent's
 * name goes by (home.ts `nameTask`); `taskTitle` is /api/fleet's
 * `task_title`, which a worker carries even before /api/missions answers. */
export function taskLine(
  held: { task_id: number | null; title: string | null } | null,
  taskTitle: string | null,
): string | null {
  const title = (held?.title ?? taskTitle ?? "").trim() || null;
  const id = held?.task_id ?? null;
  if (id === null) return title;
  return title ? `T${id} · ${title}` : `T${id}`;
}

/** What the summary leads with: the person's input the agent holds
 * (/api/fleet `task`, a resident's or the supervisor's note), else the task
 * it works on ([`taskLine`]), else nothing ("No task recorded."). A worker
 * has no input: its `task` is null or empty, its task is `task_title`. */
export function summaryTask(
  agent: { task: string | null; task_title: string | null },
  held: { task_id: number | null; title: string | null } | null,
): { input: string } | { task: string } | null {
  if (agent.task && agent.task.trim() !== "") return { input: agent.task };
  const line = taskLine(held, agent.task_title);
  return line ? { task: line } : null;
}

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** A history time, in local time: `clock`'s alone on `now`'s day, with its
 * weekday within the last six days ("Mon 18:43:20"), with its date before
 * that ("Nov 3 18:43:20"), so a history across midnight reads in order.
 * `clock` formats the time of day (format.ts `clockTime`). */
export function historyClock(ms: number | null, now: number, clock: (ms: number | null) => string): string {
  if (ms === null || !Number.isFinite(ms)) return clock(ms);
  const at = new Date(ms);
  const today = new Date(now);
  const startOf = (date: Date) => new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const days = Math.round((startOf(today) - startOf(at)) / 86_400_000);
  if (days === 0) return clock(ms);
  if (days > 0 && days < 7) return `${WEEKDAYS[at.getDay()]} ${clock(ms)}`;
  return `${MONTHS[at.getMonth()]} ${at.getDate()} ${clock(ms)}`;
}
