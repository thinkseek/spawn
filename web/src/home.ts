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
  /** The command that acts on it, to copy (no write path from the page). */
  command: string | null;
  /** The session whose page answers it (a question, an escalated agent). */
  session: string | null;
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
    items.push({
      key: `halted:${mission.id}`,
      tone: "act",
      title: `Mission #${mission.id} ${missionName(mission)} is halted`,
      detail: mission.halted,
      since_ms: mission.updated_at_ms,
      command: `spawn mission resume --id ${mission.id}`,
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
  for (const alert of liveAlerts(input.alerts, input.agents)) {
    // A halted mission is said once, with its command.
    if (alert.kind === "mission_halted" && [...halted].some((id) => alert.subject === `mission #${id}`)) continue;
    const named = input.nameOf?.(alert.subject) ?? null;
    const stale = alertStale(alert, input.agents);
    const score = (alert.evidence as { stuck?: unknown } | null)?.stuck;
    items.push({
      key: `alert:${alert.key}`,
      tone: stale ? "know" : "act",
      title: alertTitle(alert, named),
      detail: stale
        ? `scored stuck${typeof score === "number" ? ` ${score.toFixed(2)}` : ""} ${ageText(input.now - alert.fired_at_ms)} ago; not scored since (its latest judgement, ${ageText(input.now - (stale.judged_at_ms ?? input.now))} ago, has no score${stale.judgement?.reason ? `: ${stale.judgement.reason}` : ""})`
        : alert.text,
      since_ms: alert.fired_at_ms,
      command: null,
      // A session's alert opens that agent's page.
      session: named !== null ? alert.subject : null,
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
  mission_halted: "A mission is halted",
  task_undelivered: "A task was never delivered",
  admission_wait: "Work waits for room",
  backup_stale: "A backup is late",
  source_away: "A mission's source is missing on this host",
};

export function alertTitle(alert: Pick<Alert, "kind" | "subject">, named: string | null = null): string {
  const title = ALERT_TITLES[alert.kind] ?? alert.kind.replaceAll("_", " ");
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
}

export interface MissionRow {
  id: number | null;
  name: string;
  halted: string | null;
  inFlight: InFlight[];
  landedToday: number;
  /** The latest send-back verdict today, with its reason. */
  lastRejection: { task_id: number | null; verdict: string; reason: string | null; at_ms: number } | null;
}

export interface MissionsToday {
  /** Active missions with open tasks (or halted), most recently moved first. */
  rows: MissionRow[];
  /** Active missions with nothing open: one folded line. */
  quiet: Array<{ id: number | null; name: string }>;
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
      quiet.push({ id: mission.id, name });
      continue;
    }
    const inFlight: InFlight[] = mission.tasks
      .filter((task) => isOpenTask(task, mission))
      .map((task) => ({
        task_id: task.id,
        title: task.task_title,
        state: taskStateLabel(task, mission),
        since_ms: latestRecord(task),
      }))
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
    rows.push({ id: mission.id, name, halted: mission.halted, inFlight, landedToday, lastRejection, moved });
  }
  rows.sort((left, right) => right.moved - left.moved);
  return { rows: rows.map(({ moved: _moved, ...row }) => row), quiet, closed };
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
  if (task && task.label !== "assigned" && task.label !== "delivered" && agent.state !== "WORKING") return task.label;
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
  shown: Array<{ text: string; at_ms: number | null }>;
  /** Every other note (older than NOTE_FRESH_MS, or past NOTES_SHOWN). */
  more: number;
}

export function latestNotes(attention: Attention | null, now: number): NotesShown {
  const notes = [...(attention?.supervisor?.for_owner ?? [])].sort((left, right) => (right.at_ms ?? 0) - (left.at_ms ?? 0));
  const shown = notes.filter((note) => note.at_ms !== null && now - note.at_ms < NOTE_FRESH_MS).slice(0, NOTES_SHOWN);
  return { shown, more: notes.length - shown.length };
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
