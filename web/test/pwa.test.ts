// The PWA shell (issue #91 P1b): lives under web/src/pwa/ and is embedded at
// compile time into `spawn serve` by crates/spawn-cli/src/serve/pwa.rs.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

const web = new URL("../", import.meta.url);
const shell = new URL("src/pwa/", web);
const read = (path: string, root = shell) => readFileSync(new URL(path, root), "utf8");

interface Answer {
  ok: boolean;
  type: string;
  from: string;
  clone(): Answer;
}

/** The embedded sw.js run in a sandbox: its fetch handler, the caches it wrote
 * (by cache name, in the order stored), and a network that answers at once
 * unless told otherwise. */
function worker(network: (url: string) => Promise<Answer> = async (url) => answer(url)) {
  const listeners = new Map<string, (event: unknown) => void>();
  const caches = new Map<string, Map<string, Answer>>();
  const keyOf = (key: unknown) => (typeof key === "string" ? key : (key as { url: string }).url);
  const cacheNamed = (name: string) => {
    const entries = caches.get(name) ?? new Map<string, Answer>();
    caches.set(name, entries);
    return {
      match: async (key: unknown) => entries.get(keyOf(key)),
      put: async (key: unknown, value: Answer) => void entries.set(keyOf(key), value),
      addAll: async () => undefined,
      keys: async () => [...entries.keys()],
      delete: async (key: unknown) => entries.delete(keyOf(key)),
    };
  };
  const self = {
    location: { origin: "https://spawn.example" },
    addEventListener: (type: string, fn: (event: unknown) => void) => listeners.set(type, fn),
    skipWaiting: async () => undefined,
    clients: { claim: async () => undefined },
  };
  const context = vm.createContext({
    self,
    caches: { open: async (name: string) => cacheNamed(name), keys: async () => [...caches.keys()], delete: async () => true },
    fetch: async (request: { url: string }) => network(request.url),
    URL,
    Promise,
    Error,
    Math,
    setTimeout,
    clearTimeout,
  });
  vm.runInContext(read("sw.js"), context);
  /** Whether the worker answered a request (respondWith), and with what. */
  const handle = async (url: string, init: { mode?: string; method?: string; headers?: Record<string, string> } = {}) => {
    let reply: Promise<Answer> | null = null;
    const headers = new Map(Object.entries(init.headers ?? {}).map(([k, v]) => [k.toLowerCase(), v]));
    listeners.get("fetch")!({
      request: {
        url: new URL(url, "https://spawn.example").toString(),
        mode: init.mode ?? "cors",
        method: init.method ?? "GET",
        headers: { has: (name: string) => headers.has(name.toLowerCase()) },
      },
      respondWith: (value: Promise<Answer>) => (reply = value),
    });
    return reply === null ? null : await reply;
  };
  /** Every key stored, in every cache. */
  const stored = () => [...caches.values()].flatMap((entries) => [...entries.keys()]);
  return { handle, stored, caches };
}

function answer(from: string): Answer {
  const value: Answer = { ok: true, type: "basic", from, clone: () => value };
  return value;
}

test("the service worker never answers or stores the API, a token, a write or itself", async () => {
  const { handle, stored } = worker();
  assert.equal(await handle("/api/fleet"), null);
  assert.equal(await handle("/api/stream", { headers: { Accept: "text/event-stream" } }), null);
  assert.equal(await handle("/api/master/message", { method: "POST" }), null);
  assert.equal(await handle("/assets/index-abc.js", { headers: { Authorization: "Bearer t" } }), null);
  assert.equal(await handle("/?token=secret", { mode: "navigate" }), null);
  assert.equal(await handle("/api/sessions/x", { mode: "navigate" }), null);
  assert.equal(await handle("/sw.js"), null);
  assert.equal(await handle("https://elsewhere.example/assets/a.js"), null);
  assert.deepEqual(stored(), []);
});

/** Review (minor): only the page itself is kept under "/"; a navigation
 * to the manifest, the icon, the worker or an asset never is. */
test("a navigation is kept under / only from / or /index.html", async () => {
  const { handle, stored, caches } = worker();
  assert.ok(await handle("/?present", { mode: "navigate" }));
  assert.ok(await handle("/index.html", { mode: "navigate" }));
  for (const path of ["/manifest.webmanifest", "/icon.svg", "/sw.js", "/assets/index-abc.js", "/elsewhere"]) {
    assert.equal(await handle(path, { mode: "navigate" }), null, path);
  }
  assert.deepEqual(stored(), ["/"]);
  const page = [...caches.values()].find((entries) => entries.has("/"))!.get("/")!;
  assert.equal(page.from, "https://spawn.example/index.html", "the last page fetched");
  // The manifest fetched as itself is kept under its own path.
  assert.ok(await handle("/manifest.webmanifest"));
  assert.deepEqual(stored().sort(), ["/", "/manifest.webmanifest"]);
});

test("a slow or failed network answers the page from its kept copy", async () => {
  let mode: "up" | "slow" | "down" = "up";
  const { handle } = worker(async (url) => {
    if (mode === "down") throw new TypeError("Failed to fetch");
    if (mode === "slow") return new Promise<Answer>((resolve) => setTimeout(() => resolve(answer(url)), 60_000).unref());
    return answer(`${url}#fresh`);
  });
  assert.equal((await handle("/", { mode: "navigate" }))!.from, "https://spawn.example/#fresh");
  mode = "down";
  assert.equal((await handle("/", { mode: "navigate" }))!.from, "https://spawn.example/#fresh");
  mode = "slow";
  const started = Date.now();
  assert.equal((await handle("/", { mode: "navigate" }))!.from, "https://spawn.example/#fresh");
  assert.ok(Date.now() - started < 10_000, "answered after the navigation's timeout, not the network's minute");
});

test("hashed assets are cache first, at most sixty kept, the oldest evicted", async () => {
  let fetched = 0;
  const { handle, caches } = worker(async (url) => {
    fetched += 1;
    return answer(url);
  });
  await handle("/assets/a-1.js");
  await handle("/assets/a-1.js");
  assert.equal(fetched, 1, "the second read is the cache's");
  for (let at = 2; at <= 70; at += 1) await handle(`/assets/a-${at}.js`);
  const assets = [...caches.values()].find((entries) => [...entries.keys()].some((key) => key.includes("/assets/")))!;
  assert.equal(assets.size, 60);
  assert.ok(!assets.has("https://spawn.example/assets/a-1.js"), "the oldest went");
  assert.ok(assets.has("https://spawn.example/assets/a-70.js"));
});

test("the page links the manifest and the manifest names the icon served beside it", () => {
  const html = read("index.html", web);
  assert.match(html, /<link rel="manifest" href="\/manifest.webmanifest" \/>/);
  assert.match(html, /<meta name="theme-color"/);
  const manifest = JSON.parse(read("manifest.webmanifest"));
  assert.equal(manifest.start_url, "/");
  assert.equal(manifest.display, "standalone");
  for (const icon of manifest.icons) {
    assert.equal(icon.src, "/icon.svg");
    assert.ok(read("icon.svg").startsWith("<svg"));
  }
});
