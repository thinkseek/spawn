// Read model of the Feature map page: what a mission's capabilities are and
// how far each has got (`/api/features`, spawn serve's feature_map module).
// Every field is normalized, so a server that omits one renders an empty
// value instead of crashing the page.
//
// The page never derives a state of its own: the server computes it from
// Spawn's records, and the four booleans stay separate here. A live
// capability that nobody verified is shown live with "never verified"
// beside it, not quietly as verified.

export const STATES = ["planned", "implemented", "verified", "live"] as const;
export type FeatureState = (typeof STATES)[number];

export interface StateInfo {
  label: string;
  /** What the state proves, in one line. */
  blurb: string;
  badge: string;
  dot: string;
}

export const STATE_INFO: Record<FeatureState, StateInfo> = {
  planned: {
    label: "Planned",
    blurb: "No landing: a task, a merge or an issue is not the capability",
    badge: "bg-idle-tint text-muted-foreground ring-surface-border",
    dot: "bg-muted-foreground",
  },
  implemented: {
    label: "Implemented",
    blurb: "Landed on the source, not verified in the running product",
    badge: "bg-info-tint text-info ring-info/25",
    dot: "bg-info",
  },
  verified: {
    label: "Verified",
    blurb: "A pass recorded after the newest landing; not deployed yet",
    badge: "bg-ok-tint text-success ring-success/25",
    dot: "bg-success",
  },
  live: {
    label: "Live",
    blurb: "A successful deploy finished after the newest landing",
    badge: "bg-ok-tint text-success ring-success/40",
    dot: "bg-success",
  },
};

export interface FeatureSource {
  kind: string;
  ref: string;
  url: string | null;
  title: string | null;
  /** A record Spawn holds, not an external link. */
  native: boolean;
  /** The native record is present (an external source is never linked). */
  linked: boolean;
}

export interface FeatureGate {
  verdict: string;
  reason: string | null;
  at_ms: number | null;
}

export interface FeatureTask {
  id: number | null;
  state: string;
  kind: string | null;
  agent_id: string | null;
  session_id: string | null;
  objective: number | null;
  revisions: number;
  candidate: string | null;
  base: string | null;
  updated_at_ms: number | null;
  review: {
    note: string | null;
    accepted: boolean;
    sent_back: boolean;
    last_gate: FeatureGate | null;
  };
  gates: FeatureGate[];
}

export interface FeatureLanding {
  objective: number | null;
  task: number | null;
  kind: string;
  at_ms: number | null;
  event: number | null;
  commit: string | null;
  base: string | null;
}

export interface FeatureVerification {
  objective: number | null;
  result: string;
  by: string | null;
  how: string | null;
  at_ms: number | null;
  event: number | null;
}

export interface FeatureDeploy {
  event: number | null;
  at_ms: number | null;
  ok: boolean;
  head: string | null;
  by: string | null;
  reason: string | null;
}

export interface FeatureGap {
  kind: string;
  objectives: number[];
  detail: string | null;
}

export interface FeatureBlocker {
  kind: string;
  reason: string | null;
  event: number | null;
  at_ms: number | null;
  task: number | null;
  agent_id: string | null;
  session_id: string | null;
  scenario: string | null;
}

export interface FeatureOwner {
  task: number | null;
  agent_id: string | null;
  session_id: string | null;
  kind: string | null;
  state: string;
  since_ms: number | null;
}

export interface FeatureEvidence {
  tasks_total: number;
  tasks: FeatureTask[];
  landings: FeatureLanding[];
  verifications: FeatureVerification[];
  deploy: FeatureDeploy | null;
  last_deploy: FeatureDeploy | null;
  newest_landing_at_ms: number | null;
}

export interface FeatureObjective {
  id: number | null;
  title: string | null;
  status: string | null;
}

export interface Feature {
  id: string;
  title: string;
  summary: string | null;
  state: FeatureState;
  planned: boolean;
  implemented: boolean;
  verified: boolean;
  live: boolean;
  complete: boolean;
  objectives: number[];
  objective_rows: FeatureObjective[];
  sources: FeatureSource[];
  owner: FeatureOwner | null;
  last_owner: FeatureOwner | null;
  unowned: boolean;
  blocked: FeatureBlocker | null;
  gaps: FeatureGap[];
  evidence: FeatureEvidence;
}

export interface FeatureMapping {
  kind: "objectives" | "file";
  path: string | null;
  error: string | null;
}

export interface FeatureMission {
  id: number;
  text: string;
  state: string;
  halted: string | null;
  baseline: string | null;
  sources: string[];
  gate: string | null;
  accept: string;
  resident: { name: string | null; session_id: string | null; state: string | null } | null;
  backlog: string | null;
  source: FeatureSource | null;
  mapping: FeatureMapping;
  blocker: FeatureBlocker | null;
  shipped_by_deploy: boolean;
  counts: {
    features: number;
    planned: number;
    implemented: number;
    verified: number;
    live: number;
    blocked: number;
    unowned: number;
    complete: number;
  };
  features: Feature[];
  unmapped: FeatureObjective[];
}

export interface FeatureMap {
  generated_at_ms: number;
  deploy: FeatureDeploy | null;
  live_anchor: FeatureDeploy | null;
  missions: FeatureMission[];
}

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function number(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function bool(value: unknown): boolean {
  return value === true;
}

function stateOf(value: unknown): FeatureState {
  return STATES.includes(value as FeatureState) ? (value as FeatureState) : "planned";
}

function numbers(value: unknown): number[] {
  return list(value)
    .map((item) => optionalNumber(item))
    .filter((item): item is number => item !== null);
}

function normalizeGate(value: unknown): FeatureGate {
  const raw = object(value);
  return {
    verdict: text(raw.verdict, "unknown"),
    reason: optionalText(raw.reason),
    at_ms: optionalNumber(raw.at_ms),
  };
}

function normalizeSource(value: unknown): FeatureSource {
  const raw = object(value);
  return {
    kind: text(raw.kind, "link"),
    ref: text(raw.ref),
    url: optionalText(raw.url),
    title: optionalText(raw.title),
    native: bool(raw.native),
    linked: bool(raw.linked),
  };
}

function normalizeTask(value: unknown): FeatureTask {
  const raw = object(value);
  const review = object(raw.review);
  return {
    id: optionalNumber(raw.id),
    state: text(raw.state, "unknown"),
    kind: optionalText(raw.kind),
    agent_id: optionalText(raw.agent_id),
    session_id: optionalText(raw.session_id),
    objective: optionalNumber(raw.objective),
    revisions: number(raw.revisions),
    candidate: optionalText(raw.candidate),
    base: optionalText(raw.base),
    updated_at_ms: optionalNumber(raw.updated_at_ms),
    review: {
      note: optionalText(review.note),
      accepted: bool(review.accepted),
      sent_back: bool(review.sent_back),
      last_gate:
        review.last_gate === null || review.last_gate === undefined
          ? null
          : normalizeGate(review.last_gate),
    },
    gates: list(raw.gates).map(normalizeGate),
  };
}

function normalizeLanding(value: unknown): FeatureLanding {
  const raw = object(value);
  return {
    objective: optionalNumber(raw.objective),
    task: optionalNumber(raw.task),
    kind: text(raw.kind, "landed"),
    at_ms: optionalNumber(raw.at_ms),
    event: optionalNumber(raw.event),
    commit: optionalText(raw.commit),
    base: optionalText(raw.base),
  };
}

function normalizeVerification(value: unknown): FeatureVerification {
  const raw = object(value);
  return {
    objective: optionalNumber(raw.objective),
    result: text(raw.result, "none"),
    by: optionalText(raw.by),
    how: optionalText(raw.how),
    at_ms: optionalNumber(raw.at_ms),
    event: optionalNumber(raw.event),
  };
}

function normalizeDeploy(value: unknown): FeatureDeploy | null {
  if (value === null || value === undefined) return null;
  const raw = object(value);
  return {
    event: optionalNumber(raw.event),
    at_ms: optionalNumber(raw.at_ms),
    ok: bool(raw.ok),
    head: optionalText(raw.head),
    by: optionalText(raw.by),
    reason: optionalText(raw.reason),
  };
}

function normalizeGap(value: unknown): FeatureGap {
  const raw = object(value);
  return {
    kind: text(raw.kind, "gap"),
    objectives: numbers(raw.objectives),
    detail: optionalText(raw.detail),
  };
}

function normalizeBlocker(value: unknown): FeatureBlocker | null {
  if (value === null || value === undefined) return null;
  const raw = object(value);
  const kind = text(raw.kind);
  if (!kind) return null;
  return {
    kind,
    reason: optionalText(raw.reason),
    event: optionalNumber(raw.event),
    at_ms: optionalNumber(raw.at_ms),
    task: optionalNumber(raw.task),
    agent_id: optionalText(raw.agent_id),
    session_id: optionalText(raw.session_id),
    scenario: optionalText(raw.scenario),
  };
}

function normalizeOwner(value: unknown): FeatureOwner | null {
  if (value === null || value === undefined) return null;
  const raw = object(value);
  return {
    task: optionalNumber(raw.task),
    agent_id: optionalText(raw.agent_id),
    session_id: optionalText(raw.session_id),
    kind: optionalText(raw.kind),
    state: text(raw.state, "unknown"),
    since_ms: optionalNumber(raw.since_ms),
  };
}

function normalizeObjective(value: unknown): FeatureObjective {
  const raw = object(value);
  return {
    id: optionalNumber(raw.id),
    title: optionalText(raw.title),
    status: optionalText(raw.status),
  };
}

function normalizeFeature(value: unknown): Feature {
  const raw = object(value);
  const evidence = object(raw.evidence);
  const state = stateOf(raw.state);
  return {
    id: text(raw.id),
    title: text(raw.title, "Untitled capability"),
    summary: optionalText(raw.summary),
    state,
    // The server's booleans, not the state's: live does not imply verified.
    planned: bool(raw.planned),
    implemented: bool(raw.implemented),
    verified: bool(raw.verified),
    live: bool(raw.live),
    complete: bool(raw.complete),
    objectives: numbers(raw.objectives),
    objective_rows: list(raw.objective_rows).map(normalizeObjective),
    sources: list(raw.sources).map(normalizeSource),
    owner: normalizeOwner(raw.owner),
    last_owner: normalizeOwner(raw.last_owner),
    unowned: bool(raw.unowned),
    blocked: normalizeBlocker(raw.blocked),
    gaps: list(raw.gaps).map(normalizeGap),
    evidence: {
      tasks_total: number(evidence.tasks_total),
      tasks: list(evidence.tasks).map(normalizeTask),
      landings: list(evidence.landings).map(normalizeLanding),
      verifications: list(evidence.verifications).map(normalizeVerification),
      deploy: normalizeDeploy(evidence.deploy),
      last_deploy: normalizeDeploy(evidence.last_deploy),
      newest_landing_at_ms: optionalNumber(evidence.newest_landing_at_ms),
    },
  };
}

function normalizeMission(value: unknown): FeatureMission {
  const raw = object(value);
  const mapping = object(raw.mapping);
  const counts = object(raw.counts);
  const resident = raw.resident === null || raw.resident === undefined ? null : object(raw.resident);
  return {
    id: number(raw.id),
    text: text(raw.text),
    state: text(raw.state, "open"),
    halted: optionalText(raw.halted),
    baseline: optionalText(raw.baseline),
    sources: list(raw.sources).filter((source): source is string => typeof source === "string"),
    gate: optionalText(raw.gate),
    accept: text(raw.accept, "review"),
    resident: resident && {
      name: optionalText(resident.name),
      session_id: optionalText(resident.session_id),
      state: optionalText(resident.state),
    },
    backlog: optionalText(raw.backlog),
    source: raw.source === null || raw.source === undefined ? null : normalizeSource(raw.source),
    mapping: {
      kind: mapping.kind === "file" ? "file" : "objectives",
      path: optionalText(mapping.path),
      error: optionalText(mapping.error),
    },
    blocker: normalizeBlocker(raw.blocker),
    shipped_by_deploy: bool(raw.shipped_by_deploy),
    counts: {
      features: number(counts.features),
      planned: number(counts.planned),
      implemented: number(counts.implemented),
      verified: number(counts.verified),
      live: number(counts.live),
      blocked: number(counts.blocked),
      unowned: number(counts.unowned),
      complete: number(counts.complete),
    },
    features: list(raw.features).map(normalizeFeature),
    unmapped: list(raw.unmapped).map(normalizeObjective),
  };
}

export function normalizeFeatureMap(value: unknown): FeatureMap {
  const raw = object(value);
  return {
    generated_at_ms: number(raw.generated_at_ms),
    deploy: normalizeDeploy(raw.deploy),
    live_anchor: normalizeDeploy(raw.live_anchor),
    missions: list(raw.missions).map(normalizeMission),
  };
}

/** The capabilities in one column each, planned first; an unknown state is
 * planned, so a capability never disappears from the page. */
export function groupByState(features: Feature[]): Array<{
  state: FeatureState;
  info: StateInfo;
  features: Feature[];
}> {
  return STATES.map((state) => ({
    state,
    info: STATE_INFO[state],
    features: features.filter((feature) => feature.state === state),
  }));
}

/** `4/003`: an objective's name in the mission. */
export function objectiveName(mission: number, objective: number | null): string {
  return objective === null ? "objective ?" : `${mission}/${String(objective).padStart(3, "0")}`;
}

/** What a mapping gap is about, in words: the objectives it names, or its
 * detail. */
export function gapText(gap: FeatureGap, mission: number): string {
  const named = gap.objectives.map((objective) => objectiveName(mission, objective)).join(", ");
  switch (gap.kind) {
    case "objective_missing":
      return `mapping names ${named}, not in the backlog`;
    case "not_landed":
      return `not landed: ${named}`;
    case "unverified":
      return `landed, not verified: ${named}`;
    case "verification_failed":
      return `verification failed: ${named}`;
    case "not_deployed":
      return `landed after the newest successful deploy: ${named}`;
    case "no_deploy_record":
      return "no deploy record: live is unknown";
    case "live_not_observable":
      return "no deploy ships this source: live is not observable";
    case "no_task":
      return "no task has ever been assigned";
    case "no_objective":
      return "names no objective";
    case "accepted_without_landing":
      return "accepted outside a gate: it never lands";
    default:
      return gap.detail ?? gap.kind;
  }
}

/** A source's short name: `Objective 4/003`, `Issue #63`, `Owner request`. */
export function sourceLabel(source: FeatureSource): string {
  const reference = source.ref;
  const number = /^(\d+)$/.exec(reference)?.[1] ?? null;
  switch (source.kind) {
    case "objective":
      return `Objective ${reference.replace(/^objective:/, "")}`;
    case "verification":
      return `Verification ${reference.replace(/^verification:/, "")}`;
    case "mission":
      return `Mission ${reference.replace(/^mission:/, "")}`;
    case "task":
      return `Task #${reference.replace(/^task:/, "")}`;
    case "deploy":
      return `Deploy #${reference.replace(/^deploy:/, "")}`;
    case "issue":
      return number ? `Issue #${number}` : `Issue ${reference.replace(/^issue:/, "")}`;
    case "pr":
      return number ? `PR #${number}` : `PR ${reference.replace(/^pr:/, "")}`;
    case "request":
      return "Owner request";
    default: {
      const bare = reference.startsWith(`${source.kind}:`)
        ? reference.slice(source.kind.length + 1)
        : reference;
      return `${source.kind} ${bare}`;
    }
  }
}

/** Whether an external source is a real link. */
export function isExternal(source: FeatureSource): boolean {
  return !source.native && source.url !== null;
}

/** The newest verification evidence, or null. */
export function newestVerification(feature: Feature): FeatureVerification | null {
  let newest: FeatureVerification | null = null;
  for (const verification of feature.evidence.verifications) {
    if (verification.result === "none") continue;
    if (
      newest === null ||
      (verification.at_ms ?? 0) > (newest.at_ms ?? 0)
    ) {
      newest = verification;
    }
  }
  return newest;
}

/** One line about verification: a live capability nobody verified says so. */
export function verificationNote(feature: Feature): string {
  const newest = newestVerification(feature);
  if (newest === null) {
    return feature.live ? "never verified" : "no verification recorded";
  }
  if (newest.result !== "pass") {
    return `verification ${newest.result} by ${newest.by ?? "someone"}`;
  }
  return `verified by ${newest.by ?? "someone"}`;
}

/** The deploy a live claim stands on, or the newest deploy of any outcome. */
export function deployNote(feature: Feature): string {
  const deploy = feature.evidence.deploy ?? feature.evidence.last_deploy;
  if (deploy === null) return "no deploy record";
  const head = deploy.head ? deploy.head.slice(0, 12) : "unknown";
  return deploy.ok
    ? `deployed ${head}`
    : `deploy ${head} failed${deploy.reason ? `: ${deploy.reason}` : ""}`;
}

/** The blocker in one line. */
export function blockerText(blocker: FeatureBlocker): string {
  switch (blocker.kind) {
    case "mission_halted":
      return `mission halted${blocker.reason ? `: ${blocker.reason}` : ""}`;
    case "mission_paused":
      return `mission paused${blocker.reason ? `: ${blocker.reason}` : ""}`;
    case "main_red":
      return `main is red${blocker.scenario ? ` on ${blocker.scenario}` : ""}`;
    case "sent_back":
      return `task #${blocker.task ?? "?"} sent back${blocker.reason ? `: ${blocker.reason}` : ""}`;
    case "cancelled":
      return `task #${blocker.task ?? "?"} cancelled${blocker.reason ? `: ${blocker.reason}` : ""}`;
    case "accepted_without_landing":
      return "accepted outside a gate: it never lands";
    default:
      return blocker.kind;
  }
}

/** The owner, or who last held the work, in one line. */
export function ownerText(owner: FeatureOwner | null): string | null {
  if (owner === null) return null;
  const who = owner.agent_id ?? owner.session_id ?? "unknown agent";
  return `${who} · task #${owner.task ?? "?"} (${owner.state})`;
}
