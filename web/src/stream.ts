// The live log (issue #91 P1b): `GET /api/stream`, Server-Sent Events read
// with `fetch()` and a `ReadableStream`, not `EventSource`, so the serve
// token goes in the `Authorization` header like every other API read and
// never in a query string (crates/spawn-cli/src/serve/stream.rs).
//
// - The parser ([`SseParser`]) is the HTML standard's event-stream
//   algorithm: lines end in CRLF, LF or CR (a CR at a chunk's end waits for
//   the next chunk), `data` lines join with LF, `id` sets the last event id,
//   `retry` takes digits only, a line starting with `:` is a comment
//   (`: keepalive`), an empty line dispatches.
// - Resume: every event's `id` is kept, and a reconnect sends it as both
//   `Last-Event-ID` and `?after=<id>` (the server takes the header first,
//   the same value), so nothing is missed or shown twice. `event: ready`
//   says the stream is live (and, with no cursor yet, where it starts);
//   `event: resync` says the server could not catch this page up (too far
//   behind, a reset log, a slow page): every view reads its board again.
// - Reconnect: a failed or ended stream is tried again after a pause that
//   doubles from the server's `retry:` (at least [`BACKOFF_MIN_MS`]) to
//   [`BACKOFF_MAX_MS`], jittered ([`backoffMs`]); a stream silent for
//   [`STALE_MS`] (the server's keepalive is every 15 s) is dropped and made
//   again. A page that shows again, or a network that comes back, connects
//   at once instead of waiting out its pause.
// - Tabs share one stream: the tab holding the Web Lock [`LOCK_NAME`] is
//   the leader, reads the stream and forwards every signal to the others
//   over the `BroadcastChannel` [`CHANNEL_NAME`]; when it closes, the next
//   tab takes the lock and resumes from the last id it heard. Without Web
//   Locks (a plain-http origin other than localhost: Web Locks need a
//   secure context) or without `BroadcastChannel` each tab reads its own
//   stream, and closes it while the tab is hidden (resuming from its cursor
//   when it shows again), so open tabs never take every one of the
//   browser's six connections to the host.
//
// Pure: no React, no import, every browser API injected ([`LiveDeps`]), so
// web/test/stream.test.ts drives it with fakes.

export const STREAM_URL = "/api/stream";
export const LOCK_NAME = "spawn.stream.leader";
export const CHANNEL_NAME = "spawn.stream";
export const BACKOFF_MIN_MS = 1_000;
export const BACKOFF_MAX_MS = 30_000;
/** No byte for this long (two keepalives missed, and some) ends a stream. */
export const STALE_MS = 40_000;
/** A 401 or 403 is not worth trying every second: the token is wrong. */
export const UNAUTHORIZED_RETRY_MS = 60_000;

/** One dispatched frame of an event stream. */
export interface SseFrame {
  event: string;
  data: string;
  /** The last event id after this frame (the stream's, kept across frames). */
  id: string | null;
  /** A `retry:` the frame carried, in milliseconds. */
  retry: number | null;
}

/** The event-stream parser of the HTML standard, fed text as it arrives. */
export class SseParser {
  private buffer = "";
  private started = false;
  private pendingCr = false;
  private event = "";
  private data: string[] = [];
  private hasData = false;
  private retry: number | null = null;
  lastEventId: string | null = null;

  /** The frames `chunk` completes, in order. */
  feed(chunk: string): SseFrame[] {
    let text = chunk;
    if (!this.started && text.length > 0) {
      this.started = true;
      if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);
    }
    if (this.pendingCr) {
      this.pendingCr = false;
      if (text.startsWith("\n")) text = text.slice(1);
    }
    this.buffer += text;
    const frames: SseFrame[] = [];
    let start = 0;
    for (let at = 0; at < this.buffer.length; at += 1) {
      const code = this.buffer.charCodeAt(at);
      if (code !== 10 && code !== 13) continue;
      const line = this.buffer.slice(start, at);
      if (code === 13) {
        if (at + 1 === this.buffer.length) this.pendingCr = true;
        else if (this.buffer.charCodeAt(at + 1) === 10) at += 1;
      }
      start = at + 1;
      const frame = this.line(line);
      if (frame) frames.push(frame);
    }
    this.buffer = this.buffer.slice(start);
    return frames;
  }

  private line(line: string): SseFrame | null {
    if (line === "") return this.dispatch();
    if (line.startsWith(":")) return null;
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    switch (field) {
      case "event":
        this.event = value;
        break;
      case "data":
        this.data.push(value);
        this.hasData = true;
        break;
      case "id":
        if (!value.includes("\u0000")) this.lastEventId = value;
        break;
      case "retry":
        if (/^\d+$/.test(value)) this.retry = Number(value);
        break;
      default:
        break;
    }
    return null;
  }

  private dispatch(): SseFrame | null {
    const frame: SseFrame | null = this.hasData
      ? { event: this.event || "message", data: this.data.join("\n"), id: this.lastEventId, retry: this.retry }
      : this.retry !== null
        ? { event: "", data: "", id: this.lastEventId, retry: this.retry }
        : null;
    this.event = "";
    this.data = [];
    this.hasData = false;
    this.retry = null;
    return frame;
  }
}

/** What the live stream tells its listeners. */
export type LiveSignal =
  /** One event of the log, as `/api/events` lists it. */
  | { kind: "event"; id: number; name: string; data: unknown }
  /** The stream is live: `replayed` events were caught up first. */
  | { kind: "ready"; latest_event_id: number | null; replayed: number }
  /** The page could not be caught up: read everything again. */
  | { kind: "resync"; reason: string; latest_event_id: number | null }
  | { kind: "status"; status: LiveStatus };

export type LiveState =
  | "idle"
  /** Closed while the tab is hidden (tabs not sharing one stream). */
  | "paused"
  | "connecting"
  | "live"
  | "retrying"
  | "unauthorized";

export interface LiveStatus {
  state: LiveState;
  /** When the next attempt is made, while retrying. */
  retry_at_ms: number | null;
  /** Why the last attempt failed, if it did. */
  error: string | null;
  /** This tab reads the stream itself (false: another tab shares it). */
  leader: boolean;
  last_event_id: number | null;
}

/** The pause before attempt `attempt` (0 first): `min` doubling to `max`,
 * between half and all of it (`random` in [0, 1)). */
export function backoffMs(attempt: number, random: number, min = BACKOFF_MIN_MS, max = BACKOFF_MAX_MS): number {
  const ceiling = Math.min(max, min * 2 ** Math.max(0, Math.min(attempt, 30)));
  return Math.round(ceiling / 2 + (ceiling / 2) * Math.min(Math.max(random, 0), 1));
}

/** The stream's URL resuming after `lastId` (`?after=<id>`), or `base`. */
export function streamUrl(base: string, lastId: number | null): string {
  if (lastId === null) return base;
  return `${base}${base.includes("?") ? "&" : "?"}after=${lastId}`;
}

/** The request headers of a stream resuming after `lastId`: the page's
 * own (the bearer token among them), asking for an event stream. */
export function streamHeaders(page: Record<string, string>, lastId: number | null): Record<string, string> {
  const headers: Record<string, string> = { ...page, Accept: "text/event-stream", "Cache-Control": "no-cache" };
  if (lastId !== null) headers["Last-Event-ID"] = String(lastId);
  return headers;
}

const wholeId = (value: unknown): number | null =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;

function json(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

const record = (value: unknown): Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};

/** A frame as a signal, and the cursor after it (`cursor` before it). */
export function frameSignal(frame: SseFrame, cursor: number | null): { signal: LiveSignal | null; cursor: number | null } {
  if (frame.event === "ready") {
    const body = record(json(frame.data));
    const latest = wholeId(body.latest_event_id);
    const replayed = wholeId(body.replayed) ?? 0;
    // Without a cursor the stream starts live at `latest`: a reconnect
    // resumes from there. With one, the replayed frames that follow move it.
    return { signal: { kind: "ready", latest_event_id: latest, replayed }, cursor: cursor ?? latest };
  }
  if (frame.event === "resync") {
    const body = record(json(frame.data));
    const reason = typeof body.reason === "string" ? body.reason : "resync";
    const latest = wholeId(body.latest_event_id);
    // A slow page keeps its cursor (the reconnect catches it up); any
    // other resync goes on from the log's end now.
    const next = reason === "slow_consumer" ? cursor : latest;
    return { signal: { kind: "resync", reason, latest_event_id: latest }, cursor: next };
  }
  if (frame.event === "") return { signal: null, cursor };
  const id = frame.id !== null && /^\d+$/.test(frame.id) ? Number(frame.id) : null;
  if (id === null || !Number.isSafeInteger(id)) return { signal: null, cursor };
  if (cursor !== null && id <= cursor) return { signal: null, cursor };
  return { signal: { kind: "event", id, name: frame.event, data: json(frame.data) }, cursor: id };
}

/** What a view reads again for a signal. */
export type LiveTopic = "fleet" | "overview" | "attention" | "missions" | "nodes" | "approvals" | "digest" | "alerts";

export const ALL_TOPICS: readonly LiveTopic[] = [
  "fleet",
  "overview",
  "attention",
  "missions",
  "nodes",
  "approvals",
  "digest",
  "alerts",
];

/** The board topics a `control.recorded` event's record makes stale, by
 * its `type` (`detail.record.type`: `task.gated`, `mission.configured`,
 * `alert.fired`, `node.joined`...); a record of no type it knows marks all
 * three, so nothing it might have changed stays stale. */
export function controlTopics(recordType: string | null): LiveTopic[] {
  if (recordType === null) return ["missions", "alerts", "nodes"];
  const topics: LiveTopic[] = [];
  if (/^(mission|task|objective|resident|gate)\./.test(recordType)) topics.push("missions");
  if (/^alert\./.test(recordType)) topics.push("alerts");
  if (/^node\./.test(recordType)) topics.push("nodes");
  return topics.length > 0 ? topics : ["missions", "alerts", "nodes"];
}

/** A `control.recorded` event's record type, as `/api/events` lists it
 * (`{..., event_name, detail: {record: {type, ...}}}`); null without one. */
export function recordType(data: unknown): string | null {
  const type = record(record(record(data).detail).record).type;
  return typeof type === "string" && type !== "" ? type : null;
}

/** The topics a signal makes stale: any event the fleet's boards; a
 * `control.recorded` one (how missions, tasks, objectives, alerts and
 * nodes are written) by its record's type ([`controlTopics`]); an event
 * named for a mission, task, node or alert by its name; a resync or a
 * catch-up everything; a status none. */
export function topicsOf(signal: LiveSignal): readonly LiveTopic[] {
  if (signal.kind === "status") return [];
  if (signal.kind === "resync") return ALL_TOPICS;
  if (signal.kind === "ready") return signal.replayed > 0 ? ALL_TOPICS : [];
  const name = signal.name;
  const topics = new Set<LiveTopic>(["fleet", "overview", "attention", "approvals", "digest"]);
  if (name === "control.recorded") for (const topic of controlTopics(recordType(signal.data))) topics.add(topic);
  if (/^(mission|task|objective|gate)\./.test(name)) topics.add("missions");
  if (name.startsWith("node.")) topics.add("nodes");
  if (name.startsWith("alert")) topics.add("alerts");
  return [...topics];
}

/** The parts of a `BroadcastChannel` used. */
export interface ChannelLike {
  postMessage(message: unknown): void;
  addEventListener(type: "message", listener: (event: { data: unknown }) => void): void;
  close(): void;
}

/** The part of the Web Locks API used. */
export interface LocksLike {
  request(name: string, callback: () => Promise<void>): Promise<unknown>;
}

export interface VisibilityLike {
  visibilityState: string;
  addEventListener(type: "visibilitychange", listener: () => void): void;
}

export interface LiveDeps {
  fetch: (url: string, init: { headers: Record<string, string>; signal: AbortSignal; cache: "no-store" }) => Promise<{
    ok: boolean;
    status: number;
    body: { getReader(): { read(): Promise<{ done: boolean; value?: Uint8Array }>; cancel(): Promise<void> } } | null;
  }>;
  headers: () => Record<string, string>;
  url?: string;
  channel?: (() => ChannelLike | null) | null;
  locks?: LocksLike | null;
  document?: VisibilityLike | null;
  /** Where `online` is heard (the window). */
  online?: { addEventListener(type: "online", listener: () => void): void } | null;
  now?: () => number;
  random?: () => number;
  setTimer?: (callback: () => void, ms: number) => unknown;
  clearTimer?: (timer: unknown) => void;
}

type Wire =
  | { type: "signal"; signal: LiveSignal }
  | { type: "hello" }
  | { type: "wake" };

/** The live stream of one page, shared among its tabs (module doc). */
export class LiveStream {
  private readonly deps: Required<Omit<LiveDeps, "channel" | "locks" | "document" | "online">> &
    Pick<LiveDeps, "channel" | "locks" | "document" | "online">;
  private readonly listeners = new Set<(signal: LiveSignal) => void>();
  private channel: ChannelLike | null = null;
  private started = false;
  private leader = false;
  private cursor: number | null = null;
  private attempt = 0;
  private retryFloor = BACKOFF_MIN_MS;
  private timer: unknown = null;
  private staleTimer: unknown = null;
  private abort: AbortController | null = null;
  private lastByteAt = 0;
  /** Tabs do not share this stream: it is closed while the tab is hidden. */
  private solo = false;
  private paused = false;
  private stopped = false;
  private lockRequested = false;
  private releaseLock: (() => void) | null = null;
  /** The stream being ended is made again at once, without a pause. */
  private reconnectNow = false;
  private current: LiveStatus = { state: "idle", retry_at_ms: null, error: null, leader: false, last_event_id: null };

  constructor(deps: LiveDeps) {
    this.deps = {
      url: STREAM_URL,
      now: () => Date.now(),
      random: Math.random,
      setTimer: (callback, ms) => setTimeout(callback, ms),
      clearTimer: (timer) => clearTimeout(timer as ReturnType<typeof setTimeout>),
      ...deps,
    };
  }

  get status(): LiveStatus {
    return this.current;
  }

  /** The last event id this tab heard. */
  get lastEventId(): number | null {
    return this.cursor;
  }

  subscribe(listener: (signal: LiveSignal) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** Begin (or resume after [`stop`]): lead when this tab gets the lock, else follow. */
  start(): void {
    this.stopped = false;
    if (this.started) {
      this.paused = false;
      this.attempt = 0;
      if (!this.solo && !this.leader) {
        this.requestLeadership();
        return;
      }
      if (!this.leader) {
        this.wake();
        return;
      }
      if (this.solo && this.hidden()) {
        this.paused = true;
        this.setStatus({ state: "paused", retry_at_ms: null, leader: true });
        return;
      }
      if (this.abort !== null) {
        this.reconnectNow = true;
        this.abort.abort();
        return;
      }
      this.clear();
      void this.connect();
      return;
    }
    this.started = true;
    try {
      this.channel = this.deps.channel?.() ?? null;
    } catch {
      this.channel = null;
    }
    this.channel?.addEventListener("message", (event) => this.heard(event.data as Wire));
    this.deps.document?.addEventListener("visibilitychange", () => {
      if (this.deps.document?.visibilityState === "visible") this.wake();
      else if (this.solo) this.pause();
    });
    this.deps.online?.addEventListener("online", () => this.wake());
    const locks = this.channel ? this.deps.locks : null;
    if (!locks) {
      this.solo = true;
      this.leader = true;
      if (this.hidden()) {
        this.paused = true;
        this.setStatus({ state: "paused", retry_at_ms: null, leader: true });
        return;
      }
      this.leader = false;
      this.lead();
      return;
    }
    this.requestLeadership();
  }

  /** Stop reconnecting, close any active stream, and release the shared lock if held. */
  stop(): void {
    this.stopped = true;
    this.paused = true;
    this.reconnectNow = false;
    this.attempt = 0;
    this.clear();
    this.abort?.abort();
    const stepDown = !this.solo && this.leader;
    if (stepDown) this.leader = false;
    const release = this.releaseLock;
    this.releaseLock = null;
    release?.();
    this.setStatus({ state: "idle", retry_at_ms: null, error: null, leader: this.leader });
  }

  private requestLeadership(): void {
    const locks = this.channel ? this.deps.locks : null;
    if (!locks || this.lockRequested || this.stopped) return;
    this.lockRequested = true;
    this.setStatus({ state: "connecting", retry_at_ms: null, error: null, leader: false });
    this.post({ type: "hello" });
    locks
      .request(LOCK_NAME, () => {
        if (this.stopped) return Promise.resolve();
        this.lead();
        return new Promise<void>((resolve) => {
          this.releaseLock = resolve;
        });
      })
      .catch(() => {
        if (!this.stopped) this.lead();
      })
      .finally(() => {
        this.lockRequested = false;
        this.releaseLock = null;
      });
  }

  /** Connect now if this tab leads and is not connected (a page shown
   * again, a network back), and make a stale stream again at once; a
   * follower asks the leader. */
  wake(): void {
    if (!this.started || this.stopped) return;
    if (!this.leader) {
      this.post({ type: "wake" });
      return;
    }
    if (this.solo && this.hidden()) return;
    this.paused = false;
    if (this.abort !== null) {
      if (this.deps.now() - this.lastByteAt > STALE_MS) {
        // Ended now and made again by its own connect, with no pause.
        this.reconnectNow = true;
        this.abort.abort();
      }
      return;
    }
    this.clear();
    this.attempt = 0;
    void this.connect();
  }

  /** Close the stream while the tab is hidden (tabs not sharing one); the
   * cursor is kept, and [`wake`] resumes from it. */
  private pause(): void {
    if (this.paused) return;
    this.paused = true;
    this.clear();
    this.abort?.abort();
    this.setStatus({ state: "paused", retry_at_ms: null, error: null });
  }

  private hidden(): boolean {
    return this.deps.document?.visibilityState === "hidden";
  }

  private lead(): void {
    if (this.leader) return;
    this.leader = true;
    this.setStatus({ leader: true });
    if (!this.stopped && !this.paused) void this.connect();
  }

  private heard(message: Wire): void {
    if (this.stopped || !message || typeof message !== "object") return;
    if (message.type === "hello") {
      if (this.leader) this.post({ type: "signal", signal: { kind: "status", status: this.current } });
      return;
    }
    if (message.type === "wake") {
      if (this.leader) this.wake();
      return;
    }
    if (message.type !== "signal" || this.leader) return;
    const signal = message.signal;
    if (signal.kind === "status") {
      this.cursor = signal.status.last_event_id ?? this.cursor;
      this.setStatus({ ...signal.status, leader: false });
      return;
    }
    if (signal.kind === "event") this.cursor = Math.max(this.cursor ?? 0, signal.id);
    if (signal.kind === "resync") this.cursor = signal.latest_event_id ?? this.cursor;
    this.emit(signal);
  }

  private post(message: Wire): void {
    try {
      this.channel?.postMessage(message);
    } catch {
      // A closed channel: this tab goes on alone.
    }
  }

  private emit(signal: LiveSignal): void {
    for (const listener of [...this.listeners]) {
      try {
        listener(signal);
      } catch {
        // One listener's mistake is not the stream's.
      }
    }
  }

  /** Tell this tab's listeners and, as the leader, the other tabs. */
  private send(signal: LiveSignal): void {
    this.emit(signal);
    if (this.leader) this.post({ type: "signal", signal });
  }

  private setStatus(change: Partial<LiveStatus>): void {
    this.current = { ...this.current, ...change, last_event_id: this.cursor };
    this.send({ kind: "status", status: this.current });
  }

  private clear(): void {
    if (this.timer !== null) this.deps.clearTimer(this.timer);
    this.timer = null;
  }

  private schedule(error: string, unauthorized = false): void {
    this.clear();
    const wait = unauthorized
      ? UNAUTHORIZED_RETRY_MS
      : backoffMs(this.attempt, this.deps.random(), Math.max(BACKOFF_MIN_MS, this.retryFloor));
    this.attempt += 1;
    this.setStatus({
      state: unauthorized ? "unauthorized" : "retrying",
      retry_at_ms: this.deps.now() + wait,
      error,
    });
    this.timer = this.deps.setTimer(() => {
      this.timer = null;
      void this.connect();
    }, wait);
  }

  private watchStale(controller: AbortController): void {
    if (this.staleTimer !== null) this.deps.clearTimer(this.staleTimer);
    this.staleTimer = this.deps.setTimer(() => {
      this.staleTimer = null;
      if (this.abort === controller && this.deps.now() - this.lastByteAt >= STALE_MS) controller.abort();
      else if (this.abort === controller) this.watchStale(controller);
    }, STALE_MS);
  }

  private async connect(): Promise<void> {
    if (this.abort !== null) return;
    const controller = new AbortController();
    this.abort = controller;
    this.lastByteAt = this.deps.now();
    this.setStatus({ state: "connecting", retry_at_ms: null });
    let failure = "the stream ended";
    let unauthorized = false;
    // Armed before the headers come: a server that never answers is a
    // stream gone quiet too.
    this.watchStale(controller);
    try {
      const response = await this.deps.fetch(streamUrl(this.deps.url, this.cursor), {
        headers: streamHeaders(this.deps.headers(), this.cursor),
        signal: controller.signal,
        cache: "no-store",
      });
      if (response.status === 401) {
        unauthorized = true;
        failure = "HTTP 401: the token is missing or wrong";
      } else if (!response.ok || response.body === null) {
        failure = `HTTP ${response.status}`;
      } else {
        failure = await this.read(response.body.getReader(), controller);
        if (controller.signal.aborted) failure = "no keepalive: the stream went quiet";
      }
    } catch (cause) {
      failure = controller.signal.aborted
        ? "no keepalive: the stream went quiet"
        : cause instanceof Error
          ? cause.message
          : "the server was not reached";
    } finally {
      if (this.abort === controller) this.abort = null;
      if (this.staleTimer !== null) this.deps.clearTimer(this.staleTimer);
      this.staleTimer = null;
    }
    if (this.paused) return;
    if (this.reconnectNow) {
      this.reconnectNow = false;
      this.attempt = 0;
      void this.connect();
      return;
    }
    this.schedule(failure, unauthorized);
  }

  private async read(
    reader: { read(): Promise<{ done: boolean; value?: Uint8Array }>; cancel(): Promise<void> },
    controller: AbortController,
  ): Promise<string> {
    const parser = new SseParser();
    const decoder = new TextDecoder();
    controller.signal.addEventListener("abort", () => void reader.cancel().catch(() => undefined));
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return "the stream ended";
      this.lastByteAt = this.deps.now();
      for (const frame of parser.feed(decoder.decode(value, { stream: true }))) {
        if (frame.retry !== null) this.retryFloor = frame.retry;
        const { signal, cursor } = frameSignal(frame, this.cursor);
        this.cursor = cursor;
        if (!signal) continue;
        if (signal.kind === "ready") {
          this.attempt = 0;
          this.setStatus({ state: "live", retry_at_ms: null, error: null });
        }
        this.send(signal);
        if (signal.kind === "event" && this.current.last_event_id !== this.cursor) {
          this.current = { ...this.current, last_event_id: this.cursor };
        }
      }
    }
  }
}
