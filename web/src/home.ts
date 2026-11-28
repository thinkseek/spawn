// The home page's read model (design-ui-information.md §3): what needs the
// owner, how each mission moves today, who is working now and on what, the
// supervisor's latest lines, and what happened today. Every answer is read
// from the endpoints the page already polls (/api/attention, /api/missions,
// /api/fleet, /api/alerts); nothing here decides what the dispatcher or the
// alert step decides, it only selects and words it.
//
// Pure: its runtime imports (attention.ts, ordering.ts) are pure too, so
// `node --test` loads it as it is (web/test/home.test.ts).

import type { Alert } from "./api";
import type { Attention, AttentionItem } from "./attention";
import { itemSince, questionSummary } from "./attention.ts";
import type { Activity, FleetAgent, FleetHealth } from "./fleet";
import type { Mission, Missions, MissionTask } from "./missions";
import type { FleetContext, TaskRef } from "./ordering";
import {
  heldTask,
  isActiveMission,
  isOpenTask,
  missionClass,
  missionName,
  residentName,
  taskStateLabel,
} from "./ordering.ts";

/** "Today" on the home page: the last 24 hours (the lead's default). */
export const TODAY_MS = 24 * 3_600_000;
/** A supervisor note for the owner is shown in full while younger. */
export const NOTE_FRESH_MS = 6 * 3_600_000;
/** At most this many of the fresh notes are shown; the rest are counted. */
export const NOTES_SHOWN = 2;
/** A held source asks for the owner this long before its hold expires. */
export const HOLD_SOON_MS = 30 * 60_000;

// ---------------------------------------------------------------------------
// Alerts that no longer apply

/** An open alert about a session that has ended is moot: there is nothing
 * left to act on (the stuck agent is gone). It is never in the top band nor
 * in "Needs you", and it is not counted. Unknown sessions (no fleet yet, or
 * a subject that is not a session) are not guessed moot. */
export function alertMoot(alert: Pick<Alert, "kind" | "subject">, agents: readonly FleetAgent[] | null): boolean {
  if (alert.kind !== "agent_stuck" || !agents) return false;
  const agent = agents.find((entry) => entry.session_id === alert.subject);
  return agent !== undefined && !agent.live;
}

/** The open alerts that still apply. */
export function liveAlerts(alerts: readonly Alert[] | null, agents: readonly FleetAgent[] | null): Alert[] {
  return (alerts ?? []).filter((alert) => alert.state === "open" && !alertMoot(alert, agents));
}

/** A stuck alert whose score is stale: the live session was judged again
 * after the alert fired, and that judgement carries no `stuck` score (the
 * model judge did not answer, so the rules judged it), so nothing has
 * scored it since. The alert stays open on the old score (the server clears
 * it only on a score under 0.5 or the session's end); the page says so and
 * does not ask the owner to act on it. Activity after the alert is not the
 * test: a stuck agent loops, so it keeps acting. Returns the session's
 * agent, or null when the alert is not stale. */
export function alertStale(
  alert: Pick<Alert, "kind" | "subject" | "fired_at_ms">,
  agents: readonly FleetAgent[] | null,
): FleetAgent | null {
  if (alert.kind !== "agent_stuck" || !agents) return null;
  const agent = agents.find((entry) => entry.session_id === alert.subject);
  if (!agent || !agent.live || agent.judged_at_ms === null) return null;
  if (agent.judged_at_ms <= alert.fired_at_ms) return null;
  return agent.judgement && agent.judgement.stuck === null ? agent : null;
}

/** The open alerts the owner may act on: those that apply and are not
 * stale. The band and the Alerts badge count these. */
export function actionableAlerts(alerts: readonly Alert[] | null, agents: readonly FleetAgent[] | null): Alert[] {
  return liveAlerts(alerts, agents).filter((alert) => alertStale(alert, agents) === null);
}

/** "5 h", "52 min": an age in the words a line can hold. */
export function ageText(ms: number): string {
  const minutes = Math.max(0, Math.round(ms / 60_000));
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.round(minutes / 60);
  return hours < 48 ? `${hours} h` : `${Math.round(hours / 24)} d`;
}

// ---------------------------------------------------------------------------
// Needs you

/** `act`: the owner has something to do; `know`: worth knowing, nothing
 * to do yet. */
export type NeedTone = "act" | "know";

export interface NeedItem {
  key: string;
  tone: NeedTone;
  /** What it is, in a few words. */
  title: string;
  /** One line of detail, or null. */
  detail: string | null;
  /** The whole text when `detail` shortens it; shown on expand. */
  full?: string | null;
  since_ms: number | null;
  /** The command that acts on it, to copy. */
  command: string | null;
  /** A halted mission's resume from the page (`POST
   * /api/missions/<id>/resume`): the mission and the halt it resumes
   * (its `mission.halted` event), so a page not refreshed resumes no other. */
  resume?: { mission: number; halt: number } | null;
  /** The session whose page answers it (a question, an escalated agent). */
  session: string | null;
  /** An alert's judgement verb (objective 092), named beside it: `spawn
   * alert judge <key> false|stale|duplicate`, for one that should not have
   * been raised. */
  judge?: string | null;
}

/** The owner's verb for an alert that should not have been raised. */
export function judgeCommand(key: string): string {
  return `spawn alert judge ${key} false|stale|duplicate`;
}

export interface NeedsInput {
  attention: Attention | null;
  missions: Missions | null;
  alerts: readonly Alert[] | null;
  agents: readonly FleetAgent[] | null;
  health: FleetHealth | null;
  /** Always-on agents expected and not running (ordering.missingAlwaysOn). */
  missingAlwaysOn: number;
  now: number;
  /** An agent's one name by its session id (agentName), for an alert whose
   * subject is a session; null keeps the subject as it is. */
  nameOf?: (session: string) => string | null;
}

const GIB = 1024 ** 3;

function gib(bytes: number): string {
  return `${(bytes / GIB).toFixed(bytes < 10 * GIB ? 1 : 0)} GiB`;
}

/** A dispatcher stall line ("task #221 (mission #6) has not moved from
 * step judgement for 1h31m (bound 1h00m): …") in one line: "T221 waiting
 * for its judgement 1h31m (bound 1h)". */
const STALL_LINE = /^task #(\d+)(?: \(mission #\d+\))? has not moved from step ([\w ]+?) for (\S+) \(bound (\S+)\)/;

function shortDuration(text: string): string {
  return text.replace(/(\d+h)00m$/, "$1").replace(/^0h(\d+m)$/, "$1");
}

/** An escalation's line in one line: a stall in words, else its first
 * clause (up to the first ": "), else the line. */
export function oneLine(line: string): string {
  const stall = STALL_LINE.exec(line);
  if (stall) {
    const [, task, step, waited, bound] = stall;
    return `T${task} waiting for its ${step} ${shortDuration(waited)} (bound ${shortDuration(bound)})`;
  }
  const clause = line.indexOf(": ");
  return clause > 0 && clause < line.length - 2 ? line.slice(0, clause) : line;
}

function escalation(item: AttentionItem, told: boolean): NeedItem {
  const short = oneLine(item.line);
  const full = told ? `${item.line} (the supervisor was told)` : item.line;
  return {
    key: `attention:${item.tag}:${item.session_id}`,
    // Told items went to the supervisor and are unresolved: worth knowing;
    // an untold one is the owner's to look at now.
    tone: told ? "know" : "act",
    title: item.why.charAt(0).toUpperCase() + item.why.slice(1),
    detail: told ? `${short} · the supervisor was told` : short,
    full: full !== short ? full : null,
    since_ms: itemSince(item),
    command: null,
    session: item.agent_id ? item.session_id : null,
  };
}

/** Scenario names a halted mission's item names in its line; the rest are
 * counted ("and N more") and named in full on expand. */
export const GONE_SHOWN = 5;

/** A halt's line: the reason, then, when scenarios are gone, at most
 * GONE_SHOWN of their names and "and N more" for the rest; `full` names
 * them all when the line does not. */
export function haltWords(reason: string, gone: readonly string[] | null): { detail: string; full: string | null } {
  if (!gone || gone.length === 0) return { detail: reason, full: null };
  const shown = gone.slice(0, GONE_SHOWN).join(", ");
  const rest = gone.length - GONE_SHOWN;
  const detail = `${reason}; gone: ${shown}${rest > 0 ? ` and ${rest} more` : ""}`;
  return { detail, full: rest > 0 ? `${reason}; gone: ${gone.join(", ")}` : null };
}

/** An owner landing's `validator_defect` alerts from before objective 056,
 * one per mission (evidence `owner` and `mission`, no `source`), folded by
 * landing and scenario (evidence `event_id`, `scenario`): the first alert of
 * each group maps to the missions it names together (`mission #4, mission
 * #7`), every other to null (said by the first). Alerts of one source
 * (objective 056) and a task's own are not in the map. */
export function foldedDefects(alerts: readonly Alert[]): Map<string, string | null> {
  const groups = new Map<string, Alert[]>();
  for (const alert of alerts) {
    if (alert.kind !== "validator_defect") continue;
    const evidence = (alert.evidence ?? {}) as Record<string, unknown>;
    if (evidence.owner !== true || typeof evidence.mission !== "number" || "source" in evidence) continue;
    const group = `${String(evidence.event_id)}\u0000${String(evidence.scenario)}`;
    groups.set(group, [...(groups.get(group) ?? []), alert]);
  }
  const folded = new Map<string, string | null>();
  for (const group of groups.values()) {
    if (group.length < 2) continue;
    const named = group
      .map((alert) => (alert.evidence as { mission: number }).mission)
      .sort((left, right) => left - right)
      .map((mission) => `mission #${mission}`)
      .join(", ");
    group.forEach((alert, index) => folded.set(alert.key, index === 0 ? named : null));
  }
  return folded;
}

/** An alert's words as of `now`. A `mission_idle` alert's text says how
 * long the mission had been without work when it fired; said from its
 * evidence's `since_ms`, it says how long as of now (objective 071). Any
 * other alert, or one without a numeric `since_ms`, keeps its text. */
export function alertWords(alert: Pick<Alert, "kind" | "subject" | "text" | "evidence">, now: number): string {
  if (alert.kind !== "mission_idle") return alert.text;
  const since = (alert.evidence as { since_ms?: unknown } | null)?.since_ms;
  if (typeof since !== "number" || !Number.isFinite(since)) return alert.text;
  return `${alert.subject} has had no task in work for ${ageText(now - since)} since its last task ended: its objective backlog has nothing open to assign`;
}

/** A row's words on the Alerts page as of `now`: an open alert's are
 * `alertWords`; a cleared or notice one keeps its stored text (its mission is
 * no longer without work, so its time does not count on; objective 080). */
export function alertRowWords(
  alert: Pick<Alert, "kind" | "subject" | "text" | "evidence" | "state">,
  now: number,
): string {
  return alert.state === "open" ? alertWords(alert, now) : alert.text;
}

/** The open-alerts banner's line on every page but Home: how many, the
 * newest one's (by `fired_at_ms`) words as of `now`, and when it fired; null
 * for none (objective 080). */
export function openAlertsLine(
  alerts: readonly Pick<Alert, "kind" | "subject" | "text" | "evidence" | "fired_at_ms">[],
  now: number,
): { count: number; text: string; fired_at_ms: number } | null {
  if (alerts.length === 0) return null;
  const newest = alerts.reduce((latest, alert) => (alert.fired_at_ms > latest.fired_at_ms ? alert : latest));
  return { count: alerts.length, text: alertWords(newest, now), fired_at_ms: newest.fired_at_ms };
}

/** Every item that needs the owner, actions first, oldest first within. */
export function needsYou(input: NeedsInput): NeedItem[] {
  const items: NeedItem[] = [];
  const { attention, missions, health } = input;
  const supervisor = attention?.supervisor ?? null;
  if (supervisor?.question) {
    const summary = questionSummary(supervisor.question);
    items.push({
      key: "question",
      tone: "act",
      title: "The supervisor asks you",
      detail: summary.text,
      since_ms: supervisor.question.asked_at_ms,
      command: null,
      session: supervisor.session_id,
    });
  }
  for (const item of attention?.pending ?? []) items.push(escalation(item, false));
  for (const item of attention?.told ?? []) items.push(escalation(item, true));

  const halted = new Set<number>();
  for (const mission of missions?.missions ?? []) {
    if (!mission.halted || mission.id === null) continue;
    halted.add(mission.id);
    // normalizeMissions read the halt: null without an event to resume.
    const halt = mission.halt ?? null;
    const words = haltWords(mission.halted, halt?.gone ?? null);
    items.push({
      key: `halted:${mission.id}`,
      tone: "act",
      title: `Mission #${mission.id} ${missionName(mission)} is halted`,
      detail: words.detail,
      full: words.full,
      since_ms: halt?.at_ms ?? mission.updated_at_ms,
      command: `spawn mission resume --id ${mission.id}`,
      resume: halt ? { mission: mission.id, halt: halt.event } : null,
      session: null,
    });
  }
  for (const hold of missions?.holds ?? attention?.holds ?? []) {
    const left = hold.expires_ms === null ? null : hold.expires_ms - input.now;
    const soon = left !== null && left <= HOLD_SOON_MS;
    if (!soon && hold.waiting.length === 0) continue;
    const waiting = hold.waiting.length;
    items.push({
      key: `hold:${hold.hold}`,
      tone: "act",
      title: `Source held${left !== null ? `, ${left <= 0 ? "expired" : `expires in ${Math.ceil(left / 60_000)} min`}` : ""}`,
      detail: `${hold.source}${waiting ? ` · ${waiting} task${waiting === 1 ? "" : "s"} waiting` : ""}${hold.reason ? ` · ${hold.reason}` : ""}`,
      since_ms: hold.held_at_ms,
      command: `spawn mission release --source ${hold.source}`,
      session: null,
    });
  }
  const live = liveAlerts(input.alerts, input.agents);
  const folded = foldedDefects(live);
  for (const alert of live) {
    // A halted mission is said once, with its command.
    if (alert.kind === "mission_halted" && [...halted].some((id) => alert.subject === `mission #${id}`)) continue;
    // An owner landing's validator alerts from before they were one per
    // source (one per mission) are one item, naming every mission.
    const fold = folded.get(alert.key);
    if (fold === null) continue;
    const named = input.nameOf?.(alert.subject) ?? null;
    const stale = alertStale(alert, input.agents);
    const evidence = alert.evidence as { stuck?: unknown; command?: unknown; withdraw?: unknown } | null;
    const score = evidence?.stuck;
    // The one command that acts on it, when the alert step named one (a
    // stuck worker's `spawn task restart --id <task>`; an owner's landing
    // request waiting for its verdict, the `spawn mission push --withdraw
    // <request>` that ends it).
    const namedCommand = alert.kind === "owner_request_waiting" ? evidence?.withdraw : evidence?.command;
    const command = typeof namedCommand === "string" ? namedCommand : null;
    items.push({
      key: `alert:${alert.key}`,
      tone: stale ? "know" : "act",
      title: fold !== undefined ? alertTitle({ kind: alert.kind, subject: fold }) : alertTitle(alert, named),
      detail: stale
        ? `scored stuck${typeof score === "number" ? ` ${score.toFixed(2)}` : ""} ${ageText(input.now - alert.fired_at_ms)} ago; not scored since (its latest judgement, ${ageText(input.now - (stale.judged_at_ms ?? input.now))} ago, has no score${stale.judgement?.reason ? `: ${stale.judgement.reason}` : ""})`
        : alertWords(alert, input.now),
      since_ms: alert.fired_at_ms,
      command,
      // A session's alert opens that agent's page.
      session: named !== null ? alert.subject : null,
      // A folded owner's validator item stands for several alerts: no one
      // key to judge.
      judge: fold === undefined ? judgeCommand(alert.key) : null,
    });
  }

  if (input.missingAlwaysOn > 0) {
    items.push({
      key: "liveness",
      tone: "know",
      title: `${input.missingAlwaysOn} always-on agent${input.missingAlwaysOn === 1 ? " is" : "s are"} not running`,
      detail: "The supervisor restarts them; it is said here until they are back.",
      since_ms: null,
      command: null,
      session: null,
    });
  }
  for (const place of health?.disk ?? []) {
    if (place.free_bytes === null || place.free_bytes >= place.warn_bytes) continue;
    items.push({
      key: `disk:${place.label}`,
      tone: "know",
      title: `Disk ${place.label}: ${gib(place.free_bytes)} free`,
      detail: `under its warning (${gib(place.warn_bytes)}); no execution starts under ${gib(place.floor_bytes)}`,
      since_ms: null,
      command: null,
      session: null,
    });
  }
  const memory = health?.memory ?? null;
  if (memory && memory.available_bytes !== null && memory.available_bytes < memory.warn_bytes) {
    items.push({
      key: "memory",
      tone: "know",
      title: `Memory: ${gib(memory.available_bytes)} available`,
      detail: `under its warning (${gib(memory.warn_bytes)})`,
      since_ms: null,
      command: null,
      session: null,
    });
  }
  for (const [index, warning] of (attention?.warnings ?? []).entries()) {
    items.push({
      key: `warning:${index}`,
      tone: "know",
      title: "The dispatcher's view is incomplete",
      detail: warning,
      since_ms: null,
      command: null,
      session: null,
    });
  }
  const rank = (item: NeedItem) => (item.tone === "act" ? 0 : 1);
  return items
    .map((item, order) => ({ item, order }))
    .sort((left, right) => {
      const byTone = rank(left.item) - rank(right.item);
      if (byTone !== 0) return byTone;
      const a = left.item.since_ms;
      const b = right.item.since_ms;
      if (a !== null && b !== null && a !== b) return a - b;
      return left.order - right.order;
    })
    .map(({ item }) => item);
}

const ALERT_TITLES: Record<string, string> = {
  agent_stuck: "An agent looks stuck",
  judge_down: "The model judge answers no call",
  integrator_down: "The integrator is down",
  controller_behind: "A landing waits for its deploy",
  deploy_failed: "A deploy failed",
  mission_halted: "A mission is halted",
  node_lost: "A node does not answer",
  task_undelivered: "A task was never delivered",
  task_stalled: "A delivered task looks stalled",
  admission_wait: "Work waits for room",
  backup_stale: "A backup is late",
  source_away: "A mission's source is not a directory on this host",
  validator_defect: "A validator fails on main too",
  task_parked: "A task is parked: load-sensitive twice",
  mission_idle: "A mission has no work",
  message_not_taken: "A message was not taken",
  owner_request_waiting: "An owner's landing waits for its verdict",
  guardrail_out_of_range: "A guardrail is out of its range",
};

/** A message not taken whose evidence says Spawn has read no line of the
 * transcript since it was typed (`read_since_typed` 0, the Bridge's own
 * record): whether the agent took it is unknown, so the title does not say
 * it was not taken (S17). */
const UNREAD_TITLE = "Spawn has read nothing of a transcript since a message was typed";

export function alertTitle(
  alert: Pick<Alert, "kind" | "subject"> & Partial<Pick<Alert, "evidence">>,
  named: string | null = null,
): string {
  const evidence = (alert.evidence ?? null) as { read_since_typed?: unknown } | null;
  const unread = alert.kind === "message_not_taken" && evidence?.read_since_typed === 0;
  const title = unread ? UNREAD_TITLE : (ALERT_TITLES[alert.kind] ?? alert.kind.replaceAll("_", " "));
  return `${title} · ${named ?? alert.subject}`;
}

/** What "Needs you" holds, in words: the page header and the card say the
 * same, from this. */
export function needsSummary(items: readonly NeedItem[]): string {
  const act = items.filter((item) => item.tone === "act").length;
  const know = items.length - act;
  if (items.length === 0) return "Nothing needs you";
  return [act > 0 ? `${act} to act on` : null, know > 0 ? `${know} to know` : null].filter(Boolean).join(" · ");
}

// ---------------------------------------------------------------------------
// Missions, today

export interface InFlight {
  task_id: number | null;
  title: string | null;
  /** The state in words (`gating`, `sent back`, …). */
  state: string;
  /** Since the task's latest record. */
  since_ms: number | null;
  /** How many of the task's gate verdicts were `conflict` (objective 069). */
  conflicts: number;
  /** How many `conflict` gate verdicts the mission's other tasks for the
   * same objective, assigned before it, had (objective 073); 0 with no
   * objective. */
  earlierConflicts: number;
  /** "sent back <n> times as a conflict[, <e> on earlier attempts]" when
   * `conflicts` and `earlierConflicts` are two or more in all, else null. */
  conflictText: string | null;
}

export interface MissionRow {
  id: number | null;
  name: string;
  halted: string | null;
  /** The owner paused it (objective 099): its work in flight waits. */
  paused: boolean;
  inFlight: InFlight[];
  landedToday: number;
  /** The latest send-back verdict today, with its reason. */
  lastRejection: { task_id: number | null; verdict: string; reason: string | null; at_ms: number } | null;
}

export interface MissionsToday {
  /** Active missions with open tasks (or halted), most recently moved first. */
  rows: MissionRow[];
  /** Active missions with nothing open: one folded line. `idle_ms`: how
   * long a mission that takes its work from an objective backlog has had
   * no task in work, since its last task ended (objective 058); null for
   * owner work, a mission that never had a task, or one the owner paused
   * (`paused`, objective 099: held by the owner, not idle). */
  quiet: Array<{ id: number | null; name: string; idle_ms: number | null; paused?: boolean }>;
  closed: number;
}

const SEND_BACK = new Set(["rejected", "conflict", "incompatible"]);

function latestRecord(task: MissionTask): number | null {
  let latest = task.updated_at_ms ?? task.created_at_ms ?? null;
  for (const stage of task.stages) {
    if (stage.at_ms !== null && (latest === null || stage.at_ms > latest)) latest = stage.at_ms;
  }
  return latest;
}

/** When a task landed (its `landed` stage, else its last record while
 * landed), or null. */
function landedAt(task: MissionTask, mission: Mission): number | null {
  const landed = task.state === "landed" || (task.state === "accepted" && !mission.gate);
  if (!landed) return null;
  const stage = [...task.stages]
    .reverse()
    .find((entry) => entry.stage === "landed" || (entry.stage === "reviewed" && entry.verdict === "accepted"));
  return stage?.at_ms ?? latestRecord(task);
}

/** Each send-back of a task: its gate's or review's verdict, with when. */
function sendBacks(task: MissionTask): Array<{ verdict: string; at_ms: number }> {
  return task.stages
    .filter((stage) => (stage.stage === "gated" || stage.stage === "reviewed") && stage.at_ms !== null)
    .filter((stage) => SEND_BACK.has(stage.verdict ?? ""))
    .map((stage) => ({ verdict: stage.verdict ?? "rejected", at_ms: stage.at_ms ?? 0 }));
}

/** How many of a task's gate verdicts were `conflict`: a `rejected` or
 * `incompatible` one, or a review's verdict, does not count. */
export function conflictCount(task: MissionTask): number {
  return task.stages.filter((stage) => stage.stage === "gated" && stage.verdict === "conflict").length;
}

/** The in-flight line's words for a task that lost rounds to conflicts,
 * its own and its objective's earlier attempts' together (objective 073):
 * "sent back 3 times as a conflict, 2 on earlier attempts"; null under two
 * in all. */
export function conflictText(conflicts: number, earlier = 0): string | null {
  const total = conflicts + earlier;
  if (total < 2) return null;
  return `sent back ${total} times as a conflict${earlier > 0 ? `, ${earlier} on earlier attempts` : ""}`;
}

/** Whether `other` was assigned before `task`: by when each was made, the
 * id between two made at once (or with no time). */
function assignedBefore(other: MissionTask, task: MissionTask): boolean {
  const at = (entry: MissionTask) => entry.created_at_ms ?? Number.NEGATIVE_INFINITY;
  if (at(other) !== at(task)) return at(other) < at(task);
  return (other.id ?? 0) < (task.id ?? 0);
}

/** The `conflict` gate verdicts of the earlier attempts at `task`'s
 * objective: the mission's other tasks for the same objective assigned
 * before it (objective 073). A task with no objective has none; another
 * mission's objective of the same number is not the same objective (only
 * `mission`'s tasks are read). */
export function earlierConflictCount(task: MissionTask, mission: Mission): number {
  if (task.objective === null) return 0;
  return mission.tasks
    .filter((other) => other !== task && other.objective === task.objective && assignedBefore(other, task))
    .reduce((sum, other) => sum + conflictCount(other), 0);
}

export function missionsToday(missions: Missions | null, now: number): MissionsToday {
  const rows: Array<MissionRow & { moved: number }> = [];
  const quiet: MissionsToday["quiet"] = [];
  let closed = 0;
  const since = now - TODAY_MS;
  for (const mission of missions?.missions ?? []) {
    const name = missionName(mission);
    const kind = missionClass(mission);
    if (kind === "closed") {
      closed += 1;
      continue;
    }
    if (kind === "idle" && !mission.halted) {
      let ended: number | null = null;
      for (const task of mission.tasks) {
        const at = latestRecord(task);
        if (at !== null && (ended === null || at > ended)) ended = at;
      }
      if (mission.paused) {
        quiet.push({ id: mission.id, name, idle_ms: null, paused: true });
        continue;
      }
      const idle_ms = mission.objectives && ended !== null ? Math.max(0, now - ended) : null;
      quiet.push({ id: mission.id, name, idle_ms });
      continue;
    }
    const inFlight: InFlight[] = mission.tasks
      .filter((task) => isOpenTask(task, mission))
      .map((task) => {
        const conflicts = conflictCount(task);
        const earlierConflicts = earlierConflictCount(task, mission);
        return {
          task_id: task.id,
          title: task.task_title,
          state: taskStateLabel(task, mission),
          since_ms: latestRecord(task),
          conflicts,
          earlierConflicts,
          conflictText: conflictText(conflicts, earlierConflicts),
        };
      })
      .sort((left, right) => (left.task_id ?? 0) - (right.task_id ?? 0));
    let landedToday = 0;
    let lastRejection: MissionRow["lastRejection"] = null;
    let moved = mission.updated_at_ms ?? 0;
    for (const task of mission.tasks) {
      const landed = landedAt(task, mission);
      if (landed !== null && landed >= since) landedToday += 1;
      moved = Math.max(moved, latestRecord(task) ?? 0);
      for (const back of sendBacks(task)) {
        if (back.at_ms < since || (lastRejection && lastRejection.at_ms >= back.at_ms)) continue;
        const gate = task.last_gate;
        lastRejection = {
          task_id: task.id,
          verdict: back.verdict,
          // The reason is the last gate's when that is this verdict.
          reason: gate && gate.verdict === back.verdict ? gate.reason : null,
          at_ms: back.at_ms,
        };
      }
    }
    rows.push({
      id: mission.id,
      name,
      halted: mission.halted,
      paused: Boolean(mission.paused),
      inFlight,
      landedToday,
      lastRejection,
      moved,
    });
  }
  rows.sort((left, right) => right.moved - left.moved);
  return { rows: rows.map(({ moved: _moved, ...row }) => row), quiet, closed };
}

/** The card's words when no mission has work open: each mission that
 * takes its work from a backlog, with how long it has had none ("No mission
 * has work open: #4 Spawn RSI none for 3 h, #6 Supply PA none for 20 h.").
 * A paused mission is the owner's choice, not a want of work: left out. */
export function noWorkText(model: MissionsToday): string {
  const idle = model.quiet.filter((mission) => mission.idle_ms !== null && !mission.paused);
  if (idle.length === 0) return "No mission has work open.";
  return `No mission has work open: ${idle
    .map((mission) => `#${mission.id} ${mission.name} none for ${ageText(mission.idle_ms ?? 0)}`)
    .join(", ")}.`;
}

/** The folded line's words for the missions with nothing open, a backlog
 * mission with how long, a paused one as paused ("#4 Spawn RSI (no work
 * for 3 h), #6 Supply PA (paused), #8 Owner work"). */
export function quietText(quiet: MissionsToday["quiet"]): string {
  return quiet
    .map((mission) =>
      mission.paused
        ? `#${mission.id} ${mission.name} (paused)`
        : mission.idle_ms === null
          ? `#${mission.id} ${mission.name}`
          : `#${mission.id} ${mission.name} (no work for ${ageText(mission.idle_ms)})`,
    )
    .join(", ");
}

// ---------------------------------------------------------------------------
// Agents working now

export interface WorkingAgent {
  session_id: string;
  /** One name (design §4.5): `supervisor`, `resident · Supply PA`,
   * `worker · Supply PA T222`. */
  name: string;
  /** What the task is, in words. */
  task: string | null;
  /** What it is doing, in words. */
  doing: string;
  since_ms: number | null;
}

export interface AgentsNow {
  working: WorkingAgent[];
  /** Live agents with nothing to do, by name: one folded line. */
  idle: Array<{ session_id: string; name: string; since_ms: number | null }>;
}

/** The one name of an agent: its role and what it works for. */
export function agentName(agent: FleetAgent, context: FleetContext, task: TaskRef | null = heldTask(agent, context)): string {
  if (agent.role === "supervisor") return "supervisor";
  const resident = residentName(agent, context);
  // A resident on a task is named by its mission, as a worker is.
  if (resident) return task ? `resident · ${task.mission_name}${task.task_id !== null ? ` T${task.task_id}` : ""}` : `resident · ${resident}`;
  if (task) return `worker · ${task.mission_name}${task.task_id !== null ? ` T${task.task_id}` : ""}`;
  return agent.agent_id || agent.session_id;
}

/** The task an agent's name and second line go by: the one it holds, and
 * for a worker that holds none (it ended, or its task closed), its latest.
 * A resident without an open task goes by its own name instead. */
export function nameTask(agent: FleetAgent, context: FleetContext): TaskRef | null {
  const held = heldTask(agent, context);
  if (held || agent.role === "supervisor" || residentName(agent, context)) return held;
  return context.lastTasks.get(agent.session_id) ?? (agent.agent_id ? context.lastTasks.get(agent.agent_id) : undefined) ?? null;
}

/**
 * Every agent's one name (design §4.5), unique across the board: the
 * sidebar, home, the agent page, the trace and alerts all read this map, by
 * session id. Two agents can share a name (two workers on one task after a
 * reassign, or an ended worker and its successor): each then carries a
 * short suffix from its session id, the id's last word where those differ
 * ("worker · Supply PA T222 · 239"), else its last six characters.
 */
export function agentNames(agents: readonly FleetAgent[], context: FleetContext): Map<string, string> {
  const base = new Map<string, string>();
  const bySame = new Map<string, FleetAgent[]>();
  for (const agent of agents) {
    const name = agentName(agent, context, nameTask(agent, context));
    base.set(agent.session_id, name);
    const same = bySame.get(name);
    if (same) same.push(agent);
    else bySame.set(name, [agent]);
  }
  const names = new Map(base);
  for (const [name, same] of bySame) {
    if (same.length < 2) continue;
    const word = (agent: FleetAgent) => agent.session_id.split(/[_\-.]/).filter(Boolean).pop() ?? agent.session_id;
    const tail = (agent: FleetAgent) => agent.session_id.slice(-6);
    const words = same.map(word);
    const tails = same.map(tail);
    const suffixes = new Set(words).size === same.length ? words : new Set(tails).size === same.length ? tails : same.map((agent) => agent.session_id);
    same.forEach((agent, index) => names.set(agent.session_id, `${name} · ${suffixes[index]}`));
  }
  return names;
}

/** Whether an agent's row matches the sidebar's search: every word of the
 * query (case aside) is in its name (which carries its task's T-number),
 * its ids, its task's title, or its mission. An empty query matches every
 * agent. */
export function agentMatches(query: string, fields: ReadonlyArray<string | null | undefined>): boolean {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const text = fields.filter(Boolean).join(" \n ").toLowerCase();
  return words.every((word) => text.includes(word));
}

/** What a row is searched by: `agentMatches`'s fields for an agent. */
export function agentSearchFields(agent: FleetAgent, context: FleetContext, name: string): string[] {
  const task = nameTask(agent, context);
  return [
    name,
    agent.agent_id,
    agent.session_id,
    task?.title ?? agent.task_title,
    task?.mission_name ?? agent.mission_name,
  ].filter((field): field is string => typeof field === "string" && field !== "");
}

const STATE_WORDS: Record<string, string> = {
  WORKING: "working",
  WAITING: "waiting at its prompt",
  BLOCKED: "blocked",
  COMPLETED: "done, at its prompt",
  UNKNOWN: "state unknown",
};

/** The open tool call in one line: `Bash · cargo test -p spawn-cli`, or the
 * tool alone when its call carries no summary; null when no tool is open. */
export function currentToolText(activity: Activity): string | null {
  if (!activity.current_tool) return null;
  const summary = activity.current_tool_summary;
  return summary ? `${activity.current_tool} · ${summary}` : activity.current_tool;
}

/** What an agent is doing, in words: its open tool, its background job,
 * else its task's step, else its judged state. */
export function doingWords(agent: FleetAgent, task: TaskRef | null): string {
  const activity = agent.activity;
  const tool = currentToolText(activity);
  if (tool) return `running ${tool}`;
  const job = activity.jobs[0];
  if (job) return `waiting on its background ${job.kind} job`;
  if (task && task.state !== "assigned" && task.state !== "delivered" && agent.state !== "WORKING") return task.label;
  return STATE_WORDS[agent.state] ?? agent.state.toLowerCase();
}

export function agentsNow(agents: readonly FleetAgent[] | null, context: FleetContext): AgentsNow {
  const working: WorkingAgent[] = [];
  const idle: AgentsNow["idle"] = [];
  // The board's one names (unique across it), as the sidebar shows them.
  const names = agentNames(agents ?? [], context);
  for (const agent of agents ?? []) {
    if (!agent.live) continue;
    const task = heldTask(agent, context);
    const busy = task !== null || agent.state === "WORKING" || agent.activity.current_tool !== null || agent.activity.jobs.length > 0;
    const name = names.get(agent.session_id) ?? agentName(agent, context, task);
    if (!busy) {
      idle.push({ session_id: agent.session_id, name, since_ms: agent.state_since_ms });
      continue;
    }
    working.push({
      session_id: agent.session_id,
      name,
      task: task?.title ?? agent.task_title,
      doing: doingWords(agent, task),
      since_ms: agent.state_since_ms,
    });
  }
  const rank = (row: { name: string }) => (row.name === "supervisor" ? 0 : row.name.startsWith("resident") ? 1 : 2);
  working.sort((left, right) => rank(left) - rank(right) || left.name.localeCompare(right.name));
  idle.sort((left, right) => rank(left) - rank(right) || left.name.localeCompare(right.name));
  return { working, idle };
}

// ---------------------------------------------------------------------------
// The supervisor's lines for the owner

export interface NotesShown {
  /** The shown lines, each with what became of the tasks it names ("T3
   * accepted"; "" when it names none), worded as `allNotes` words them. */
  shown: Array<{ text: string; at_ms: number | null; taskStates: string }>;
  /** Every other current note that is not settled (older than
   * NOTE_FRESH_MS, or past NOTES_SHOWN). */
  more: number;
  /** The notes of the supervisor's earlier turns that are not settled
   * (objective 078): its newest block restates what it still asks, so
   * these are neither shown nor in `more`. The full list has them. */
  earlier: number;
  /** The settled notes, left out of `shown` and `more`: a review ask whose
   * tasks have all ended (objective 064). The full list still has them. */
  settled: number;
}

/** A line for the owner as Home and the full list read it: the API's item,
 * whose `tasks` and `settled` an older server does not send. */
type NoteLike = { text: string; at_ms: number | null; tasks?: unknown; settled?: unknown; current?: unknown };

/** Whether a line is of the supervisor's newest block: an older server
 * that does not say counts every line as current. */
function isCurrent(note: NoteLike): boolean {
  return note.current !== false;
}

/** What became of the tasks a line names, "T2 accepted, T4 cancelled"; ""
 * when it names none (or an older server sends no `tasks`). */
function taskStates(note: NoteLike): string {
  const tasks = Array.isArray(note.tasks) ? note.tasks : [];
  return tasks
    .flatMap((task: unknown) => {
      const entry = typeof task === "object" && task !== null ? (task as { id?: unknown; state?: unknown }) : null;
      return entry && typeof entry.id === "number"
        ? [`T${entry.id} ${typeof entry.state === "string" ? entry.state : "unknown"}`]
        : [];
    })
    .join(", ");
}

function newestFirst<T extends NoteLike>(notes: readonly T[]): T[] {
  return [...notes].sort((left, right) => (right.at_ms ?? 0) - (left.at_ms ?? 0));
}

/** Home's "From the supervisor" card: the newest fresh lines of the
 * supervisor's newest block that are not settled, each with its tasks'
 * states, and how many others there are. A settled line (its review ask's
 * tasks have all ended) is neither shown nor counted in `more`, nor is a
 * line of an earlier turn (counted in `earlier`, objective 078). */
export function latestNotes(attention: Attention | null, now: number): NotesShown {
  const all = newestFirst<NoteLike>(attention?.supervisor?.for_owner ?? []);
  const notes = all.filter((note) => note.settled !== true);
  const current = notes.filter(isCurrent);
  const shown = current
    .filter((note) => note.at_ms !== null && now - note.at_ms < NOTE_FRESH_MS)
    .slice(0, NOTES_SHOWN)
    .map((note) => ({ text: note.text, at_ms: note.at_ms, taskStates: taskStates(note) }));
  return {
    shown,
    more: current.length - shown.length,
    earlier: notes.length - current.length,
    settled: all.length - notes.length,
  };
}

/** One line of the supervisor's full list. */
export interface NoteRow {
  text: string;
  at_ms: number | null;
  settled: boolean;
  /** What became of the tasks it names, "T2 accepted, T4 cancelled"; ""
   * when it names none. */
  tasks: string;
  /** Of the supervisor's newest block; false for an earlier turn's line
   * (objective 078). */
  current: boolean;
}

/** The supervisor's full list of lines for the owner, newest first: every
 * line, a settled one and an earlier turn's too, marked so, each with its
 * named tasks' states. */
export function allNotes(attention: Attention | null): NoteRow[] {
  return newestFirst<NoteLike>(attention?.supervisor?.for_owner ?? []).map((note) => ({
    text: note.text,
    at_ms: note.at_ms,
    settled: note.settled === true,
    tasks: taskStates(note),
    current: isCurrent(note),
  }));
}

// ---------------------------------------------------------------------------
// The digest: what happened while you were away (design §3 Q4, §2.9)

/** The shortest digest window, and the longest. */
export const DIGEST_FLOOR_MS = TODAY_MS;
export const DIGEST_CAP_MS = 7 * TODAY_MS;
/** How long the page must have been hidden or closed to count as away. */
export const AWAY_GAP_MS = 30 * 60_000;

/** When the reader went away, from the last time the page was seen (stored
 * in the browser): that time, if it is at least `AWAY_GAP_MS` ago, else
 * null (they never left, or this is their first visit). */
export function awayFrom(lastSeen: number | null, now: number): number | null {
  if (lastSeen === null || !Number.isFinite(lastSeen) || lastSeen > now) return null;
  return now - lastSeen >= AWAY_GAP_MS ? lastSeen : null;
}

/** The digest's window: since the reader went away, never less than the
 * last 24 h (a quick reload never empties it) and never more than 7 days.
 * `away` says the absence was longer than the floor, so the card says
 * "Since you were away" and not "Today". */
export function digestWindow(now: number, away: number | null): { since: number; away: boolean } {
  const floor = now - DIGEST_FLOOR_MS;
  if (away === null || away >= floor) return { since: floor, away: false };
  return { since: Math.max(away, now - DIGEST_CAP_MS), away: true };
}

/** How far back the digest asks for alerts (`/api/alerts?since=`): its
 * window, in whole hours rounded up (so the poll's key changes at most once
 * an hour), and never under the server's own 24 h. */
export function alertsSpanMs(now: number, since: number): number {
  const hours = Math.ceil(Math.max(0, now - since) / 3_600_000);
  return Math.max(hours * 3_600_000, DIGEST_FLOOR_MS);
}

export type DigestKind = "landed" | "sent back" | "stopped" | "moved" | "mission" | "alert";

export interface DigestEntry {
  at_ms: number;
  kind: DigestKind;
  text: string;
}

export interface DigestBand {
  /** "Last hour", "Earlier today", "Yesterday", "Mon". */
  label: string;
  /** Newest first. */
  entries: DigestEntry[];
}

export interface Digest {
  since_ms: number;
  /** The window starts where the reader went away (more than 24 h ago). */
  away: boolean;
  /** Tasks landed (or accepted) in the window. */
  landed: number;
  /** Send-backs in the window (a task sent back twice counts twice). */
  sentBack: number;
  alerts: number;
  /** Newest first, every line. */
  entries: DigestEntry[];
  /** The same lines by when (newest band first). */
  bands: DigestBand[];
}

/** One task's day in a line: what it came to in the window, said once. */
function taskStory(task: MissionTask, mission: Mission, since: number): { at: number; kind: DigestKind; story: string; landed: boolean; backs: number } | null {
  const inWindow = task.stages.filter((stage) => stage.at_ms !== null && stage.at_ms >= since);
  if (inWindow.length === 0) return null;
  const at = Math.max(...inWindow.map((stage) => stage.at_ms ?? 0));
  const runs = (stage: string) => task.stages.filter((entry) => entry.stage === stage).length;
  const times = (count: number, word: string) => `${count} ${word}${count === 1 ? "" : "s"}`;
  const backs = sendBacks(task).filter((back) => back.at_ms >= since);
  const landed = landedAt(task, mission);
  if (landed !== null && landed >= since) {
    const gates = runs("gated");
    const reviews = runs("reviewed");
    const after = gates > 0 ? ` after ${times(gates, "gate")}` : reviews > 0 ? ` after ${times(reviews, "review")}` : "";
    const word = task.state === "accepted" ? "accepted" : "landed";
    return { at, kind: "landed", story: `${word}${after}${backs.length ? `, sent back ${backs.length}× on the way` : ""}`, landed: true, backs: backs.length };
  }
  const label = taskStateLabel(task, mission);
  if (task.state === "cancelled" && inWindow.some((stage) => stage.stage === "cancelled" || stage.stage === "aborted")) {
    return { at, kind: "stopped", story: label, landed: false, backs: backs.length };
  }
  if (backs.length > 0) {
    const verdicts = [...new Set(backs.map((back) => back.verdict))].join(", ");
    const now = isOpenTask(task, mission) ? `; now ${label}` : `; ${label}`;
    return { at, kind: "sent back", story: `sent back${backs.length > 1 ? ` ${backs.length}×` : ""} (${verdicts})${now}`, landed: false, backs: backs.length };
  }
  const started = inWindow.some((stage) => stage.stage === "assigned");
  return { at, kind: "moved", story: started ? `started; now ${label}` : `now ${label}`, landed: false, backs: 0 };
}

/** A mission's own change, from its notice: only a change of state or of
 * its halt (a counter moving is not news). */
function missionChange(alert: Alert, names: ReadonlyMap<number, string>): string | null {
  const evidence = alert.evidence as {
    mission?: unknown;
    before?: { state?: unknown; halted?: unknown } | null;
    after?: { state?: unknown; halted?: unknown } | null;
  } | null;
  const before = evidence?.before ?? null;
  const after = evidence?.after ?? null;
  if (!after) return null;
  const id = typeof evidence?.mission === "number" ? evidence.mission : null;
  const name = (id !== null ? names.get(id) : undefined) ?? alert.subject;
  const halted = after.halted ?? null;
  if ((before?.halted ?? null) !== halted) {
    return halted ? `${name}: halted${typeof halted === "string" ? ` (${halted})` : ""}` : `${name}: resumed`;
  }
  if (before?.state !== after.state && typeof after.state === "string") return `${name}: ${after.state}`;
  return null;
}

/** The band a time falls in, seen from `now` (local days). */
export function digestBand(at: number, now: number): string {
  if (now - at < 3_600_000) return "Last hour";
  const midnight = new Date(now);
  midnight.setHours(0, 0, 0, 0);
  const today = midnight.getTime();
  if (at >= today) return "Earlier today";
  const yesterday = new Date(today);
  yesterday.setDate(yesterday.getDate() - 1);
  if (at >= yesterday.getTime()) return "Yesterday";
  return new Date(at).toLocaleDateString("en", { weekday: "short", day: "numeric", month: "short" });
}

/**
 * What happened since `since`, for a reader who was away: one line per task
 * (its day: landed after 2 gates, sent back and now gating, cancelled), one
 * per mission halted, resumed or ended, and one per alert raised, grouped
 * by when. A moot alert (an ended session's) is not news. Task notices are
 * not read: the missions say the same, by task.
 */
export function digestSince(
  missions: Missions | null,
  alerts: readonly Alert[] | null,
  now: number,
  since: number,
  agents: readonly FleetAgent[] | null = null,
  away = false,
): Digest {
  const entries: DigestEntry[] = [];
  let landed = 0;
  let sentBack = 0;
  const names = new Map<number, string>();
  for (const mission of missions?.missions ?? []) {
    const name = missionName(mission);
    if (mission.id !== null) names.set(mission.id, name);
    if (!isActiveMission(mission) && (mission.updated_at_ms ?? 0) < since) continue;
    for (const task of mission.tasks) {
      const story = taskStory(task, mission, since);
      if (!story) continue;
      if (story.landed) landed += 1;
      sentBack += story.backs;
      const title = task.task_title ?? `task #${task.id}`;
      entries.push({ at_ms: story.at, kind: story.kind, text: `${name} · T${task.id} ${title}: ${story.story}` });
    }
  }
  let alertCount = 0;
  for (const alert of alerts ?? []) {
    if (alert.fired_at_ms < since) continue;
    if (alert.state === "notice") {
      if (alert.kind !== "mission_state") continue;
      const change = missionChange(alert, names);
      if (change) entries.push({ at_ms: alert.fired_at_ms, kind: "mission", text: change });
      continue;
    }
    if (alertMoot(alert, agents)) continue;
    alertCount += 1;
    entries.push({ at_ms: alert.fired_at_ms, kind: "alert", text: `${alertTitle(alert)}${alert.state === "cleared" ? " (cleared)" : ""}` });
  }
  entries.sort((left, right) => right.at_ms - left.at_ms);
  const bands: DigestBand[] = [];
  for (const entry of entries) {
    const label = digestBand(entry.at_ms, now);
    const last = bands[bands.length - 1];
    if (last && last.label === label) last.entries.push(entry);
    else bands.push({ label, entries: [entry] });
  }
  return { since_ms: since, away, landed, sentBack, alerts: alertCount, entries, bands };
}

/** The last 24 h (the digest's floor), as home showed it before P-digest. */
export function digestToday(
  missions: Missions | null,
  alerts: readonly Alert[] | null,
  now: number,
  agents: readonly FleetAgent[] | null = null,
): Digest {
  return digestSince(missions, alerts, now, now - TODAY_MS, agents);
}

// ---------------------------------------------------------------------------
// Focused Home models (Actionable needs, Mission Completion, System Status)

/**
 * Filter `needsYou` items down to genuine human action/blocking items for Home,
 * omitting noisy system `alert:*` chatter so the front page stays calm.
 */
export function actionableHomeNeeds(items: readonly NeedItem[]): NeedItem[] {
  return items.filter((item) => {
    if (item.key.startsWith("alert:")) return false;
    if (item.key.startsWith("halt:")) return true;
    if (item.key.startsWith("attention:")) return item.tone === "act";
    if (item.key.startsWith("disk:") || item.key === "memory") return true;
    return item.tone === "act";
  });
}

export type MissionProgressStatus = "active" | "halted" | "paused" | "idle";

export interface MissionProgressRow {
  id: number | null;
  name: string;
  status: MissionProgressStatus;
  completedTasks: number;
  totalTasks: number;
  inFlightTasks: number;
  blockedTasks: number;
  /** Whole percentage `0..100`, or null when the mission has zero non-cancelled tasks. */
  completionPct: number | null;
  landedToday: number;
  inFlight: InFlight[];
  lastRejection: MissionRow["lastRejection"];
  haltedReason: string | null;
}

export interface MissionProgressSummary {
  rows: MissionProgressRow[];
  openMissions: number;
  closedMissions: number;
  totalTasks: number;
  completedTasks: number;
  inFlightTasks: number;
  blockedTasks: number;
  landedToday: number;
  overallPct: number | null;
}

function isTaskCompleted(task: MissionTask, mission: Mission): boolean {
  return task.state === "landed" || (!mission.gate && task.state === "accepted");
}

function isTaskBlocked(task: MissionTask): boolean {
  return task.state === "revise" || Boolean(task.parked) || task.held_by !== null;
}

const PROGRESS_STATUS_RANK: Record<MissionProgressStatus, number> = {
  halted: 0,
  active: 1,
  paused: 2,
  idle: 3,
};

/**
 * Summarize completion progress across all open missions and their non-cancelled tasks.
 */
export function missionProgressSummary(
  missions: Missions | null,
  now: number,
): MissionProgressSummary {
  const rows: Array<MissionProgressRow & { moved: number }> = [];
  let closedMissions = 0;
  let totalTasks = 0;
  let completedTasks = 0;
  let inFlightTasks = 0;
  let blockedTasks = 0;
  let landedToday = 0;
  const since = now - TODAY_MS;

  for (const mission of missions?.missions ?? []) {
    const kind = missionClass(mission);
    if (kind === "closed") {
      closedMissions += 1;
      continue;
    }

    const countable = mission.tasks.filter((task) => task.state !== "cancelled");
    const completed = countable.filter((task) => isTaskCompleted(task, mission)).length;
    const blocked = countable.filter(isTaskBlocked).length;
    const inFlight: InFlight[] = mission.tasks
      .filter((task) => isOpenTask(task, mission))
      .map((task) => {
        const conflicts = conflictCount(task);
        const earlierConflicts = earlierConflictCount(task, mission);
        return {
          task_id: task.id,
          title: task.task_title,
          state: taskStateLabel(task, mission),
          since_ms: latestRecord(task),
          conflicts,
          earlierConflicts,
          conflictText: conflictText(conflicts, earlierConflicts),
        };
      })
      .sort((left, right) => (left.task_id ?? 0) - (right.task_id ?? 0));

    let missionLandedToday = 0;
    let lastRejection: MissionRow["lastRejection"] = null;
    let moved = mission.updated_at_ms ?? 0;
    for (const task of mission.tasks) {
      const landed = landedAt(task, mission);
      if (landed !== null && landed >= since) missionLandedToday += 1;
      moved = Math.max(moved, latestRecord(task) ?? 0);
      for (const back of sendBacks(task)) {
        if (back.at_ms < since || (lastRejection && lastRejection.at_ms >= back.at_ms)) continue;
        const gate = task.last_gate;
        lastRejection = {
          task_id: task.id,
          verdict: back.verdict,
          reason: gate && gate.verdict === back.verdict ? gate.reason : null,
          at_ms: back.at_ms,
        };
      }
    }

    const status: MissionProgressStatus = mission.halted
      ? "halted"
      : mission.paused
        ? "paused"
        : inFlight.length > 0
          ? "active"
          : "idle";

    const missionTotal = countable.length;
    const completionPct = missionTotal > 0 ? Math.round((completed / missionTotal) * 100) : null;

    totalTasks += missionTotal;
    completedTasks += completed;
    inFlightTasks += inFlight.length;
    blockedTasks += blocked;
    landedToday += missionLandedToday;

    rows.push({
      id: mission.id,
      name: missionName(mission),
      status,
      completedTasks: completed,
      totalTasks: missionTotal,
      inFlightTasks: inFlight.length,
      blockedTasks: blocked,
      completionPct,
      landedToday: missionLandedToday,
      inFlight,
      lastRejection,
      haltedReason: mission.halted ?? null,
      moved,
    });
  }

  rows.sort((left, right) => {
    const byStatus = PROGRESS_STATUS_RANK[left.status] - PROGRESS_STATUS_RANK[right.status];
    if (byStatus !== 0) return byStatus;
    return right.moved - left.moved;
  });

  const overallPct = totalTasks > 0 ? Math.round((completedTasks / totalTasks) * 100) : null;

  return {
    rows: rows.map(({ moved: _moved, ...row }) => row),
    openMissions: rows.length,
    closedMissions,
    totalTasks,
    completedTasks,
    inFlightTasks,
    blockedTasks,
    landedToday,
    overallPct,
  };
}

export type SystemHealthTone = "healthy" | "attention" | "degraded";

export interface SystemLivenessInput {
  total: number;
  up: number;
}

export interface NodeStatusInput {
  state: string;
}

export interface SystemStatusInput {
  health: FleetHealth | null;
  liveness: SystemLivenessInput | null;
  agents: readonly FleetAgent[] | null;
  nodes: readonly NodeStatusInput[];
  missionProgress: MissionProgressSummary;
  actionCount: number;
  freshnessLevel: "fresh" | "aging" | "frozen";
}

export interface SystemStatusModel {
  tone: SystemHealthTone;
  nodesOnline: number;
  nodesTotal: number;
  alwaysOnUp: number;
  alwaysOnTotal: number;
  restarts24h: number;
  agentsLive: number;
  agentsTotal: number;
  workingCount: number;
  waitingCount: number;
  blockedCount: number;
  memoryUsedPct: number | null;
  memoryAvailBytes: number | null;
  tightestDiskLabel: string | null;
  tightestDiskFreeBytes: number | null;
  diskLowCount: number;
  haltedMissions: number;
  actionCount: number;
}

/**
 * Aggregate live host/agent/mission health into a single Home hero model.
 */
export function buildSystemStatusModel(input: SystemStatusInput): SystemStatusModel {
  const { health, liveness, agents, nodes, missionProgress, actionCount, freshnessLevel } = input;
  const nodesTotal = nodes.length;
  const nodesOnline = nodes.filter((node) => node.state === "live").length;
  const alwaysOnTotal = liveness?.total ?? 0;
  const alwaysOnUp = liveness?.up ?? 0;
  const restarts24h = health?.ladder_24h.liveness ?? 0;
  const agentsLive = health?.live ?? (agents?.filter((agent) => agent.live).length ?? 0);
  const agentsTotal = health?.agents_total ?? (agents?.length ?? 0);
  const workingCount =
    health?.by_state.WORKING ?? (agents?.filter((agent) => agent.live && agent.state === "WORKING").length ?? 0);
  const waitingCount =
    health?.by_state.WAITING ?? (agents?.filter((agent) => agent.live && agent.state === "WAITING").length ?? 0);
  const blockedCount =
    health?.by_state.BLOCKED ?? (agents?.filter((agent) => agent.live && agent.state === "BLOCKED").length ?? 0);

  let memoryUsedPct: number | null = null;
  const memoryAvailBytes = health?.memory?.available_bytes ?? null;
  const memoryTotalBytes = health?.memory?.total_bytes ?? null;
  if (
    memoryAvailBytes !== null &&
    memoryTotalBytes !== null &&
    memoryTotalBytes > 0
  ) {
    memoryUsedPct = Math.max(
      0,
      Math.min(100, Math.round(((memoryTotalBytes - memoryAvailBytes) / memoryTotalBytes) * 100)),
    );
  }
  const memoryLow =
    health?.memory !== null &&
    health?.memory !== undefined &&
    health.memory.available_bytes !== null &&
    health.memory.available_bytes < health.memory.warn_bytes;

  let tightestDiskLabel: string | null = null;
  let tightestDiskFreeBytes: number | null = null;
  let diskLowCount = 0;
  for (const place of health?.disk ?? []) {
    if (place.free_bytes === null) continue;
    if (place.free_bytes < place.warn_bytes) diskLowCount += 1;
    if (tightestDiskFreeBytes === null || place.free_bytes < tightestDiskFreeBytes) {
      tightestDiskFreeBytes = place.free_bytes;
      tightestDiskLabel = place.label;
    }
  }

  const haltedMissions = missionProgress.rows.filter((row) => row.status === "halted").length;
  const nodesLost = nodes.some((node) => node.state === "lost");
  const alwaysOnDown = alwaysOnTotal > 0 && alwaysOnUp < alwaysOnTotal;

  const tone: SystemHealthTone =
    freshnessLevel === "frozen" || haltedMissions > 0 || nodesLost
      ? "degraded"
      : freshnessLevel === "aging" ||
          alwaysOnDown ||
          blockedCount > 0 ||
          diskLowCount > 0 ||
          memoryLow ||
          actionCount > 0
        ? "attention"
        : "healthy";

  return {
    tone,
    nodesOnline,
    nodesTotal,
    alwaysOnUp,
    alwaysOnTotal,
    restarts24h,
    agentsLive,
    agentsTotal,
    workingCount,
    waitingCount,
    blockedCount,
    memoryUsedPct,
    memoryAvailBytes,
    tightestDiskLabel,
    tightestDiskFreeBytes,
    diskLowCount,
    haltedMissions,
    actionCount,
  };
}
