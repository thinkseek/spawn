import assert from "node:assert/strict";
import { test } from "node:test";

import type { OutboxEntry, SendOutcome } from "../src/outbox.ts";
import type { IdbFactoryLike } from "../src/outbox.ts";
import { IdbOutboxStore, MemoryOutboxStore, Outbox, openOutboxStore } from "../src/outbox.ts";

function counter() {
  let now = 0;
  return () => (now += 1);
}

test("without IndexedDB, or when it fails to open, the outbox lives in memory", async () => {
  const none = await openOutboxStore(null);
  assert.equal(none.durable, false);
  const broken = await openOutboxStore({
    open: () => {
      throw new Error("SecurityError");
    },
  });
  assert.equal(broken.durable, false);
});

test("a message the network dropped is kept and replayed with the same key until taken", async () => {
  const box = new Outbox(new MemoryOutboxStore(), counter());
  await box.add("key-1", "first", "auto");
  await box.add("key-2", "second", "steer");
  const seen: string[] = [];
  let online = false;
  // The server: the same key again answers the first call (typed once).
  const typed = new Map<string, number>();
  const send = async (entry: OutboxEntry): Promise<SendOutcome> => {
    seen.push(entry.key);
    if (!online) return { state: "retry", error: "Cannot reach the server" };
    typed.set(entry.key, (typed.get(entry.key) ?? 0) + 1);
    return { state: "sent" };
  };
  const down = await box.flush(send);
  assert.deepEqual(down.sent, []);
  assert.deepEqual(
    down.kept.map((entry) => entry.key),
    ["key-1", "key-2"],
  );
  assert.equal(down.kept[0].attempts, 1);
  assert.equal(down.kept[0].last_error, "Cannot reach the server");
  assert.deepEqual(seen, ["key-1"], "the first failure stops the flush");
  online = true;
  const up = await box.flush(send);
  assert.deepEqual(
    up.sent.map((entry) => [entry.key, entry.text, entry.mode]),
    [
      ["key-1", "first", "auto"],
      ["key-2", "second", "steer"],
    ],
  );
  assert.deepEqual(await box.pending(), []);
  assert.deepEqual(seen, ["key-1", "key-1", "key-2"], "each replay carries its own key");
});

test("a refused message leaves the outbox with its reason; a throw is a retry", async () => {
  const box = new Outbox(new MemoryOutboxStore(), counter());
  await box.add("a", "bad", "auto");
  await box.add("b", "good", "auto");
  const result = await box.flush(async (entry) =>
    entry.key === "a" ? { state: "refused", error: "the key was used" } : { state: "sent" },
  );
  assert.deepEqual(result.refused.map(({ entry, error }) => [entry.key, error]), [["a", "the key was used"]]);
  assert.deepEqual(result.sent.map((entry) => entry.key), ["b"]);
  await box.add("c", "thrown", "auto");
  const thrown = await box.flush(async () => {
    throw new Error("TypeError: Failed to fetch");
  });
  assert.equal(thrown.kept[0].key, "c");
  assert.equal(thrown.kept[0].last_error, "TypeError: Failed to fetch");
});

test("flushes never overlap, and one asked meanwhile picks up a message added since", async () => {
  const box = new Outbox(new MemoryOutboxStore(), counter());
  await box.add("one", "1", "auto");
  let inFlight = 0;
  let most = 0;
  const sent: string[] = [];
  let gate: () => void = () => undefined;
  const send = async (entry: OutboxEntry): Promise<SendOutcome> => {
    inFlight += 1;
    most = Math.max(most, inFlight);
    if (entry.key === "one") await new Promise<void>((resolve) => (gate = resolve));
    sent.push(entry.key);
    inFlight -= 1;
    return { state: "sent" };
  };
  const first = box.flush(send);
  await new Promise((resolve) => setTimeout(resolve, 0));
  await box.add("two", "2", "auto");
  const second = box.flush(send);
  assert.equal(first, second, "the same flush");
  gate();
  const result = await first;
  assert.equal(most, 1);
  assert.deepEqual(sent, ["one", "two"], "each sent once, in order");
  assert.deepEqual(result.sent.map((entry) => entry.key), ["one", "two"]);
  assert.deepEqual(await box.pending(), []);
});

/** An IndexedDB of one object store, in memory, as the outbox uses it:
 * `open` succeeds (or never answers: `hang`), and a write's transaction
 * commits, or aborts after its request succeeded (`abortWrites`: a full
 * disk), as a browser's does. */
function fakeIdb(options: { hang?: boolean; abortWrites?: () => boolean } = {}): IdbFactoryLike {
  const rows = new Map<string, unknown>();
  const db = {
    objectStoreNames: { contains: () => true },
    createObjectStore: () => undefined,
    transaction: (_name: string, mode: string) => {
      const transaction: Record<string, unknown> = {};
      const finish = (request: Record<string, unknown>, result: unknown, write: () => void) => {
        setTimeout(() => {
          request.result = result;
          (request.onsuccess as (() => void) | undefined)?.();
          if (mode === "readwrite" && options.abortWrites?.()) {
            transaction.error = new Error("QuotaExceededError");
            (transaction.onabort as () => void)();
            return;
          }
          write();
          (transaction.oncomplete as () => void)();
        }, 0);
        return request;
      };
      transaction.objectStore = () => ({
        getAll: () => finish({}, [...rows.values()], () => undefined),
        put: (entry: { key: string }) => finish({}, entry.key, () => rows.set(entry.key, structuredClone(entry))),
        delete: (key: string) => finish({}, undefined, () => rows.delete(key)),
      });
      return transaction;
    },
  };
  return {
    open: () => {
      const request: Record<string, unknown> = {};
      if (!options.hang) {
        setTimeout(() => {
          request.result = db;
          (request.onsuccess as () => void)();
        }, 0);
      }
      return request as unknown as IDBOpenDBRequest;
    },
  };
}

test("IndexedDB keeps the outbox; one that does not open in time leaves it in memory", async () => {
  const durable = await openOutboxStore(fakeIdb());
  assert.equal(durable.durable, true);
  const box = new Outbox(durable, counter());
  await box.add("k", "kept", "auto");
  assert.deepEqual((await box.pending()).map((entry) => entry.text), ["kept"]);
  const slow = await openOutboxStore(fakeIdb({ hang: true }), 20);
  assert.equal(slow.durable, false);
  await assert.rejects(IdbOutboxStore.open(fakeIdb({ hang: true }), 10), /did not open in time/);
});

/** Review S2: a write is kept only when its transaction commits; one that
 * aborts after its request succeeded is a failure, and the outbox moves to
 * memory with the message rather than losing it. */
test("a write IndexedDB aborts moves the outbox to memory with the message", async () => {
  let failing = false;
  const store = await openOutboxStore(fakeIdb({ abortWrites: () => failing }));
  await assert.rejects(
    (async () => {
      failing = true;
      await store.put({ key: "x", text: "", mode: "auto", created_at_ms: 0, attempts: 0, last_error: null });
    })(),
    /QuotaExceededError/,
  );
  failing = false;
  const box = new Outbox(store, counter());
  await box.add("before", "first", "auto");
  failing = true;
  await box.add("after", "second", "queue");
  assert.equal(box.store.durable, false);
  assert.match(box.degraded ?? "", /QuotaExceededError/);
  assert.deepEqual(
    (await box.pending()).map((entry) => entry.key),
    ["before", "after"],
    "what IndexedDB held and the new message",
  );
  // A memory store that fails has nowhere to go: the error is the caller's.
  const broken = new Outbox(
    {
      durable: false,
      all: async () => [],
      put: async () => {
        throw new Error("no room");
      },
      remove: async () => undefined,
    },
    counter(),
  );
  await assert.rejects(broken.add("k", "t", "auto"), /no room/);
});

test("a message too old to send by itself is held; the person discards it or sends it anyway", async () => {
  let now = 0;
  const box = new Outbox(new MemoryOutboxStore(), () => (now += 1));
  await box.add("old-steer", "stop that", "steer");
  await box.add("note", "fyi", "auto");
  const sent: string[] = [];
  const send = async (entry: OutboxEntry): Promise<SendOutcome> => {
    sent.push(entry.key);
    return { state: "sent" };
  };
  const result = await box.flush(send, (entry) => entry.key === "old-steer" && entry.created_at_ms < 10);
  assert.deepEqual(sent, ["note"]);
  assert.deepEqual(result.held.map((entry) => entry.key), ["old-steer"]);
  assert.deepEqual((await box.pending()).map((entry) => entry.key), ["old-steer"]);
  // Sent anyway: as if written now, with its key.
  now = 100;
  await box.renew("old-steer");
  const renewed = await box.flush(send, (entry) => entry.created_at_ms < 10);
  assert.deepEqual(renewed.sent.map((entry) => entry.key), ["old-steer"]);
  // Discarded: never sent.
  await box.add("restart", "restart it", "restart");
  await box.discard("restart");
  assert.deepEqual(await box.pending(), []);
  await box.renew("gone");
});
