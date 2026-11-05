// Reads of `/api/events`: the whole event log in log order, a page at a
// time, after a position or from a time.
import { apiError, apiHeaders } from "./api";
import type { LogEvent } from "./show";

export interface EventPage {
  events: LogEvent[];
  /** Position to read after next time. */
  next: number;
  /** Server clock when it answered. */
  now_ms: number;
}

/** Page size asked for; the server's cap. */
const PAGE = 1000;

function num(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function str(value: unknown): string {
  return typeof value === "string" ? value : "";
}

export function normalizeEventPage(value: unknown): EventPage {
  const body = (value ?? {}) as Record<string, unknown>;
  const rows = Array.isArray(body.events) ? body.events : [];
  const events: LogEvent[] = rows.map((row) => {
    const event = (row ?? {}) as Record<string, unknown>;
    return {
      id: num(event.id),
      agent_id: str(event.agent_id),
      session_id: str(event.session_id),
      execution_id: typeof event.execution_id === "string" ? event.execution_id : null,
      source: str(event.source),
      event_name: str(event.event_name),
      detail: event.detail ?? null,
      recorded_at_ms: num(event.recorded_at_ms),
    };
  });
  return { events, next: num(body.next), now_ms: num(body.now_ms, Date.now()) };
}

export async function fetchEvents(
  start: { after: number } | { sinceMs: number },
  signal: AbortSignal,
): Promise<EventPage> {
  const from = "after" in start ? `after=${start.after}` : `since_ms=${Math.max(0, Math.floor(start.sinceMs))}`;
  const response = await fetch(`/api/events?${from}&limit=${PAGE}`, {
    signal,
    cache: "no-store",
    headers: apiHeaders(),
  });
  if (!response.ok) throw apiError(response.status);
  return normalizeEventPage(await response.json());
}

/** Every event from `sinceMs` to now, page after page; with `before`,
 * only those at positions below it. `keep` drops what the caller does not
 * need as the pages arrive. */
export async function fetchEventsSince(
  sinceMs: number,
  signal: AbortSignal,
  options: { before?: number; keep?: (event: LogEvent) => boolean } = {},
): Promise<EventPage> {
  const { before = Number.POSITIVE_INFINITY, keep = () => true } = options;
  const take = (page: EventPage) => page.events.filter((event) => event.id < before && keep(event));
  let page = await fetchEvents({ sinceMs }, signal);
  const events = take(page);
  while (page.events.length >= PAGE && page.next < before) {
    page = await fetchEvents({ after: page.next }, signal);
    events.push(...take(page));
  }
  return { events, next: page.next, now_ms: page.now_ms };
}
