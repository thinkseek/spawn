import type { Alert } from "./api.ts";

/** A notice no later notice supersedes, and how many notices'
 * `superseded_by` chains end at it (objective 151). */
export interface CurrentNotice {
  alert: Alert;
  earlier: number;
}

/** The notices of `alerts` no later notice supersedes, in the answer's
 * order, each with how many came before it: the notices whose
 * `superseded_by` chain ends at it. A notice whose next one is not in the
 * answer stands as current, so nothing is hidden. Open and cleared rows are
 * not notices and are never listed. */
export function currentNotices(alerts: readonly Alert[]): CurrentNotice[] {
  const notices = alerts.filter((alert) => alert.state === "notice");
  const byKey = new Map(notices.map((alert) => [alert.key, alert]));
  const next = (alert: Alert): Alert | undefined => {
    const key = alert.superseded_by;
    return typeof key === "string" ? byKey.get(key) : undefined;
  };
  const earlier = new Map<string, number>();
  for (const alert of notices) {
    // Follow the chain to its end; a cycle (never served) stops it.
    let at = alert;
    const seen = new Set([at.key]);
    for (let step = next(at); step !== undefined && !seen.has(step.key); step = next(at)) {
      at = step;
      seen.add(at.key);
    }
    if (at !== alert) earlier.set(at.key, (earlier.get(at.key) ?? 0) + 1);
  }
  return notices
    .filter((alert) => next(alert) === undefined)
    .map((alert) => ({ alert, earlier: earlier.get(alert.key) ?? 0 }));
}

/** "N earlier", or null for none. */
export function earlierWords(earlier: number): string | null {
  return earlier > 0 ? `${earlier} earlier` : null;
}

export type MainUpdateKind = "fast_forward" | "rewound" | "non_fast_forward";

export interface SourceMainNotice {
  kind: "source_main_advanced" | "source_main_synced";
  title: string;
  source: string;
  ref: string;
  from: string | null;
  to: string | null;
  range: string | null;
  updateKind: MainUpdateKind | null;
  badge: string;
  oldReachable: boolean;
  ahead: number | null;
  behind: number | null;
  delta: string | null;
  summary: string;
  subjects: string[];
  moreSubjects: number;
  missions: number[];
  reason: string | null;
  landingEventId: number | null;
}

type JsonObject = Record<string, unknown>;

const MAX_SUBJECTS = 10;
const MAX_SUBJECT_CHARS = 160;
const URL_WITH_SCHEME = /^[a-z][a-z0-9+.-]*:\/\//i;

function asObject(value: unknown): JsonObject | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : null;
}

function cleanInline(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const cleaned = value.replace(/[\u0000-\u001f\u007f]+/g, " ").replace(/\s+/g, " ").trim();
  return cleaned === "" ? null : cleaned;
}

function sanitizeSource(value: unknown): string {
  const cleaned = cleanInline(value);
  if (!cleaned) return "";
  // Strip embedded URL userinfo and query/fragment tokens before rendering.
  return cleaned
    .split(" ")
    .map((part) =>
      URL_WITH_SCHEME.test(part)
        ? part.replace(/^([a-z][a-z0-9+.-]*:\/\/)[^/@?#\s]+@/i, "$1").replace(/[?#].*$/, "")
        : part,
    )
    .join(" ");
}

function cleanRefSha(value: unknown): string | null {
  const cleaned = cleanInline(value);
  if (!cleaned) return null;
  const token = cleaned.replace(/[^0-9a-zA-Z._-]/g, "");
  return token === "" ? null : token;
}

function shortSha(sha: string | null): string | null {
  return sha ? sha.slice(0, 12) : null;
}

function nonNegInt(value: unknown): number | null {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : null;
}

function parseUpdateKind(value: unknown): MainUpdateKind | null {
  return value === "fast_forward" || value === "rewound" || value === "non_fast_forward"
    ? value
    : null;
}

function parseMissions(value: unknown): number[] {
  if (!Array.isArray(value)) return [];
  const unique = new Set<number>();
  for (const entry of value) {
    if (typeof entry === "number" && Number.isInteger(entry) && entry > 0) unique.add(entry);
  }
  return [...unique].sort((a, b) => a - b);
}

function parseSubjects(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const entry of value) {
    if (out.length >= MAX_SUBJECTS) break;
    const cleaned = cleanInline(entry);
    if (!cleaned) continue;
    const sanitized = sanitizeSource(cleaned);
    out.push(
      sanitized.length > MAX_SUBJECT_CHARS
        ? `${sanitized.slice(0, MAX_SUBJECT_CHARS - 1)}…`
        : sanitized,
    );
  }
  return out;
}

function advanceBadge(kind: MainUpdateKind | null): string {
  switch (kind) {
    case "fast_forward":
      return "fast-forward";
    case "rewound":
      return "rewound";
    case "non_fast_forward":
      return "non-fast-forward";
    default:
      return "updated";
  }
}

function advanceDelta(
  kind: MainUpdateKind | null,
  oldReachable: boolean,
  ahead: number | null,
  behind: number | null,
): string | null {
  if (kind === "fast_forward" && ahead !== null) {
    return `+${ahead} commit${ahead === 1 ? "" : "s"}`;
  }
  if (kind === "rewound" && behind !== null) {
    return `-${behind} commit${behind === 1 ? "" : "s"}`;
  }
  if (kind === "non_fast_forward" && !oldReachable) {
    return "previous commit unreachable";
  }
  if (ahead !== null || behind !== null) {
    return `+${ahead ?? 0} / -${behind ?? 0}`;
  }
  return null;
}

/** Format mission ids as a compact label, or null when none are attached. */
export function missionListText(missions: readonly number[]): string | null {
  if (missions.length === 0) return null;
  const noun = missions.length === 1 ? "mission" : "missions";
  return `${noun} ${missions.map((id) => `#${id}`).join(", ")}`;
}

/** Structured read model for PR #171 `source_main_advanced` and
 * `source_main_synced` notices. Returns null for all other alert kinds and
 * tolerates sparse or malformed evidence without exposing URL credentials. */
export function sourceMainNotice(
  alert: Pick<Alert, "kind" | "subject" | "text" | "evidence">,
): SourceMainNotice | null {
  if (alert.kind !== "source_main_advanced" && alert.kind !== "source_main_synced") {
    return null;
  }
  const ev = asObject(alert.evidence);
  const source = sanitizeSource(ev?.source ?? alert.subject);
  const ref = cleanInline(ev?.ref) ?? "refs/heads/main";
  const from = shortSha(cleanRefSha(ev?.from));
  const to = shortSha(cleanRefSha(ev?.to ?? ev?.head));
  const range = from && to ? `${from}..${to}` : to;
  const missions = parseMissions(ev?.missions);
  const missionLabel = missionListText(missions);

  if (alert.kind === "source_main_synced") {
    const reason = cleanInline(ev?.reason);
    const badge = reason ?? "synced";
    const landingEventId = nonNegInt(ev?.landing_event_id);
    const title = "Main tracking synced";
    const summary =
      reason === "baseline" && to
        ? `Tracking ${source || "source"} ${ref} at ${to}`
        : range
          ? `Synced ${source || "source"} ${ref} (${range}${reason ? `, ${reason}` : ""})`
          : sanitizeSource(alert.text) || `${title} on ${source || "source"}`;
    return {
      kind: "source_main_synced",
      title,
      source,
      ref,
      from,
      to,
      range,
      updateKind: null,
      badge,
      oldReachable: true,
      ahead: null,
      behind: null,
      delta: null,
      summary: missionLabel ? `${summary} (${missionLabel})` : summary,
      subjects: [],
      moreSubjects: 0,
      missions,
      reason,
      landingEventId: landingEventId !== null && landingEventId > 0 ? landingEventId : null,
    };
  }

  const updateKind = parseUpdateKind(ev?.update_kind);
  const badge = advanceBadge(updateKind);
  const oldReachable = ev?.old_reachable !== false;
  const ahead = nonNegInt(ev?.ahead);
  const behind = nonNegInt(ev?.behind);
  const delta = advanceDelta(updateKind, oldReachable, ahead, behind);
  const subjects = parseSubjects(ev?.subjects);
  const moreSubjects = oldReachable && ahead !== null && ahead > subjects.length ? ahead - subjects.length : 0;
  const title = "External main advance";
  const detailParts = [badge, delta].filter((part): part is string => part !== null);
  const rangePart = range ? ` ${range}` : "";
  const detailPart = detailParts.length > 0 ? ` (${detailParts.join(", ")})` : "";
  const baseSummary =
    ev || range || source
      ? `${source || "source"} ${ref} moved externally${rangePart}${detailPart}`
      : sanitizeSource(alert.text) || title;
  return {
    kind: "source_main_advanced",
    title,
    source,
    ref,
    from,
    to,
    range,
    updateKind,
    badge,
    oldReachable,
    ahead,
    behind,
    delta,
    summary: missionLabel ? `${baseSummary} (${missionLabel})` : baseSummary,
    subjects,
    moreSubjects,
    missions,
    reason: null,
    landingEventId: null,
  };
}

/** Structured row title for source-main notices, or null to keep default row headers. */
export function noticeTitle(
  alert: Pick<Alert, "kind" | "subject" | "text" | "evidence">,
): string | null {
  const parsed = sourceMainNotice(alert);
  if (!parsed) return null;
  return parsed.source ? `${parsed.title} · ${parsed.source}` : parsed.title;
}

/** Allowlisted, credential-free evidence payload for source-main notices; all
 * other alert kinds return their original evidence unchanged. */
export function safeNoticeEvidence(
  alert: Pick<Alert, "kind" | "subject" | "text" | "evidence">,
): unknown {
  if (alert.kind !== "source_main_advanced" && alert.kind !== "source_main_synced") {
    return alert.evidence;
  }
  const ev = asObject(alert.evidence);
  if (!ev) return null;
  const parsed = sourceMainNotice(alert);
  if (!parsed) return null;

  const out: JsonObject = {};
  if (parsed.source) out.source = parsed.source;
  if (parsed.ref) out.ref = parsed.ref;
  const fromFull = cleanRefSha(ev.from);
  const toFull = cleanRefSha(ev.to);
  const headFull = cleanRefSha(ev.head);
  if (fromFull) out.from = fromFull;
  if (parsed.kind === "source_main_advanced") {
    if (toFull) out.to = toFull;
    if (parsed.updateKind) out.update_kind = parsed.updateKind;
    if (typeof ev.old_reachable === "boolean") out.old_reachable = ev.old_reachable;
    if (parsed.ahead !== null) out.ahead = parsed.ahead;
    if (parsed.behind !== null) out.behind = parsed.behind;
    if (parsed.subjects.length > 0) out.subjects = parsed.subjects;
  } else {
    if (headFull) out.head = headFull;
    if (parsed.reason) out.reason = parsed.reason;
    if (parsed.landingEventId !== null) out.landing_event_id = parsed.landingEventId;
  }
  if (parsed.missions.length > 0) out.missions = parsed.missions;
  return out;
}

/** How many current notices report external `refs/heads/main` advances. */
export function externalMainAdvanceCount(notices: readonly CurrentNotice[]): number {
  return notices.filter(({ alert }) => alert.kind === "source_main_advanced").length;
}
