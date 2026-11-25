// The offline outbox of Master Control (issue #91 P1b): a message to the
// supervisor is written here with its idempotency key before it is sent,
// and leaves only when the server took it (or refused it for good), or the
// person discards it. A message the network dropped stays, in IndexedDB,
// across reloads and restarts of the browser, and is sent again when the
// page is back online, shown again or reconnected, with the same key: the
// server answers a repeated request with the first call's command ids, so
// a replay types it once, however many times (or from however many tabs)
// it is sent. A message the caller says is too old to send by itself (an
// expired steer or restart, master.ts `expired`) is held for the person.
//
// Without IndexedDB (a private window that refuses it, an old browser, one
// that does not open within [`IDB_OPEN_TIMEOUT_MS`]) the outbox lives in
// memory: it still retries while the page is open, and says it does not
// survive a reload ([`OutboxStore.durable`]). A write IndexedDB fails (a
// full disk, an aborted transaction) moves the outbox to memory too, with
// the reason ([`Outbox.degraded`]), rather than losing the message.
//
// Pure (no imports), tested in web/test/outbox.test.ts.

export interface OutboxEntry {
  /** The idempotency key, the message's for good. */
  key: string;
  text: string;
  mode: string;
  created_at_ms: number;
  attempts: number;
  last_error: string | null;
}

export interface OutboxStore {
  /** Kept across reloads (IndexedDB), or in this page only (memory). */
  readonly durable: boolean;
  all(): Promise<OutboxEntry[]>;
  put(entry: OutboxEntry): Promise<void>;
  remove(key: string): Promise<void>;
}

export class MemoryOutboxStore implements OutboxStore {
  readonly durable = false;
  private readonly entries = new Map<string, OutboxEntry>();

  async all(): Promise<OutboxEntry[]> {
    return [...this.entries.values()].map((entry) => ({ ...entry }));
  }

  async put(entry: OutboxEntry): Promise<void> {
    this.entries.set(entry.key, { ...entry });
  }

  async remove(key: string): Promise<void> {
    this.entries.delete(key);
  }
}

export const OUTBOX_DB = "spawn.outbox";
export const OUTBOX_STORE = "messages";
/** How long IndexedDB may take to open before the outbox is in memory. */
export const IDB_OPEN_TIMEOUT_MS = 3_000;

/** The parts of the IndexedDB API used (the browser's `indexedDB`). */
export interface IdbFactoryLike {
  open(name: string, version?: number): IDBOpenDBRequest;
}

/** A request's result once its transaction is done: committed
 * (`oncomplete`), not merely answered, so a write the transaction then
 * aborts (a full disk) is a failure, never a message thought kept. */
function committed<T>(transaction: IDBTransaction, request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve(request.result);
    transaction.onerror = () =>
      reject(transaction.error ?? request.error ?? new Error("the IndexedDB transaction failed"));
    transaction.onabort = () => reject(transaction.error ?? new Error("the IndexedDB transaction was aborted"));
  });
}

export class IdbOutboxStore implements OutboxStore {
  readonly durable = true;
  private readonly db: IDBDatabase;

  private constructor(db: IDBDatabase) {
    this.db = db;
  }

  /** Open the outbox's database, or fail within `timeoutMs`. */
  static async open(factory: IdbFactoryLike, timeoutMs = IDB_OPEN_TIMEOUT_MS): Promise<IdbOutboxStore> {
    const request = factory.open(OUTBOX_DB, 1);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(OUTBOX_STORE)) db.createObjectStore(OUTBOX_STORE, { keyPath: "key" });
    };
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("IndexedDB did not open in time")), timeoutMs);
      request.onsuccess = () => {
        clearTimeout(timer);
        resolve(request.result);
      };
      request.onerror = () => {
        clearTimeout(timer);
        reject(request.error ?? new Error("IndexedDB did not open"));
      };
      request.onblocked = () => {
        clearTimeout(timer);
        reject(new Error("IndexedDB is blocked by another tab"));
      };
    });
    return new IdbOutboxStore(db);
  }

  private run<T>(mode: IDBTransactionMode, act: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
    const transaction = this.db.transaction(OUTBOX_STORE, mode);
    return committed(transaction, act(transaction.objectStore(OUTBOX_STORE)));
  }

  async all(): Promise<OutboxEntry[]> {
    const rows = await this.run("readonly", (store) => store.getAll());
    return (rows as OutboxEntry[]).filter((row) => row && typeof row.key === "string");
  }

  async put(entry: OutboxEntry): Promise<void> {
    await this.run("readwrite", (store) => store.put(entry));
  }

  async remove(key: string): Promise<void> {
    await this.run("readwrite", (store) => store.delete(key));
  }
}

/** The durable store when IndexedDB opens in time, else one in memory. */
export async function openOutboxStore(
  factory: IdbFactoryLike | null | undefined,
  timeoutMs = IDB_OPEN_TIMEOUT_MS,
): Promise<OutboxStore> {
  if (!factory) return new MemoryOutboxStore();
  try {
    return await IdbOutboxStore.open(factory, timeoutMs);
  } catch {
    return new MemoryOutboxStore();
  }
}

/** What one send of an entry came to. */
export type SendOutcome = { state: "sent" } | { state: "retry"; error: string } | { state: "refused"; error: string };

export interface FlushResult {
  sent: OutboxEntry[];
  refused: { entry: OutboxEntry; error: string }[];
  /** Still waiting (the first one failed to reach the server). */
  kept: OutboxEntry[];
  /** Too old to send by themselves (`hold`): waiting for the person. */
  held: OutboxEntry[];
}

const message = (cause: unknown, fallback: string) => (cause instanceof Error ? cause.message : fallback);

/** The outbox over a store: add, then flush in the order written. */
export class Outbox {
  private current: OutboxStore;
  private flushing: Promise<FlushResult> | null = null;
  private again = false;
  private readonly now: () => number;
  /** Why the outbox moved from IndexedDB to memory, if it did. */
  degraded: string | null = null;

  constructor(store: OutboxStore, now: () => number = () => Date.now()) {
    this.current = store;
    this.now = now;
  }

  get store(): OutboxStore {
    return this.current;
  }

  /** Move to memory after a durable store failed, keeping what it still
   * gives; the entry being written is written there. */
  private async fallBack(cause: unknown): Promise<void> {
    if (!this.current.durable) throw cause;
    const memory = new MemoryOutboxStore();
    const kept = await this.current.all().catch(() => [] as OutboxEntry[]);
    for (const entry of kept) await memory.put(entry);
    this.degraded = `IndexedDB failed (${message(cause, "a write failed")}): the outbox is kept in this tab only`;
    this.current = memory;
  }

  private async write(act: (store: OutboxStore) => Promise<void>): Promise<void> {
    try {
      await act(this.current);
    } catch (cause) {
      await this.fallBack(cause);
      await act(this.current);
    }
  }

  /** Write a message before it is sent: kept in memory if IndexedDB fails. */
  async add(key: string, text: string, mode: string): Promise<OutboxEntry> {
    const entry: OutboxEntry = { key, text, mode, created_at_ms: this.now(), attempts: 0, last_error: null };
    await this.write((store) => store.put(entry));
    return entry;
  }

  /** The waiting messages, oldest first. */
  async pending(): Promise<OutboxEntry[]> {
    let entries: OutboxEntry[];
    try {
      entries = await this.current.all();
    } catch (cause) {
      await this.fallBack(cause);
      entries = await this.current.all();
    }
    return entries.sort((left, right) => left.created_at_ms - right.created_at_ms);
  }

  /** The person discards a waiting message: it is never sent. */
  async discard(key: string): Promise<void> {
    await this.write((store) => store.remove(key));
  }

  /** The person sends a held message anyway: it is as if written now (its
   * key unchanged, so a copy that did reach the server types once). */
  async renew(key: string): Promise<void> {
    const entry = (await this.pending()).find((candidate) => candidate.key === key);
    if (!entry) return;
    await this.write((store) => store.put({ ...entry, created_at_ms: this.now() }));
  }

  /** Send every waiting message, oldest first, each with its own key; one
   * `hold` says is too old is left for the person. One flush at a time: a
   * flush asked during another runs once more after it (so a message
   * added meanwhile is not left behind), and both callers get the last
   * result. The first message the server is not reached for stops the
   * flush (the network is down: the rest wait with it). */
  flush(
    send: (entry: OutboxEntry) => Promise<SendOutcome>,
    hold: (entry: OutboxEntry) => boolean = () => false,
  ): Promise<FlushResult> {
    if (this.flushing) {
      this.again = true;
      return this.flushing;
    }
    const run = async (): Promise<FlushResult> => {
      const result: FlushResult = { sent: [], refused: [], kept: [], held: [] };
      do {
        this.again = false;
        result.kept = [];
        result.held = [];
        const entries = await this.pending();
        for (let at = 0; at < entries.length; at += 1) {
          const entry = entries[at];
          if (hold(entry)) {
            result.held.push(entry);
            continue;
          }
          let outcome: SendOutcome;
          try {
            outcome = await send(entry);
          } catch (cause) {
            outcome = { state: "retry", error: message(cause, "send failed") };
          }
          if (outcome.state === "sent") {
            await this.write((store) => store.remove(entry.key));
            result.sent.push(entry);
          } else if (outcome.state === "refused") {
            await this.write((store) => store.remove(entry.key));
            result.refused.push({ entry, error: outcome.error });
          } else {
            const kept = { ...entry, attempts: entry.attempts + 1, last_error: outcome.error };
            await this.write((store) => store.put(kept));
            const rest = entries.slice(at + 1);
            result.kept = [kept, ...rest.filter((other) => !hold(other))];
            result.held.push(...rest.filter(hold));
            this.again = false;
            break;
          }
        }
      } while (this.again);
      return result;
    };
    this.flushing = run().finally(() => {
      this.flushing = null;
    });
    return this.flushing;
  }
}
